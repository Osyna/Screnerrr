//! Short region recording: a portal screen cast (PipeWire) is read through GStreamer, the user
//! picks a region on the first real frame, records 5 to 30 seconds, previews the clip, trims it
//! and exports a new GIF or WebM file.
//!
//! Frames never queue up. The PipeWire callback keeps only the newest *cropped* frame; a
//! recorder thread samples it at a constant `FPS` and pipes it to ffmpeg (MJPEG in Matroska, a
//! private temporary clip). That also keeps timing exact when the compositor only sends frames
//! on damage. The clip is previewed with a second GStreamer pipeline and exported by ffmpeg with
//! a frame-exact trim, so what the preview shows is what the export contains.
//!
//! By default the control window is hidden while recording, so it cannot appear in the clip; the
//! time limit (5 to 30 s) then ends the recording, or the desktop's screen-sharing indicator
//! (which ends the stream and keeps what was recorded). The user may keep the window visible for
//! its Stop button, but a Wayland client cannot place itself, so then it can be recorded.
//!
//! shortcut: a killed process leaves its temporary clip (`sharpshot-recording-*` under the
//! temp dir) behind; upgrade: a startup sweep of stale directories if that ever matters.

use crate::{chrome, document::Crop, portal, precision};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_app::prelude::*;
use gstreamer_video::{self as gst_video, VideoFrameExt};
use gtk::{gdk, glib, prelude::*};
use image::RgbaImage;
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Constant frame rate of the clip, of the preview and of both exports.
const FPS: u32 = 15;
const MIN_LIMIT: u32 = 5;
const MAX_LIMIT: u32 = 30;
const DEFAULT_LIMIT: u32 = 10;
/// At most 8 Mi pixels (32 MiB per raw frame), approximately one 4K region.
const MIN_SIDE: u32 = 16;
const MAX_PIXELS: u64 = 1 << 23;
/// A trimmed clip keeps at least this many frames (a third of a second).
const MIN_KEPT: u32 = 5;
/// Hard cap on the temporary clip, enforced by ffmpeg itself.
const CLIP_BYTES: u64 = 2 << 30;
const CLIP: &str = "clip.mkv";
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(15);
const FINISH_WAIT: Duration = Duration::from_secs(20);
/// Time for our own window and the portal dialog to leave the screen before the first frame.
const HIDE_DELAY: Duration = Duration::from_millis(500);
const PREVIEW_W: u32 = 960;
const PREVIEW_H: u32 = 540;

// Palette of the other utility windows (cairo cannot read the CSS tokens).
const TRACK: (f64, f64, f64, f64) = (0.376, 0.439, 0.47, 0.5);
const ACCENT: (f64, f64, f64) = (0.388, 0.867, 0.835);

// Pure helpers: time, trim, crop and pixel handling.

fn secs(frames: u32) -> f64 {
    f64::from(frames) / f64::from(FPS)
}

fn max_frames(limit: u32) -> u32 {
    limit.clamp(MIN_LIMIT, MAX_LIMIT) * FPS
}

/// Frames that must have been written `elapsed` after the first one, capped at `max`. When the
/// encoder lags this exceeds what was written and the newest picture is repeated, so the clip's
/// duration stays the wall-clock duration.
fn frames_due(elapsed: Duration, max: u32) -> u32 {
    (elapsed.as_millis() as u64 * u64::from(FPS) / 1000 + 1).min(u64::from(max)) as u32
}

/// Seek position showing `frame`. Rounded up: Matroska stores millisecond stamps that may be one
/// below the exact frame time, and an accurate seek lands on the last frame at or before it.
fn seek_ms(frame: u32) -> u64 {
    (u64::from(frame) * 1000).div_ceil(u64::from(FPS))
}

fn frame_at(pts_ms: u64) -> u32 {
    ((pts_ms * u64::from(FPS) + 500) / 1000) as u32
}

/// The kept range of a clip of `frames` frames: `start..end`, `end` exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Trim {
    frames: u32,
    start: u32,
    end: u32,
}

impl Trim {
    fn new(frames: u32) -> Option<Self> {
        (frames >= MIN_KEPT).then_some(Self {
            frames,
            start: 0,
            end: frames,
        })
    }

    fn set_start(&mut self, frame: u32) {
        self.start = frame.min(self.end - MIN_KEPT);
    }

    fn set_end(&mut self, frame: u32) {
        self.end = frame.clamp(self.start + MIN_KEPT, self.frames);
    }

    fn kept(&self) -> u32 {
        self.end - self.start
    }

    /// Frame-exact selection; the clip is constant frame rate, so frame numbers are time.
    fn filter(&self) -> String {
        format!(
            "trim=start_frame={}:end_frame={},setpts=PTS-STARTPTS",
            self.start, self.end
        )
    }
}

fn check_crop(crop: Crop, frame: (u32, u32)) -> Result<()> {
    ensure!(
        crop.width >= MIN_SIDE && crop.height >= MIN_SIDE,
        "Select a region of at least {MIN_SIDE} × {MIN_SIDE} pixels"
    );
    ensure!(
        crop.x.checked_add(crop.width).is_some_and(|r| r <= frame.0)
            && crop
                .y
                .checked_add(crop.height)
                .is_some_and(|b| b <= frame.1),
        "The selected region is outside the captured frame"
    );
    ensure!(
        u64::from(crop.width) * u64::from(crop.height) <= MAX_PIXELS,
        "The region of {} × {} px is too large: select at most {} megapixels (about 3840 × 2160)",
        crop.width,
        crop.height,
        MAX_PIXELS >> 20
    );
    Ok(())
}

/// Appends the `crop` rectangle of a BGRA frame with row pitch `stride` to `out`, tightly packed.
fn copy_crop(
    src: &[u8],
    stride: usize,
    frame: (u32, u32),
    crop: Crop,
    out: &mut Vec<u8>,
) -> Result<()> {
    let (fw, fh) = (frame.0 as usize, frame.1 as usize);
    ensure!(fw > 0 && fh > 0, "empty frame");
    ensure!(
        stride >= fw * 4 && src.len() >= stride * (fh - 1) + fw * 4,
        "the frame buffer is smaller than its size says"
    );
    check_crop(crop, frame)?;
    let row = crop.width as usize * 4;
    out.clear();
    out.reserve(row * crop.height as usize);
    for y in 0..crop.height as usize {
        let start = (crop.y as usize + y) * stride + crop.x as usize * 4;
        out.extend_from_slice(&src[start..start + row]);
    }
    Ok(())
}

fn rgba_from_bgra(src: &[u8], stride: usize, (w, h): (u32, u32)) -> Result<RgbaImage> {
    let (width, height) = (w as usize, h as usize);
    ensure!(
        width > 0
            && height > 0
            && stride >= width * 4
            && src.len() >= stride * (height - 1) + width * 4,
        "the frame buffer is smaller than its size says"
    );
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in src.chunks(stride).take(height) {
        for p in row[..width * 4].chunks_exact(4) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
    }
    RgbaImage::from_raw(w, h, rgba).context("the frame buffer has the wrong size")
}

/// Preview size: the clip scaled down (never up) to fit the preview box.
fn fit((w, h): (u32, u32)) -> (u32, u32) {
    let scale = (f64::from(PREVIEW_W) / f64::from(w))
        .min(f64::from(PREVIEW_H) / f64::from(h))
        .min(1.0);
    let side = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
    (side(w), side(h))
}

// The newest frame, shared between the PipeWire streaming thread and everyone else.

#[derive(Default)]
enum Mode {
    /// Frames are ignored.
    #[default]
    Idle,
    /// Deliver the next whole frame, once.
    First(async_channel::Sender<Result<RgbaImage, String>>),
    /// Keep the newest cropped frame.
    Record { crop: Crop, frame: (u32, u32) },
}

#[derive(Default)]
struct TapState {
    mode: Mode,
    latest: Vec<u8>,
    have: bool,
    error: Option<String>,
}

#[derive(Default)]
struct Tap {
    state: Mutex<TapState>,
    fresh: Condvar,
}

