//! Always-on-top pinned reference screenshots.
//!
//! Pins live in a helper process (`sharpshot --pin-helper`, see [`run_helper`]) so they outlive
//! the editor and so gtk4-layer-shell can be injected with `LD_PRELOAD`: it must precede
//! libwayland-client, which GTK has already loaded in any process that started normally.
//! The editor and `--toggle-pins` talk to it over a private unix socket
//! (`$XDG_RUNTIME_DIR/sharpshot-pins/pins.sock`, directory 0700, socket 0600, owner-checked).
//!
//! Protocol: one request per connection, header line <= 64 bytes, one reply line.
//!   `PIN <w> <h>\n` + w*h*4 raw RGBA bytes   `TOGGLE\n`      ->  `OK <text>\n` | `ERR <text>\n`
//!
//! Stacking: Wayland uses the wlr-layer-shell OVERLAY layer (GNOME/Mutter lacks it, so pinning
//! fails there with an error); X11 asks the window manager for EWMH `_NET_WM_STATE_ABOVE`.
//! A plain GTK window is never claimed to be on top.

use anyhow::{Context, Result, anyhow, bail, ensure};
use gtk::{cairo, gdk, glib, glib::translate::ToGlibPtr, prelude::*};
use image::RgbaImage;
use libloading::Library;
use std::{
    cell::{Cell, RefCell},
    ffi::{CString, OsString, c_char, c_int, c_long, c_ulong, c_void},
    fs::{self, DirBuilder, File},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::{Rc, Weak},
    sync::mpsc,
    time::{Duration, Instant},
};

/// Hidden CLI flag that runs [`run_helper`].
pub const HELPER_FLAG: &str = "--pin-helper";

const SOCKET_DIR: &str = "sharpshot-pins";
const SOCKET_NAME: &str = "pins.sock";
const HEADER_MAX: usize = 64;
const MAX_SIDE: u32 = 16_384;
const MAX_PIXELS: u64 = 64 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(15);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);
const REPLY_MAX: usize = 512;
const REPLY_TEXT_MAX: usize = 300;
const LAYER_SONAMES: [&str; 2] = ["libgtk4-layer-shell.so.0", "libgtk4-layer-shell.so"];

const MIN_ZOOM: f64 = 0.1;
const MAX_ZOOM: f64 = 8.0;
/// Largest pin edge in logical pixels; keeps the window inside GPU texture limits.
const MAX_LOGICAL: f64 = 8192.0;
const KEEP_ON_SCREEN: i32 = 48;
const CASCADE: i32 = 32;

// ---------------------------------------------------------------------------------------------
// Protocol

#[derive(Debug, PartialEq, Eq)]
enum Request {
    Pin { width: u32, height: u32 },
    Toggle,
}

fn parse_request(line: &str) -> Result<Request> {
    let mut words = line.split(' ');
    match words.next() {
        Some("TOGGLE") if words.next().is_none() => Ok(Request::Toggle),
        Some("PIN") => {
            let mut dim = || -> Result<u32> {
                words
                    .next()
                    .and_then(|w| w.parse().ok())
                    .filter(|&d| (1..=MAX_SIDE).contains(&d))
                    .with_context(|| format!("image sides must be 1..={MAX_SIDE}"))
            };
            let (width, height) = (dim()?, dim()?);
            ensure!(
                words.next().is_none(),
                "unexpected data after the image size"
            );
            ensure!(
                u64::from(width) * u64::from(height) <= MAX_PIXELS,
                "image exceeds {MAX_PIXELS} pixels"
            );
            Ok(Request::Pin { width, height })
        }
        _ => bail!("unknown pin request"),
    }
}

fn reply_line(result: &Result<String>) -> String {
    let (tag, text) = match result {
        Ok(text) => ("OK", text.clone()),
        Err(error) => ("ERR", format!("{error:#}")),
    };
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(REPLY_TEXT_MAX)
        .collect();
    format!("{tag} {text}\n")
}

fn parse_reply(line: &str) -> Result<String> {
    match line
        .trim_end()
        .split_once(' ')
        .unwrap_or((line.trim_end(), ""))
    {
        ("OK", text) => Ok(text.to_owned()),
        ("ERR", text) => Err(anyhow!("{text}")),
        _ => Err(anyhow!("unrecognised reply from the pin helper")),
    }
}

// ---------------------------------------------------------------------------------------------
// Private socket directory

fn own_uid() -> Result<u32> {
    Ok(fs::metadata("/proc/self")
        .context("cannot read /proc/self")?
        .uid())
}

fn socket_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .context("XDG_RUNTIME_DIR is not set to an absolute path; pins need a private per-user runtime directory")?;
    Ok(base.join(SOCKET_DIR))
}

/// The directory must be a real directory we own, closed to group/other.
fn verify_dir(dir: &Path, create: bool) -> Result<bool> {
    if create {
        match DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
        }
    }
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", dir.display())),
    };
    ensure!(
        meta.file_type().is_dir(),
        "{} is not a directory",
        dir.display()
    );
    ensure!(
        meta.uid() == own_uid()?,
        "{} belongs to another user",
        dir.display()
    );
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot restrict {}", dir.display()))?;
    }
    Ok(true)
}

fn verify_socket(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.file_type().is_socket() && meta.uid() == own_uid()?,
                "{} is not a socket owned by this user",
                path.display()
            );
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

// ---------------------------------------------------------------------------------------------
// Client side (editor process and `--toggle-pins`; no GTK needed for the toggle)

/// Connects to a running helper; `None` when no helper is listening.
fn connect() -> Result<Option<UnixStream>> {
    let dir = socket_dir()?;
    if !verify_dir(&dir, false)? {
        return Ok(None);
    }
    let path = dir.join(SOCKET_NAME);
    if !verify_socket(&path)? {
        return Ok(None);
    }
    match UnixStream::connect(&path) {
        Ok(stream) => {
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            Ok(Some(stream))
        }
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(None),
        Err(e) => Err(e).context("cannot reach the pin helper"),
    }
}

