//! xdg-desktop-portal Screenshot and ScreenCast clients over the session bus,
//! written against gio's D-Bus API (no extra D-Bus crate). They work with
//! whichever backend the desktop provides (GNOME, KDE, wlroots).
//!
//! Every request follows the portal Request protocol: a unique `handle_token`
//! fixes the Request object path, the `Response` subscription is made before
//! the call so a fast answer cannot be missed, and the wait is bounded. A
//! user's refusal (`response == 1`) is `Ok(None)`; response 2 is `Ok(None)` for Screenshot only
//! (compat caveat: portal-gnome cannot tell Esc, denial and failure apart) and an error for every
//! other call; any other non-success
//! response, timeout or D-Bus error is an error. A request that is abandoned
//! (timeout, or the future dropped) is closed with `Request.Close`.
//!
//! The futures run on the GLib main context; await them from
//! `glib::spawn_future_local` or a nested main loop.

use anyhow::{Context, Result, anyhow, bail, ensure};
use gtk::{gio, glib, glib::VariantDict, glib::VariantTy, prelude::*};
use image::RgbaImage;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

const BUS: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const SHOT: &str = "org.freedesktop.portal.Screenshot";
const CAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";
const REQUEST_PREFIX: &str = "/org/freedesktop/portal/desktop/request/";
const SESSION_PREFIX: &str = "/org/freedesktop/portal/desktop/session/";

/// A method call is answered at once; the user's time is in the `Response`.
const CALL_MS: i32 = 15_000;
/// Portal-side session setup without any user interaction.
const SETUP_WAIT: Duration = Duration::from_secs(20);
/// First use may ask for permission before the capture itself.
const SHOT_WAIT: Duration = Duration::from_secs(120);
/// Source pickers and interactive screenshot dialogs wait for a human.
const USER_WAIT: Duration = Duration::from_secs(300);

const SOURCE_MONITOR: u32 = 1;
const SOURCE_WINDOW: u32 = 2;
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
const CURSOR_METADATA: u32 = 4;

/// Take a screenshot through the portal. `interactive` asks the portal to
/// offer its own selection UI first. `Ok(None)` means no image was produced: the user cancelled
/// (response 1) or the portal ended without a result (response 2, which GNOME 48 uses for Esc,
/// a denied permission and backend failures alike; a note goes to stderr).
///
/// The returned file belongs to the portal or the user (GNOME and KDE keep it
/// in the Pictures folder), so it is read but never deleted.
pub async fn screenshot(interactive: bool) -> Result<Option<RgbaImage>> {
    let conn = connect().await?;
    let wait = if interactive { USER_WAIT } else { SHOT_WAIT };
    let response = request(&conn, SHOT, "Screenshot", wait, |opts| {
        opts.insert("interactive", interactive);
        tuple([String::new().into(), opts.end()])
    })
    .await?;
    let results = match screenshot_outcome(response)? {
        ScreenshotOutcome::Image(results) => results,
        ScreenshotOutcome::Cancelled => return Ok(None),
        ScreenshotOutcome::NotCompleted => {
            eprintln!("{NOT_COMPLETED}");
            return Ok(None);
        }
    };
    let uri = results
        .lookup::<String>("uri")
        .ok()
        .flatten()
        .context("the portal reported success but returned no screenshot URI")?;
    let path = screenshot_path(&uri)?;
    let image = gio::spawn_blocking(move || read_screenshot(&path))
        .await
        .map_err(|_| anyhow!("the screenshot decoder thread panicked"))??;
    Ok(Some(image))
}

/// A started ScreenCast portal session with its single stream. Dropping it
/// closes the portal session, which ends the stream; `fd` is the PipeWire
/// remote and stays valid on its own until dropped.
pub struct ScreenCast {
    /// PipeWire remote for `pw_context_connect_fd` / `pipewiresrc fd=`.
    pub fd: OwnedFd,
    /// Node of the stream on that remote.
    pub node: u32,
    /// Set when the portal emits `Session.Closed` for this session, that is when the desktop (not
    /// this client) ended the cast, e.g. "Stop Screen Sharing". The stream then fails or ends;
    /// classify that as an early stop only when this is true. Delivered on the GLib main context,
    /// so it can lag a PipeWire error slightly.
    pub closed: Arc<AtomicBool>,
    _session: Closer,
    /// Keeps the `Closed` subscriptions alive for the cast's lifetime; unsubscribed on drop.
    _watch: Vec<gio::SignalSubscription>,
}

