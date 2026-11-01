//! Opt-in local autosave and crash recovery.
//!
//! Autosaves are ordinary `.sharp` projects, so they hold the ORIGINAL screenshot, including the
//! pixels privacy masks cover. They are therefore written only after the user has said yes, into
//! an owner-only folder, by a worker thread, and never while nothing has changed. Closing a
//! window normally removes that window's autosaves; what remains is a crashed session, offered
//! for restore in a new window until the chosen retention expires.
//!
//! Every write is guarded by an epoch: disabling autosave or deleting autosaves bumps it, and a
//! save that was already running removes its own file instead of leaving it behind.
use crate::{
    chrome::{self, Icon},
    designer::{caption, named},
    document::{Crop, Document, MAX_MARKS, MAX_TEXT, MIN_BOX_WIDTH, Mark, MarkKind},
    interaction::State,
    presentation::Presentation,
    presets::{self, confirm, private_dir, read_bounded, write_private},
    project::Snapshot,
};
use anyhow::{Context, Result, ensure};
use gtk::{gdk, glib, prelude::*};
use image::RgbaImage;
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const VERSION: u32 = 1;
/// `(seconds, label)`; settings outside this list but inside 5..=3600 are still accepted.
pub const INTERVALS: [(u32, &str); 5] = [
    (15, "15 seconds"),
    (30, "30 seconds"),
    (60, "1 minute"),
    (120, "2 minutes"),
    (300, "5 minutes"),
];
pub const RETENTIONS: [(u32, &str); 5] = [
    (1, "1 hour"),
    (24, "1 day"),
    (72, "3 days"),
    (168, "1 week"),
    (720, "30 days"),
];
/// Newest autosaves kept per window; older ones are rotated out.
const KEEP_OWN: usize = 2;
/// Hard ceiling for the whole folder; the newest file of the running window is exempt.
const MAX_TOTAL: u64 = 4 << 30;
const MAX_SETTINGS: u64 = 4 << 10;
const TICK_SECS: u32 = 5;
const STALE_TEMP: Duration = Duration::from_secs(3600);

// --- settings ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Never answered: nothing is written until the user decides.
    #[default]
    Ask,
    On,
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub mode: Mode,
    pub interval_secs: u32,
    pub retention_hours: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: VERSION,
            mode: Mode::Ask,
            interval_secs: 30,
            retention_hours: 72,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "Unsupported recovery settings version"
        );
        ensure!(
            (5..=3600).contains(&self.interval_secs),
            "Autosave interval must be 5 to 3600 seconds"
        );
        ensure!(
            (1..=8760).contains(&self.retention_hours),
            "Retention must be 1 to 8760 hours"
        );
        Ok(())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct Head {
            version: u32,
        }
        let head: Head =
            serde_json::from_slice(bytes).context("Not a SharpShot recovery settings file")?;
        ensure!(
            head.version <= VERSION,
            "These recovery settings were written by a newer SharpShot (version {}); they are left untouched",
            head.version
        );
        let settings: Self = serde_json::from_slice(bytes).context("Invalid recovery settings")?;
        settings.validate()?;
        Ok(settings)
    }
}

fn settings_path() -> PathBuf {
    presets::config_dir().join("recovery.json")
}

fn data_dir() -> PathBuf {
    glib::user_data_dir().join("sharpshot").join("recovery")
}

fn load_settings_at(path: &Path) -> Result<Settings> {
    match read_bounded(path, MAX_SETTINGS)? {
        None => Ok(Settings::default()),
        Some(bytes) => Settings::parse(&bytes)
            .with_context(|| format!("Cannot use recovery settings in {}", path.display())),
    }
}

/// Refuses to replace settings written by a newer SharpShot.
fn save_settings_at(path: &Path, settings: &Settings) -> Result<()> {
    settings.validate()?;
    if let Ok(Some(bytes)) = read_bounded(path, MAX_SETTINGS)
        && let Err(error) = Settings::parse(&bytes)
        && format!("{error:#}").contains("newer SharpShot")
    {
        return Err(error);
    }
    let bytes = serde_json::to_vec_pretty(settings)?;
    write_private(path, &bytes)
}

// --- autosave files ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `(process id, start stamp)`; unique per window, also between windows of one process.
type Session = (u32, u64);

fn file_name((pid, start): Session, ms: u64) -> String {
    format!("rec-{pid}-{start:x}-{ms:x}.sharp")
}

fn parse_name(name: &str) -> Option<(Session, u64)> {
    let rest = name.strip_prefix("rec-")?.strip_suffix(".sharp")?;
    let mut parts = rest.split('-');
    let pid: u32 = parts.next()?.parse().ok()?;
    let start = u64::from_str_radix(parts.next()?, 16).ok()?;
    let ms = u64::from_str_radix(parts.next()?, 16).ok()?;
    let session = (pid, start);
    // Only names this module would write: nothing else in the folder is ever touched.
    (parts.next().is_none() && file_name(session, ms) == name).then_some((session, ms))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub session: Session,
    pub ms: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Written by this window.
    Own,
    /// The writing process still runs, so its window is probably open.
    Running,
    /// The writer is gone: a crashed or killed session.
    Interrupted,
}

fn alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn origin(entry: &Entry, own: Session) -> Origin {
    if entry.session == own {
        Origin::Own
    } else if alive(entry.session.0) {
        Origin::Running
    } else {
        Origin::Interrupted
    }
}

/// Newest first. A missing folder is simply empty; foreign files are ignored.
fn list_dir(dir: &Path) -> Result<Vec<Entry>> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("Cannot read {}", dir.display())),
    };
    let mut out = vec![];
    for item in read {
        let item = item?;
        let Some((session, ms)) = item.file_name().to_str().and_then(parse_name) else {
            continue;
        };
        // Another window or worker may delete a file between the directory read and here.
        let meta = match fs::symlink_metadata(item.path()) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("Cannot inspect {}", item.path().display()));
            }
        };
        if meta.is_file() {
            out.push(Entry {
                path: item.path(),
                session,
                ms,
                bytes: meta.len(),
            });
        }
    }
    out.sort_by(|a, b| b.ms.cmp(&a.ms).then_with(|| a.path.cmp(&b.path)));
    Ok(out)
}

/// What to delete: expired files, this window's rotated-out generations, then the oldest files
/// until the folder fits `cap`. `entries` is newest first; this window's newest file is exempt
/// from age and size limits so the latest recoverable state is never pruned from under it.
fn plan_prune(
    entries: &[Entry],
    now: u64,
    retention_ms: u64,
    own: Option<Session>,
    cap: u64,
) -> Vec<PathBuf> {
    let newest_own = entries
        .iter()
        .find(|e| Some(e.session) == own)
        .map(|e| &e.path);
    let mut doomed = vec![];
    let mut kept = vec![];
    let mut own_seen = 0;
    for e in entries {
        let mine = Some(e.session) == own;
        own_seen += usize::from(mine);
        let protected = newest_own == Some(&e.path);
        let expired = now.saturating_sub(e.ms) > retention_ms;
        if !protected && (expired || (mine && own_seen > KEEP_OWN)) {
            doomed.push(e.path.clone());
        } else {
            kept.push(e);
        }
    }
    let mut total: u64 = kept.iter().map(|e| e.bytes).sum();
    for e in kept.iter().rev() {
        if total <= cap {
            break;
        }
        if newest_own != Some(&e.path) {
            total -= e.bytes;
            doomed.push(e.path.clone());
        }
    }
    doomed
}