/// One request/reply exchange. The outer error is transport failure (retryable), the inner
/// one is the helper's refusal.
fn exchange(stream: &UnixStream, header: &str, payload: &[u8]) -> std::io::Result<Result<String>> {
    let mut writer = stream;
    writer.write_all(header.as_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    let mut line = Vec::new();
    BufReader::new(stream)
        .take(REPLY_MAX as u64)
        .read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(parse_reply(&String::from_utf8_lossy(&line)))
}

/// Hides all pins if any is visible, otherwise shows them again with click-through cleared,
/// so a click-through pin is always recoverable. Used by `--toggle-pins` and the editor.
pub fn toggle_all() -> Result<()> {
    let stream = connect()?.context("no pinned screenshots are open")?;
    exchange(&stream, "TOGGLE\n", &[])
        .context("pin helper connection failed")?
        .map(drop)
}

/// Pins `image` in an always-on-top window, starting the helper if needed. The display is
/// inspected synchronously here on the UI thread; the returned `'static` future (no borrow of
/// `parent`) does helper start-up (up to 10 s), the socket and the image transfer (up to
/// 256 MiB) on a gio worker thread, so awaiting it never blocks the UI. Poll it on the main
/// context: `glib::spawn_future_local(pin::show(&win, img))`.
pub fn show(
    parent: &gtk::ApplicationWindow,
    image: RgbaImage,
) -> impl Future<Output = Result<()>> + 'static {
    let wayland = gtk::prelude::WidgetExt::display(parent)
        .type_()
        .name()
        .contains("Wayland");
    async move {
        let (width, height) = image.dimensions();
        ensure!(
            (1..=MAX_SIDE).contains(&width)
                && (1..=MAX_SIDE).contains(&height)
                && u64::from(width) * u64::from(height) <= MAX_PIXELS,
            "{width}x{height} is too large to pin (limit {MAX_SIDE} per side, {MAX_PIXELS} pixels)"
        );
        gtk::gio::spawn_blocking(move || send_pin(wayland, &image))
            .await
            .map_err(|_| anyhow!("pin worker thread panicked"))?
    }
}

fn send_pin(wayland: bool, image: &RgbaImage) -> Result<()> {
    let header = format!("PIN {} {}\n", image.width(), image.height());
    let mut last = None;
    for _ in 0..2 {
        let stream = match connect()? {
            Some(stream) => stream,
            None => spawn_helper(wayland)?,
        };
        match exchange(&stream, &header, image.as_raw()) {
            Ok(reply) => return reply.map(drop),
            // The helper may have been shutting down as we connected; start a fresh one once.
            Err(e) => last = Some(e),
        }
    }
    Err(last.map_or_else(|| anyhow!("pin helper unavailable"), anyhow::Error::from))
        .context("pin helper connection failed")
}

/// Layer-shell candidates, beside the executable first, then the system search path.
fn layer_shell_targets() -> Vec<OsString> {
    let mut targets = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for sub in ["", "lib"] {
            for name in LAYER_SONAMES {
                let path = dir.join(sub).join(name);
                if path.is_file() {
                    targets.push(path.into_os_string());
                }
            }
        }
    }
    targets.extend(LAYER_SONAMES.map(OsString::from));
    targets
}