impl Tap {
    fn lock(&self) -> MutexGuard<'_, TapState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn arm(&self, crop: Crop, frame: (u32, u32)) {
        let mut st = self.lock();
        st.mode = Mode::Record { crop, frame };
        st.have = false;
        st.error = None;
        st.latest.clear();
    }

    fn disarm(&self) {
        self.lock().mode = Mode::Idle;
    }

    /// Streaming-thread callback.
    fn on_sample(&self, sink: &gst_app::AppSink) -> Result<gst::FlowSuccess, gst::FlowError> {
        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
        let mut guard = self.lock();
        let st = &mut *guard;
        if matches!(st.mode, Mode::Idle) {
            return Ok(gst::FlowSuccess::Ok);
        }
        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
        let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
        let info =
            gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::NotNegotiated)?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .map_err(|_| gst::FlowError::Error)?;
        let dims = (frame.width(), frame.height());
        let stride = frame.plane_stride()[0] as usize;
        let data = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
        match std::mem::take(&mut st.mode) {
            Mode::Idle => {}
            Mode::First(tx) => {
                let _ =
                    tx.try_send(rgba_from_bgra(data, stride, dims).map_err(|e| format!("{e:#}")));
            }
            Mode::Record { crop, frame: size } => {
                st.mode = Mode::Record { crop, frame: size };
                let result = if dims == size {
                    copy_crop(data, stride, dims, crop, &mut st.latest)
                } else {
                    Err(anyhow!(
                        "The shared source changed size from {} × {} to {} × {} px while recording; record again",
                        size.0,
                        size.1,
                        dims.0,
                        dims.1
                    ))
                };
                match result {
                    Ok(()) => st.have = true,
                    Err(error) => {
                        st.error = Some(format!("{error:#}"));
                        st.mode = Mode::Idle;
                    }
                }
                self.fresh.notify_all();
            }
        }
        Ok(gst::FlowSuccess::Ok)
    }

    /// Blocks until the first cropped frame is there. Stop, cancel and the deadline end the wait.
    fn wait_first(
        &self,
        wait: Duration,
        stop: &mpsc::Receiver<()>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let deadline = Instant::now() + wait;
        let mut st = self.lock();
        loop {
            if let Some(error) = st.error.take() {
                bail!("{error}");
            }
            if st.have {
                return Ok(());
            }
            ensure!(!cancel.load(Ordering::Relaxed), "recording cancelled");
            if matches!(stop.try_recv(), Ok(()) | Err(TryRecvError::Disconnected)) {
                bail!("Recording stopped before the screen delivered a frame");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !left.is_zero(),
                "The screen stream delivered no frame within {} s. Check that the desktop's portal backend can capture this source",
                wait.as_secs()
            );
            st = self
                .fresh
                .wait_timeout(st, left.min(Duration::from_millis(100)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn snapshot(&self, buf: &mut Vec<u8>) -> Result<()> {
        let st = self.lock();
        if let Some(error) = &st.error {
            bail!("{error}");
        }
        buf.clone_from(&st.latest);
        Ok(())
    }
}

async fn first_frame(tap: &Tap, alive: impl Fn() -> bool) -> Result<RgbaImage> {
    let (tx, rx) = async_channel::bounded(1);
    tap.lock().mode = Mode::First(tx);
    let deadline = Instant::now() + FIRST_FRAME_WAIT;
    let result = loop {
        match rx.try_recv() {
            Ok(frame) => break frame.map_err(|e| anyhow!(e)),
            Err(async_channel::TryRecvError::Closed) => {
                bail!("The screen stream closed before sending a frame")
            }
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if !alive() {
            bail!("cancelled");
        }
        if Instant::now() > deadline {
            break Err(anyhow!(
                "The screen stream delivered no frame within {} s. Check that the desktop's portal backend can capture this source",
                FIRST_FRAME_WAIT.as_secs()
            ));
        }
        glib::timeout_future(Duration::from_millis(30)).await;
    };
    tap.disarm();
    result
}

/// One frame is written per tick of the clip; see `frames_due`. Returns the frames written.
fn record_frames(
    tap: &Tap,
    out: &mut impl Write,
    max: u32,
    written: &AtomicU32,
    stop: &mpsc::Receiver<()>,
    cancel: &AtomicBool,
    first_wait: Duration,
) -> Result<u32> {
    tap.wait_first(first_wait, stop, cancel)?;
    let start = Instant::now();
    let mut frame = Vec::new();
    let mut count = 0u32;
    loop {
        ensure!(!cancel.load(Ordering::Relaxed), "recording cancelled");
        let due = frames_due(start.elapsed(), max);
        if count < due {
            tap.snapshot(&mut frame)?;
            while count < due {
                out.write_all(&frame)
                    .context("Cannot hand a frame to ffmpeg")?;
                count += 1;
                written.store(count, Ordering::Relaxed);
            }
        }
        if count >= max {
            return Ok(count);
        }
        let next = Duration::from_micros(u64::from(count) * 1_000_000 / u64::from(FPS));
        match stop.recv_timeout(next.saturating_sub(start.elapsed())) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return Ok(count),
        }
    }
}

// ffmpeg.

macro_rules! argv {
    ($($arg:expr),* $(,)?) => { vec![$(OsString::from($arg)),*] };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Gif,
    WebM,
}

impl Format {
    fn ext(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::WebM => "webm",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Gif => "GIF",
            Self::WebM => "WebM",
        }
    }
}

fn intake_args(size: (u32, u32), clip: &Path) -> Vec<OsString> {
    argv![
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pixel_format",
        "bgra",
        "-video_size",
        format!("{}x{}", size.0, size.1),
        "-framerate",
        FPS.to_string(),
        "-i",
        "pipe:0",
        // 4:4:4 keeps coloured text sharp; the clip is only an intermediate.
        "-an",
        "-c:v",
        "mjpeg",
        "-q:v",
        "4",
        "-pix_fmt",
        "yuvj444p",
        "-fs",
        CLIP_BYTES.to_string(),
        "-f",
        "matroska",
        "-y",
        clip,
    ]
}

fn export_args(clip: &Path, out: &Path, format: Format, trim: Trim) -> Vec<OsString> {
    let mut args = argv![
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-i",
        clip,
        "-an",
        "-r",
        FPS.to_string(),
        "-fps_mode",
        "cfr",
    ];
    match format {
        Format::Gif => args.extend(argv![
            "-vf",
            format!(
                "{},split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle",
                trim.filter()
            ),
            "-loop", "0", "-final_delay", (100.0 / f64::from(FPS)).round().to_string(), "-f", "gif",
        ]),
        Format::WebM => args.extend(argv![
            "-vf",
            format!("{},format=yuv444p", trim.filter()),
            "-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "32", "-row-mt", "1",
            "-deadline", "good", "-cpu-used", "4", "-f", "webm",
        ]),
    }
    args.push(out.into());
    args
}

/// The only way to end an ffmpeg from another thread. Shares the child with its `Ffmpeg`, so
/// it can only ever signal that very process: the child is reaped under the same lock, and a
/// reaped child is never signalled (std keeps its exit status).
#[derive(Clone)]
struct Killer(Arc<Mutex<Child>>);

impl Killer {
    fn kill(&self) {
        let _ = lock(&self.0).kill();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running ffmpeg, killed and reaped if it is dropped before it finished.
struct Ffmpeg {
    child: Arc<Mutex<Child>>,
    /// ffmpeg writes nothing here; the pipe closes only when the process ends, which lets
    /// `wait_exit` block in `poll(2)` with a deadline instead of sleeping in a loop. The lock
    /// is therefore never held while waiting, and a `Killer` can always get it.
    stdout: Option<ChildStdout>,
    log: PathBuf,
}

impl Ffmpeg {
    fn spawn(args: &[OsString], log: &Path, stdin: Stdio) -> Result<Self> {
        let mut command = Command::new("ffmpeg");
        command.args(args);
        Self::from_command(command, log, stdin)
    }

    /// stderr goes to a private file: ffmpeg can never block on a full pipe and the failure
    /// reason stays readable.
    fn from_command(mut command: Command, log: &Path, stdin: Stdio) -> Result<Self> {
        let log_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(log)
            .context("Cannot create the ffmpeg log")?;
        let mut child = command
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(log_file)
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    anyhow!("ffmpeg was not found. Install ffmpeg (with libvpx for WebM) and make sure it is in PATH")
                } else {
                    anyhow!(error).context("Cannot start ffmpeg")
                }
            })?;
        Ok(Self {
            stdout: child.stdout.take(),
            child: Arc::new(Mutex::new(child)),
            log: log.to_owned(),
        })
    }

    fn killer(&self) -> Killer {
        Killer(self.child.clone())
    }

    fn take_stdin(&self) -> Option<ChildStdin> {
        lock(&self.child).stdin.take()
    }

    /// Kills and reaps.
    fn kill(&self) {
        let mut child = lock(&self.child);
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The first lines ffmpeg wrote to stderr.
    fn log_tail(&self) -> String {
        let mut text = String::new();
        if let Ok(file) = std::fs::File::open(&self.log) {
            let _ = file.take(64 << 10).read_to_string(&mut text);
        }
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(3)
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// True once ffmpeg has closed its stdout, i.e. exited; false when `timeout` ran out first.
    fn wait_exit(&mut self, timeout: Option<Duration>) -> Result<bool> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let Some(out) = self.stdout.as_mut() else {
            return Ok(true);
        };
        let mut sink = [0u8; 256];
        loop {
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            let timespec = left.map(|l| Timespec {
                tv_sec: l.as_secs() as _,
                tv_nsec: l.subsec_nanos() as _,
            });
            let ready = poll(&mut [PollFd::new(&*out, PollFlags::IN)], timespec.as_ref());
            match ready {
                Ok(0) => return Ok(false),
                Ok(_) => match out.read(&mut sink) {
                    Ok(0) => return Ok(true),
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(anyhow!(e).context("Cannot watch ffmpeg")),
                },
                Err(Errno::INTR) => {}
                Err(e) => return Err(anyhow!(e).context("Cannot watch ffmpeg")),
            }
        }
    }

    /// Waits for ffmpeg to end, at most `timeout`, then kills and reaps it. An exit forced by
    /// `cancel` (see `Killer`) reports as cancelled.
    fn finish(mut self, cancel: &AtomicBool, timeout: Option<Duration>) -> Result<()> {
        if !self.wait_exit(timeout)? {
            self.kill();
            bail!("ffmpeg did not finish in time");
        }
        let status = lock(&self.child).wait().context("Cannot wait for ffmpeg")?;
        ensure!(!cancel.load(Ordering::Relaxed), "cancelled");
        ensure!(
            status.success(),
            "ffmpeg failed ({status}): {}",
            self.log_tail()
        );
        Ok(())
    }
}

impl Drop for Ffmpeg {
    fn drop(&mut self) {
        if !matches!(lock(&self.child).try_wait(), Ok(Some(_))) {
            self.kill();
        }
    }
}

fn check_runtime() -> Result<()> {
    gst::init().context("Cannot initialise GStreamer")?;
    let missing: Vec<String> = [
        ("pipewiresrc", "gst-plugin-pipewire"),
        ("videoconvert", "gst-plugins-base"),
        ("videoscale", "gst-plugins-base"),
        ("appsink", "gst-plugins-base"),
        ("matroskademux", "gst-plugins-good"),
        ("jpegparse", "gst-plugins-bad / gstreamer1.0-plugins-bad"),
        ("avdec_mjpeg", "gst-libav / gstreamer1.0-libav"),
    ]
    .into_iter()
    .filter(|(element, _)| gst::ElementFactory::find(element).is_none())
    .map(|(element, package)| format!("{element} ({package})"))
    .collect();
    ensure!(
        missing.is_empty(),
        "GStreamer plugins are missing: {}. Install the packages in brackets (names as on Arch Linux)",
        missing.join(", ")
    );
    let ffmpeg = Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    ensure!(
        ffmpeg.is_ok_and(|s| s.success()),
        "ffmpeg was not found. Recording needs it to encode GIF and WebM: install ffmpeg (with libvpx for WebM) and make sure it is in PATH"
    );
    Ok(())
}

/// A background thread with a way to stop it (`stop`) or abandon its work (`cancel`).
struct Job {
    cancel: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    written: Arc<AtomicU32>,
    /// The encoder the thread owns. The thread may be blocked writing to it, so abandoning the
    /// job kills the process itself rather than waiting for the thread to notice.
    kill: Arc<Mutex<Option<Killer>>>,
    handle: Option<JoinHandle<()>>,
}

impl Job {
    fn stop(&self) {
        if let Some(tx) = &self.stop {
            let _ = tx.send(());
        }
    }

    /// Abandons the work: the encoder dies, which also unblocks a thread writing to it.
    fn interrupt(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.stop();
        if let Some(killer) = lock(&self.kill).as_ref() {
            killer.kill();
        }
    }
}

/// Dropping a job abandons it, kills its encoder and only then waits for the thread, so no
/// child process outlives it and the join cannot hang on a blocked pipe.
impl Drop for Job {
    fn drop(&mut self) {
        self.interrupt();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Recorder thread body: frames to ffmpeg's stdin, then a clean finish of the clip.
fn record_clip(
    tap: Arc<Tap>,
    ffmpeg: Ffmpeg,
    mut stdin: std::process::ChildStdin,
    max: u32,
    written: &AtomicU32,
    stop: &mpsc::Receiver<()>,
    cancel: &AtomicBool,
) -> Result<u32> {
    let result = record_frames(
        &tap,
        &mut stdin,
        max,
        written,
        stop,
        cancel,
        FIRST_FRAME_WAIT,
    );
    tap.disarm();
    drop(stdin);
    match result {
        Ok(frames) => {
            ffmpeg.finish(cancel, Some(FINISH_WAIT))?;
            Ok(frames)
        }
        Err(error) => {
            ffmpeg.kill();
            let tail = ffmpeg.log_tail();
            Err(if tail.is_empty() {
                error
            } else {
                error.context(format!("ffmpeg: {tail}"))
            })
        }
    }
}

fn spawn_recorder(
    tap: Arc<Tap>,
    size: (u32, u32),
    dir: &Path,
    max: u32,
) -> Result<(Job, async_channel::Receiver<Result<u32>>)> {
    let ffmpeg = Ffmpeg::spawn(
        &intake_args(size, &dir.join(CLIP)),
        &dir.join("intake.log"),
        Stdio::piped(),
    )?;
    let stdin = ffmpeg.take_stdin().context("ffmpeg has no input pipe")?;
    let kill = Arc::new(Mutex::new(Some(ffmpeg.killer())));
    let (stop_tx, stop_rx) = mpsc::channel();
    let (tx, rx) = async_channel::bounded(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let written = Arc::new(AtomicU32::new(0));
    let handle = std::thread::Builder::new()
        .name("recorder".into())
        .spawn({
            let (cancel, written) = (cancel.clone(), written.clone());
            move || {
                let result = record_clip(tap, ffmpeg, stdin, max, &written, &stop_rx, &cancel);
                let _ = tx.send_blocking(result);
            }
        })
        .context("Cannot start the recorder thread")?;
    Ok((
        Job {
            cancel,
            stop: Some(stop_tx),
            written,
            kill,
            handle: Some(handle),
        },
        rx,
    ))
}

/// Encodes the kept part of the clip into a new file next to `dest` and moves it into place
/// only when ffmpeg succeeded. An existing file is never replaced.
fn export(
    clip: &Path,
    format: Format,
    trim: Trim,
    dest: &Path,
    log: &Path,
    cancel: &AtomicBool,
    kill: &Mutex<Option<Killer>>,
) -> Result<()> {
    ensure!(
        !dest.exists(),
        "{} already exists; choose a new filename",
        dest.display()
    );
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = tempfile::Builder::new()
        .prefix(".sharpshot-export-")
        .tempfile_in(parent)
        .context("Cannot create a file in the destination folder")?;
    let ffmpeg = Ffmpeg::spawn(
        &export_args(clip, temp.path(), format, trim),
        log,
        Stdio::null(),
    )?;
    // Publish the killer, then look at `cancel`: an abort that raced the spawn is seen by one
    // of the two sides, and a returned `ffmpeg` is killed and reaped by its drop.
    *lock(kill) = Some(ffmpeg.killer());
    ensure!(!cancel.load(Ordering::Relaxed), "cancelled");
    ffmpeg.finish(cancel, None).with_context(|| match format {
        Format::Gif => "Cannot encode the GIF".to_owned(),
        Format::WebM => "Cannot encode the WebM (ffmpeg needs the libvpx-vp9 encoder)".to_owned(),
    })?;
    std::fs::File::open(temp.path())?.sync_all()?;
    temp.persist_noclobber(dest)
        .map_err(|e| e.error)
        .with_context(|| format!("Cannot save {}", dest.display()))?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn with_extension_of(path: PathBuf, format: Format) -> PathBuf {
    let right = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(format.ext()));
    if right {
        return path;
    }
    let mut name = path.into_os_string();
    name.push(format!(".{}", format.ext()));
    name.into()
}

// GStreamer: the screen stream and the clip preview.

enum BusEvent {
    Ended,
    Failed(String),
}

fn watch_bus(
    pipeline: &gst::Pipeline,
    on_event: impl Fn(BusEvent) + 'static,
) -> Result<gst::bus::BusWatchGuard> {
    let bus = pipeline
        .bus()
        .context("The GStreamer pipeline has no bus")?;
    Ok(bus.add_watch_local(move |_, message| {
        match message.view() {
            gst::MessageView::Error(error) => {
                if let Some(debug) = error.debug() {
                    eprintln!("sharpshot --record: GStreamer: {debug}");
                }
                on_event(BusEvent::Failed(error.error().to_string()));
            }
            gst::MessageView::Eos(_) => on_event(BusEvent::Ended),
            _ => {}
        }
        glib::ControlFlow::Continue
    })?)
}

/// The portal stream through `pipewiresrc`. Dropping it stops the stream and closes the portal
/// session (the cast is dropped last).
struct Capture {
    pipeline: gst::Pipeline,
    tap: Arc<Tap>,
    _watch: gst::bus::BusWatchGuard,
    _cast: portal::ScreenCast,
}

impl Capture {
    fn start(cast: portal::ScreenCast, on_event: impl Fn(BusEvent) + 'static) -> Result<Self> {
        // pipewiresrc duplicates the fd; the cast keeps the original alive with the session.
        let src = gst::ElementFactory::make("pipewiresrc")
            .property("fd", cast.fd.as_raw_fd())
            .property("path", cast.node.to_string())
            .build()
            .context("GStreamer's pipewiresrc is missing: install gst-plugin-pipewire")?;
        // A static screen sends no frames; ask for a repeat of the last one so the first frame
        // and the first recorded frame never wait for damage.
        if src.find_property("keepalive-time").is_some() {
            src.set_property("keepalive-time", 250i32);
        }
        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .context("GStreamer's videoconvert is missing")?;
        let tap = Arc::new(Tap::default());
        let sink = gst::ElementFactory::make("appsink")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "BGRA")
                    .build(),
            )
            .property("max-buffers", 1u32)
            .property("drop", true)
            .property("sync", false)
            .build()
            .context("GStreamer's appsink is missing")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("appsink has an unexpected type"))?;
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let tap = tap.clone();
                    move |sink| tap.on_sample(sink)
                })
                .build(),
        );
        let pipeline = gst::Pipeline::with_name("sharpshot-capture");
        pipeline.add(&src)?;
        pipeline.add(&convert)?;
        pipeline.add(&sink)?;
        src.link(&convert)?;
        convert.link(&sink)?;
        let capture = Self {
            _watch: watch_bus(&pipeline, on_event)?,
            pipeline,
            tap,
            _cast: cast,
        };
        capture
            .pipeline
            .set_state(gst::State::Playing)
            .context("Cannot start the screen stream")?;
        Ok(capture)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// One decoded preview picture, handed from the GStreamer thread to the UI.
struct Shown {
    frame: u32,
    width: u32,
    height: u32,
    data: Vec<u8>,
}

fn shown_from(sample: &gst::Sample) -> Option<Shown> {
    let buffer = sample.buffer()?;
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let (width, height) = (frame.width(), frame.height());
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).ok()?;
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        out.extend_from_slice(data.get(y * stride..y * stride + row)?);
    }
    Some(Shown {
        frame: frame_at(buffer.pts()?.mseconds()),
        width,
        height,
        data: out,
    })
}