/// Removes every file in `paths`; all are attempted, the first failure is reported.
fn remove_all(paths: &[PathBuf]) -> Result<usize> {
    let mut first = None;
    let mut removed = 0;
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                first.get_or_insert_with(|| {
                    anyhow::Error::new(e).context(format!("Cannot delete {}", path.display()))
                });
            }
        }
    }
    first.map_or(Ok(removed), Err)
}

/// Also sweeps `.tmp*` leftovers of a write that died midway: they hold original pixels too.
fn prune_dir(dir: &Path, now: u64, retention_hours: u32, own: Option<Session>) -> Result<usize> {
    let mut doomed = plan_prune(
        &list_dir(dir)?,
        now,
        u64::from(retention_hours) * 3_600_000,
        own,
        MAX_TOTAL,
    );
    if let Ok(read) = fs::read_dir(dir) {
        for item in read.flatten() {
            let stale = item.file_name().to_string_lossy().starts_with(".tmp")
                && item
                    .metadata()
                    .ok()
                    .filter(|m| m.is_file())
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > STALE_TEMP);
            if stale {
                doomed.push(item.path());
            }
        }
    }
    remove_all(&doomed)
}

// --- capturing what to save -------------------------------------------------------------------

/// Identity of the saved content: the document revision plus the unfinished-caption signature.
type Key = (u64, Option<u64>);

struct Save {
    epoch: u64,
    /// Shared (all windows and processes) delete generation the job was captured under.
    generation: u64,
    key: Key,
    /// Sent once per screenshot; the worker keeps it.
    image: Option<RgbaImage>,
    marks: Vec<Mark>,
    crop: Option<Crop>,
    presentation: Presentation,
    retention_hours: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Saved,
    /// No longer permitted (disabled or deleted meanwhile, here or elsewhere); nothing remains.
    Skipped,
}

struct Done {
    epoch: u64,
    key: Key,
    result: std::result::Result<Outcome, String>,
}

/// Cancellation that every window and process shares through the private data folder, because
/// each window has its own epoch counter and none of them can see another's. Disabling autosave
/// is read back from the settings file; Delete all bumps a generation counter stored beside the
/// autosaves.
#[derive(Clone)]
struct Guard {
    settings: PathBuf,
    generation: PathBuf,
}

impl Guard {
    fn new(dir: &Path, settings: PathBuf) -> Self {
        Self {
            settings,
            generation: dir.join(".generation"),
        }
    }

    fn generation(&self) -> u64 {
        read_bounded(&self.generation, 64)
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Cancels every save in flight anywhere that was captured earlier. Call BEFORE listing files.
    fn bump(&self) -> Result<()> {
        write_private(
            &self.generation,
            self.generation().wrapping_add(1).to_string().as_bytes(),
        )
        .context("Cannot cancel autosaves in other windows")
    }

    /// Unreadable settings count as "off": nothing sensitive is written on a guess.
    fn permits(&self, generation: u64) -> bool {
        self.generation() == generation
            && load_settings_at(&self.settings).is_ok_and(|s| s.mode == Mode::On)
    }
}

/// Puts the unfinished caption into a snapshot's marks only (the live document is untouched):
/// it replaces the mark it edits, or is appended with an id nothing else uses.
fn add_draft(marks: &mut Vec<Mark>, mut draft: Mark) {
    if draft.id != 0
        && let Some(slot) = marks.iter_mut().find(|m| m.id == draft.id)
    {
        *slot = draft;
        return;
    }
    draft.id = marks
        .iter()
        .flat_map(|m| [m.id, m.group.unwrap_or(0)])
        .max()
        .unwrap_or(0)
        + 1;
    marks.push(draft);
}

/// The caption being typed, when a restored project could hold it: non-blank, bounded, finite.
/// Anything else is left out of the autosave rather than making the whole file unrestorable.
fn draft_of(state: &State) -> Option<(Mark, u64)> {
    let d = state.text_draft.as_ref()?;
    let mark = state.draft_mark()?;
    let MarkKind::Text {
        value, box_width, ..
    } = &mark.kind
    else {
        return None;
    };
    let editing = d.id.is_some_and(|id| state.doc.mark(id).is_some());
    let fits = !value.trim().is_empty()
        && value.len() <= MAX_TEXT
        && (*box_width == 0.0 || (box_width.is_finite() && *box_width >= MIN_BOX_WIDTH))
        && mark.font_size.is_finite()
        && mark.font_size >= 1.0
        && mark
            .points
            .iter()
            .all(|p| p.x.is_finite() && p.y.is_finite())
        && (editing || state.doc.marks.len() < MAX_MARKS);
    if !fits {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (d.id, d.at.x.to_bits(), d.at.y.to_bits()).hash(&mut h);
    d.value.hash(&mut h);
    (d.box_width.to_bits(), d.background, d.font_size.to_bits()).hash(&mut h);
    (d.text_style.bold, d.text_style.italic).hash(&mut h);
    std::mem::discriminant(&d.text_style.align).hash(&mut h);
    // A new caption takes the current colour and width.
    state.color.map(f64::to_bits).hash(&mut h);
    state.width.to_bits().hash(&mut h);
    Some((mark, h.finish()))
}

fn image_id(doc: &Document) -> (usize, i32, i32) {
    (
        doc.background.to_raw_none() as usize,
        doc.background.width(),
        doc.background.height(),
    )
}

/// The identity `capture` would give the window's content right now.
fn key_of(state: &State) -> Key {
    (state.doc.revision(), draft_of(state).map(|d| d.1))
}

/// `None` when nothing worth saving changed since `saved`, or the window is still pristine.
fn capture(
    state: &State,
    saved: Option<Key>,
    baseline: u64,
    epoch: u64,
    retention_hours: u32,
    with_image: bool,
) -> Result<Option<Save>> {
    let draft = draft_of(state);
    let key = (state.doc.revision(), draft.as_ref().map(|d| d.1));
    if saved == Some(key) || (key.0 == baseline && key.1.is_none()) {
        return Ok(None);
    }
    let mut marks = state.doc.marks.clone();
    if let Some((mark, _)) = draft {
        add_draft(&mut marks, mark);
    }
    Ok(Some(Save {
        epoch,
        // The caller stamps the shared generation it read BEFORE capturing.
        generation: 0,
        key,
        image: with_image
            .then(|| state.doc.background_image())
            .transpose()?,
        marks,
        crop: state.doc.crop,
        presentation: state.doc.presentation.clone(),
        retention_hours,
    }))
}

// --- worker -----------------------------------------------------------------------------------

struct Worker {
    tx: mpsc::Sender<Save>,
    handle: thread::JoinHandle<()>,
    /// Set before the sender is dropped: the worker then deletes its window's autosaves.
    discard: Arc<AtomicBool>,
}

fn spawn_worker(
    dir: PathBuf,
    session: Session,
    guard: Guard,
    epoch: Arc<AtomicU64>,
    done: async_channel::Sender<Done>,
) -> Worker {
    let (tx, jobs) = mpsc::channel::<Save>();
    let discard = Arc::new(AtomicBool::new(false));
    let flag = discard.clone();
    let handle = thread::spawn(move || {
        let mut image: Option<RgbaImage> = None;
        let mut last_ms = 0;
        for job in jobs {
            if job.image.is_some() {
                image = job.image;
            }
            let result = match image.take() {
                None => Err("the screenshot was not delivered to the autosave worker".to_owned()),
                Some(img) => {
                    let snapshot = Snapshot {
                        image: img,
                        marks: job.marks,
                        crop: job.crop,
                        presentation: job.presentation,
                    };
                    let ctx = Ctx {
                        dir: &dir,
                        session,
                        guard: &guard,
                        epoch: &epoch,
                        job_epoch: job.epoch,
                        generation: job.generation,
                        retention_hours: job.retention_hours,
                    };
                    let r = write_one(&ctx, &mut last_ms, &snapshot);
                    image = Some(snapshot.image);
                    r.map_err(|e| format!("{e:#}"))
                }
            };
            let _ = done.send_blocking(Done {
                epoch: job.epoch,
                key: job.key,
                result,
            });
        }
        if flag.load(Ordering::SeqCst) {
            discard_session(&dir, session);
        }
    });
    Worker {
        tx,
        handle,
        discard,
    }
}

/// What a worker needs to know to decide whether a save may still happen.
struct Ctx<'a> {
    dir: &'a Path,
    session: Session,
    guard: &'a Guard,
    /// This window's epoch counter and the value the job was captured under.
    epoch: &'a AtomicU64,
    job_epoch: u64,
    /// The shared generation the job was captured under.
    generation: u64,
    retention_hours: u32,
}

impl Ctx<'_> {
    /// Neither this window (epoch) nor any window or process (generation, settings on disk) has
    /// disabled or deleted autosaves since the job was captured.
    fn live(&self) -> bool {
        self.epoch.load(Ordering::SeqCst) == self.job_epoch && self.guard.permits(self.generation)
    }
}