fn spawn_helper(wayland: bool) -> Result<UnixStream> {
    let exe = std::env::current_exe().context("cannot locate the SharpShot executable")?;
    let mut command = Command::new(exe);
    command
        .arg(HELPER_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if wayland {
        // Layer shell only works if it is resolved before libwayland-client.
        let mut preload = layer_shell_targets().swap_remove(0);
        if let Some(old) = std::env::var_os("LD_PRELOAD").filter(|v| !v.is_empty()) {
            preload.push(":");
            preload.push(old);
        }
        command.env("LD_PRELOAD", preload);
    }
    let mut child = command.spawn().context("cannot start the pin helper")?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(stream) = connect()? {
            std::thread::spawn(move || drop(child.wait()));
            return Ok(stream);
        }
        if let Some(status) = child.try_wait()? {
            // Exit 0 means another helper won the start-up race and is about to listen.
            return connect()?.with_context(|| format!("pin helper exited early ({status})"));
        }
        if Instant::now() >= deadline {
            drop(child.kill());
            std::thread::spawn(move || drop(child.wait()));
            bail!(
                "pin helper did not become ready within {} s",
                START_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

// ---------------------------------------------------------------------------------------------
// Helper side: socket server

enum Msg {
    Pin {
        image: RgbaImage,
        reply: mpsc::SyncSender<Result<String, String>>,
    },
    Toggle {
        reply: mpsc::SyncSender<Result<String, String>>,
    },
}

fn serve(listener: UnixListener, tx: async_channel::Sender<Msg>) {
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => handle(&stream, &tx),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn handle(stream: &UnixStream, tx: &async_channel::Sender<Msg>) {
    drop(stream.set_read_timeout(Some(IO_TIMEOUT)));
    drop(stream.set_write_timeout(Some(IO_TIMEOUT)));
    let reply = reply_line(&serve_request(stream, tx));
    drop((&*stream).write_all(reply.as_bytes()));
}

fn serve_request(stream: &UnixStream, tx: &async_channel::Sender<Msg>) -> Result<String> {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    (&mut reader)
        .take(HEADER_MAX as u64)
        .read_until(b'\n', &mut line)?;
    ensure!(
        line.ends_with(b"\n"),
        "request header truncated or too long"
    );
    let request = parse_request(std::str::from_utf8(&line)?.trim_end())?;
    let (reply, answer) = mpsc::sync_channel(1);
    let msg = match request {
        Request::Toggle => Msg::Toggle { reply },
        Request::Pin { width, height } => {
            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            reader
                .read_exact(&mut pixels)
                .context("image data truncated")?;
            let image = RgbaImage::from_raw(width, height, pixels).context("bad image data")?;
            Msg::Pin { image, reply }
        }
    };
    tx.send_blocking(msg)
        .map_err(|_| anyhow!("pin helper is shutting down"))?;
    answer
        .recv_timeout(REPLY_TIMEOUT)
        .map_err(|_| anyhow!("pin helper did not answer"))?
        .map_err(|e| anyhow!(e))
}

// ---------------------------------------------------------------------------------------------
// Helper side: platform backends

enum Backend {
    Layer(LayerShell),
    X11(X11),
}

impl Backend {
    fn detect(display: &gdk::Display) -> Result<Backend, String> {
        let kind = display.type_().name();
        if kind.contains("Wayland") {
            LayerShell::load().map(Backend::Layer)
        } else if kind.contains("X11") {
            X11::load(display).map(Backend::X11)
        } else {
            Err(format!(
                "always-on-top pins need a Wayland (layer-shell) or X11 (EWMH) display, not {kind}"
            ))
        }
    }

    /// Must run before the window is realized.
    fn prepare(&self, window: &gtk::Window, monitor: Option<&gdk::Monitor>, margin: (i32, i32)) {
        if let Backend::Layer(layer) = self {
            layer.prepare(window, monitor, margin);
        }
    }

    /// X11 stacking can only be requested once the window manager manages the window, so it is
    /// re-sent for a short while after each map and then verified.
    fn on_map(pin: &Rc<Pin>) {
        if !matches!(*pin.backend, Backend::X11(_)) {
            return;
        }
        for ms in [30, 150, 400] {
            let weak = Rc::downgrade(pin);
            glib::timeout_add_local_once(Duration::from_millis(ms), move || {
                if let Some(pin) = weak.upgrade()
                    && pin.window.is_mapped()
                    && let Backend::X11(x11) = &*pin.backend
                {
                    x11.request_above(&pin.window);
                }
            });
        }
        let weak = Rc::downgrade(pin);
        glib::timeout_add_local_once(Duration::from_millis(800), move || {
            if let Some(pin) = weak.upgrade()
                && pin.window.is_mapped()
                && let Backend::X11(x11) = &*pin.backend
                && !x11.is_above(&pin.window)
            {
                pin.warn("The window manager did not apply always-on-top (_NET_WM_STATE_ABOVE).");
            }
        });
    }
}

fn symbol<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    // SAFETY: callers pick T to match the C prototype of `name`; the Library outlives the copy.
    unsafe { lib.get::<T>(&bytes) }
        .map(|s| *s)
        .map_err(|e| format!("{name}: {e}"))
}

type Win = *mut gtk::ffi::GtkWindow;

struct LayerShell {
    is_supported: unsafe extern "C" fn() -> c_int,
    init: unsafe extern "C" fn(Win),
    set_layer: unsafe extern "C" fn(Win, c_int),
    set_anchor: unsafe extern "C" fn(Win, c_int, c_int),
    set_margin: unsafe extern "C" fn(Win, c_int, c_int),
    set_exclusive_zone: unsafe extern "C" fn(Win, c_int),
    set_keyboard_mode: unsafe extern "C" fn(Win, c_int),
    set_namespace: unsafe extern "C" fn(Win, *const c_char),
    set_monitor: unsafe extern "C" fn(Win, *mut gdk::ffi::GdkMonitor),
    _lib: Library,
}

// GtkLayerShell enum values from gtk4-layer-shell.h.
const LAYER_OVERLAY: c_int = 3;
const EDGE_LEFT: c_int = 0;
const EDGE_TOP: c_int = 2;
const KEYBOARD_ON_DEMAND: c_int = 2;

impl LayerShell {
    fn load() -> Result<LayerShell, String> {
        let mut tried = Vec::new();
        for target in layer_shell_targets() {
            // SAFETY: loading a shared object runs its initialisers; this is the library the
            // user (or the package) placed for exactly this purpose.
            match unsafe { Library::new(&target) } {
                Ok(lib) => return Self::bind(lib),
                Err(e) => tried.push(format!("{}: {e}", target.to_string_lossy())),
            }
        }
        Err(format!(
            "Always-on-top pins on Wayland need gtk4-layer-shell (libgtk4-layer-shell.so.0): \
             install it (Arch/Fedora: gtk4-layer-shell, Debian/Ubuntu: libgtk4-layer-shell0) or \
             place it beside the sharpshot executable. Tried: {}",
            tried.join("; ")
        ))
    }

    fn bind(lib: Library) -> Result<LayerShell, String> {
        let layer = LayerShell {
            is_supported: symbol(&lib, "gtk_layer_is_supported")?,
            init: symbol(&lib, "gtk_layer_init_for_window")?,
            set_layer: symbol(&lib, "gtk_layer_set_layer")?,
            set_anchor: symbol(&lib, "gtk_layer_set_anchor")?,
            set_margin: symbol(&lib, "gtk_layer_set_margin")?,
            set_exclusive_zone: symbol(&lib, "gtk_layer_set_exclusive_zone")?,
            set_keyboard_mode: symbol(&lib, "gtk_layer_set_keyboard_mode")?,
            set_namespace: symbol(&lib, "gtk_layer_set_namespace")?,
            set_monitor: symbol(&lib, "gtk_layer_set_monitor")?,
            _lib: lib,
        };
        // SAFETY: plain query, GDK is initialised.
        if unsafe { (layer.is_supported)() } == 0 {
            return Err(
                "This Wayland compositor does not provide wlr-layer-shell (GNOME/Mutter \
                does not), or gtk4-layer-shell was loaded after libwayland-client. Pinned windows \
                cannot stay above other windows here and no fallback is used."
                    .into(),
            );
        }
        Ok(layer)
    }

    fn prepare(&self, window: &gtk::Window, monitor: Option<&gdk::Monitor>, margin: (i32, i32)) {
        let win: Win = window.to_glib_none().0;
        // SAFETY: `win` is a live, not yet realized GtkWindow; the functions match the header.
        unsafe {
            (self.init)(win);
            (self.set_layer)(win, LAYER_OVERLAY);
            (self.set_namespace)(win, c"sharpshot-pin".as_ptr());
            // -1: ignore panels' exclusive zones so a pin can be placed anywhere.
            (self.set_exclusive_zone)(win, -1);
            (self.set_keyboard_mode)(win, KEYBOARD_ON_DEMAND);
            (self.set_anchor)(win, EDGE_LEFT, 1);
            (self.set_anchor)(win, EDGE_TOP, 1);
            if let Some(monitor) = monitor {
                (self.set_monitor)(win, monitor.to_glib_none().0);
            }
        }
        self.set_margins(window, margin.0, margin.1);
    }

    fn set_margins(&self, window: &gtk::Window, x: i32, y: i32) {
        let win: Win = window.to_glib_none().0;
        // SAFETY: as in `prepare`; margins may change at any time.
        unsafe {
            (self.set_margin)(win, EDGE_LEFT, x);
            (self.set_margin)(win, EDGE_TOP, y);
        }
    }
}

type XDisplay = *mut c_void;
const XA_ATOM: c_ulong = 4;
const CLIENT_MESSAGE: c_int = 33;
/// SubstructureNotifyMask | SubstructureRedirectMask: EWMH root-window requests.
const ROOT_EVENT_MASK: c_long = (1 << 19) | (1 << 20);
const NET_WM_STATE_ADD: c_long = 1;
const SOURCE_APPLICATION: c_long = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct ClientMessage {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: XDisplay,
    window: c_ulong,
    message_type: c_ulong,
    format: c_int,
    data: [c_long; 5],
}

#[repr(C)]
union XEvent {
    client: ClientMessage,
    pad: [c_long; 24],
}

fn state_message(
    display: XDisplay,
    window: c_ulong,
    state: c_ulong,
    a: c_ulong,
    b: c_ulong,
) -> ClientMessage {
    ClientMessage {
        kind: CLIENT_MESSAGE,
        serial: 0,
        send_event: 1,
        display,
        window,
        message_type: state,
        format: 32,
        data: [
            NET_WM_STATE_ADD,
            a as c_long,
            b as c_long,
            SOURCE_APPLICATION,
            0,
        ],
    }
}

struct Atoms {
    state: c_ulong,
    above: c_ulong,
    sticky: c_ulong,
    skip_taskbar: c_ulong,
    skip_pager: c_ulong,
}

type GetProperty = unsafe extern "C" fn(
    XDisplay,
    c_ulong,
    c_ulong,
    c_long,
    c_long,
    c_int,
    c_ulong,
    *mut c_ulong,
    *mut c_int,
    *mut c_ulong,
    *mut c_ulong,
    *mut *mut u8,
) -> c_int;

/// EWMH client over libX11 (loaded at run time) sharing GDK's own X connection.
struct X11 {
    gdk_display: *mut gdk::ffi::GdkDisplay,
    display: XDisplay,
    root: c_ulong,
    atoms: Atoms,
    surface_xid: unsafe extern "C" fn(*mut gdk::ffi::GdkSurface) -> c_ulong,
    send_event: unsafe extern "C" fn(XDisplay, c_ulong, c_int, c_long, *mut XEvent) -> c_int,
    flush: unsafe extern "C" fn(XDisplay) -> c_int,
    get_property: GetProperty,
    free: unsafe extern "C" fn(*mut c_void) -> c_int,
    trap_push: unsafe extern "C" fn(*mut gdk::ffi::GdkDisplay),
    trap_pop: unsafe extern "C" fn(*mut gdk::ffi::GdkDisplay),
    _libs: (Library, Library),
}

impl X11 {
    fn load(display: &gdk::Display) -> Result<X11, String> {
        // The gdk_x11_* entry points live in libgtk-4, already part of this process.
        let this: Library = libloading::os::unix::Library::this().into();
        // SAFETY: libX11 is a plain C library without constructors that matter here.
        let xlib = unsafe { Library::new("libX11.so.6") }
            .map_err(|e| format!("libX11.so.6 is required for always-on-top on X11: {e}"))?;
        let gdk_display: *mut gdk::ffi::GdkDisplay = display.to_glib_none().0;
        let get_xdisplay: unsafe extern "C" fn(*mut gdk::ffi::GdkDisplay) -> XDisplay =
            symbol(&this, "gdk_x11_display_get_xdisplay")?;
        // SAFETY: GDK reported an X11 display, so this returns its Display*.
        let xdisplay = unsafe { get_xdisplay(gdk_display) };
        if xdisplay.is_null() {
            return Err("GDK has no X11 connection".into());
        }
        let default_root: unsafe extern "C" fn(XDisplay) -> c_ulong =
            symbol(&xlib, "XDefaultRootWindow")?;
        let intern: unsafe extern "C" fn(XDisplay, *const c_char, c_int) -> c_ulong =
            symbol(&xlib, "XInternAtom")?;
        let atom = |name: &str| {
            let name = CString::new(name).expect("static atom names have no NUL");
            // SAFETY: valid display and NUL-terminated name.
            unsafe { intern(xdisplay, name.as_ptr(), 0) }
        };
        let x11 = X11 {
            gdk_display,
            display: xdisplay,
            // SAFETY: valid display.
            root: unsafe { default_root(xdisplay) },
            atoms: Atoms {
                state: atom("_NET_WM_STATE"),
                above: atom("_NET_WM_STATE_ABOVE"),
                sticky: atom("_NET_WM_STATE_STICKY"),
                skip_taskbar: atom("_NET_WM_STATE_SKIP_TASKBAR"),
                skip_pager: atom("_NET_WM_STATE_SKIP_PAGER"),
            },
            surface_xid: symbol(&this, "gdk_x11_surface_get_xid")?,
            send_event: symbol(&xlib, "XSendEvent")?,
            flush: symbol(&xlib, "XFlush")?,
            get_property: symbol(&xlib, "XGetWindowProperty")?,
            free: symbol(&xlib, "XFree")?,
            trap_push: symbol(&this, "gdk_x11_display_error_trap_push")?,
            trap_pop: symbol(&this, "gdk_x11_display_error_trap_pop_ignored")?,
            _libs: (this, xlib),
        };
        if !x11
            .atoms_of(x11.root, atom("_NET_SUPPORTED"))
            .contains(&x11.atoms.above)
        {
            return Err(
                "The window manager does not advertise EWMH _NET_WM_STATE_ABOVE, so \
                pinned windows cannot be kept on top. Run a EWMH-compliant window manager."
                    .into(),
            );
        }
        Ok(x11)
    }

    fn atoms_of(&self, window: c_ulong, property: c_ulong) -> Vec<c_ulong> {
        let (mut kind, mut format, mut count, mut after) = (0, 0, 0, 0);
        let mut data: *mut u8 = std::ptr::null_mut();
        // SAFETY: out-pointers are valid locals; format-32 data is an array of C longs, freed
        // with XFree.
        unsafe {
            let status = (self.get_property)(
                self.display,
                window,
                property,
                0,
                4096,
                0,
                XA_ATOM,
                &mut kind,
                &mut format,
                &mut count,
                &mut after,
                &mut data,
            );
            if status != 0 || data.is_null() {
                return Vec::new();
            }
            let atoms = if kind == XA_ATOM && format == 32 {
                std::slice::from_raw_parts(data.cast::<c_ulong>(), count as usize).to_vec()
            } else {
                Vec::new()
            };
            (self.free)(data.cast());
            atoms
        }
    }

    fn xid(&self, window: &gtk::Window) -> Option<c_ulong> {
        let surface = window.surface()?;
        let ptr: *mut gdk::ffi::GdkSurface = surface.to_glib_none().0;
        // SAFETY: the surface belongs to the X11 display this backend was created for.
        Some(unsafe { (self.surface_xid)(ptr) })
    }

    fn request_above(&self, window: &gtk::Window) {
        let Some(xid) = self.xid(window) else { return };
        let a = &self.atoms;
        // SAFETY: EWMH client messages to the root window; BadWindow is trapped.
        unsafe {
            (self.trap_push)(self.gdk_display);
            for (first, second) in [(a.above, a.sticky), (a.skip_taskbar, a.skip_pager)] {
                let mut event = XEvent {
                    client: state_message(self.display, xid, a.state, first, second),
                };
                (self.send_event)(self.display, self.root, 0, ROOT_EVENT_MASK, &mut event);
            }
            (self.flush)(self.display);
            (self.trap_pop)(self.gdk_display);
        }
    }

    fn is_above(&self, window: &gtk::Window) -> bool {
        let Some(xid) = self.xid(window) else {
            return false;
        };
        // SAFETY: BadWindow is trapped.
        unsafe { (self.trap_push)(self.gdk_display) };
        let above = self
            .atoms_of(xid, self.atoms.state)
            .contains(&self.atoms.above);
        // SAFETY: balances the push above.
        unsafe { (self.trap_pop)(self.gdk_display) };
        above
    }
}

// ---------------------------------------------------------------------------------------------
// Helper side: pin windows

const CSS: &str = "
window.sharp-pin.sharpshot { background-color: transparent; background-image: none; box-shadow: none; }
.sharpshot .pin-image { outline: 1px solid @ss_border; outline-offset: -1px; }
.sharpshot menubutton.pin-more { margin: 6px; }
.sharpshot menubutton.pin-more > button { color: @ss_text; background-color: @ss_surface; border: 1px solid @ss_border; border-radius: 6px; min-width: 22px; min-height: 22px; padding: 3px; }
.sharpshot menubutton.pin-more > button:hover, .sharpshot menubutton.pin-more > button:checked { background-color: @ss_selected; }
.sharpshot .pin-hint { color: @ss_muted; font-size: 12px; }
.sharpshot .pin-warning { color: #f0b46a; font-size: 12px; }
";

fn max_zoom(size: (u32, u32), scale: i32) -> f64 {
    MAX_LOGICAL * f64::from(scale) / f64::from(size.0.max(size.1))
}

fn clamp_zoom(zoom: f64, size: (u32, u32), scale: i32) -> f64 {
    let cap = max_zoom(size, scale);
    zoom.clamp(MIN_ZOOM.min(cap), MAX_ZOOM.min(cap))
}

/// Logical (application) pixels for an image at `zoom`; 100 % is one image pixel per device pixel.
fn logical_size(size: (u32, u32), zoom: f64, scale: i32) -> (i32, i32) {
    let side = |px: u32| ((f64::from(px) * zoom / f64::from(scale)).round() as i32).max(1);
    (side(size.0), side(size.1))
}

/// Largest zoom up to 100 % that keeps the pin within 80 % of the screen.
fn fit_zoom(size: (u32, u32), screen: (i32, i32), scale: i32) -> f64 {
    let fit = |screen: i32, px: u32| 0.8 * f64::from(screen) * f64::from(scale) / f64::from(px);
    clamp_zoom(
        fit(screen.0, size.0).min(fit(screen.1, size.1)).min(1.0),
        size,
        scale,
    )
}

/// Straight RGBA to Cairo's premultiplied native-endian ARGB32.
fn to_surface(image: &RgbaImage) -> Result<cairo::ImageSurface> {
    let (width, height) = image.dimensions();
    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, width.try_into()?, height.try_into()?)?;
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data()?;
        for (y, row) in image.rows().enumerate() {
            for (x, pixel) in row.enumerate() {
                let [r, g, b, a] = pixel.0;
                let premul = |c: u8| (u32::from(c) * u32::from(a) + 127) / 255;
                let argb = (u32::from(a) << 24) | (premul(r) << 16) | (premul(g) << 8) | premul(b);
                data[y * stride + x * 4..][..4].copy_from_slice(&argb.to_ne_bytes());
            }
        }
    }
    Ok(surface)
}

fn more_icon() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(20)
        .content_height(20)
        .can_target(false)
        .build();
    area.set_draw_func(|area, cr, w, h| {
        let color = area.style_context().color();
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            color.alpha().into(),
        );
        let (cx, cy) = (f64::from(w) / 2., f64::from(h) / 2.);
        for dx in [-5., 0., 5.] {
            cr.arc(cx + dx, cy, 1.8, 0., std::f64::consts::TAU);
            drop(cr.fill());
        }
    });
    area
}

struct Pin {
    window: gtk::Window,
    area: gtk::DrawingArea,
    scale: gtk::Scale,
    check: gtk::CheckButton,
    zoom_label: gtk::Label,
    warning: gtk::Label,
    popover: gtk::Popover,
    menu: gtk::MenuButton,
    backend: Rc<Backend>,
    registry: Weak<Registry>,
    size: (u32, u32),
    device_scale: i32,
    screen: (i32, i32),
    zoom: Cell<f64>,
    through: Cell<bool>,
    hover: Cell<bool>,
    /// Layer-shell position (left/top margin); X11 positions are the window manager's.
    margin: Cell<(i32, i32)>,
}

/// Weak-capturing signal handler: `pinfn!(&pin => |p, arg, ..| body)`.
macro_rules! pinfn {
    ($pin:expr => |$p:ident $(, $arg:pat_param)* $(,)?| $body:expr) => {{
        let weak = Rc::downgrade($pin);
        move |$($arg),*| {
            if let Some($p) = weak.upgrade() {
                $body
            }
        }
    }};
}

impl Pin {
    fn new(
        registry: &Rc<Registry>,
        backend: &Rc<Backend>,
        image: RgbaImage,
        index: usize,
    ) -> Result<Rc<Pin>, String> {
        let size = image.dimensions();
        let surface = to_surface(&image).map_err(|e| format!("cannot prepare the image: {e}"))?;
        drop(image);
        let display = gdk::Display::default().ok_or("no display")?;
        let monitor = display.monitors().item(0).and_downcast::<gdk::Monitor>();
        let device_scale = monitor.as_ref().map_or(1, |m| m.scale_factor().max(1));
        let screen = monitor.as_ref().map_or((1920, 1080), |m| {
            let g = m.geometry();
            (g.width().max(1), g.height().max(1))
        });
        let zoom = fit_zoom(size, screen, device_scale);
        let (w, h) = logical_size(size, zoom, device_scale);
        let cascade = (index % 8) as i32 * CASCADE;
        let margin = ((screen.0 - w) / 2 + cascade, (screen.1 - h) / 2 + cascade);

        let window = gtk::Window::builder()
            .title("SharpShot pin")
            .decorated(false)
            .resizable(false)
            .css_classes(["sharp-pin"])
            .build();
        backend.prepare(&window, monitor.as_ref(), margin);

        let area = gtk::DrawingArea::builder()
            .content_width(w)
            .content_height(h)
            .css_classes(["pin-image"])
            .build();
        area.update_property(&[gtk::accessible::Property::Label("Pinned screenshot")]);
        let (iw, ih) = (f64::from(size.0), f64::from(size.1));
        area.set_draw_func(move |_, cr, w, h| {
            cr.scale(f64::from(w) / iw, f64::from(h) / ih);
            let pattern = cairo::SurfacePattern::create(&surface);
            pattern.set_filter(cairo::Filter::Good);
            pattern.set_extend(cairo::Extend::Pad);
            if cr.set_source(&pattern).is_ok() {
                drop(cr.paint());
            }
        });

        let zoom_label = gtk::Label::builder()
            .width_chars(5)
            .css_classes(["dimension-label"])
            .build();
        let minus = crate::chrome::button(crate::chrome::Icon::Minus, "Zoom out", "Zoom out (−)");
        let plus = crate::chrome::button(
            crate::chrome::Icon::Plus,
            "Zoom in",
            "Zoom in (+, or scroll)",
        );
        let actual = gtk::Button::builder()
            .label("100%")
            .tooltip_text("Actual size (0)")
            .build();
        let fit = crate::chrome::button(
            crate::chrome::Icon::Fit,
            "Fit to screen",
            "Fit within the screen",
        );
        let zoom_row = gtk::Box::builder().spacing(6).build();
        zoom_row.append(
            &gtk::Label::builder()
                .label("Zoom")
                .width_chars(7)
                .xalign(0.)
                .css_classes(["field-label"])
                .build(),
        );
        for widget in [
            minus.upcast_ref::<gtk::Widget>(),
            zoom_label.upcast_ref(),
            plus.upcast_ref(),
            actual.upcast_ref(),
            fit.upcast_ref(),
        ] {
            zoom_row.append(widget);
        }

        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 10., 100., 5.);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.set_size_request(150, -1);
        scale.set_value(100.);
        scale.update_property(&[gtk::accessible::Property::Label("Opacity")]);
        let opacity_row = gtk::Box::builder().spacing(6).build();
        opacity_row.append(
            &gtk::Label::builder()
                .label("Opacity")
                .width_chars(7)
                .xalign(0.)
                .css_classes(["field-label"])
                .build(),
        );
        opacity_row.append(&scale);

        let check = gtk::CheckButton::with_label("Click-through");
        let hint = gtk::Label::builder()
            .label("Clicks pass to the windows below. Run “sharpshot --toggle-pins” or use the editor's pin toggle to hide, then show again with clicks enabled.")
            .wrap(true)
            .max_width_chars(36)
            .xalign(0.)
            .css_classes(["pin-hint"])
            .build();
        let warning = gtk::Label::builder()
            .wrap(true)
            .max_width_chars(36)
            .xalign(0.)
            .visible(false)
            .css_classes(["pin-warning"])
            .build();
        let hide = gtk::Button::builder().label("Hide all pins").build();
        let close = gtk::Button::builder().label("Close pin").build();
        let actions = gtk::Box::builder().spacing(6).build();
        actions.append(&hide);
        actions.append(&close);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .build();
        for widget in [
            warning.upcast_ref::<gtk::Widget>(),
            zoom_row.upcast_ref(),
            opacity_row.upcast_ref(),
            check.upcast_ref(),
            hint.upcast_ref(),
            actions.upcast_ref(),
        ] {
            body.append(widget);
        }
        let popover = gtk::Popover::builder()
            .child(&body)
            .has_arrow(false)
            .css_classes(["pin-controls"])
            .build();
        let menu = gtk::MenuButton::builder()
            .popover(&popover)
            .child(&more_icon())
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .tooltip_text("Pin controls (right-click also opens them)")
            .css_classes(["pin-more"])
            .visible(false)
            .build();
        menu.update_property(&[gtk::accessible::Property::Label("Pin controls")]);
        let overlay = gtk::Overlay::builder().child(&area).build();
        overlay.add_overlay(&menu);
        window.set_child(Some(&overlay));

        let pin = Rc::new(Pin {
            window,
            area,
            scale,
            check,
            zoom_label,
            warning,
            popover,
            menu,
            backend: backend.clone(),
            registry: Rc::downgrade(registry),
            size,
            device_scale,
            screen,
            zoom: Cell::new(zoom),
            through: Cell::new(false),
            hover: Cell::new(false),
            margin: Cell::new(margin),
        });
        pin.zoom_label.set_text(&format!("{:.0}%", zoom * 100.));

        minus.connect_clicked(pinfn!(&pin => |p, _| p.set_zoom(p.zoom.get() / 1.25)));
        plus.connect_clicked(pinfn!(&pin => |p, _| p.set_zoom(p.zoom.get() * 1.25)));
        actual.connect_clicked(pinfn!(&pin => |p, _| p.set_zoom(1.0)));
        fit.connect_clicked(
            pinfn!(&pin => |p, _| p.set_zoom(fit_zoom(p.size, p.screen, p.device_scale))),
        );
        pin.scale
            .connect_value_changed(pinfn!(&pin => |p, s| p.set_opacity(s.value() / 100.)));
        pin.check
            .connect_toggled(pinfn!(&pin => |p, c| p.set_through(c.is_active())));
        hide.connect_clicked(
            pinfn!(&pin => |p, _| if let Some(r) = p.registry.upgrade() { r.hide_all() }),
        );
        close.connect_clicked(pinfn!(&pin => |p, _| p.window.close()));
        pin.window.connect_map(pinfn!(&pin => |p, _| {
            p.apply_input();
            Backend::on_map(&p);
        }));
        pin.window.connect_close_request({
            let weak = Rc::downgrade(&pin);
            move |_| {
                if let Some(pin) = weak.upgrade()
                    && let Some(registry) = pin.registry.upgrade()
                {
                    registry.forget(&pin);
                }
                glib::Propagation::Proceed
            }
        });
        pin.popover
            .connect_closed(pinfn!(&pin => |p, _| if !p.hover.get() { p.menu.set_visible(false) }));

        let motion = gtk::EventControllerMotion::new();
        motion.connect_enter(pinfn!(&pin => |p, _, _, _| {
            p.hover.set(true);
            p.menu.set_visible(true);
        }));
        motion.connect_leave(pinfn!(&pin => |p, _| {
            p.hover.set(false);
            if !p.popover.is_visible() {
                p.menu.set_visible(false);
            }
        }));
        overlay.add_controller(motion);

        let click = gtk::GestureClick::builder().button(3).build();
        click.connect_pressed(pinfn!(&pin => |p, _, _, _, _| {
            p.menu.set_visible(true);
            p.menu.popup();
        }));
        pin.area.add_controller(click);

        let drag = gtk::GestureDrag::builder().button(1).build();
        drag.connect_drag_begin({
            let weak = Rc::downgrade(&pin);
            move |gesture, x, y| {
                if let Some(pin) = weak.upgrade() {
                    pin.begin_drag(gesture, x, y);
                }
            }
        });
        pin.area.add_controller(drag);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll({
            let weak = Rc::downgrade(&pin);
            move |controller, _, dy| {
                let Some(pin) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let step = 1.1f64.powf(-dy);
                if controller
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
                {
                    pin.set_opacity(pin.opacity() * step.powi(2));
                } else {
                    pin.set_zoom(pin.zoom.get() * step);
                }
                glib::Propagation::Stop
            }
        });
        pin.area.add_controller(scroll);

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let weak = Rc::downgrade(&pin);
            move |_, key, _, _| {
                let Some(pin) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                match key {
                    gdk::Key::Escape => pin.window.close(),
                    gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => {
                        pin.set_zoom(pin.zoom.get() * 1.25)
                    }
                    gdk::Key::minus | gdk::Key::KP_Subtract => pin.set_zoom(pin.zoom.get() / 1.25),
                    gdk::Key::_0 | gdk::Key::KP_0 => pin.set_zoom(1.0),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        });
        pin.window.add_controller(keys);
        Ok(pin)
    }

    fn opacity(&self) -> f64 {
        self.area.opacity()
    }

    fn warn(&self, text: &str) {
        self.warning.set_text(text);
        self.warning.set_visible(true);
    }

    fn set_zoom(&self, zoom: f64) {
        let zoom = clamp_zoom(zoom, self.size, self.device_scale);
        self.zoom.set(zoom);
        let (w, h) = logical_size(self.size, zoom, self.device_scale);
        self.area.set_content_width(w);
        self.area.set_content_height(h);
        self.zoom_label.set_text(&format!("{:.0}%", zoom * 100.));
    }

    fn set_opacity(&self, opacity: f64) {
        let opacity = opacity.clamp(0.1, 1.0);
        self.area.set_opacity(opacity);
        if (self.scale.value() - opacity * 100.).abs() > 0.01 {
            self.scale.set_value(opacity * 100.);
        }
    }

    /// An empty input region makes the surface transparent to the pointer on both X11 and Wayland.
    fn set_through(&self, on: bool) {
        if self.check.is_active() != on {
            self.check.set_active(on); // re-enters through the toggled handler
            return;
        }
        if self.through.replace(on) == on {
            return;
        }
        if on {
            self.popover.popdown();
        }
        self.apply_input();
    }

    fn apply_input(&self) {
        let Some(surface) = self.window.surface() else {
            return;
        };
        let region = if self.through.get() {
            cairo::Region::create()
        } else {
            // Larger than any pin; the compositor clips it to the surface.
            cairo::Region::create_rectangle(&cairo::RectangleInt::new(0, 0, 16_384, 16_384))
        };
        surface.set_input_region(&region);
    }

    fn begin_drag(self: &Rc<Self>, gesture: &gtk::GestureDrag, x: f64, y: f64) {
        match &*self.backend {
            Backend::X11(_) => {
                if let (Some(device), Some(toplevel)) = (
                    gesture.device(),
                    self.window.surface().and_downcast::<gdk::Toplevel>(),
                ) {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    toplevel.begin_move(
                        &device,
                        gesture.current_button() as i32,
                        x,
                        y,
                        gesture.current_event_time(),
                    );
                    gesture.reset();
                }
            }
            // Layer surfaces cannot be moved by the compositor: follow the pointer through the
            // margins, at most once per 25 ms so the compositor's reply is seen before the next step.
            Backend::Layer(_) => {
                let (weak, gesture) = (Rc::downgrade(self), gesture.clone());
                glib::timeout_add_local(Duration::from_millis(25), move || {
                    let Some(pin) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    if !gesture.is_active() {
                        return glib::ControlFlow::Break;
                    }
                    if let Some((dx, dy)) = gesture.offset() {
                        pin.move_by(dx.round() as i32, dy.round() as i32);
                    }
                    glib::ControlFlow::Continue
                });
            }
        }
    }

    fn move_by(&self, dx: i32, dy: i32) {
        let Backend::Layer(layer) = &*self.backend else {
            return;
        };
        let (w, h) = logical_size(self.size, self.zoom.get(), self.device_scale);
        let (x, y) = self.margin.get();
        let limit = |value: i32, extent: i32, screen: i32| {
            let low = KEEP_ON_SCREEN - extent;
            value.clamp(low, (screen - KEEP_ON_SCREEN).max(low))
        };
        let next = (
            limit(x + dx, w, self.screen.0),
            limit(y + dy, h, self.screen.1),
        );
        if next != (x, y) {
            self.margin.set(next);
            layer.set_margins(&self.window, next.0, next.1);
        }
    }
}