/// Decodes the clip in real time into one RGBA picture per frame. Only the newest picture is
/// kept, so a slow UI skips frames instead of queueing them.
struct Player {
    pipeline: gst::Pipeline,
    slot: Arc<Mutex<Option<Shown>>>,
    _watch: gst::bus::BusWatchGuard,
}

impl Player {
    fn new(
        clip: &Path,
        size: (u32, u32),
        wake: async_channel::Sender<()>,
        on_event: impl Fn(BusEvent) + 'static,
    ) -> Result<Self> {
        let pipeline = gst::parse::launch(
            "filesrc name=src ! matroskademux ! jpegparse ! avdec_mjpeg ! videoconvert ! videoscale ! appsink name=sink",
        )
        .context("Cannot build the preview pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("the preview pipeline has an unexpected type"))?;
        pipeline
            .by_name("src")
            .context("The preview pipeline has no source")?
            .set_property(
                "location",
                clip.to_str().context("The clip path is not UTF-8")?,
            );
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .context("The preview pipeline has no sink")?;
        let (w, h) = fit(size);
        sink.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .field("width", w as i32)
                .field("height", h as i32)
                .build(),
        );
        sink.set_property("max-buffers", 2u32);
        sink.set_property("sync", true);
        let slot: Arc<Mutex<Option<Shown>>> = Arc::default();
        let deliver = {
            let slot = slot.clone();
            move |sample: &gst::Sample| {
                if let Some(shown) = shown_from(sample) {
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(shown);
                    let _ = wake.try_send(());
                }
            }
        };
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_preroll({
                    let deliver = deliver.clone();
                    move |sink| {
                        deliver(&sink.pull_preroll().map_err(|_| gst::FlowError::Eos)?);
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .new_sample(move |sink| {
                    deliver(&sink.pull_sample().map_err(|_| gst::FlowError::Eos)?);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let player = Self {
            _watch: watch_bus(&pipeline, on_event)?,
            pipeline,
            slot,
        };
        player
            .pipeline
            .set_state(gst::State::Paused)
            .context("Cannot start the preview pipeline")?;
        player
            .pipeline
            .state(gst::ClockTime::from_seconds(10))
            .0
            .context("The clip cannot be decoded. Install gst-plugins-good (Matroska and JPEG)")?;
        Ok(player)
    }

    /// Frames in the clip according to the decoder, if it is clearly shorter than recorded
    /// (ffmpeg's size cap cut the clip).
    fn playable_frames(&self, recorded: u32) -> u32 {
        match self.pipeline.query_duration::<gst::ClockTime>() {
            Some(d) => {
                let measured = frame_at(d.mseconds());
                if measured + 1 < recorded {
                    measured
                } else {
                    recorded
                }
            }
            None => recorded,
        }
    }

    fn seek(&self, frame: u32) -> Result<()> {
        self.pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::from_mseconds(seek_ms(frame)),
            )
            .context("The preview cannot seek")
    }

    fn set_playing(&self, playing: bool) -> Result<()> {
        self.pipeline
            .set_state(if playing {
                gst::State::Playing
            } else {
                gst::State::Paused
            })
            .map(|_| ())
            .context("The preview cannot change state")
    }

    fn take(&self) -> Option<Shown> {
        self.slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

// Entry point and the control window.

type Failure = Rc<RefCell<Option<anyhow::Error>>>;

/// Runs the recorder as its own GTK application. Failures are shown in the window (this process
/// is usually started from the editor, which has no terminal) and returned after it closes.
pub fn run() -> Result<()> {
    let app = gtk::Application::builder()
        .application_id("io.github.sharpshot.SharpShot.Record")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let failure: Failure = Rc::default();
    let keep: Rc<RefCell<Option<Rc<Ui>>>> = Rc::default();
    app.connect_activate({
        let (failure, keep) = (failure.clone(), keep.clone());
        move |app| {
            if keep.borrow().is_some() {
                return;
            }
            chrome::install();
            let ui = Ui::new(app, failure.clone());
            ui.begin();
            *keep.borrow_mut() = Some(ui);
        }
    });
    let code = app.run_with_args::<&str>(&[]);
    // Dropping the window state ends the job, the stream, the portal session and the clip.
    keep.borrow_mut().take();
    if let Some(error) = failure.take() {
        return Err(error);
    }
    ensure!(code == glib::ExitCode::SUCCESS, "GTK exited with {code:?}");
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Setup,
    /// Portal dialog, first frame and region selection.
    Acquiring,
    Ready,
    /// Controls are leaving the screen; recording begins right after.
    Starting,
    Recording,
    Finishing,
    Review,
    Failed,
    Closed,
}

fn named<W: IsA<gtk::Widget> + IsA<gtk::Accessible>>(widget: &W, name: &str, tip: &str) {
    widget.set_tooltip_text(Some(tip));
    widget.update_property(&[gtk::accessible::Property::Label(name)]);
}

fn button(label: &str, tip: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    named(&button, label, tip);
    button
}

fn caption(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_max_width_chars(46);
    label.add_css_class(class);
    label
}

fn page(widgets: &[&gtk::Widget]) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_margin_start(14);
    page.set_margin_end(14);
    page.set_margin_top(14);
    page.set_margin_bottom(14);
    for widget in widgets {
        page.append(*widget);
    }
    page
}

fn buttons(items: &[&gtk::Button]) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_halign(gtk::Align::End);
    for item in items {
        row.append(*item);
    }
    row
}

fn on(button: &gtk::Button, ui: &Rc<Ui>, f: fn(&Rc<Ui>)) {
    let weak = Rc::downgrade(ui);
    button.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            f(&ui);
        }
    });
}

struct Ui {
    app: gtk::Application,
    _hold: gtk::gio::ApplicationHoldGuard,
    ctl: gtk::ApplicationWindow,
    pages: gtk::Stack,
    pointer: gtk::CheckButton,
    limit: gtk::SpinButton,
    choose: gtk::Button,
    close_setup: gtk::Button,
    region_label: gtk::Label,
    start: gtk::Button,
    cancel_ready: gtk::Button,
    hide: gtk::CheckButton,
    clock: gtk::Label,
    meter: gtk::DrawingArea,
    fraction: Rc<Cell<f64>>,
    stop: gtk::Button,
    error_label: gtk::Label,
    close_error: gtk::Button,
    failure: Failure,
    stage: Cell<Stage>,
    limit_secs: Cell<u32>,
    region: Cell<Option<(Crop, (u32, u32))>>,
    capture: RefCell<Option<Capture>>,
    job: RefCell<Option<Job>>,
    /// The clip's private directory, from the start of a recording until the review owns it.
    dir: RefCell<Option<tempfile::TempDir>>,
    /// A failure reported by the stream while recording; decides the outcome once the job ends.
    stream_error: RefCell<Option<String>>,
    review: RefCell<Option<Rc<Review>>>,
}

impl Ui {
    fn new(app: &gtk::Application, failure: Failure) -> Rc<Self> {
        let ctl = gtk::ApplicationWindow::builder()
            .application(app)
            .title("SharpShot — Record")
            .default_width(560)
            .default_height(360)
            // Fixed-size utility windows float instead of retiling on wlroots compositors.
            .resizable(false)
            .build();
        let pointer = gtk::CheckButton::with_label("Include the mouse pointer");
        let limit = gtk::SpinButton::with_range(f64::from(MIN_LIMIT), f64::from(MAX_LIMIT), 1.0);
        limit.set_numeric(true);
        limit.set_value(f64::from(DEFAULT_LIMIT));
        named(
            &limit,
            "Time limit in seconds",
            "The recording stops by itself after this many seconds",
        );
        let choose = button(
            "Choose screen…",
            "Pick the screen or window to record in the system dialog",
        );
        choose.add_css_class("export-button");
        let close_setup = button("Close", "Close without recording (Escape)");
        let limit_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        limit_row.append(&caption("Time limit (seconds)", "field-label"));
        limit_row.append(&limit);
        let setup = page(&[
            caption("Record a screen region", "title").upcast_ref(),
            caption(
                "Choose a screen or window, then drag the region to record for 5–30 seconds. A private temporary clip is kept until you close the preview or export a GIF or WebM.",
                "status-bar",
            )
            .upcast_ref(),
            pointer.upcast_ref(),
            limit_row.upcast_ref(),
            buttons(&[&close_setup, &choose]).upcast_ref(),
        ]);

        let region_label = caption("", "title");
        let start = button("Start recording", "Begin recording the selected region");
        start.add_css_class("export-button");
        let cancel_ready = button("Cancel", "Close without recording (Escape)");
        let hide = gtk::CheckButton::with_label("Hide these controls while recording");
        hide.set_active(true);
        hide.set_tooltip_text(Some(
            "Recommended. The window is unmapped, so the compositor does not draw it and it cannot be in the stream",
        ));
        let ready = page(&[
            region_label.upcast_ref(),
            caption(
                "With the controls hidden, nothing of this window can appear in the clip and the recording ends by itself at the time limit. To stop sooner, use your desktop's screen-sharing indicator (GNOME top bar, KDE system tray); everything recorded so far is kept. To discard it, close the preview afterwards.",
                "status-bar",
            )
            .upcast_ref(),
            hide.upcast_ref(),
            caption(
                "With the controls shown, a Stop button stays on screen, but this window is recorded if it overlaps the region: drag it clear of the region first.",
                "status-bar",
            )
            .upcast_ref(),
            buttons(&[&cancel_ready, &start]).upcast_ref(),
        ]);

        let clock = caption("", "title");
        clock.set_selectable(false);
        let fraction = Rc::new(Cell::new(0.0_f64));
        let meter = gtk::DrawingArea::new();
        meter.set_content_height(6);
        meter.set_hexpand(true);
        meter.set_draw_func({
            let fraction = fraction.clone();
            move |_, cr, w, h| {
                let (w, h) = (f64::from(w), f64::from(h));
                cr.set_source_rgba(TRACK.0, TRACK.1, TRACK.2, TRACK.3);
                cr.rectangle(0.0, 0.0, w, h);
                let _ = cr.fill();
                cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2);
                cr.rectangle(0.0, 0.0, w * fraction.get().clamp(0.0, 1.0), h);
                let _ = cr.fill();
            }
        });
        let stop = button(
            "Stop recording",
            "End the recording now and review it (Escape)",
        );
        stop.add_css_class("export-button");
        let recording = page(&[
            clock.upcast_ref(),
            meter.upcast_ref(),
            caption(
                "The recording ends by itself at the time limit.",
                "status-bar",
            )
            .upcast_ref(),
            buttons(&[&stop]).upcast_ref(),
        ]);