/// A save that is no longer permitted writes nothing, or removes what it wrote: permission is
/// checked again after the file exists, and a deleter bumps the generation (or switches the
/// setting off) BEFORE it lists files, so a file this worker made is either seen and removed by
/// the deleter or seen as stale by the worker. Holds across windows and processes.
fn write_one(ctx: &Ctx, last_ms: &mut u64, snapshot: &Snapshot) -> Result<Outcome> {
    write_hooked(ctx, last_ms, snapshot, || {})
}

/// `write_one`, with `between` run after the file exists and before the second check.
fn write_hooked(
    ctx: &Ctx,
    last_ms: &mut u64,
    snapshot: &Snapshot,
    between: impl FnOnce(),
) -> Result<Outcome> {
    if !ctx.live() {
        return Ok(Outcome::Skipped);
    }
    private_dir(ctx.dir)?;
    let ms = now_ms().max(*last_ms + 1);
    *last_ms = ms;
    let path = ctx.dir.join(file_name(ctx.session, ms));
    snapshot.save_new(&path)?;
    between();
    if !ctx.live() {
        let _ = fs::remove_file(&path);
        return Ok(Outcome::Skipped);
    }
    if let Err(error) = prune_dir(ctx.dir, now_ms(), ctx.retention_hours, Some(ctx.session)) {
        eprintln!("Cannot prune autosaves: {error:#}");
    }
    Ok(Outcome::Saved)
}

fn discard_session(dir: &Path, session: Session) {
    let own: Vec<PathBuf> = list_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.session == session)
        .map(|e| e.path)
        .collect();
    if let Err(error) = remove_all(&own) {
        eprintln!("Cannot remove autosaves: {error:#}");
    }
}

// --- the window-side object -------------------------------------------------------------------

struct Inner {
    settings: Settings,
    /// Why the settings file is unusable; autosave then stays off.
    problem: Option<String>,
    worker: Option<Worker>,
    in_flight: bool,
    saved: Option<Key>,
    image_sent: Option<(usize, i32, i32)>,
    last_try: Option<Instant>,
    last_ok_ms: Option<u64>,
    last_error: Option<String>,
    entries: Vec<Entry>,
}

pub struct RecoveryUi {
    parent: glib::WeakRef<gtk::ApplicationWindow>,
    state: Rc<RefCell<State>>,
    dir: PathBuf,
    session: Session,
    /// Revision of a window that has not been edited: nothing to recover, nothing written.
    baseline: u64,
    epoch: Arc<AtomicU64>,
    /// Shared with every other window and process; see [`Guard`].
    guard: Guard,
    done: async_channel::Sender<Done>,
    inner: RefCell<Inner>,
    on_restore: RefCell<Option<Rc<dyn Fn(Document)>>>,
    on_change: RefCell<Option<Rc<dyn Fn()>>>,
    observer: RefCell<Option<Rc<dyn Fn()>>>,
    manager: RefCell<Option<glib::WeakRef<gtk::Window>>>,
}

fn unique_start() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let next = now_ms().max(LAST.load(Ordering::SeqCst) + 1);
    LAST.store(next, Ordering::SeqCst);
    next
}