struct Registry {
    pins: RefCell<Vec<Rc<Pin>>>,
    main: glib::MainLoop,
}

impl Registry {
    fn open(self: &Rc<Self>, backend: &Rc<Backend>, image: RgbaImage) -> Result<(), String> {
        let index = self.pins.borrow().len();
        let pin = Pin::new(self, backend, image, index)?;
        self.pins.borrow_mut().push(pin.clone());
        self.show_all();
        pin.window.present();
        Ok(())
    }

    fn forget(self: &Rc<Self>, pin: &Rc<Pin>) {
        self.pins.borrow_mut().retain(|p| !Rc::ptr_eq(p, pin));
        self.quit_if_idle();
    }

    fn snapshot(&self) -> Vec<Rc<Pin>> {
        self.pins.borrow().clone()
    }

    fn hide_all(&self) {
        for pin in self.snapshot() {
            pin.popover.popdown();
            pin.window.set_visible(false);
        }
    }

    /// Showing a hidden pin also clears click-through: the way back to an interactive pin.
    fn show_all(&self) {
        for pin in self.snapshot() {
            if !pin.window.is_visible() {
                pin.set_through(false);
                pin.window.present();
            }
        }
    }

    fn toggle(&self) -> Result<String, String> {
        let pins = self.snapshot();
        if pins.is_empty() {
            return Err("no pinned screenshots are open".into());
        }
        if pins.iter().any(|p| p.window.is_visible()) {
            self.hide_all();
            Ok(format!("hid {} pin(s)", pins.len()))
        } else {
            self.show_all();
            Ok(format!("showed {} pin(s)", pins.len()))
        }
    }