        let busy = page(&[caption("Finishing the clip…", "title").upcast_ref()]);

        let error_label = caption("", "status-bar");
        error_label.set_selectable(true);
        let close_error = button("Close", "Close the recorder (Escape)");
        let failed = page(&[
            caption("Recording failed", "title").upcast_ref(),
            error_label.upcast_ref(),
            buttons(&[&close_error]).upcast_ref(),
        ]);

        let pages = gtk::Stack::new();
        pages.set_hhomogeneous(false);
        pages.set_vhomogeneous(false);
        pages.add_named(&setup, Some("setup"));
        pages.add_named(&ready, Some("ready"));
        pages.add_named(&recording, Some("recording"));
        pages.add_named(&busy, Some("busy"));
        pages.add_named(&failed, Some("error"));
        ctl.set_child(Some(&pages));

        let ui = Rc::new(Self {
            app: app.clone(),
            _hold: app.hold(),
            ctl,
            pages,
            pointer,
            limit,
            choose,
            close_setup,
            region_label,
            start,
            cancel_ready,
            hide,
            clock,
            meter,
            fraction,
            stop,
            error_label,
            close_error,
            failure,
            stage: Cell::new(Stage::Setup),
            limit_secs: Cell::new(DEFAULT_LIMIT),
            region: Cell::new(None),
            capture: RefCell::default(),
            job: RefCell::default(),
            dir: RefCell::default(),
            stream_error: RefCell::default(),
            review: RefCell::default(),
        });
        ui.wire();
        ui
    }