/// Ask the user for one monitor or window and open its PipeWire stream.
/// `cursor` embeds the pointer in the frames; an unsupported cursor mode is an
/// error. `Ok(None)` means the user cancelled the source picker.
pub async fn screencast(cursor: bool) -> Result<Option<ScreenCast>> {
    let conn = connect().await?;
    let version = property(&conn, CAST, "version").await?;
    let modes = if version >= 2 {
        property(&conn, CAST, "AvailableCursorModes").await?
    } else {
        0
    };
    let cursor_mode = cursor_mode(cursor, version, modes)?;
    let sources =
        property(&conn, CAST, "AvailableSourceTypes").await? & (SOURCE_MONITOR | SOURCE_WINDOW);
    ensure!(
        sources != 0,
        "the portal backend offers neither monitor nor window screen casts"
    );

    let sender = unique_name(&conn)?;
    let token = new_token("session");
    let expected = handle_path("session", &sender, &token);
    // Subscribed before CreateSession so a close that races setup is not missed.
    let closed = Arc::new(AtomicBool::new(false));
    let mut watches = vec![watch_closed(&conn, &expected, &closed)];
    // Armed before CreateSession so a timeout cannot leave a half-created session behind.
    let mut session = Closer::new(&conn, SESSION, expected);
    let created = request(&conn, CAST, "CreateSession", SETUP_WAIT, |opts| {
        opts.insert("session_handle_token", token.as_str());
        tuple([opts.end()])
    })
    .await?
    .done("CreateSession")?;
    let handle = created
        .lookup::<String>("session_handle")
        .ok()
        .flatten()
        .context("the portal returned no session handle")?;
    ensure!(
        handle.starts_with(SESSION_PREFIX) && glib::Variant::is_object_path(&handle),
        "the portal returned an invalid session handle {handle:?}"
    );
    // Portals older than 0.9 ignore session_handle_token and pick the path themselves.
    if handle != session.path {
        watches.push(watch_closed(&conn, &handle, &closed));
    }
    session.path = handle;
    let session_path = object_path(&session.path)?;

    request(&conn, CAST, "SelectSources", SETUP_WAIT, |opts| {
        opts.insert("types", sources);
        opts.insert("multiple", false);
        if let Some(mode) = cursor_mode {
            opts.insert("cursor_mode", mode);
        }
        tuple([session_path.clone(), opts.end()])
    })
    .await?
    .done("SelectSources")?;

    let started = request(&conn, CAST, "Start", USER_WAIT, |opts| {
        tuple([session_path.clone(), String::new().into(), opts.end()])
    })
    .await?;
    let results = match started {
        Response::Done(results) => results,
        Response::Cancelled => return Ok(None),
        ended @ Response::Ended(_) => ended.done("the screen cast source selection")?,
    };
    let node = parse_stream(&results)?;

    let (reply, fds) = conn
        .call_with_unix_fd_list_future(
            Some(BUS),
            PATH,
            CAST,
            "OpenPipeWireRemote",
            Some(&tuple([session_path, VariantDict::new(None).end()])),
            Some(VariantTy::new("(h)")?),
            gio::DBusCallFlags::NONE,
            CALL_MS,
            None::<&gio::UnixFDList>,
        )
        .await
        .map_err(|e| dbus_error("opening the PipeWire remote", e))?;
    let index = reply
        .child_value(0)
        .get::<glib::variant::Handle>()
        .context("OpenPipeWireRemote returned no file descriptor handle")?
        .0;
    let fd = fds
        .context("OpenPipeWireRemote reply carried no file descriptors")?
        .get(index)
        .context("OpenPipeWireRemote returned an invalid file descriptor index")?;
    Ok(Some(ScreenCast {
        fd,
        node,
        closed,
        _session: session,
        _watch: watches,
    }))
}

/// Cursor mode to request, or `None` to keep the pre-version-2 default
/// (hidden). Unsupported modes are errors instead of silent substitution.
fn cursor_mode(cursor: bool, version: u32, available: u32) -> Result<Option<u32>> {
    if cursor {
        ensure!(
            available & CURSOR_EMBEDDED != 0,
            "the portal backend cannot embed the mouse pointer in screen casts; record without the cursor"
        );
        return Ok(Some(CURSOR_EMBEDDED));
    }
    if version < 2 {
        return Ok(None);
    }
    [CURSOR_HIDDEN, CURSOR_METADATA]
        .into_iter()
        .find(|m| available & m != 0)
        .map(Some)
        .context("the portal backend cannot record without the mouse pointer")
}