impl RecoveryUi {
    /// Starts watching `state`. Nothing is written until the user has enabled autosave; the
    /// first window of a fresh install asks. Interrupted sessions found on disk are announced.
    /// Give it a restore callback with [`set_on_restore`](Self::set_on_restore).
    pub fn attach(
        parent: &gtk::ApplicationWindow,
        state: Rc<RefCell<State>>,
        _refresh: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let baseline = state.borrow().doc.revision();
        let (done, results) = async_channel::unbounded();
        let ui = Rc::new(Self {
            parent: parent.downgrade(),
            state,
            dir: data_dir(),
            session: (std::process::id(), unique_start()),
            baseline,
            epoch: Arc::new(AtomicU64::new(0)),
            guard: Guard::new(&data_dir(), settings_path()),
            done,
            inner: RefCell::new(Inner {
                settings: Settings::default(),
                problem: None,
                worker: None,
                in_flight: false,
                saved: None,
                image_sent: None,
                last_try: None,
                last_ok_ms: None,
                last_error: None,
                entries: vec![],
            }),
            on_restore: RefCell::new(None),
            on_change: RefCell::new(None),
            observer: RefCell::new(None),
            manager: RefCell::new(None),
        });
        ui.reload_settings();
        ui.prune();
        ui.rescan();

        let weak = Rc::downgrade(&ui);
        glib::spawn_future_local({
            let weak = weak.clone();
            async move {
                while let Ok(done) = results.recv().await {
                    let Some(ui) = weak.upgrade() else { break };
                    ui.on_done(done);
                }
            }
        });
        glib::timeout_add_seconds_local(TICK_SECS, {
            let weak = weak.clone();
            move || match weak.upgrade() {
                Some(ui) => {
                    ui.tick();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        let keep = ui.clone();
        parent.connect_destroy(move |_| keep.shutdown());
        glib::idle_add_local_once({
            let weak = weak.clone();
            move || {
                if let Some(ui) = weak.upgrade() {
                    ui.startup();
                }
            }
        });
        ui
    }

    /// Called with a fully validated document to open as a NEW editor window.
    pub fn set_on_restore(&self, restore: Rc<dyn Fn(Document)>) {
        *self.on_restore.borrow_mut() = Some(restore);
    }

    /// Called whenever [`status`](Self::status) or [`available`](Self::available) changes; use it
    /// to show or hide a recover button.
    pub fn set_on_change(&self, changed: Rc<dyn Fn()>) {
        *self.on_change.borrow_mut() = Some(changed);
    }

    /// Interrupted sessions (not this window, not a still-running one) that can be restored.
    pub fn available(&self) -> usize {
        let inner = self.inner.borrow();
        let sessions: HashSet<Session> = inner
            .entries
            .iter()
            .filter(|e| origin(e, self.session) == Origin::Interrupted)
            .map(|e| e.session)
            .collect();
        sessions.len()
    }

    /// One line for a tooltip or label.
    pub fn status(&self) -> String {
        let inner = self.inner.borrow();
        if let Some(problem) = &inner.problem {
            return format!("Autosave off: {problem}");
        }
        match inner.settings.mode {
            Mode::Ask => "Autosave not enabled".to_owned(),
            Mode::Off => "Autosave off".to_owned(),
            Mode::On => match (&inner.last_error, inner.last_ok_ms) {
                (Some(error), _) => format!("Autosave failed: {error}"),
                (None, Some(ms)) => format!("Autosaved {}", clock(ms)),
                (None, None) => "Autosave on; saves after your next change".to_owned(),
            },
        }
    }

    fn changed(&self) {
        let cb = self.on_change.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
        let observer = self.observer.borrow().clone();
        if let Some(observer) = observer {
            observer();
        }
    }

    fn mode(&self) -> Mode {
        let inner = self.inner.borrow();
        if inner.problem.is_some() {
            Mode::Off
        } else {
            inner.settings.mode
        }
    }

    /// Re-reads settings from disk so a change made in another window applies here.
    fn reload_settings(&self) {
        let (settings, problem) = match load_settings_at(&settings_path()) {
            Ok(settings) => (settings, None),
            Err(error) => (
                Settings {
                    mode: Mode::Off,
                    ..Settings::default()
                },
                Some(format!("{error:#}")),
            ),
        };
        let was_on = self.mode() == Mode::On;
        {
            let mut inner = self.inner.borrow_mut();
            inner.settings = settings;
            inner.problem = problem;
        }
        if was_on && self.mode() != Mode::On {
            self.stop_worker();
        }
    }

    /// Invalidates every save in flight and drops the worker (it finishes by itself).
    fn stop_worker(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let mut inner = self.inner.borrow_mut();
        inner.worker = None;
        inner.in_flight = false;
        inner.image_sent = None;
        inner.saved = None;
    }

    fn prune(&self) {
        let hours = self.inner.borrow().settings.retention_hours;
        if let Err(error) = prune_dir(&self.dir, now_ms(), hours, Some(self.session)) {
            self.inner.borrow_mut().last_error = Some(format!("{error:#}"));
        }
    }

    fn rescan(&self) {
        match list_dir(&self.dir) {
            Ok(entries) => self.inner.borrow_mut().entries = entries,
            Err(error) => self.inner.borrow_mut().last_error = Some(format!("{error:#}")),
        }
    }

    fn tick(&self) {
        let before = (self.status(), self.available());
        self.reload_settings();
        self.rescan();
        self.maybe_save();
        if before != (self.status(), self.available()) {
            self.changed();
        }
    }

    fn maybe_save(&self) {
        if self.mode() != Mode::On {
            return;
        }
        let mut inner = self.inner.borrow_mut();
        let interval = Duration::from_secs(u64::from(inner.settings.interval_secs));
        if inner.in_flight || inner.last_try.is_some_and(|t| t.elapsed() < interval) {
            return;
        }
        let Ok(state) = self.state.try_borrow() else {
            return;
        };
        let generation = self.guard.generation();
        if inner.worker.is_none() {
            inner.worker = Some(spawn_worker(
                self.dir.clone(),
                self.session,
                self.guard.clone(),
                self.epoch.clone(),
                self.done.clone(),
            ));
            inner.image_sent = None;
        }
        let id = image_id(&state.doc);
        let job = capture(
            &state,
            inner.saved,
            self.baseline,
            self.epoch.load(Ordering::SeqCst),
            inner.settings.retention_hours,
            inner.image_sent != Some(id),
        )
        .map(|job| job.map(|job| Save { generation, ..job }));
        match job {
            Ok(None) => {}
            Ok(Some(job)) => {
                let sent_image = job.image.is_some();
                let sent = inner
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.tx.send(job).is_ok());
                if sent {
                    inner.in_flight = true;
                    inner.last_try = Some(Instant::now());
                    if sent_image {
                        inner.image_sent = Some(id);
                    }
                } else {
                    inner.worker = None;
                    inner.image_sent = None;
                    inner.last_error = Some("the autosave worker stopped".to_owned());
                }
            }
            Err(error) => {
                inner.last_try = Some(Instant::now());
                inner.last_error = Some(format!("{error:#}"));
            }
        }
    }

    fn on_done(&self, done: Done) {
        if done.epoch != self.epoch.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut inner = self.inner.borrow_mut();
            inner.in_flight = false;
            match done.result {
                Ok(Outcome::Saved) => {
                    inner.saved = Some(done.key);
                    inner.last_ok_ms = Some(now_ms());
                    inner.last_error = None;
                }
                // Another window or process deleted everything or switched autosave off while
                // this was in flight. Nothing was left behind, and the deletion is respected:
                // no retry until the content changes again (a switch-off stops the worker on
                // the next settings reload).
                Ok(Outcome::Skipped) => inner.saved = Some(done.key),
                Err(error) => inner.last_error = Some(error),
            }
        }
        self.rescan();
        self.changed();
    }

    /// Window closed: its autosaves go with it. Waits for a save in progress so none is left.
    fn shutdown(&self) {
        *self.observer.borrow_mut() = None;
        let worker = self.inner.borrow_mut().worker.take();
        if let Some(worker) = worker {
            worker.discard.store(true, Ordering::SeqCst);
            drop(worker.tx);
            let _ = worker.handle.join();
        }
        self.epoch.fetch_add(1, Ordering::SeqCst);
        discard_session(&self.dir, self.session);
    }

    fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<()> {
        let path = settings_path();
        let mut settings = load_settings_at(&path).unwrap_or_default();
        change(&mut settings);
        save_settings_at(&path, &settings)?;
        self.reload_settings();
        self.prune();
        self.rescan();
        self.changed();
        Ok(())
    }

    /// Deletes `paths` after bumping the epoch, so a save finishing now removes its own file.
    fn delete(&self, paths: &[PathBuf]) -> Result<usize> {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        {
            let mut inner = self.inner.borrow_mut();
            inner.in_flight = false;
            inner.saved = None;
        }
        let result = remove_all(paths);
        self.rescan();
        self.changed();
        result
    }

    /// Deletes every autosave in the folder, whichever window or process wrote it, and cancels
    /// saves in flight everywhere: the shared generation moves BEFORE the folder is listed, so
    /// a file a worker creates meanwhile is either listed here or removed by that worker.
    /// Nothing is rewritten until this window's content changes again.
    fn delete_all(&self) -> Result<usize> {
        let bumped = self.guard.bump();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        {
            let key = self.state.try_borrow().ok().map(|s| key_of(&s));
            let mut inner = self.inner.borrow_mut();
            inner.in_flight = false;
            inner.saved = key;
        }
        let removed = list_dir(&self.dir)
            .map(|entries| entries.into_iter().map(|e| e.path).collect::<Vec<_>>())
            .and_then(|paths| remove_all(&paths));
        self.rescan();
        self.changed();
        bumped?;
        removed
    }

    fn restore(self: &Rc<Self>, path: PathBuf, finished: impl Fn(Result<(), String>) + 'static) {
        let valid = path.parent() == Some(self.dir.as_path())
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_name)
                .is_some();
        let Some(restore) = self.on_restore.borrow().clone().filter(|_| valid) else {
            return finished(Err(if valid {
                "Restoring is not connected to the editor".to_owned()
            } else {
                "That is not an autosave file".to_owned()
            }));
        };
        let (tx, rx) = async_channel::bounded(1);
        thread::spawn(move || {
            let _ = tx.send_blocking(Snapshot::load(&path).map_err(|e| format!("{e:#}")));
        });
        glib::spawn_future_local(async move {
            let loaded = match rx.recv().await {
                Ok(Ok(snapshot)) => snapshot.into_document().map_err(|e| format!("{e:#}")),
                Ok(Err(error)) => Err(error),
                Err(_) => Err("the loader stopped unexpectedly".to_owned()),
            };
            match loaded {
                Ok(doc) => {
                    restore(doc);
                    finished(Ok(()));
                }
                Err(error) => finished(Err(error)),
            }
        });
    }

    // --- dialogs ---------------------------------------------------------------------------

    fn startup(self: &Rc<Self>) {
        if self.mode() == Mode::Ask {
            self.ask_consent();
        } else {
            self.announce();
        }
    }

    fn ask_consent(self: &Rc<Self>) {
        let Some(parent) = self.parent.upgrade() else {
            return;
        };
        let dialog = gtk::MessageDialog::builder()
            .transient_for(&parent)
            .modal(true)
            .message_type(gtk::MessageType::Question)
            .text("Save your work automatically?")
            .secondary_text(format!(
                "SharpShot can keep a recovery copy of what you are editing, so a crash does not \
                 lose it.\n\nThe copy contains the ORIGINAL screenshot, including anything your \
                 privacy masks hide, and is not encrypted. It is private to your account and \
                 stored in {}. It is deleted when you close the window normally, and after the \
                 retention you choose.\n\nNothing is written unless you enable it. You can change \
                 or delete everything later under Recovery.",
                self.dir.display()
            ))
            .build();
        dialog.add_button("Not now", gtk::ResponseType::Cancel);
        dialog.add_button("Never", gtk::ResponseType::Reject);
        dialog.add_button("Enable autosave", gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Cancel);
        let weak = Rc::downgrade(self);
        dialog.connect_response(move |dialog, response| {
            dialog.close();
            let Some(ui) = weak.upgrade() else { return };
            let mode = match response {
                gtk::ResponseType::Accept => Some(Mode::On),
                gtk::ResponseType::Reject => Some(Mode::Off),
                _ => None,
            };
            if let Some(mode) = mode
                && let Err(error) = ui.update_settings(|s| s.mode = mode)
            {
                ui.note_error(&format!("{error:#}"));
            }
            ui.announce();
        });
        dialog.present();
    }

    /// Tells the user about recoverable sessions; the dialog is not modal.
    fn announce(self: &Rc<Self>) {
        let count = self.available();
        let Some(parent) = self.parent.upgrade().filter(|_| count > 0) else {
            return;
        };
        let dialog = gtk::MessageDialog::builder()
            .transient_for(&parent)
            .modal(false)
            .message_type(gtk::MessageType::Info)
            .text("Unsaved work can be recovered")
            .secondary_text(format!(
                "SharpShot found autosaved work from {count} interrupted session{}.",
                if count == 1 { "" } else { "s" }
            ))
            .build();
        dialog.add_button("Later", gtk::ResponseType::Cancel);
        dialog.add_button("Review", gtk::ResponseType::Accept);
        let weak = Rc::downgrade(self);
        dialog.connect_response(move |dialog, response| {
            dialog.close();
            if response == gtk::ResponseType::Accept
                && let Some(ui) = weak.upgrade()
            {
                ui.open();
            }
        });
        dialog.present();
    }

    fn note_error(&self, message: &str) {
        self.inner.borrow_mut().last_error = Some(message.to_owned());
        self.changed();
    }

    /// Opens (or raises) the recovery manager: settings, saved work, restore and delete.
    pub fn open(self: &Rc<Self>) {
        let existing = self.manager.borrow().as_ref().and_then(|w| w.upgrade());
        if let Some(window) = existing {
            window.present();
            return;
        }
        self.rescan();
        let Some(parent) = self.parent.upgrade() else {
            return;
        };
        let window = gtk::Window::builder()
            .transient_for(&parent)
            .title("Recovery")
            .default_width(560)
            .default_height(560)
            .build();
        window.add_css_class("sharpshot");
        window.set_titlebar(Some(&gtk::HeaderBar::new()));
        *self.manager.borrow_mut() = Some(window.downgrade());
        let panel = Panel::build(self, &window);
        let render = {
            let panel = Rc::downgrade(&panel);
            Rc::new(move || {
                if let Some(panel) = panel.upgrade() {
                    panel.render();
                }
            }) as Rc<dyn Fn()>
        };
        *self.observer.borrow_mut() = Some(render);
        window.connect_destroy({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(ui) = weak.upgrade() {
                    *ui.observer.borrow_mut() = None;
                }
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let window = window.downgrade();
            move |_, key, _, _| {
                if key != gdk::Key::Escape {
                    return glib::Propagation::Proceed;
                }
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                glib::Propagation::Stop
            }
        });
        window.add_controller(keys);
        panel.render();
        window.present();
    }
}

fn clock(ms: u64) -> String {
    glib::DateTime::from_unix_local((ms / 1000) as i64)
        .ok()
        .and_then(|d| d.format("%Y-%m-%d %H:%M:%S").ok())
        .map_or_else(|| "at an unknown time".to_owned(), |s| s.to_string())
}

fn choice(options: &[(u32, &str)], name: &str, tip: &str) -> gtk::DropDown {
    let labels: Vec<&str> = options.iter().map(|o| o.1).collect();
    let drop = gtk::DropDown::new(Some(gtk::StringList::new(&labels)), gtk::Expression::NONE);
    named(&drop, name, tip);
    drop
}

fn nearest(options: &[(u32, &str)], value: u32) -> u32 {
    options
        .iter()
        .enumerate()
        .min_by_key(|(_, o)| o.0.abs_diff(value))
        .map_or(0, |(i, _)| i as u32)
}

struct Panel {
    ui: std::rc::Weak<RecoveryUi>,
    window: glib::WeakRef<gtk::Window>,
    enabled: gtk::CheckButton,
    interval: gtk::DropDown,
    retention: gtk::DropDown,
    list: gtk::ListBox,
    status: gtk::Label,
    note: gtk::Label,
    delete_all: gtk::Button,
    /// Set while code writes the controls, so their handlers stay quiet.
    guard: Cell<bool>,
    /// A restore is loading; a second click waits.
    busy: Rc<Cell<bool>>,
}

impl Panel {
    fn build(ui: &Rc<RecoveryUi>, window: &gtk::Window) -> Rc<Self> {
        let panel = Rc::new(Self {
            ui: Rc::downgrade(ui),
            window: window.downgrade(),
            enabled: gtk::CheckButton::with_label("Save recovery copies while I edit"),
            interval: choice(
                &INTERVALS,
                "Autosave interval",
                "How often changes are saved",
            ),
            retention: choice(
                &RETENTIONS,
                "Keep recovery copies for",
                "Older recovery copies are deleted automatically",
            ),
            list: gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .build(),
            status: caption("", "dimension-label"),
            note: caption("", "dimension-label"),
            delete_all: chrome::button(
                Icon::Delete,
                "Delete all recovery copies",
                "Delete every recovery copy, including interrupted sessions",
            ),
            guard: Cell::new(false),
            busy: Rc::new(Cell::new(false)),
        });
        panel.status.set_wrap(true);
        panel.note.set_wrap(true);
        named(
            &panel.list,
            "Recovery copies",
            "Recovery copies on this computer",
        );
        let none = caption("No recovery copies", "dimension-label");
        none.set_margin_top(12);
        none.set_margin_bottom(12);
        panel.list.set_placeholder(Some(&none));

        let weak = Rc::downgrade(&panel);
        panel.enabled.connect_toggled({
            let weak = weak.clone();
            move |check| {
                if let Some(panel) = weak.upgrade() {
                    let on = check.is_active();
                    panel.change(move |s| s.mode = if on { Mode::On } else { Mode::Off });
                }
            }
        });
        panel.interval.connect_selected_notify({
            let weak = weak.clone();
            move |drop| {
                if let Some(panel) = weak.upgrade() {
                    let secs = INTERVALS[(drop.selected() as usize).min(INTERVALS.len() - 1)].0;
                    panel.change(move |s| s.interval_secs = secs);
                }
            }
        });
        panel.retention.connect_selected_notify({
            let weak = weak.clone();
            move |drop| {
                if let Some(panel) = weak.upgrade() {
                    let hours = RETENTIONS[(drop.selected() as usize).min(RETENTIONS.len() - 1)].0;
                    panel.change(move |s| s.retention_hours = hours);
                }
            }
        });
        panel.delete_all.connect_clicked({
            let weak = weak.clone();
            move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.confirm_delete_all();
                }
            }
        });