    fn wire(self: &Rc<Self>) {
        on(&self.choose, self, Ui::choose_screen);
        on(&self.start, self, Ui::start_recording);
        on(&self.stop, self, Ui::stop_recording);
        for b in [&self.close_setup, &self.cancel_ready, &self.close_error] {
            on(b, self, Ui::cancel);
        }
        let weak = Rc::downgrade(self);
        self.ctl.connect_close_request({
            let weak = weak.clone();
            move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.escape();
                }
                glib::Propagation::Stop
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(ui) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gdk::Key::Escape {
                ui.escape();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.ctl.add_controller(keys);
    }

    /// Escape and the window's close button: a running recording is kept, everything else ends.
    fn escape(self: &Rc<Self>) {
        match self.stage.get() {
            Stage::Recording => self.stop_recording(),
            Stage::Starting | Stage::Finishing => {}
            _ => self.cancel(),
        }
    }

    fn begin(self: &Rc<Self>) {
        match check_runtime() {
            Ok(()) => self.show("setup"),
            Err(error) => self.fail(error),
        }
    }

    fn show(&self, page: &str) {
        self.pages.set_visible_child_name(page);
        let default: Option<&gtk::Button> = match page {
            "setup" => Some(&self.choose),
            "ready" => Some(&self.start),
            "recording" => Some(&self.stop),
            "error" => Some(&self.close_error),
            _ => None,
        };
        self.ctl.set_default_widget(default);
        self.ctl.set_visible(true);
        self.ctl.present();
    }

    fn fail(&self, error: anyhow::Error) {
        if matches!(self.stage.get(), Stage::Failed | Stage::Closed) {
            return;
        }
        eprintln!("sharpshot --record: {error:#}");
        self.stage.set(Stage::Failed);
        self.job.borrow_mut().take();
        self.capture.borrow_mut().take();
        self.dir.borrow_mut().take();
        self.error_label.set_text(&format!("{error:#}"));
        *self.failure.borrow_mut() = Some(error);
        self.review.borrow_mut().take();
        self.show("error");
    }

    fn shutdown(&self) {
        self.stage.set(Stage::Closed);
        self.job.borrow_mut().take();
        self.capture.borrow_mut().take();
        self.review.borrow_mut().take();
        self.dir.borrow_mut().take();
    }

    fn cancel(self: &Rc<Self>) {
        self.shutdown();
        self.ctl.destroy();
        self.app.quit();
    }

    // -- Choosing the source and the region ------------------------------------------------

    fn choose_screen(self: &Rc<Self>) {
        if self.stage.get() != Stage::Setup {
            return;
        }
        self.stage.set(Stage::Acquiring);
        self.limit_secs.set(
            self.limit
                .value_as_int()
                .clamp(MIN_LIMIT as i32, MAX_LIMIT as i32) as u32,
        );
        let cursor = self.pointer.is_active();
        // Out of the picture for the portal dialog, the first frame and the region selection.
        self.ctl.set_visible(false);
        let ui = self.clone();
        glib::spawn_future_local(async move {
            if let Err(error) = ui.acquire(cursor).await {
                ui.fail(error);
            }
        });
    }

    async fn acquire(self: &Rc<Self>, cursor: bool) -> Result<()> {
        glib::timeout_future(HIDE_DELAY).await;
        let Some(cast) = portal::screencast(cursor).await? else {
            self.cancel();
            return Ok(());
        };
        let capture = Capture::start(cast, {
            let weak = Rc::downgrade(self);
            move |event| {
                // Handled outside the bus callback: the handler drops the pipeline.
                if let Some(ui) = weak.upgrade() {
                    glib::idle_add_local_once(move || ui.stream_event(event));
                }
            }
        })?;
        let tap = capture.tap.clone();
        *self.capture.borrow_mut() = Some(capture);
        // The portal dialog has to be gone before the frame that the region is chosen on.
        glib::timeout_future(HIDE_DELAY).await;
        let frame = first_frame(&tap, || self.stage.get() == Stage::Acquiring).await?;
        let size = frame.dimensions();
        let crop = precision::select(&frame, MIN_SIDE, MAX_PIXELS)?;
        drop(frame);
        if self.stage.get() != Stage::Acquiring {
            return Ok(()); // the stream failed or the window was closed meanwhile
        }
        let Some(crop) = crop else {
            self.cancel();
            return Ok(());
        };
        check_crop(crop, size)?;
        self.region.set(Some((crop, size)));
        self.region_label.set_text(&format!(
            "Region {} × {} px · up to {} s",
            crop.width,
            crop.height,
            self.limit_secs.get()
        ));
        self.stage.set(Stage::Ready);
        self.show("ready");
        Ok(())
    }

    fn stream_event(self: &Rc<Self>, event: BusEvent) {
        match (self.stage.get(), event) {
            (Stage::Recording | Stage::Finishing, BusEvent::Ended) => self.stop_recording(),
            (Stage::Recording | Stage::Finishing, BusEvent::Failed(message)) => {
                // PipeWire's flow error can precede the authoritative portal Closed signal.
                let closed = self
                    .capture
                    .borrow()
                    .as_ref()
                    .map(|c| c._cast.closed.clone());
                let weak = Rc::downgrade(self);
                glib::spawn_future_local(async move {
                    glib::timeout_future(Duration::from_millis(250)).await;
                    let Some(ui) = weak.upgrade() else { return };
                    if !matches!(ui.stage.get(), Stage::Recording | Stage::Finishing) {
                        return;
                    }
                    if !closed.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                        *ui.stream_error.borrow_mut() =
                            Some(format!("The screen stream failed: {message}"));
                    }
                    ui.stop_recording();
                });
            }
            (Stage::Acquiring | Stage::Ready | Stage::Starting, BusEvent::Ended) => {
                self.fail(anyhow!(
                    "Screen sharing ended before the recording started. Choose the screen again"
                ))
            }
            (Stage::Acquiring | Stage::Ready | Stage::Starting, BusEvent::Failed(message)) => {
                self.fail(anyhow!("The screen stream failed: {message}"))
            }
            _ => {}
        }
    }

    // -- Recording --------------------------------------------------------------------------

    /// Hiding the controls takes a moment to reach the screen, so recording starts after it.
    fn start_recording(self: &Rc<Self>) {
        if self.stage.get() != Stage::Ready {
            return;
        }
        if self
            .capture
            .borrow()
            .as_ref()
            .is_none_or(|c| c._cast.closed.load(Ordering::Acquire))
        {
            return self.fail(anyhow!(
                "Screen sharing ended. Choose the screen again before recording."
            ));
        }
        self.stage.set(Stage::Starting);
        let hide = self.hide.is_active();
        if hide {
            self.ctl.set_visible(false);
        }
        let ui = self.clone();
        glib::spawn_future_local(async move {
            if hide {
                glib::timeout_future(HIDE_DELAY).await;
            }
            ui.record_now(hide);
        });
    }

    fn record_now(self: &Rc<Self>, hidden: bool) {
        if self.stage.get() != Stage::Starting {
            return; // the stream failed or the window was closed meanwhile
        }
        let region = self.region.get();
        let tap = self.capture.borrow().as_ref().map(|c| c.tap.clone());
        let (Some((crop, frame)), Some(tap)) = (region, tap) else {
            return self.fail(anyhow!(
                "The screen stream is gone. Choose the screen again"
            ));
        };
        let (dir, job, rx) = match begin_recording(&tap, crop, frame, self.limit_secs.get()) {
            Ok(started) => started,
            Err(error) => return self.fail(error),
        };
        *self.dir.borrow_mut() = Some(dir);
        *self.job.borrow_mut() = Some(job);
        self.stage.set(Stage::Recording);
        self.fraction.set(0.0);
        self.tick();
        if hidden {
            self.pages.set_visible_child_name("recording");
        } else {
            self.show("recording");
        }
        glib::timeout_add_local(Duration::from_millis(100), {
            let weak = Rc::downgrade(self);
            move || match weak.upgrade() {
                Some(ui) if matches!(ui.stage.get(), Stage::Recording | Stage::Finishing) => {
                    if ui
                        .capture
                        .borrow()
                        .as_ref()
                        .is_some_and(|c| c._cast.closed.load(Ordering::Acquire))
                    {
                        ui.stop_recording();
                    }
                    ui.tick();
                    glib::ControlFlow::Continue
                }
                _ => glib::ControlFlow::Break,
            }
        });
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let result = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err(anyhow!("The recorder stopped unexpectedly")));
            ui.recorded(result);
        });
    }

    fn tick(&self) {
        if !self.ctl.is_visible() {
            return;
        }
        let limit = self.limit_secs.get();
        let frames = self
            .job
            .borrow()
            .as_ref()
            .map_or(0, |job| job.written.load(Ordering::Relaxed));
        self.fraction
            .set(f64::from(frames) / f64::from(max_frames(limit)));
        self.meter.queue_draw();
        self.clock
            .set_text(&format!("Recording  {:.1} / {limit} s", secs(frames)));
    }

    fn stop_recording(self: &Rc<Self>) {
        if self.stage.get() != Stage::Recording {
            return;
        }
        self.stage.set(Stage::Finishing);
        // Hidden controls stay hidden: reappearing now could still land in the last frames.
        if self.ctl.is_visible() {
            self.show("busy");
        }
        if let Some(job) = self.job.borrow().as_ref() {
            job.stop();
        }
    }

    fn recorded(self: &Rc<Self>, result: Result<u32>) {
        if matches!(self.stage.get(), Stage::Closed | Stage::Failed) {
            return;
        }
        // The screen is no longer needed: close the stream and the portal session now.
        self.job.borrow_mut().take();
        self.capture.borrow_mut().take();
        if let Some(message) = self.stream_error.take() {
            return self.fail(anyhow!(message));
        }
        match result.and_then(|frames| {
            Trim::new(frames).with_context(|| {
                format!(
                    "The recording is shorter than {:.1} s. Record again and stop later",
                    secs(MIN_KEPT)
                )
            })
        }) {
            Ok(trim) => self.open_review(trim.frames),
            Err(error) => self.fail(error.context("Recording failed")),
        }
    }

    fn open_review(self: &Rc<Self>, frames: u32) {
        let region = self.region.get();
        let dir = self.dir.borrow_mut().take();
        let (Some((crop, _)), Some(dir)) = (region, dir) else {
            return self.fail(anyhow!("The recording is gone"));
        };
        match Review::new(&self.app, dir, frames, (crop.width, crop.height)) {
            Ok(review) => {
                self.stage.set(Stage::Review);
                self.ctl.set_visible(false);
                review.window.present();
                *self.review.borrow_mut() = Some(review);
            }
            Err(error) => self.fail(error),
        }
    }
}