// Request protocol

enum Response {
    Done(VariantDict),
    /// Response 1: the user declined.
    Cancelled,
    /// Response 2: ended "in some other way" with the printed details, empty when the backend
    /// sent none. GNOME sends this for Esc, a denied permission and backend failures alike.
    Ended(String),
}

impl Response {
    /// Setup steps cannot be cancelled by the user in a meaningful way; every non-success is an error.
    fn done(self, step: &str) -> Result<VariantDict> {
        match self {
            Response::Done(results) => Ok(results),
            Response::Cancelled => bail!("the portal cancelled {step}"),
            Response::Ended(details) => bail!(
                "the portal ended {step} without a result (response 2); access may have been denied or the backend failed{}",
                if details.is_empty() {
                    String::new()
                } else {
                    format!(": {details}")
                }
            ),
        }
    }
}

/// Shown when a Screenshot request ends with response 2 and no details.
const NOT_COMPLETED: &str = "Screenshot not completed: cancelled, denied, or desktop backend failed. If no dialog appears, review your desktop's screenshot permissions.";

enum ScreenshotOutcome {
    Image(VariantDict),
    Cancelled,
    NotCompleted,
}

/// Screenshot-only reading of a `Response`. portal-gnome 48 reports Esc, a denied permission and
/// backend failures all as response 2 without details, so that case is a no-image outcome rather
/// than an error; response 2 that carries details is still an error showing them. ScreenCast
/// setup uses `Response::done`, which never treats response 2 as benign.
fn screenshot_outcome(response: Response) -> Result<ScreenshotOutcome> {
    match response {
        Response::Done(results) => Ok(ScreenshotOutcome::Image(results)),
        Response::Cancelled => Ok(ScreenshotOutcome::Cancelled),
        Response::Ended(details) if details.is_empty() => Ok(ScreenshotOutcome::NotCompleted),
        Response::Ended(details) => bail!("the portal ended the screenshot request: {details}"),
    }
}

/// Call a portal method that returns a Request and wait for its `Response`.
/// `build` receives the options dict (already holding the `handle_token`) and
/// returns the method's complete argument tuple.
async fn request(
    conn: &gio::DBusConnection,
    interface: &str,
    method: &str,
    wait: Duration,
    build: impl FnOnce(&VariantDict) -> glib::Variant,
) -> Result<Response> {
    let token = new_token("request");
    let expected = handle_path("request", &unique_name(conn)?, &token);
    let (tx, rx) = async_channel::unbounded();
    let _subscription = subscribe(conn, &expected, tx.clone());

    let opts = VariantDict::new(None);
    opts.insert("handle_token", token.as_str());
    let reply = conn
        .call_future(
            Some(BUS),
            PATH,
            interface,
            method,
            Some(&build(&opts)),
            Some(VariantTy::new("(o)")?),
            gio::DBusCallFlags::NONE,
            CALL_MS,
        )
        .await
        .map_err(|e| dbus_error(&format!("portal {method}"), e))?;
    let handle = reply
        .child_value(0)
        .str()
        .map(str::to_owned)
        .context("the portal returned no request handle")?;
    ensure!(
        handle.starts_with(REQUEST_PREFIX),
        "the portal returned an invalid request handle {handle:?}"
    );
    // Portals older than 0.9 ignore handle_token and choose the path themselves.
    let _fallback = (handle != expected).then(|| subscribe(conn, &handle, tx));
    let mut request = Closer::new(conn, REQUEST, handle);

    let signal = glib::future_with_timeout(wait, rx.recv())
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s waiting for the portal ({method})",
                wait.as_secs()
            )
        })?
        .map_err(|_| anyhow!("the portal response subscription closed ({method})"))?;
    request.armed = false;
    parse_response(&signal).with_context(|| format!("portal {method}"))
}

fn subscribe(
    conn: &gio::DBusConnection,
    path: &str,
    tx: async_channel::Sender<glib::Variant>,
) -> impl Sized + use<> {
    conn.subscribe_to_signal(
        Some(BUS),
        Some(REQUEST),
        Some("Response"),
        Some(path),
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let _ = tx.try_send(signal.parameters.clone());
        },
    )
}