        let grid = gtk::Grid::builder()
            .row_spacing(8)
            .column_spacing(12)
            .build();
        grid.attach(&panel.enabled, 0, 0, 2, 1);
        grid.attach(&caption("Save every", "field-label"), 0, 1, 1, 1);
        grid.attach(&panel.interval, 1, 1, 1, 1);
        grid.attach(&caption("Keep copies for", "field-label"), 0, 2, 1, 1);
        grid.attach(&panel.retention, 1, 2, 1, 1);

        let warning = caption(
            "Recovery copies contain the original screenshot, including areas hidden by privacy \
             masks. They are private to your account, unencrypted, and removed when you close \
             the window normally.",
            "dimension-label",
        );
        warning.set_wrap(true);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&panel.list)
            .build();
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = caption("Saved work", "title");
        title.set_hexpand(true);
        head.append(&title);
        head.append(&panel.delete_all);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.set_margin_start(12);
        body.set_margin_end(12);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.append(&caption("Autosave", "title"));
        body.append(&grid);
        body.append(&warning);
        body.append(&panel.status);
        body.append(&head);
        body.append(&scroll);
        body.append(&panel.note);
        window.set_child(Some(&body));
        // The window owns the panel; dropping the window drops it.
        let keep = panel.clone();
        window.connect_destroy(move |_| {
            let _ = &keep;
        });
        panel
    }

    fn say(&self, message: &str) {
        self.note.set_text(message);
    }

    fn change(&self, change: impl FnOnce(&mut Settings)) {
        if self.guard.get() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        match ui.update_settings(change) {
            Ok(()) => self.say(""),
            Err(error) => {
                self.say(&format!("{error:#}"));
                self.render();
            }
        }
    }

    fn render(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        self.guard.set(true);
        let (settings, entries) = {
            let inner = ui.inner.borrow();
            (inner.settings.clone(), inner.entries.clone())
        };
        let usable = ui.inner.borrow().problem.is_none();
        self.enabled.set_active(usable && settings.mode == Mode::On);
        self.enabled.set_sensitive(usable);
        self.interval
            .set_selected(nearest(&INTERVALS, settings.interval_secs));
        self.retention
            .set_selected(nearest(&RETENTIONS, settings.retention_hours));
        self.guard.set(false);
        self.status.set_text(&ui.status());
        self.delete_all.set_sensitive(!entries.is_empty());

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for entry in &entries {
            self.list.append(&self.row(&ui, entry));
        }
    }

    fn row(&self, ui: &Rc<RecoveryUi>, entry: &Entry) -> gtk::Box {
        let kind = match origin(entry, ui.session) {
            Origin::Own => "this window",
            Origin::Running => "open in another window",
            Origin::Interrupted => "interrupted session",
        };
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        line.set_margin_top(4);
        line.set_margin_bottom(4);
        line.set_margin_start(6);
        line.set_margin_end(6);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        let when = gtk::Label::new(Some(&clock(entry.ms)));
        when.set_xalign(0.0);
        text.append(&when);
        text.append(&caption(
            &format!("{} · {kind}", glib::format_size(entry.bytes)),
            "dimension-label",
        ));
        line.append(&text);

        let restore = chrome::button(
            Icon::Open,
            "Restore in a new window",
            "Open this recovery copy in a new editor window; the copy is kept",
        );
        let delete = chrome::button(
            Icon::Delete,
            "Delete recovery copy",
            "Delete this recovery copy",
        );
        let path = entry.path.clone();
        let ms = entry.ms;
        let weak = self.window.clone();
        let me = self.ui.clone();
        restore.connect_clicked({
            let path = path.clone();
            let note = self.note.clone();
            let busy = self.busy.clone();
            move |button| {
                let Some(ui) = me.upgrade() else { return };
                if busy.get() {
                    return;
                }
                busy.set(true);
                button.set_sensitive(false);
                note.set_text("Opening recovery copy…");
                let (note, busy, button) = (note.clone(), busy.clone(), button.clone());
                ui.restore(path.clone(), move |result| {
                    busy.set(false);
                    button.set_sensitive(true);
                    note.set_text(&match result {
                        Ok(()) => "Opened in a new window".to_owned(),
                        Err(error) => format!("Cannot restore: {error}"),
                    });
                });
            }
        });
        let me = self.ui.clone();
        let note = self.note.clone();
        delete.connect_clicked(move |_| {
            let (Some(ui), Some(window)) = (me.upgrade(), weak.upgrade()) else {
                return;
            };
            let (path, note) = (path.clone(), note.clone());
            let name = clock(ms);
            confirm(
                &window,
                &format!("Delete the recovery copy from {name}?"),
                "This cannot be undone.",
                "Delete",
                move || {
                    note.set_text(&match ui.delete(std::slice::from_ref(&path)) {
                        Ok(_) => "Deleted".to_owned(),
                        Err(error) => format!("{error:#}"),
                    });
                },
            );
        });
        line.append(&restore);
        line.append(&delete);
        line
    }

    fn confirm_delete_all(&self) {
        let (Some(ui), Some(window)) = (self.ui.upgrade(), self.window.upgrade()) else {
            return;
        };
        let count = ui.inner.borrow().entries.len();
        let note = self.note.clone();
        confirm(
            &window,
            &format!(
                "Delete {count} recovery cop{}?",
                if count == 1 { "y" } else { "ies" }
            ),
            "This includes work from interrupted sessions and cannot be undone. While autosave is \
             on, this window saves a fresh copy after its next change.",
            "Delete all",
            move || {
                note.set_text(&match ui.delete_all() {
                    Ok(n) => format!(
                        "Deleted {n} recovery cop{}",
                        if n == 1 { "y" } else { "ies" }
                    ),
                    Err(error) => format!("{error:#}"),
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{document::TextStyle, interaction::TextDraft, shapes::Point};

    fn entry(pid: u32, start: u64, ms: u64, bytes: u64) -> Entry {
        Entry {
            path: PathBuf::from(file_name((pid, start), ms)),
            session: (pid, start),
            ms,
            bytes,
        }
    }

    #[test]
    fn settings_are_validated_and_versioned() {
        let ok = serde_json::to_vec(&Settings::default()).unwrap();
        assert_eq!(Settings::parse(&ok).unwrap(), Settings::default());
        let text = String::from_utf8(ok).unwrap();
        let edit = |from: &str, to: &str| Settings::parse(text.replacen(from, to, 1).as_bytes());
        assert!(edit("\"interval_secs\":30", "\"interval_secs\":1").is_err());
        assert!(edit("\"retention_hours\":72", "\"retention_hours\":0").is_err());
        assert!(edit("\"mode\":\"ask\"", "\"mode\":\"always\"").is_err());
        assert!(
            edit("\"version\":1", "\"version\":9")
                .unwrap_err()
                .to_string()
                .contains("newer")
        );
        assert!(Settings::parse(b"{}").is_err());
        assert_eq!(
            Settings::default().mode,
            Mode::Ask,
            "no consent is the default"
        );
    }

    #[test]
    fn settings_file_is_private_and_newer_files_survive() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cfg").join("recovery.json");
        assert_eq!(load_settings_at(&path).unwrap(), Settings::default());
        let on = Settings {
            mode: Mode::On,
            ..Settings::default()
        };
        save_settings_at(&path, &on).unwrap();
        assert_eq!(load_settings_at(&path).unwrap(), on);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::write(&path, br#"{"version":2,"mode":"on"}"#).unwrap();
        assert!(save_settings_at(&path, &on).is_err());
        assert!(fs::read_to_string(&path).unwrap().contains("\"version\":2"));
        fs::write(&path, b"garbage").unwrap();
        save_settings_at(&path, &on).unwrap();
        assert_eq!(
            load_settings_at(&path).unwrap(),
            on,
            "unreadable settings may be replaced"
        );
    }

    #[test]
    fn only_our_file_names_are_recognised() {
        let name = file_name((42, 0xabc), 0x1234);
        assert_eq!(parse_name(&name), Some(((42, 0xabc), 0x1234)));
        for bad in [
            "rec-42-abc.sharp",
            "rec-42-abc-1234-9.sharp",
            "rec-+42-abc-1234.sharp",
            "rec-42-ABC-1234.sharp",
            "rec-42-abc-1234.png",
            "notes.sharp",
            "rec--abc-1.sharp",
        ] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn pruning_expires_rotates_and_bounds_but_keeps_the_newest_own() {
        let own = (1, 1);
        let hour = 3_600_000;
        let now = 100 * hour;
        let entries = vec![
            entry(1, 1, now - hour, 10),      // own newest
            entry(1, 1, now - 2 * hour, 10),  // own second
            entry(1, 1, now - 3 * hour, 10),  // own third: rotated out
            entry(2, 5, now - 5 * hour, 10),  // other, fresh
            entry(3, 6, now - 80 * hour, 10), // other, expired at 72 h
        ];
        let gone = plan_prune(&entries, now, 72 * hour, Some(own), u64::MAX);
        assert_eq!(gone, vec![entries[2].path.clone(), entries[4].path.clone()]);

        // Everything expired, yet this window's newest file survives.
        let gone = plan_prune(&entries, now, 0, Some(own), u64::MAX);
        assert!(!gone.contains(&entries[0].path));
        assert!(gone.contains(&entries[3].path));

        // Size cap removes the oldest first and never the exempt file.
        let gone = plan_prune(&entries[..2], now, 72 * hour, Some(own), 5);
        assert_eq!(gone, vec![entries[1].path.clone()]);
        let gone = plan_prune(&entries[3..4], now, 72 * hour, Some(own), 5);
        assert_eq!(gone, vec![entries[3].path.clone()]);
    }

    #[test]
    fn prune_dir_deletes_only_matching_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let session = (7, 7);
        let old = dir.join(file_name((9, 9), 1));
        let fresh = dir.join(file_name(session, now_ms()));
        let foreign = dir.join("keep-me.sharp");
        for p in [&old, &fresh, &foreign] {
            fs::write(p, b"x").unwrap();
        }
        assert_eq!(prune_dir(dir, now_ms(), 1, Some(session)).unwrap(), 1);
        assert!(!old.exists() && fresh.exists() && foreign.exists());
        assert_eq!(list_dir(&dir.join("missing")).unwrap(), vec![]);
    }

    fn text_mark(id: u64) -> Mark {
        Mark::text(id, Point { x: 5., y: 6. }, "hello", [1.; 4], 2.)
    }

    #[test]
    fn draft_replaces_its_mark_or_gets_a_free_id() {
        let mut grouped = text_mark(3);
        grouped.group = Some(9);
        let mut marks = vec![text_mark(1), grouped];
        let mut edit = text_mark(1);
        edit.points[0].x = 50.;
        add_draft(&mut marks, edit.clone());
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0], edit);

        add_draft(&mut marks, text_mark(0));
        assert_eq!(marks.len(), 3);
        assert_eq!(marks[2].id, 10, "beyond every mark id and group id");
        // A draft whose mark was deleted meanwhile becomes a new mark, not a dangling id.
        add_draft(&mut marks, text_mark(77));
        assert_eq!(marks[3].id, 11);
    }

    fn state() -> State {
        let (tx, _) = mpsc::channel();
        State::new(Document::new(RgbaImage::new(40, 30)).unwrap(), tx)
    }

    fn draft(value: &str) -> TextDraft {
        TextDraft {
            id: None,
            at: Point { x: 4., y: 4. },
            value: value.into(),
            box_width: 0.0,
            background: false,
            font_size: 20.0,
            text_style: TextStyle::default(),
        }
    }

    #[test]
    fn capture_tracks_revision_and_unfinished_caption() {
        let mut state = state();
        let baseline = state.doc.revision();
        assert!(
            capture(&state, None, baseline, 0, 24, true)
                .unwrap()
                .is_none(),
            "pristine"
        );

        state.text_draft = Some(draft("   "));
        assert!(
            capture(&state, None, baseline, 0, 24, true)
                .unwrap()
                .is_none(),
            "blank caption"
        );

        state.text_draft = Some(draft("typing"));
        let save = capture(&state, None, baseline, 3, 24, true)
            .unwrap()
            .unwrap();
        assert_eq!(save.marks.len(), 1);
        assert_eq!(save.epoch, 3);
        assert!(save.image.is_some());
        assert!(state.doc.marks.is_empty(), "the live document is untouched");
        let saved = Some(save.key);
        assert!(
            capture(&state, saved, baseline, 0, 24, false)
                .unwrap()
                .is_none(),
            "unchanged"
        );

        state.text_draft.as_mut().unwrap().value.push('!');
        let again = capture(&state, saved, baseline, 0, 24, false)
            .unwrap()
            .unwrap();
        assert!(again.image.is_none());
        assert_ne!(again.key, save.key, "typing changes the signature");

        state.text_draft = None;
        state.doc.add(text_mark(1));
        let committed = capture(&state, saved, baseline, 0, 24, false)
            .unwrap()
            .unwrap();
        assert_eq!(committed.key.1, None);
        assert_ne!(committed.key.0, baseline);
    }

    /// Settings on disk with autosave on, and the guard that reads them.
    fn guard_in(root: &Path, dir: &Path) -> Guard {
        let settings = root.join("cfg").join("recovery.json");
        let on = Settings {
            mode: Mode::On,
            ..Settings::default()
        };
        save_settings_at(&settings, &on).unwrap();
        Guard::new(dir, settings)
    }

    fn save_job(epoch: u64, generation: u64, image: Option<RgbaImage>) -> Save {
        Save {
            epoch,
            generation,
            key: (epoch, None),
            image,
            marks: vec![text_mark(1)],
            crop: None,
            presentation: Presentation::default(),
            retention_hours: 24,
        }
    }

    fn plain_snapshot() -> Snapshot {
        Snapshot {
            image: RgbaImage::new(8, 8),
            marks: vec![],
            crop: None,
            presentation: Presentation::default(),
        }
    }

    #[test]
    fn worker_saves_restorable_projects_and_honours_the_epoch() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("rec");
        let guard = guard_in(root.path(), &dir);
        let epoch = Arc::new(AtomicU64::new(0));
        let (tx, rx) = async_channel::unbounded();
        let session = (std::process::id(), 99);
        let worker = spawn_worker(dir.clone(), session, guard, epoch.clone(), tx);
        let job = |epoch: u64, image: Option<RgbaImage>| save_job(epoch, 0, image);

        worker
            .tx
            .send(job(0, Some(RgbaImage::new(16, 12))))
            .unwrap();
        let done = rx.recv_blocking().unwrap();
        assert_eq!(done.result, Ok(Outcome::Saved));
        let files = list_dir(&dir).unwrap();
        assert_eq!(files.len(), 1);
        let snapshot = Snapshot::load(&files[0].path).unwrap();
        assert_eq!(snapshot.marks.len(), 1);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&files[0].path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // The image is cached: later saves need not resend it.
        worker.tx.send(job(0, None)).unwrap();
        assert_eq!(rx.recv_blocking().unwrap().result, Ok(Outcome::Saved));
        assert_eq!(list_dir(&dir).unwrap().len(), 2);
        worker.tx.send(job(0, None)).unwrap();
        rx.recv_blocking().unwrap();
        assert_eq!(
            list_dir(&dir).unwrap().len(),
            KEEP_OWN,
            "older generations rotate out"
        );

        // Disabled or deleted in this window meanwhile: a stale job leaves nothing behind.
        let before = list_dir(&dir).unwrap();
        epoch.fetch_add(1, Ordering::SeqCst);
        worker.tx.send(job(0, None)).unwrap();
        assert_eq!(rx.recv_blocking().unwrap().result, Ok(Outcome::Skipped));
        assert_eq!(list_dir(&dir).unwrap(), before);

        // Window closed: the worker removes its autosaves.
        worker.discard.store(true, Ordering::SeqCst);
        drop(worker.tx);
        worker.handle.join().unwrap();
        assert!(list_dir(&dir).unwrap().is_empty());
    }

    #[test]
    fn write_one_removes_its_file_when_cancelled_midway() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("rec");
        let guard = guard_in(root.path(), &dir);
        let epoch = AtomicU64::new(0);
        let snapshot = plain_snapshot();
        let ctx = |job_epoch: u64, generation: u64| Ctx {
            dir: &dir,
            session: (1, 1),
            guard: &guard,
            epoch: &epoch,
            job_epoch,
            generation,
            retention_hours: 24,
        };
        let mut last = 0;
        assert_eq!(
            write_one(&ctx(0, 0), &mut last, &snapshot).unwrap(),
            Outcome::Saved
        );
        assert_eq!(list_dir(&dir).unwrap().len(), 1);
        assert_eq!(
            write_one(&ctx(5, 0), &mut last, &snapshot).unwrap(),
            Outcome::Skipped
        );
        assert_eq!(
            list_dir(&dir).unwrap().len(),
            1,
            "stale epoch writes nothing"
        );

        // Another window's Delete all lands after the file exists, before the final check.
        let gone = write_hooked(&ctx(0, 0), &mut last, &snapshot, || guard.bump().unwrap());
        assert_eq!(gone.unwrap(), Outcome::Skipped);
        assert_eq!(
            list_dir(&dir).unwrap().len(),
            1,
            "the new file removed itself"
        );
        assert_eq!(guard.generation(), 1);

        // Another window switches autosave off in the same window of time.
        let g = guard.generation();
        let off = Settings {
            mode: Mode::Off,
            ..Settings::default()
        };
        let gone = write_hooked(&ctx(0, g), &mut last, &snapshot, || {
            save_settings_at(&guard.settings, &off).unwrap();
        });
        assert_eq!(gone.unwrap(), Outcome::Skipped);
        assert_eq!(
            list_dir(&dir).unwrap().len(),
            1,
            "no file survives a switch-off"
        );
        assert_eq!(
            write_one(&ctx(0, g), &mut last, &snapshot).unwrap(),
            Outcome::Skipped,
            "and nothing is written while it stays off"
        );
        assert_eq!(list_dir(&dir).unwrap().len(), 1);

        // Unreadable settings are treated as off, never as consent.
        fs::write(&guard.settings, b"garbage").unwrap();
        assert!(!guard.permits(g));
    }

    #[test]
    fn delete_all_in_one_window_cancels_the_other_windows_workers() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("rec");
        let guard = guard_in(root.path(), &dir);
        let (tx, rx) = async_channel::unbounded();
        // Two windows: each has its own epoch counter, as `attach` creates them.
        let (epoch_a, epoch_b) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let a = spawn_worker(
            dir.clone(),
            (std::process::id(), 1),
            guard.clone(),
            epoch_a,
            tx.clone(),
        );
        let b = spawn_worker(
            dir.clone(),
            (std::process::id(), 2),
            guard.clone(),
            epoch_b,
            tx,
        );
        let old = guard.generation();

        a.tx.send(save_job(0, old, Some(RgbaImage::new(8, 8))))
            .unwrap();
        b.tx.send(save_job(0, old, Some(RgbaImage::new(8, 8))))
            .unwrap();
        rx.recv_blocking().unwrap();
        rx.recv_blocking().unwrap();
        assert_eq!(list_dir(&dir).unwrap().len(), 2);

        // Window A's "Delete all": bump first, then list and remove.
        guard.bump().unwrap();
        let paths: Vec<PathBuf> = list_dir(&dir)
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(remove_all(&paths).unwrap(), 2);

        // Window B's job was captured before the delete and is still queued: it must not
        // recreate anything, although B's own epoch never moved.
        b.tx.send(save_job(0, old, None)).unwrap();
        assert_eq!(rx.recv_blocking().unwrap().result, Ok(Outcome::Skipped));
        assert!(list_dir(&dir).unwrap().is_empty());

        // Saves captured after the delete are fine again.
        b.tx.send(save_job(0, guard.generation(), None)).unwrap();
        assert_eq!(rx.recv_blocking().unwrap().result, Ok(Outcome::Saved));
        assert_eq!(list_dir(&dir).unwrap().len(), 1);

        for w in [a, b] {
            w.discard.store(true, Ordering::SeqCst);
            drop(w.tx);
            w.handle.join().unwrap();
        }
        assert!(list_dir(&dir).unwrap().is_empty());
    }

    #[test]
    fn delete_all_racing_in_flight_saves_from_two_windows_leaves_nothing() {
        for round in 0..8 {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("rec");
            let guard = guard_in(root.path(), &dir);
            let old = guard.generation();
            let (tx, rx) = async_channel::unbounded();
            let workers: Vec<Worker> = (1..=2)
                .map(|n| {
                    spawn_worker(
                        dir.clone(),
                        (std::process::id(), n),
                        guard.clone(),
                        Arc::new(AtomicU64::new(0)),
                        tx.clone(),
                    )
                })
                .collect();
            drop(tx);
            let per_worker = 6;
            for w in &workers {
                w.tx.send(save_job(0, old, Some(RgbaImage::new(64, 64))))
                    .unwrap();
                for _ in 1..per_worker {
                    w.tx.send(save_job(0, old, None)).unwrap();
                }
            }
            // Delete all from a third context while the workers are mid-save; staggered per round.
            thread::sleep(Duration::from_millis(round));
            guard.bump().unwrap();
            let paths: Vec<PathBuf> = list_dir(&dir)
                .unwrap()
                .into_iter()
                .map(|e| e.path)
                .collect();
            remove_all(&paths).unwrap();

            // Every job was captured before the bump, so once they have all finished no
            // autosave of theirs may exist, whichever way the writes interleaved.
            for _ in 0..2 * per_worker {
                let done = rx.recv_blocking().unwrap();
                assert!(done.result.is_ok(), "{:?}", done.result);
            }
            assert!(list_dir(&dir).unwrap().is_empty(), "round {round}");
            for w in workers {
                drop(w.tx);
                w.handle.join().unwrap();
            }
        }
    }

    #[test]
    fn remove_all_reports_failures_but_tries_everything() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let missing = root.path().join("missing");
        let dir = root.path().join("dir");
        fs::write(&a, b"x").unwrap();
        fs::create_dir(&dir).unwrap();
        let err = remove_all(&[dir.clone(), missing.clone(), a.clone()]).unwrap_err();
        assert!(err.to_string().contains("Cannot delete"));
        assert!(!a.exists(), "later files are still deleted");
        assert_eq!(remove_all(&[missing]).unwrap(), 0);
    }
}