fn private_clip_dir() -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("sharpshot-recording-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .context("Cannot create a private folder for the clip")?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.path().join(CLIP))
        .context("Cannot create the private clip")?;
    Ok(dir)
}

/// Private clip folder plus the recorder thread for `crop`. The tap is armed before the thread
/// exists, so the thread's first wait sees the very next frame.
fn begin_recording(
    tap: &Arc<Tap>,
    crop: Crop,
    frame: (u32, u32),
    limit: u32,
) -> Result<(tempfile::TempDir, Job, async_channel::Receiver<Result<u32>>)> {
    let dir = private_clip_dir()?;
    tap.arm(crop, frame);
    match spawn_recorder(
        tap.clone(),
        (crop.width, crop.height),
        dir.path(),
        max_frames(limit),
    ) {
        Ok((job, rx)) => Ok((dir, job, rx)),
        Err(error) => {
            tap.disarm();
            Err(error)
        }
    }
}

// Review window: preview, trim, export.

struct Review {
    app: gtk::Application,
    window: gtk::ApplicationWindow,
    /// Keeps the private clip folder alive; removed when the review is dropped.
    dir: tempfile::TempDir,
    clip: PathBuf,
    size: (u32, u32),
    player: Player,
    picture: gtk::Picture,
    trim: Cell<Trim>,
    position: Cell<u32>,
    playing: Cell<bool>,
    syncing: Cell<bool>,
    start: gtk::Scale,
    end: gtk::Scale,
    start_label: gtk::Label,
    end_label: gtk::Label,
    info: gtk::Label,
    status: gtk::Label,
    play: gtk::Button,
    gif: gtk::Button,
    webm: gtk::Button,
    close: gtk::Button,
    export: RefCell<Option<Job>>,
}

impl Review {
    fn new(
        app: &gtk::Application,
        dir: tempfile::TempDir,
        frames: u32,
        size: (u32, u32),
    ) -> Result<Rc<Self>> {
        let clip = dir.path().join(CLIP);
        let (wake_tx, wake_rx) = async_channel::bounded::<()>(1);
        let (event_tx, event_rx) = async_channel::unbounded::<BusEvent>();
        let player = Player::new(&clip, size, wake_tx, move |event| {
            let _ = event_tx.try_send(event);
        })?;
        let trim =
            Trim::new(player.playable_frames(frames)).context("The clip is too short to review")?;

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("SharpShot — Recording")
            .resizable(false)
            .build();
        let (pw, ph) = fit(size);
        let picture = gtk::Picture::new();
        picture.set_size_request(pw as i32, ph as i32);
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.update_property(&[gtk::accessible::Property::Label("Recording preview")]);
        let stage = gtk::Box::new(gtk::Orientation::Vertical, 0);
        stage.add_css_class("canvas");
        stage.append(&picture);

        let scale = |name: &str, tip: &str| {
            let scale = gtk::Scale::with_range(
                gtk::Orientation::Horizontal,
                0.0,
                f64::from(trim.frames),
                1.0,
            );
            scale.set_draw_value(false);
            scale.set_digits(0);
            scale.set_hexpand(true);
            scale.set_width_request(360);
            named(&scale, name, tip);
            scale
        };
        let start = scale(
            "Trim start",
            "First frame of the export; the preview shows it (arrow keys move one frame)",
        );
        let end = scale(
            "Trim end",
            "Last frame of the export; the preview shows it (arrow keys move one frame)",
        );
        let start_label = caption("", "field-label");
        let end_label = caption("", "field-label");
        for l in [&start_label, &end_label] {
            l.set_width_chars(8);
        }
        let grid = gtk::Grid::new();
        grid.set_column_spacing(10);
        grid.set_row_spacing(2);
        grid.attach(&caption("Start", "field-label"), 0, 0, 1, 1);
        grid.attach(&start, 1, 0, 1, 1);
        grid.attach(&start_label, 2, 0, 1, 1);
        grid.attach(&caption("End", "field-label"), 0, 1, 1, 1);
        grid.attach(&end, 1, 1, 1, 1);
        grid.attach(&end_label, 2, 1, 1, 1);
        let status = caption("", "status-bar");
        status.set_selectable(true);
        let controls = gtk::Box::new(gtk::Orientation::Vertical, 8);
        controls.set_margin_start(12);
        controls.set_margin_end(12);
        controls.set_margin_top(12);
        controls.append(&grid);
        controls.append(&status);

        let info = caption("", "dimension-label");
        info.set_wrap(false);
        info.set_hexpand(true);
        let play = button("Play", "Play the kept part of the recording");
        play.set_width_request(80);
        let gif = button("Save GIF…", "Export the kept part as a new GIF file");
        let webm = button(
            "Save WebM…",
            "Export the kept part as a new WebM video file",
        );
        let close = button("Close", "Close and delete the temporary recording (Escape)");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.add_css_class("context-bar");
        bar.append(&info);
        for b in [&play, &gif, &webm, &close] {
            bar.append(b);
        }
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&stage);
        root.append(&controls);
        root.append(&bar);
        window.set_child(Some(&root));
        window.set_default_widget(Some(&play));