/// Set `closed` when the portal emits `Session.Closed` for `path`: the backend, not this client,
/// ended the session (for instance the desktop's "Stop Screen Sharing").
fn watch_closed(
    conn: &gio::DBusConnection,
    path: &str,
    closed: &Arc<AtomicBool>,
) -> gio::SignalSubscription {
    let closed = Arc::clone(closed);
    conn.subscribe_to_signal(
        Some(BUS),
        Some(SESSION),
        Some("Closed"),
        Some(path),
        None,
        gio::DBusSignalFlags::NONE,
        move |_| closed.store(true, Ordering::SeqCst),
    )
}

fn parse_response(parameters: &glib::Variant) -> Result<Response> {
    ensure!(
        parameters.is_type(VariantTy::new("(ua{sv})")?),
        "unexpected Response signature {}",
        parameters.type_()
    );
    match parameters.child_value(0).get::<u32>() {
        Some(0) => Ok(Response::Done(VariantDict::new(Some(
            &parameters.child_value(1),
        )))),
        Some(1) => Ok(Response::Cancelled),
        Some(2) => {
            let details = parameters.child_value(1);
            Ok(Response::Ended(if details.n_children() == 0 {
                String::new()
            } else {
                details.print(false).to_string()
            }))
        }
        Some(code) => bail!("the portal answered with unknown response code {code}"),
        None => bail!("malformed Response code"),
    }
}

/// Closes a portal Request or Session object unless disarmed. Fire and
/// forget: a vanished client is also treated as a close by the portal.
struct Closer {
    conn: gio::DBusConnection,
    interface: &'static str,
    path: String,
    armed: bool,
}

impl Closer {
    fn new(conn: &gio::DBusConnection, interface: &'static str, path: String) -> Self {
        Self {
            conn: conn.clone(),
            interface,
            path,
            armed: true,
        }
    }
}

impl Drop for Closer {
    fn drop(&mut self) {
        if self.armed {
            self.conn.call(
                Some(BUS),
                &self.path,
                self.interface,
                "Close",
                None,
                None,
                gio::DBusCallFlags::NONE,
                CALL_MS,
                None::<&gio::Cancellable>,
                |_| (),
            );
        }
    }
}

async fn connect() -> Result<gio::DBusConnection> {
    gio::bus_get_future(gio::BusType::Session)
        .await
        .map_err(|e| dbus_error("connecting to the D-Bus session bus", e))
}

fn unique_name(conn: &gio::DBusConnection) -> Result<String> {
    conn.unique_name()
        .map(|n| n.to_string())
        .context("the session bus connection has no unique name")
}

async fn property(conn: &gio::DBusConnection, interface: &str, name: &str) -> Result<u32> {
    let reply = conn
        .call_future(
            Some(BUS),
            PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(interface, name).to_variant()),
            Some(VariantTy::new("(v)")?),
            gio::DBusCallFlags::NONE,
            CALL_MS,
        )
        .await
        .map_err(|e| dbus_error(&format!("reading portal property {name}"), e))?;
    reply
        .child_value(0)
        .as_variant()
        .and_then(|v| v.get::<u32>())
        .with_context(|| format!("portal property {name} is not an unsigned integer"))
}

fn dbus_error(what: &str, mut error: glib::Error) -> anyhow::Error {
    use gio::DBusError as E;
    let missing = [
        E::ServiceUnknown,
        E::NameHasNoOwner,
        E::UnknownMethod,
        E::UnknownObject,
        E::UnknownInterface,
    ]
    .into_iter()
    .any(|kind| error.matches(kind));
    E::strip_remote_error(&mut error);
    if missing {
        anyhow!(
            "{what}: the xdg-desktop-portal service or its backend does not provide this ({}); \
             install xdg-desktop-portal and the backend for your desktop \
             (xdg-desktop-portal-gnome, -kde or -wlr) and restart it inside this session",
            error.message()
        )
    } else {
        anyhow!("{what}: {}", error.message())
    }
}

fn tuple<const N: usize>(children: [glib::Variant; N]) -> glib::Variant {
    glib::Variant::tuple_from_iter(children)
}

fn object_path(path: &str) -> Result<glib::Variant> {
    Ok(glib::variant::ObjectPath::try_from(path)?.to_variant())
}