    /// Delayed so the reply that triggered it still reaches its client.
    fn quit_if_idle(self: &Rc<Self>) {
        if self.pins.borrow().is_empty() {
            let registry = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(400), move || {
                if registry.pins.borrow().is_empty() {
                    registry.main.quit();
                }
            });
        }
    }
}

/// Entry point of `sharpshot --pin-helper`: owns every pin window, serves the socket and exits
/// (removing the socket) when the last pin closes.
pub fn run_helper() -> Result<()> {
    let dir = socket_dir()?;
    verify_dir(&dir, true)?;
    // The directory lock makes helpers exclusive; a stale socket is only ever removed under it.
    let lock = File::open(&dir).context("cannot open the pin socket directory")?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            // Another helper is running (or is about to take over): nothing to do.
            Err(fs::TryLockError::WouldBlock) => return Ok(()),
            Err(fs::TryLockError::Error(e)) => {
                return Err(e).context("cannot lock the pin socket directory");
            }
        }
    }
    gtk::init().context("cannot open a display for pinned screenshots")?;
    crate::chrome::install();
    let display = gdk::Display::default().context("no display for pinned screenshots")?;
    let css = gtk::CssProvider::new();
    css.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 2,
    );
    let backend = Backend::detect(&display).map(Rc::new);

    let path = dir.join(SOCKET_NAME);
    if verify_socket(&path)? {
        fs::remove_file(&path).context("cannot remove the stale pin socket")?;
    }
    let listener = UnixListener::bind(&path).context("cannot create the pin socket")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

    let main = glib::MainLoop::new(None, false);
    let registry = Rc::new(Registry {
        pins: RefCell::new(Vec::new()),
        main: main.clone(),
    });
    for signal in [2, 15] {
        let main = main.clone();
        glib::unix_signal_add_local(signal, move || {
            main.quit();
            glib::ControlFlow::Break
        });
    }
    let (tx, rx) = async_channel::bounded(8);
    std::thread::spawn(move || serve(listener, tx));
    glib::spawn_future_local({
        let registry = registry.clone();
        async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    Msg::Pin { image, reply } => {
                        let result = match &backend {
                            Ok(backend) => registry.open(backend, image).map(|()| String::new()),
                            Err(error) => Err(error.clone()),
                        };
                        drop(reply.send(result));
                    }
                    Msg::Toggle { reply } => drop(reply.send(registry.toggle())),
                }
                registry.quit_if_idle();
            }
        }
    });
    main.run();
    drop(fs::remove_file(&path));
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair_with_helper(request: &[u8]) -> (String, Vec<Msg>) {
        let (client, server) = UnixStream::pair().unwrap();
        let (tx, rx) = async_channel::bounded(2);
        let worker = std::thread::spawn(move || {
            let mut seen = Vec::new();
            while let Ok(msg) = rx.recv_blocking() {
                let reply = match &msg {
                    Msg::Pin { reply, .. } | Msg::Toggle { reply } => reply.clone(),
                };
                reply.send(Ok("done".into())).unwrap();
                seen.push(msg);
            }
            seen
        });
        (&client).write_all(request).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        handle(&server, &tx);
        drop(tx);
        let mut reply = String::new();
        BufReader::new(&client).read_line(&mut reply).unwrap();
        (reply, worker.join().unwrap())
    }

    #[test]
    fn requests_are_validated() {
        assert_eq!(parse_request("TOGGLE").unwrap(), Request::Toggle);
        assert_eq!(
            parse_request("PIN 3 2").unwrap(),
            Request::Pin {
                width: 3,
                height: 2
            }
        );
        for bad in [
            "",
            "TOGGLE now",
            "PIN",
            "PIN 0 4",
            "PIN 4 0",
            "PIN 16385 1",
            "PIN 1 1 1",
            "PIN -1 4",
            "PIN 9000 9000",
            "pin 1 1",
        ] {
            assert!(parse_request(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn pin_request_delivers_exact_pixels() {
        let mut request = b"PIN 2 1\n".to_vec();
        request.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let (reply, seen) = pair_with_helper(&request);
        assert_eq!(reply, "OK done\n");
        let [Msg::Pin { image, .. }] = seen.as_slice() else {
            panic!("one pin message expected")
        };
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(image.as_raw(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn truncated_or_oversized_requests_are_refused_without_reaching_the_ui() {
        let (reply, seen) = pair_with_helper(b"PIN 2 2\n\x01\x02");
        // The read times out only after IO_TIMEOUT on a live peer; a closed one ends at once.
        assert!(reply.is_empty() || reply.starts_with("ERR"), "{reply:?}");
        assert!(seen.is_empty());
        let (reply, seen) = pair_with_helper(&[b'A'; HEADER_MAX + 8]);
        assert!(reply.starts_with("ERR"), "{reply:?}");
        assert!(seen.is_empty());
        let (reply, seen) = pair_with_helper(b"PIN 99999 1\n");
        assert!(reply.starts_with("ERR"), "{reply:?}");
        assert!(seen.is_empty());
    }

    #[test]
    fn toggle_round_trip_and_reply_parsing() {
        let (reply, seen) = pair_with_helper(b"TOGGLE\n");
        assert_eq!(reply, "OK done\n");
        assert!(matches!(seen.as_slice(), [Msg::Toggle { .. }]));
        assert_eq!(parse_reply("OK shown 2\n").unwrap(), "shown 2");
        assert_eq!(parse_reply("ERR nope\n").unwrap_err().to_string(), "nope");
        assert!(parse_reply("garbage").is_err());
    }

    #[test]
    fn reply_lines_are_single_line_and_bounded() {
        let line = reply_line(&Err(anyhow!("a\nb\r{}", "x".repeat(2000))));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.starts_with("ERR a b "));
        assert!(line.len() <= REPLY_MAX);
        assert_eq!(reply_line(&Ok(String::new())), "OK \n");
    }

    #[test]
    fn socket_directory_must_be_private_and_ours() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("pins");
        assert!(!verify_dir(&dir, false).unwrap());
        assert!(verify_dir(&dir, true).unwrap());
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_dir(&dir, false).unwrap());
        assert_eq!(
            fs::metadata(&dir).unwrap().mode() & 0o077,
            0,
            "loose modes are tightened"
        );
        let file = root.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(
            verify_dir(&file, false).is_err(),
            "a file is not a socket directory"
        );
        assert!(
            verify_socket(&file).is_err(),
            "a regular file is not a socket"
        );
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(
            verify_dir(&link, false).is_err(),
            "symlinked directories are refused"
        );
    }

    #[test]
    fn zoom_is_bounded_and_fit_never_enlarges() {
        let size = (4000, 2000);
        assert_eq!(clamp_zoom(100.0, size, 1), MAX_ZOOM.min(max_zoom(size, 1)));
        assert_eq!(clamp_zoom(0.0, size, 1), MIN_ZOOM);
        assert!(logical_size(size, clamp_zoom(100.0, size, 1), 1).0 <= MAX_LOGICAL as i32);
        let huge = (MAX_SIDE, MAX_SIDE);
        assert!(logical_size(huge, clamp_zoom(8.0, huge, 1), 1).0 <= MAX_LOGICAL as i32);
        assert_eq!(fit_zoom((100, 50), (1920, 1080), 1), 1.0);
        let fit = fit_zoom(size, (1920, 1080), 1);
        let (w, h) = logical_size(size, fit, 1);
        assert!(w <= 1536 && h <= 864, "{w}x{h}");
        // 100 % is one image pixel per device pixel.
        assert_eq!(logical_size((2000, 1000), 1.0, 2), (1000, 500));
    }

    #[test]
    fn surface_conversion_premultiplies() {
        let mut image = RgbaImage::new(2, 1);
        image.put_pixel(0, 0, image::Rgba([255, 0, 0, 128]));
        image.put_pixel(1, 0, image::Rgba([10, 20, 30, 255]));
        let mut surface = to_surface(&image).unwrap();
        let data = surface.data().unwrap();
        let px = |i: usize| u32::from_ne_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(px(0), 0x8080_0000);
        assert_eq!(px(1), 0xFF0A_141E);
    }

    #[test]
    fn ewmh_state_message_matches_the_spec() {
        let message = state_message(std::ptr::null_mut(), 7, 11, 22, 33);
        assert_eq!(
            (
                message.kind,
                message.format,
                message.window,
                message.message_type
            ),
            (33, 32, 7, 11)
        );
        assert_eq!(message.data, [1, 22, 33, 1, 0]);
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<ClientMessage>(), 96);
            assert_eq!(std::mem::size_of::<XEvent>(), 192);
        }
    }
}