        let review = Rc::new(Self {
            app: app.clone(),
            window,
            dir,
            clip,
            size,
            player,
            picture,
            trim: Cell::new(trim),
            position: Cell::new(0),
            playing: Cell::new(false),
            syncing: Cell::new(false),
            start,
            end,
            start_label,
            end_label,
            info,
            status,
            play,
            gif,
            webm,
            close,
            export: RefCell::default(),
        });
        review.wire();
        review.sync();
        let weak = Rc::downgrade(&review);
        glib::spawn_future_local({
            let weak = weak.clone();
            async move {
                while wake_rx.recv().await.is_ok() {
                    let Some(review) = weak.upgrade() else { break };
                    review.show_frame();
                }
            }
        });
        glib::spawn_future_local(async move {
            while let Ok(event) = event_rx.recv().await {
                let Some(review) = weak.upgrade() else { break };
                match event {
                    BusEvent::Ended => review.stop_playback(),
                    BusEvent::Failed(message) => {
                        review.pause();
                        review.note(&format!("The preview failed: {message}"));
                    }
                }
            }
        });
        Ok(review)
    }

    fn wire(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let on = |button: &gtk::Button, f: fn(&Rc<Review>)| {
            let weak = weak.clone();
            button.connect_clicked(move |_| {
                if let Some(review) = weak.upgrade() {
                    f(&review);
                }
            });
        };
        on(&self.play, Review::toggle_play);
        on(&self.gif, |r| r.export_to(Format::Gif));
        on(&self.webm, |r| r.export_to(Format::WebM));
        on(&self.close, Review::close);
        self.start.connect_value_changed({
            let weak = weak.clone();
            move |scale| {
                if let Some(review) = weak.upgrade().filter(|r| !r.syncing.get()) {
                    let mut trim = review.trim.get();
                    trim.set_start(scale.value() as u32);
                    review.trimmed(trim, trim.start);
                }
            }
        });
        self.end.connect_value_changed({
            let weak = weak.clone();
            move |scale| {
                if let Some(review) = weak.upgrade().filter(|r| !r.syncing.get()) {
                    let mut trim = review.trim.get();
                    trim.set_end(scale.value() as u32);
                    review.trimmed(trim, trim.end - 1);
                }
            }
        });
        self.window.connect_close_request({
            let weak = weak.clone();
            move |_| {
                if let Some(review) = weak.upgrade() {
                    review.close();
                }
                glib::Propagation::Stop
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(review) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gdk::Key::Escape {
                review.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.window.add_controller(keys);
    }

    fn exporting(&self) -> bool {
        self.export.borrow().is_some()
    }

    fn note(&self, text: &str) {
        self.status.set_text(text);
    }

    /// Refreshes every widget from the trim and playback state.
    fn sync(&self) {
        let trim = self.trim.get();
        let busy = self.exporting();
        self.syncing.set(true);
        self.start.set_value(f64::from(trim.start));
        self.end.set_value(f64::from(trim.end));
        self.syncing.set(false);
        self.start_label
            .set_text(&format!("{:.1} s", secs(trim.start)));
        self.end_label.set_text(&format!("{:.1} s", secs(trim.end)));
        self.info.set_text(&format!(
            "{:.1} s kept of {:.1} s · {} × {} px · {FPS} fps",
            secs(trim.kept()),
            secs(trim.frames),
            self.size.0,
            self.size.1
        ));
        let (play, close) = (
            if self.playing.get() { "Pause" } else { "Play" },
            if busy { "Cancel export" } else { "Close" },
        );
        self.play.set_label(play);
        self.close.set_label(close);
        // An explicit accessible name would otherwise keep announcing the old label.
        self.play
            .update_property(&[gtk::accessible::Property::Label(play)]);
        self.close
            .update_property(&[gtk::accessible::Property::Label(close)]);
        for w in [
            self.start.upcast_ref::<gtk::Widget>(),
            self.end.upcast_ref(),
        ] {
            w.set_sensitive(!busy);
        }
        for b in [&self.play, &self.gif, &self.webm] {
            b.set_sensitive(!busy);
        }
    }

    fn show_frame(&self) {
        let Some(shown) = self.player.take() else {
            return;
        };
        if self.playing.get() && shown.frame >= self.trim.get().end {
            return self.stop_playback();
        }
        self.position.set(shown.frame);
        let bytes = glib::Bytes::from_owned(shown.data);
        self.picture.set_paintable(Some(&gdk::MemoryTexture::new(
            shown.width as i32,
            shown.height as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            shown.width as usize * 4,
        )));
    }

    fn seek(&self, frame: u32) {
        if let Err(error) = self.player.seek(frame) {
            self.note(&format!("{error:#}"));
        }
    }

    fn pause(&self) {
        if self.playing.replace(false) {
            if let Err(error) = self.player.set_playing(false) {
                self.note(&format!("{error:#}"));
            }
            self.sync();
        }
    }

    /// Paused at the first kept frame.
    fn stop_playback(&self) {
        self.pause();
        self.seek(self.trim.get().start);
    }

    fn trimmed(&self, trim: Trim, show: u32) {
        self.pause();
        self.trim.set(trim);
        self.seek(show);
        self.sync();
    }

    fn toggle_play(self: &Rc<Self>) {
        if self.playing.get() {
            return self.pause();
        }
        let trim = self.trim.get();
        if !(trim.start..trim.end).contains(&self.position.get()) {
            self.seek(trim.start);
        }
        match self.player.set_playing(true) {
            Ok(()) => {
                self.playing.set(true);
                self.sync();
            }
            Err(error) => self.note(&format!("{error:#}")),
        }
    }

    fn close(self: &Rc<Self>) {
        if let Some(job) = self.export.borrow().as_ref() {
            job.interrupt();
            return;
        }
        self.window.destroy();
        self.app.quit();
    }

    fn export_to(self: &Rc<Self>, format: Format) {
        if self.exporting() {
            return;
        }
        self.pause();
        let dialog = gtk::FileChooserNative::new(
            Some(&format!("Save {}", format.name())),
            Some(&self.window),
            gtk::FileChooserAction::Save,
            Some("Save"),
            Some("Cancel"),
        );
        dialog.set_modal(true);
        dialog.set_current_name(&format!(
            "SharpShot-recording-{}.{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            format.ext()
        ));
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(format.name()));
        filter.add_pattern(&format!("*.{}", format.ext()));
        dialog.add_filter(&filter);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let response = dialog.run_future().await;
            let path = dialog.file().and_then(|f| f.path());
            dialog.destroy();
            if response != gtk::ResponseType::Accept {
                return;
            }
            let Some(review) = weak.upgrade() else { return };
            let Some(path) = path else {
                return review.note("Choose a local folder for the export.");
            };
            let rx = match review.begin_export(format, with_extension_of(path.clone(), format)) {
                Ok(rx) => rx,
                Err(error) => return review.note(&format!("Export failed: {error:#}")),
            };
            drop(review);
            let result = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err(anyhow!("The encoder stopped unexpectedly")));
            let Some(review) = weak.upgrade() else { return };
            let cancelled = review
                .export
                .borrow_mut()
                .take()
                .is_some_and(|job| job.cancel.load(Ordering::Relaxed));
            review.note(&match result {
                Ok(saved) => format!("Saved {}", saved.display()),
                Err(_) if cancelled => "Export cancelled; nothing was written.".to_owned(),
                Err(error) => format!("Export failed: {error:#}"),
            });
            review.sync();
        });
    }

    fn begin_export(
        &self,
        format: Format,
        dest: PathBuf,
    ) -> Result<async_channel::Receiver<Result<PathBuf>>> {
        ensure!(
            !dest.exists(),
            "{} already exists; choose a new filename",
            dest.display()
        );
        let (tx, rx) = async_channel::bounded(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let kill: Arc<Mutex<Option<Killer>>> = Arc::default();
        let (clip, log, trim) = (
            self.clip.clone(),
            self.dir.path().join("export.log"),
            self.trim.get(),
        );
        let handle = std::thread::Builder::new()
            .name("exporter".into())
            .spawn({
                let (cancel, kill) = (cancel.clone(), kill.clone());
                move || {
                    let result =
                        export(&clip, format, trim, &dest, &log, &cancel, &kill).map(|()| dest);
                    let _ = tx.send_blocking(result);
                }
            })
            .context("Cannot start the encoder thread")?;
        *self.export.borrow_mut() = Some(Job {
            cancel,
            stop: None,
            written: Arc::default(),
            kill,
            handle: Some(handle),
        });
        self.note(&format!("Encoding the {}…", format.name()));
        self.sync();
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_recording_files_are_owner_only() -> Result<()> {
        let dir = private_clip_dir()?;
        assert_eq!(dir.path().metadata()?.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            dir.path().join(CLIP).metadata()?.permissions().mode() & 0o777,
            0o600
        );
        let mut command = Command::new("sleep");
        command.arg("30");
        let log = dir.path().join("intake.log");
        let encoder = Ffmpeg::from_command(command, &log, Stdio::null())?;
        assert_eq!(log.metadata()?.permissions().mode() & 0o777, 0o600);
        drop(encoder);
        Ok(())
    }

    #[test]
    fn full_chroma_preview_keeps_colour_edges_and_webm_keeps_odd_dimensions() -> Result<()> {
        gst::init()?;
        let dir = private_clip_dir()?;
        let size = (301u32, 251u32);
        let clip = dir.path().join(CLIP);
        let encoder = Ffmpeg::spawn(
            &intake_args(size, &clip),
            &dir.path().join("intake.log"),
            Stdio::piped(),
        )?;
        let mut pixels = vec![0u8; size.0 as usize * size.1 as usize * 4];
        for y in 0..size.1 as usize {
            for x in 0..size.0 as usize {
                pixels[(y * size.0 as usize + x) * 4..][..4].copy_from_slice(
                    if (152..182).contains(&y) {
                        &[255, 255, 255, 255]
                    } else {
                        &[24, 25, 229, 255]
                    },
                );
            }
        }
        let mut input = encoder.take_stdin().context("encoder input")?;
        for _ in 0..MIN_KEPT {
            input.write_all(&pixels)?;
        }
        drop(input);
        encoder.finish(&AtomicBool::new(false), Some(Duration::from_secs(10)))?;
        let context = glib::MainContext::new();
        context.with_thread_default(|| -> Result<()> {
            let (wake, _) = async_channel::bounded(1);
            let player = Player::new(&clip, size, wake, |_| {})?;
            let shown = player
                .take()
                .context("preview did not produce its first frame")?;
            let pixel = |x: usize, y: usize| &shown.data[(y * shown.width as usize + x) * 4..][..3];
            assert!(
                pixel(20, 153).iter().all(|&v| v > 240),
                "white edge was contaminated by shifted chroma"
            );
            let red = pixel(20, 184);
            assert!(
                red[0] > 210 && red[1] < 50 && red[2] < 50,
                "red edge gained a decoder ghost: {red:?}"
            );
            Ok(())
        })??;
        let out = dir.path().join("odd.webm");
        let trim = Trim {
            frames: MIN_KEPT,
            start: 0,
            end: MIN_KEPT,
        };
        Ffmpeg::spawn(
            &export_args(&clip, &out, Format::WebM, trim),
            &dir.path().join("export.log"),
            Stdio::null(),
        )?
        .finish(&AtomicBool::new(false), Some(Duration::from_secs(10)))?;
        let info = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=width,height:format=duration",
                "-of",
                "json",
            ])
            .arg(out)
            .output()?;
        ensure!(
            info.status.success(),
            "ffprobe could not inspect the exported clip"
        );
        let info: serde_json::Value = serde_json::from_slice(&info.stdout)?;
        assert_eq!(info["streams"][0]["width"], size.0);
        assert_eq!(info["streams"][0]["height"], size.1);
        let duration: f64 = info["format"]["duration"]
            .as_str()
            .context("missing duration")?
            .parse()?;
        assert!(
            (duration - secs(MIN_KEPT)).abs() < 0.01,
            "wrong WebM duration: {duration}"
        );
        Ok(())
    }

    fn crop(x: u32, y: u32, width: u32, height: u32) -> Crop {
        Crop {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn trim_keeps_a_minimum_and_stays_inside_the_clip() {
        assert!(Trim::new(MIN_KEPT - 1).is_none());
        let mut t = Trim::new(30).unwrap();
        t.set_start(100);
        assert_eq!((t.start, t.end), (30 - MIN_KEPT, 30));
        t.set_end(0);
        assert_eq!(
            (t.start, t.end),
            (30 - MIN_KEPT, 30),
            "end cannot pass start + minimum"
        );
        t.set_start(0);
        t.set_end(2);
        assert_eq!((t.start, t.end, t.kept()), (0, MIN_KEPT, MIN_KEPT));
        t.set_end(1000);
        assert_eq!(t.end, 30);
    }

    #[test]
    fn frame_times_round_trip_even_with_millisecond_stamps() {
        for frame in 0..MAX_LIMIT * FPS {
            let exact = u64::from(frame) * 1000 / u64::from(FPS);
            for ms in [seek_ms(frame), exact, exact + 1] {
                assert_eq!(frame_at(ms), frame, "frame {frame} at {ms} ms");
            }
            assert!(seek_ms(frame) >= exact);
        }
    }

    #[test]
    fn recording_length_is_bounded() {
        assert_eq!(max_frames(0), MIN_LIMIT * FPS);
        assert_eq!(max_frames(1000), MAX_LIMIT * FPS);
        let max = max_frames(5);
        assert_eq!(frames_due(Duration::ZERO, max), 1);
        assert_eq!(frames_due(Duration::from_millis(66), max), 1);
        assert_eq!(frames_due(Duration::from_millis(67), max), 2);
        assert_eq!(frames_due(Duration::from_secs(2), max), 31);
        assert_eq!(frames_due(Duration::from_secs(3600), max), max);
    }

    #[test]
    fn region_must_be_inside_the_frame_and_within_the_pixel_budget() {
        let frame = (1920, 1080);
        assert!(check_crop(crop(0, 0, 1920, 1080), frame).is_ok());
        assert!(check_crop(crop(0, 0, MIN_SIDE - 1, 100), frame).is_err());
        assert!(check_crop(crop(1900, 0, 100, 100), frame).is_err());
        assert!(check_crop(crop(u32::MAX, 0, 100, 100), frame).is_err());
        assert!(check_crop(crop(0, 0, 3840, 2160), (3840, 2160)).is_ok());
        assert!(check_crop(crop(0, 0, 4000, 2160), (4000, 2160)).is_err());
    }

    #[test]
    fn cropping_honours_the_row_pitch() {
        // 32 × 20 BGRA frame with 8 bytes of padding per row; pixel (x, y) is [x, y, 0xAA, 0xBB].
        let stride = 32 * 4 + 8;
        let mut src = vec![0xEE; stride * 20];
        for y in 0..20 {
            for x in 0..32 {
                src[y * stride + x * 4..][..4].copy_from_slice(&[x as u8, y as u8, 0xAA, 0xBB]);
            }
        }
        let mut out = vec![9];
        copy_crop(&src, stride, (32, 20), crop(4, 2, 16, 16), &mut out).unwrap();
        assert_eq!(out.len(), 16 * 16 * 4);
        assert_eq!(out[..4], [4, 2, 0xAA, 0xBB]);
        assert_eq!(out[16 * 4..16 * 4 + 4], [4, 3, 0xAA, 0xBB], "second row");
        assert_eq!(out[out.len() - 4..], [19, 17, 0xAA, 0xBB], "last pixel");
        let mut short = Vec::new();
        assert!(
            copy_crop(
                &src[..stride * 19],
                stride,
                (32, 20),
                crop(0, 0, 16, 16),
                &mut short
            )
            .is_err(),
            "a buffer shorter than the frame is refused"
        );
        assert!(copy_crop(&src, stride, (32, 20), crop(20, 0, 16, 16), &mut short).is_err());
    }

    #[test]
    fn bgra_becomes_opaque_rgba() {
        let stride = 2 * 4 + 4;
        let mut src = vec![0; stride * 2];
        src[..8].copy_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        src[stride..stride + 8].copy_from_slice(&[7, 8, 9, 0, 10, 11, 12, 0]);
        let image = rgba_from_bgra(&src, stride, (2, 2)).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [3, 2, 1, 255]);
        assert_eq!(image.get_pixel(1, 1).0, [12, 11, 10, 255]);
        assert!(rgba_from_bgra(&src[..stride], stride, (2, 2)).is_err());
    }

    fn armed(tap: &Tap, bytes: usize) {
        let mut st = tap.lock();
        st.mode = Mode::Record {
            crop: crop(0, 0, 16, 16),
            frame: (16, 16),
        };
        st.latest = vec![7; bytes];
        st.have = true;
    }

    #[test]
    fn the_recorder_never_writes_more_than_its_limit() {
        let tap = Tap::default();
        armed(&tap, 16);
        let (_keep, stop) = mpsc::channel();
        let written = AtomicU32::new(0);
        let mut out = Vec::new();
        let started = Instant::now();
        let frames = record_frames(
            &tap,
            &mut out,
            3,
            &written,
            &stop,
            &AtomicBool::new(false),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            (frames, out.len(), written.load(Ordering::Relaxed)),
            (3, 48, 3)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stop_keeps_what_was_recorded_and_failures_discard_it() {
        let tap = Tap::default();
        armed(&tap, 16);
        let (tx, stop) = mpsc::channel();
        tx.send(()).unwrap();
        let written = AtomicU32::new(0);
        let mut out = Vec::new();
        let never = AtomicBool::new(false);
        let wait = Duration::from_secs(1);
        assert_eq!(
            record_frames(&tap, &mut out, 450, &written, &stop, &never, wait).unwrap(),
            1
        );

        tap.lock().error = Some("source resized".into());
        let err =
            record_frames(&tap, &mut Vec::new(), 450, &written, &stop, &never, wait).unwrap_err();
        assert!(format!("{err:#}").contains("source resized"));

        let silent = Tap::default();
        silent.arm(crop(0, 0, 16, 16), (16, 16));
        let err = record_frames(
            &silent,
            &mut Vec::new(),
            450,
            &written,
            &stop,
            &never,
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no frame"));

        let err = record_frames(
            &tap,
            &mut Vec::new(),
            450,
            &written,
            &stop,
            &AtomicBool::new(true),
            wait,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("cancelled"));
    }

    #[test]
    fn abandoning_a_job_kills_a_blocked_encoder_before_joining_its_thread() {
        let dir = tempfile::tempdir().unwrap();
        let mut sleep = Command::new("sleep");
        sleep.arg("30");
        let ffmpeg = Ffmpeg::from_command(sleep, &dir.path().join("log"), Stdio::piped()).unwrap();
        let pid = lock(&ffmpeg.child).id();
        let mut stdin = ffmpeg.take_stdin().unwrap();
        let kill = Arc::new(Mutex::new(Some(ffmpeg.killer())));
        let handle = std::thread::spawn(move || {
            // `sleep` never reads, so this blocks for good once the pipe is full.
            let _ = stdin.write_all(&vec![0u8; 4 << 20]);
            ffmpeg.kill();
        });
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        drop(Job {
            cancel: Arc::default(),
            stop: None,
            written: Arc::default(),
            kill,
            handle: Some(handle),
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the join did not hang"
        );
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "the encoder is gone"
        );
    }

    #[test]
    fn an_encoder_never_outlives_its_owner_or_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let spawn = || {
            let mut sleep = Command::new("sleep");
            sleep.arg("30");
            Ffmpeg::from_command(sleep, &dir.path().join("log"), Stdio::null()).unwrap()
        };

        let child = spawn();
        let proc = PathBuf::from(format!("/proc/{}", lock(&child.child).id()));
        assert!(proc.exists());
        drop(child);
        assert!(!proc.exists(), "dropped: killed and reaped");

        let child = spawn();
        let proc = PathBuf::from(format!("/proc/{}", lock(&child.child).id()));
        let started = Instant::now();
        let err = child
            .finish(&AtomicBool::new(false), Some(Duration::from_millis(100)))
            .unwrap_err();
        assert!(format!("{err:#}").contains("in time"));
        assert!(started.elapsed() < Duration::from_secs(2) && !proc.exists());

        // No deadline: a kill from another thread ends the wait without any polling.
        let child = spawn();
        let killer = child.killer();
        let killer_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            killer.kill();
        });
        let err = child.finish(&AtomicBool::new(true), None).unwrap_err();
        killer_thread.join().unwrap();
        assert!(format!("{err:#}").contains("cancelled"));
    }

    #[test]
    fn an_export_never_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("keep.gif");
        std::fs::write(&dest, b"original").unwrap();
        let trim = Trim::new(30).unwrap();
        let err = export(
            Path::new("/nonexistent.mkv"),
            Format::Gif,
            trim,
            &dest,
            &dir.path().join("log"),
            &AtomicBool::new(false),
            &Mutex::new(None),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"original");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temporary file is left behind");
    }
}