/// A token that is a valid object path element, unique per call and not guessable.
fn new_token(kind: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "sharpshot_{kind}_{}_{:08x}{:08x}",
        COUNTER.fetch_add(1, Ordering::Relaxed),
        glib::random_int(),
        glib::random_int()
    )
}

/// `/org/freedesktop/portal/desktop/<kind>/SENDER/TOKEN` with the unique bus
/// name's leading `:` dropped and `.` turned into `_`.
fn handle_path(kind: &str, unique_name: &str, token: &str) -> String {
    format!(
        "/org/freedesktop/portal/desktop/{kind}/{}/{token}",
        unique_name.trim_start_matches(':').replace('.', "_")
    )
}

// Results

/// The one stream of a `multiple = false` session; anything else is a protocol error.
fn parse_stream(results: &VariantDict) -> Result<u32> {
    let streams = results
        .lookup_value("streams", Some(VariantTy::new("a(ua{sv})")?))
        .context("the portal returned no streams")?;
    ensure!(
        streams.n_children() == 1,
        "expected one screen cast stream, the portal returned {}",
        streams.n_children()
    );
    let stream = streams.child_value(0);
    let node = stream
        .child_value(0)
        .get::<u32>()
        .context("malformed stream node")?;
    ensure!(node != 0, "the portal returned stream node 0");
    Ok(node)
}

/// Local path of a portal screenshot URI. Only `file:` URIs without a remote
/// host name an openable file.
fn screenshot_path(uri: &str) -> Result<PathBuf> {
    let (path, host) = glib::filename_from_uri(uri).map_err(|e| {
        anyhow!(
            "the portal returned an unusable screenshot URI: {}",
            e.message()
        )
    })?;
    ensure!(
        host.as_deref()
            .is_none_or(|h| h.is_empty() || h == "localhost"),
        "the portal screenshot is on a remote host"
    );
    ensure!(
        path.is_absolute(),
        "the portal screenshot path is not absolute"
    );
    Ok(path)
}

/// Open without blocking on pipes, require a regular file within the input caps, then decode.
fn read_screenshot(path: &Path) -> Result<RgbaImage> {
    let (file, metadata) = crate::open_regular_file(path)
        .with_context(|| format!("cannot open the portal screenshot {}", path.display()))?;
    ensure!(
        metadata.len() <= crate::MAX_INPUT_BYTES,
        "the portal screenshot is larger than {} MiB",
        crate::MAX_INPUT_BYTES >> 20
    );
    crate::decode_image(std::io::BufReader::new(file))
        .with_context(|| format!("cannot decode the portal screenshot {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(ty: &str, text: &str) -> glib::Variant {
        glib::Variant::parse(Some(VariantTy::new(ty).unwrap()), text).unwrap()
    }

    #[test]
    fn handles_follow_the_portal_naming_rule() {
        assert_eq!(
            handle_path("request", ":1.84", "tok_1"),
            "/org/freedesktop/portal/desktop/request/1_84/tok_1"
        );
        let (a, b) = (new_token("request"), new_token("request"));
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(glib::Variant::is_object_path(&handle_path(
            "session", ":1.2", &a
        )));
    }

    #[test]
    fn responses_distinguish_success_cancel_and_failure() {
        let ok = variant("(ua{sv})", "(uint32 0, {'uri': <'file:///a.png'>})");
        let Response::Done(r) = parse_response(&ok).unwrap() else {
            panic!("expected success")
        };
        assert_eq!(
            r.lookup::<String>("uri").unwrap().as_deref(),
            Some("file:///a.png")
        );
        assert!(matches!(
            parse_response(&variant("(ua{sv})", "(uint32 1, @a{sv} {})")).unwrap(),
            Response::Cancelled
        ));
        assert!(parse_response(&variant("(uu)", "(uint32 0, uint32 0)")).is_err());
        assert!(parse_response(&variant("(ua{sv})", "(uint32 3, @a{sv} {})")).is_err());
    }

    // xdg-desktop-portal-gnome 48 answers a dismissed picker (Esc), a denied permission and a
    // failed shell call all with response 2 and no details (screenshot.c: DELETE_EVENT and
    // InteractiveScreenshot errors/`success == false` map to 2; only the Cancel button is 1).
    #[test]
    fn gnome_style_response_two_is_not_completed_not_success_and_not_an_error() {
        let empty = parse_response(&variant("(ua{sv})", "(uint32 2, @a{sv} {})")).unwrap();
        assert!(matches!(&empty, Response::Ended(d) if d.is_empty()));
        assert!(matches!(
            screenshot_outcome(empty).unwrap(),
            ScreenshotOutcome::NotCompleted
        ));

        // Real cancellation stays silent; success yields the results.
        let cancelled = parse_response(&variant("(ua{sv})", "(uint32 1, @a{sv} {})")).unwrap();
        assert!(matches!(
            screenshot_outcome(cancelled).unwrap(),
            ScreenshotOutcome::Cancelled
        ));
        let ok = parse_response(&variant(
            "(ua{sv})",
            "(uint32 0, {'uri': <'file:///a.png'>})",
        ))
        .unwrap();
        assert!(matches!(
            screenshot_outcome(ok).unwrap(),
            ScreenshotOutcome::Image(_)
        ));

        // When a backend does return details they are kept, not masked as a plain no-image outcome.
        let detailed = parse_response(&variant(
            "(ua{sv})",
            "(uint32 2, {'error': <'compositor refused'>})",
        ))
        .unwrap();
        let error = screenshot_outcome(detailed)
            .err()
            .expect("details must surface");
        assert!(format!("{error:#}").contains("compositor refused"));
    }

    #[test]
    fn setup_steps_never_swallow_response_two() {
        let ended = parse_response(&variant("(ua{sv})", "(uint32 2, @a{sv} {})")).unwrap();
        assert!(ended.done("Start").is_err());
    }

    #[test]
    fn streams_must_contain_exactly_one_nonzero_node() {
        let dict = |text: &str| VariantDict::new(Some(&variant("a{sv}", text)));
        let one = dict(
            "{'streams': <[(uint32 57, {'size': <(1920, 1080)>, 'position': <(-1920, 0)>})]>}",
        );
        assert_eq!(parse_stream(&one).unwrap(), 57);
        let bare = dict("{'streams': <[(uint32 7, @a{sv} {})]>}");
        assert_eq!(parse_stream(&bare).unwrap(), 7);

        assert!(parse_stream(&dict("{}")).is_err());
        assert!(parse_stream(&dict("{'streams': <@a(ua{sv}) []>}")).is_err());
        assert!(
            parse_stream(&dict(
                "{'streams': <[(uint32 1, @a{sv} {}), (uint32 2, @a{sv} {})]>}"
            ))
            .is_err()
        );
        assert!(parse_stream(&dict("{'streams': <[(uint32 0, @a{sv} {})]>}")).is_err());
    }

    #[test]
    fn unsupported_cursor_modes_are_refused() {
        assert_eq!(cursor_mode(true, 4, 3).unwrap(), Some(CURSOR_EMBEDDED));
        assert!(cursor_mode(true, 4, CURSOR_HIDDEN).is_err());
        assert!(cursor_mode(true, 1, 0).is_err());
        assert_eq!(cursor_mode(false, 1, 0).unwrap(), None);
        assert_eq!(cursor_mode(false, 4, 7).unwrap(), Some(CURSOR_HIDDEN));
        assert_eq!(
            cursor_mode(false, 4, CURSOR_METADATA).unwrap(),
            Some(CURSOR_METADATA)
        );
        assert!(cursor_mode(false, 4, CURSOR_EMBEDDED).is_err());
    }

    #[test]
    fn screenshot_uris_must_be_local_files() {
        assert_eq!(
            screenshot_path("file:///tmp/a%20b.png").unwrap(),
            PathBuf::from("/tmp/a b.png")
        );
        assert_eq!(
            screenshot_path("file://localhost/tmp/a.png").unwrap(),
            PathBuf::from("/tmp/a.png")
        );
        for bad in [
            "http://example.org/a.png",
            "file://other-host/tmp/a.png",
            "/tmp/a.png",
            "a.png",
            "",
        ] {
            assert!(screenshot_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn screenshot_files_are_validated_before_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255]))
            .save(&png)
            .unwrap();
        assert_eq!(read_screenshot(&png).unwrap().dimensions(), (3, 2));

        let junk = dir.path().join("junk.png");
        std::fs::write(&junk, b"not an image").unwrap();
        assert!(read_screenshot(&junk).is_err());
        assert!(read_screenshot(dir.path()).is_err());
        assert!(read_screenshot(&dir.path().join("missing.png")).is_err());
    }
}
