//! Direct in-process screen capture over `zwlr_screencopy_unstable_v1`
//! (Hyprland, sway and other wlroots-based compositors). No `grim`/`slurp`.
//!
//! The whole output is copied once into a `wl_shm` buffer, decoded to an
//! opaque RGBA image at native pixel resolution (HiDPI is not downscaled) and
//! rotated/flipped so it matches what the user sees. A global logical region
//! copies its intersection with each normal output (`capture_output_region`).
//! Transformed outputs are captured upright before cropping to avoid legacy
//! wlroots ROI-position bugs. Pieces use the highest participating output scale.
//!
//! Every wait on the compositor is a `poll(2)` with a deadline; nothing blocks
//! indefinitely.

use anyhow::{Context, Result, anyhow, ensure};
use image::{RgbaImage, imageops};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use std::fmt;
use std::os::fd::AsFd;
use std::str::FromStr;
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_callback::WlCallback,
    wl_output::{self, WlOutput},
    wl_registry::{self, WlRegistry},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// Connect, global discovery and output description.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Noninteractive frame description.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// First-use compositor permission dialogs need time for a human response.
const COPY_TIMEOUT: Duration = Duration::from_secs(60);
/// Sanity cap on one side of the captured buffer.
const MAX_DIMENSION: u32 = 1 << 16;

/// Names of all outputs, in compositor order. Names fall back to `output-N`
/// when the compositor does not implement `wl_output` v4 names.
pub fn outputs() -> Result<Vec<String>> {
    let s = Session::connect()?;
    Ok(s.state
        .outputs
        .iter()
        .enumerate()
        .map(|(i, o)| o.label(i))
        .collect())
}

/// A rectangle in the global logical compositor space (the coordinates slurp
/// and `xdg_output` use). Negative origins are valid on multi-monitor layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    fn right(&self) -> i64 {
        self.x as i64 + self.width as i64
    }

    fn bottom(&self) -> i64 {
        self.y as i64 + self.height as i64
    }

    fn intersect(&self, o: &Region) -> Option<Region> {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = (self.right().min(o.right()), self.bottom().min(o.bottom()));
        (x1 > x0 as i64 && y1 > y0 as i64).then(|| Region {
            x: x0,
            y: y0,
            width: (x1 - x0 as i64) as u32,
            height: (y1 - y0 as i64) as u32,
        })
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{} {}x{}", self.x, self.y, self.width, self.height)
    }
}

impl FromStr for Region {
    type Err = anyhow::Error;

    /// Parses slurp's `X,Y WxH`. Zero sizes and rectangles whose far edge
    /// overflows `i32` are rejected.
    fn from_str(s: &str) -> Result<Self> {
        let bad = || anyhow!("invalid region {s:?}; expected \"X,Y WxH\", e.g. \"10,20 640x480\"");
        let mut parts = s.split_whitespace();
        let (pos, size) = (parts.next().ok_or_else(bad)?, parts.next().ok_or_else(bad)?);
        if parts.next().is_some() {
            return Err(bad());
        }
        let (x, y) = pos.split_once(',').ok_or_else(bad)?;
        let (w, h) = size.split_once('x').ok_or_else(bad)?;
        let region = Region {
            x: x.parse::<i32>().map_err(|_| bad())?,
            y: y.parse::<i32>().map_err(|_| bad())?,
            width: w.parse::<u32>().map_err(|_| bad())?,
            height: h.parse::<u32>().map_err(|_| bad())?,
        };
        ensure!(
            region.width > 0 && region.height > 0,
            "region {s:?} has zero width or height"
        );
        ensure!(
            region.right() <= i32::MAX as i64 && region.bottom() <= i32::MAX as i64,
            "region {s:?} extends past the largest supported coordinate"
        );
        Ok(region)
    }
}

/// Capture a global logical rectangle at the highest participating output scale.
/// Gaps remain transparent. Transformed outputs use an upright full-frame crop.
pub fn capture_region(region: Region, cursor: bool) -> Result<RgbaImage> {
    capture_region_in(&mut Session::connect()?, region, cursor)
}

/// One screencopy capture on a single compositor session: the whole `desktop`,
/// a global logical `region`, or one `output` (the first by default); at most
/// one selector may be given. `Ok(None)` only means the compositor cannot be
/// reached or has no `zwlr_screencopy_manager_v1` (use the portal instead);
/// capture, permission and geometry failures are errors.
pub fn try_capture(
    output: Option<&str>,
    region: Option<Region>,
    desktop: bool,
    cursor: bool,
) -> Result<Option<RgbaImage>> {
    ensure!(
        u8::from(output.is_some()) + u8::from(region.is_some()) + u8::from(desktop) <= 1,
        "choose one of an output, a region or the whole desktop"
    );
    let mut s = match Session::connect() {
        Ok(s) => s,
        Err(e) if e.is::<Unavailable>() => return Ok(None),
        Err(e) => return Err(e),
    };
    let iface = ZwlrScreencopyManagerV1::interface().name;
    if !s.state.globals.iter().any(|g| g.interface == iface) {
        return Ok(None);
    }
    let image = match region {
        Some(region) => capture_region_in(&mut s, region, cursor)?,
        None if desktop => desktop_in(&mut s, cursor)?,
        None => capture_output_in(&mut s, output, cursor)?,
    };
    Ok(Some(image))
}

/// The Wayland socket could not be opened; the one failure `try_capture` treats as "no screencopy here".
#[derive(Debug)]
struct Unavailable(String);

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

fn capture_output_in(s: &mut Session, output: Option<&str>, cursor: bool) -> Result<RgbaImage> {
    let idx = s.pick_output(output)?;
    let (manager, shm) = s.bind_screencopy()?;
    grab_output(s, &manager, &shm, idx, cursor)
}

fn capture_region_in(s: &mut Session, region: Region, cursor: bool) -> Result<RgbaImage> {
    s.load_logical()?;
    capture_logical(s, region, cursor)
}

fn desktop_in(s: &mut Session, cursor: bool) -> Result<RgbaImage> {
    s.load_logical()?;
    let mut rects = s.state.outputs.iter().filter_map(OutputInfo::logical);
    let first = rects
        .next()
        .context("no enabled output has a logical size")?;
    let (mut x0, mut y0) = (i64::from(first.x), i64::from(first.y));
    let (mut x1, mut y1) = (x0 + i64::from(first.width), y0 + i64::from(first.height));
    for r in rects {
        x0 = x0.min(i64::from(r.x));
        y0 = y0.min(i64::from(r.y));
        x1 = x1.max(i64::from(r.x) + i64::from(r.width));
        y1 = y1.max(i64::from(r.y) + i64::from(r.height));
    }
    let region = Region {
        x: i32::try_from(x0)?,
        y: i32::try_from(y0)?,
        width: u32::try_from(x1 - x0)?,
        height: u32::try_from(y1 - y0)?,
    };
    capture_logical(s, region, cursor)
}

fn capture_logical(s: &mut Session, region: Region, cursor: bool) -> Result<RgbaImage> {
    let (manager, shm) = s.bind_screencopy()?;

    let rects: Vec<(usize, Region)> = s
        .state
        .outputs
        .iter()
        .enumerate()
        .filter_map(|(i, o)| Some((i, o.logical()?)))
        .collect();
    let pieces = plan(region, &rects)?;
    let scales = pieces
        .iter()
        .map(|p| s.state.outputs[p.output].pixel_scale(p.output))
        .collect::<Result<Vec<f64>>>()?;
    let best = scales.iter().copied().fold(0.0, f64::max);

    let mut parts = Vec::with_capacity(pieces.len());
    for (p, &scale) in pieces.iter().zip(&scales) {
        let (wl, transform, label, logical) = {
            let o = &s.state.outputs[p.output];
            (
                o.wl.clone(),
                o.transform,
                o.label(p.output),
                o.logical().context("output has no logical geometry")?,
            )
        };
        let l = p.local;
        let img = if transform == 0 {
            grab(s, &shm, |qh| {
                manager.capture_output_region(
                    cursor as i32,
                    &wl,
                    l.x,
                    l.y,
                    l.width as i32,
                    l.height as i32,
                    qh,
                    (),
                )
            })
        } else {
            // shortcut: transformed outputs use a full frame until compositor ROI coordinates agree.
            grab_output(s, &manager, &shm, p.output, cursor)
                .and_then(|image| crop_upright(image, l, (logical.width, logical.height)))
        }
        .with_context(|| format!("capturing {l} of output {label}"))?;
        let expect = (l.width as f64 * scale, l.height as f64 * scale);
        let tol = scale.ceil() + 1.0;
        ensure!(
            (img.width() as f64 - expect.0).abs() <= tol
                && (img.height() as f64 - expect.1).abs() <= tol,
            "the compositor returned {}x{} px for {l} of output {label}, expected about {:.0}x{:.0}",
            img.width(),
            img.height(),
            expect.0,
            expect.1
        );
        parts.push((p.global, img));
    }
    compose(region, best, parts)
}

fn grab_output(
    s: &mut Session,
    manager: &ZwlrScreencopyManagerV1,
    shm: &WlShm,
    idx: usize,
    cursor: bool,
) -> Result<RgbaImage> {
    let wl = s.state.outputs[idx].wl.clone();
    let image = grab(s, shm, |qh| {
        manager.capture_output(cursor as i32, &wl, qh, ())
    })?;
    let output = &s.state.outputs[idx];
    Ok(
        if buffer_is_physical(
            output.transform,
            image.dimensions(),
            output.mode,
            s.is_hyprland(),
        ) {
            apply_transform(image, invert_transform(output.transform))
        } else {
            image
        },
    )
}

/// Copy the frame produced by `request` into shm and decode it (y-inversion
/// applied, output transform not).
fn grab(
    s: &mut Session,
    shm: &WlShm,
    request: impl FnOnce(&QueueHandle<State>) -> ZwlrScreencopyFrameV1,
) -> Result<RgbaImage> {
    let qh = s.queue.handle();
    s.state.frame = FrameState::default();
    let frame = request(&qh);
    s.wait("screencopy frame description", FRAME_TIMEOUT, |st| {
        st.frame.buffer_done || st.frame.failed
    })?;
    ensure!(!s.state.frame.failed, FAILED_MSG);
    let info = s.state.frame.shm.ok_or_else(|| {
        anyhow!(
            "the compositor offers only dmabuf screencopy buffers; wl_shm capture is not available"
        )
    })?;
    let raw_format = raw(info.format);
    let layout = Layout::from_fourcc(raw_format).ok_or_else(|| {
        anyhow!(
            "unsupported screencopy pixel format {raw_format:#010x}; supported: \
             Argb8888, Xrgb8888, Abgr8888, Xbgr8888 and the 10-bit variants"
        )
    })?;
    let format = info
        .format
        .into_result()
        .map_err(|e| anyhow!("unknown wl_shm format: {e}"))?;
    let size = validate_shm(info.width, info.height, info.stride)?;

    // Prefer tmpfs-backed runtime dir; any unlinked temp file works.
    let file = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => tempfile::tempfile_in(dir).or_else(|_| tempfile::tempfile()),
        None => tempfile::tempfile(),
    }
    .context("creating shared-memory file for the screencopy buffer")?;
    file.set_len(size as u64)
        .context("sizing shared-memory file")?;
    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        info.width as i32,
        info.height as i32,
        info.stride as i32,
        format,
        &qh,
        (),
    );
    frame.copy(&buffer);
    s.wait("screencopy frame copy", COPY_TIMEOUT, |st| {
        st.frame.ready || st.frame.failed
    })
    .with_context(|| {
        let binary = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "sharpshot".into());
        format!(
            "Screen capture did not complete. If Hyprland enforces permissions, approve \
            this executable's screencopy prompt or configure an allow rule and restart \
            Hyprland (see README): {binary}"
        )
    })?;
    ensure!(!s.state.frame.failed, FAILED_MSG);

    // SAFETY: the file is private (unlinked tempfile) and sized by us; the compositor has
    // finished writing (`ready`) and we hold the only other mapping. A compositor that
    // truncated the file afterwards could fault us, but it is the trusted display server.
    let map = unsafe { memmap2::Mmap::map(&file) }
        .context("mapping the shared-memory screencopy buffer")?;
    let img = decode(
        &map,
        info.width,
        info.height,
        info.stride,
        layout,
        s.state.frame.y_invert,
    )?;
    frame.destroy();
    buffer.destroy();
    pool.destroy();
    Ok(img)
}

const FAILED_MSG: &str = "the compositor reported a screencopy failure (output disabled or asleep, \
                          or the compositor refused the buffer); try again";

// ---------------------------------------------------------------------------
// Wayland session
// ---------------------------------------------------------------------------

struct Global {
    name: u32,
    interface: String,
    version: u32,
}

struct OutputInfo {
    wl: WlOutput,
    name: Option<String>,
    /// `wl_output.transform` raw value.
    transform: u32,
    scale: i32,
    /// Current native mode in physical pixels.
    mode: Option<(u32, u32)>,
    /// `xdg_output` position and size in the global logical space.
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    done: bool,
}

impl OutputInfo {
    fn label(&self, idx: usize) -> String {
        self.name.clone().unwrap_or_else(|| format!("output-{idx}"))
    }

    fn describe(&self, idx: usize) -> String {
        let (w, h) = self.mode.unwrap_or((0, 0));
        format!(
            "{} ({w}x{h}, scale {}, transform {})",
            self.label(idx),
            self.scale,
            self.transform
        )
    }

    /// Global logical rectangle; `None` before `load_logical` or for a disabled
    /// (zero-sized) output.
    fn logical(&self) -> Option<Region> {
        let ((x, y), (w, h)) = (self.logical_pos?, self.logical_size?);
        (w > 0 && h > 0).then_some(Region {
            x,
            y,
            width: w as u32,
            height: h as u32,
        })
    }

    /// Native pixels per logical pixel (fractional scales included).
    fn pixel_scale(&self, idx: usize) -> Result<f64> {
        let mode = self
            .mode
            .ok_or_else(|| anyhow!("output {} reports no current mode", self.label(idx)))?;
        let logical = self
            .logical()
            .ok_or_else(|| anyhow!("output {} has no logical size", self.label(idx)))?;
        Ok(output_scale(
            mode,
            self.transform,
            (logical.width, logical.height),
        ))
    }
}

#[derive(Clone, Copy)]
struct ShmInfo {
    format: WEnum<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Default)]
struct FrameState {
    shm: Option<ShmInfo>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    globals: Vec<Global>,
    outputs: Vec<OutputInfo>,
    synced: bool,
    frame: FrameState,
}

struct Session {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    registry: WlRegistry,
}

impl Session {
    /// Connect and learn about all globals and outputs.
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().map_err(|e| {
            anyhow::Error::new(Unavailable(format!(
                "connecting to the Wayland compositor (is WAYLAND_DISPLAY set and a Wayland session running?): {e}"
            )))
        })?;
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let mut s = Session {
            conn,
            queue,
            state: State::default(),
            registry,
        };
        s.sync("registry globals")?;

        for i in 0..s.state.globals.len() {
            let g = &s.state.globals[i];
            if g.interface != WlOutput::interface().name {
                continue;
            }
            let version = g.version.min(4);
            let wl = s
                .registry
                .bind::<WlOutput, _, _>(g.name, version, &qh, s.state.outputs.len());
            s.state.outputs.push(OutputInfo {
                wl,
                name: None,
                transform: 0,
                scale: 1,
                mode: None,
                logical_pos: None,
                logical_size: None,
                done: false,
            });
        }
        ensure!(
            !s.state.outputs.is_empty(),
            "the compositor exposes no outputs"
        );
        s.wait("output descriptions", SETUP_TIMEOUT, |st| {
            st.outputs.iter().all(|o| o.done)
        })?;
        Ok(s)
    }

    /// Round-trip to the compositor with a deadline.
    fn sync(&mut self, what: &str) -> Result<()> {
        let qh = self.queue.handle();
        self.state.synced = false;
        self.conn.display().sync(&qh, ());
        self.wait(what, SETUP_TIMEOUT, |st| st.synced)
    }

    /// Dispatch events until `done(state)` holds or `timeout` elapses.
    fn wait(&mut self, what: &str, timeout: Duration, done: impl Fn(&State) -> bool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while !done(&self.state) {
            self.pump(deadline)
                .with_context(|| format!("waiting for the compositor ({what})"))?;
        }
        Ok(())
    }

    /// One bounded step: dispatch queued events, else flush and read the socket.
    fn pump(&mut self, deadline: Instant) -> Result<()> {
        if self.queue.dispatch_pending(&mut self.state)? > 0 {
            return Ok(());
        }
        match self.queue.flush() {
            Ok(()) => {}
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        // None means events arrived meanwhile; the caller loops and dispatches them.
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("timed out; the compositor is not responding"))?;
        let timeout = Timespec {
            tv_sec: left.as_secs() as _,
            tv_nsec: left.subsec_nanos() as _,
        };
        let ready = {
            let fd = guard.connection_fd();
            match poll(&mut [PollFd::new(&fd, PollFlags::IN)], Some(&timeout)) {
                Ok(n) => n > 0,
                Err(Errno::INTR) => false,
                Err(e) => return Err(e.into()),
            }
        };
        if ready {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn bind_global<I>(&self, max_version: u32) -> Result<I>
    where
        I: Proxy + 'static,
        State: Dispatch<I, ()>,
    {
        let iface = I::interface().name;
        let g = self
            .state
            .globals
            .iter()
            .find(|g| g.interface == iface)
            .ok_or_else(|| anyhow!("global {iface} is not advertised"))?;
        Ok(self.registry.bind::<I, _, _>(
            g.name,
            g.version.min(max_version),
            &self.queue.handle(),
            (),
        ))
    }

    fn pick_output(&self, wanted: Option<&str>) -> Result<usize> {
        let outs = &self.state.outputs;
        match wanted {
            None => Ok(0),
            Some(name) => outs
                .iter()
                .enumerate()
                .position(|(i, o)| o.label(i) == name)
                .ok_or_else(|| {
                    let list: Vec<_> = outs
                        .iter()
                        .enumerate()
                        .map(|(i, o)| o.describe(i))
                        .collect();
                    anyhow!(
                        "unknown monitor {name:?}; available outputs: {}",
                        list.join(", ")
                    )
                }),
        }
    }

    fn bind_screencopy(&self) -> Result<(ZwlrScreencopyManagerV1, WlShm)> {
        let manager = self.bind_global(3).context(
            "the compositor does not offer zwlr_screencopy_manager_v1; SharpShot needs a wlroots-based \
             compositor (Hyprland, sway, river, ...). GNOME and KDE are not supported",
        )?;
        let shm = self
            .bind_global(1)
            .context("the compositor does not offer wl_shm")?;
        Ok((manager, shm))
    }

    /// Hyprland advertises its own `hyprland_*` globals; independent of the process environment.
    fn is_hyprland(&self) -> bool {
        self.state
            .globals
            .iter()
            .any(|g| g.interface.starts_with("hyprland_"))
    }

    /// Learn every output's logical position and size from `xdg_output`.
    fn load_logical(&mut self) -> Result<()> {
        let manager: ZxdgOutputManagerV1 = self.bind_global(3).context(
            "the compositor does not offer zxdg_output_manager_v1, needed to place a region \
             in the global layout",
        )?;
        let qh = self.queue.handle();
        for (i, o) in self.state.outputs.iter().enumerate() {
            manager.get_xdg_output(&o.wl, &qh, i);
        }
        self.wait("output logical geometry", SETUP_TIMEOUT, |st| {
            st.outputs
                .iter()
                .all(|o| o.logical_pos.is_some() && o.logical_size.is_some())
        })
    }
}

fn raw<T: Into<u32>>(e: WEnum<T>) -> u32 {
    match e {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(v) => v,
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.push(Global {
                name,
                interface,
                version,
            });
        }
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        _: <WlCallback as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = true;
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(*idx) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { transform, .. } => o.transform = raw(transform),
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if raw(flags) & 1 != 0 {
                    o.mode = Some((width.max(0) as u32, height.max(0) as u32));
                }
                // wl_output v1 has no `done` event.
                o.done |= proxy.version() < 2;
            }
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Done => o.done = true,
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(*idx) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.logical_size = Some((width, height));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        let f = &mut state.frame;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                f.shm.get_or_insert(ShmInfo {
                    format,
                    width,
                    height,
                    stride,
                });
                // Versions before 3 send no `buffer_done`.
                f.buffer_done |= proxy.version() < 3;
            }
            Event::Flags { flags } => f.y_invert = raw(flags) & 1 != 0,
            Event::BufferDone => f.buffer_done = true,
            Event::Ready { .. } => f.ready = true,
            Event::Failed => f.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ZwlrScreencopyManagerV1);
delegate_noop!(State: ZxdgOutputManagerV1);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlBuffer);

// ---------------------------------------------------------------------------
// Pixels
// ---------------------------------------------------------------------------

/// Channel order/depth of a 32-bit little-endian `wl_shm` word. The alpha
/// variants share a layout with the `X` variants; alpha is forced opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Xrgb8,
    Xbgr8,
    Xrgb10,
    Xbgr10,
}

impl Layout {
    /// `wl_shm` codes: Argb8888 = 0 and Xrgb8888 = 1, others are DRM fourcc.
    fn from_fourcc(f: u32) -> Option<Self> {
        match f {
            0 | 1 => Some(Self::Xrgb8),
            0x3432_4241 | 0x3432_4258 => Some(Self::Xbgr8),
            0x3033_5241 | 0x3033_5258 => Some(Self::Xrgb10),
            0x3033_4241 | 0x3033_4258 => Some(Self::Xbgr10),
            _ => None,
        }
    }

    /// `b` is the pixel's four bytes in memory (little-endian word).
    fn pixel(self, b: [u8; 4]) -> [u8; 4] {
        let to8 = |c: u32| ((c * 255 + 511) / 1023) as u8;
        match self {
            Self::Xrgb8 => [b[2], b[1], b[0], 255],
            Self::Xbgr8 => [b[0], b[1], b[2], 255],
            Self::Xrgb10 | Self::Xbgr10 => {
                let v = u32::from_le_bytes(b);
                let (lo, mid, hi) = (
                    to8(v & 0x3ff),
                    to8((v >> 10) & 0x3ff),
                    to8((v >> 20) & 0x3ff),
                );
                if self == Self::Xrgb10 {
                    [hi, mid, lo, 255]
                } else {
                    [lo, mid, hi, 255]
                }
            }
        }
    }
}

/// Check compositor-supplied geometry; returns the pool size in bytes.
fn validate_shm(width: u32, height: u32, stride: u32) -> Result<usize> {
    ensure!(
        (1..=MAX_DIMENSION).contains(&width) && (1..=MAX_DIMENSION).contains(&height),
        "compositor reported an invalid buffer size {width}x{height}"
    );
    let min_stride = width
        .checked_mul(4)
        .expect("width bounded by MAX_DIMENSION");
    ensure!(
        stride >= min_stride,
        "compositor reported stride {stride}, smaller than a {width}px row ({min_stride} bytes)"
    );
    let size = (stride as u64) * (height as u64);
    ensure!(
        size <= i32::MAX as u64,
        "screencopy buffer of {size} bytes is too large for wl_shm"
    );
    Ok(size as usize)
}

/// Convert a strided shm buffer to RGBA, flipping vertically if `y_invert`.
fn decode(
    raw: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    layout: Layout,
    y_invert: bool,
) -> Result<RgbaImage> {
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    ensure!(
        stride >= w * 4 && raw.len() >= stride * h,
        "pixel buffer smaller than its declared geometry"
    );
    let mut out = vec![0u8; w * h * 4];
    for (y, src) in raw.chunks_exact(stride).take(h).enumerate() {
        let dy = if y_invert { h - 1 - y } else { y };
        let dst = &mut out[dy * w * 4..][..w * 4];
        for (s, d) in src[..w * 4].chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            d.copy_from_slice(&layout.pixel([s[0], s[1], s[2], s[3]]));
        }
    }
    RgbaImage::from_raw(width, height, out)
        .ok_or_else(|| anyhow!("decoded pixel buffer has the wrong length"))
}

// ---------------------------------------------------------------------------
// Output transform
// ---------------------------------------------------------------------------

/// Apply a `wl_output.transform` (flip around the vertical axis first, then
/// rotate counter-clockwise by 90° steps) to an image.
fn apply_transform(img: RgbaImage, t: u32) -> RgbaImage {
    let img = if t & 4 != 0 {
        imageops::flip_horizontal(&img)
    } else {
        img
    };
    match t & 3 {
        0 => img,
        1 => imageops::rotate270(&img), // image's rotate270 is 270° clockwise = 90° CCW
        2 => imageops::rotate180(&img),
        _ => imageops::rotate90(&img),
    }
}

/// Inverse of `apply_transform`'s argument: flipped 90/270 are involutions,
/// plain 90 and 270 swap.
fn invert_transform(t: u32) -> u32 {
    if t & 1 != 0 && t & 4 == 0 { t ^ 2 } else { t }
}

/// wlroots (sway) copies the buffer in the output's physical orientation, so
/// the client must undo the output transform. Hyprland already delivers the
/// upright picture. Where the buffer dimensions disambiguate (90°/270° against
/// the native mode) trust them; otherwise assume wlroots unless on Hyprland.
/// shortcut: 180°/flipped on an unknown compositor assumes the wlroots
/// behaviour; upgrade by checking the compositor's identity if one deviates.
fn buffer_is_physical(
    transform: u32,
    buf: (u32, u32),
    mode: Option<(u32, u32)>,
    hyprland: bool,
) -> bool {
    if transform == 0 {
        return false;
    }
    if transform & 1 != 0 && buf.0 != buf.1 {
        if let Some((mw, mh)) = mode {
            if buf == (mw, mh) {
                return true;
            }
            if buf == (mh, mw) {
                return false;
            }
        }
    }
    !hyprland
}

// ---------------------------------------------------------------------------
// Region geometry
// ---------------------------------------------------------------------------

/// One output's share of a requested region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
    output: usize,
    /// Rectangle in the output's own logical coordinates (what
    /// `capture_output_region` takes).
    local: Region,
    /// The same rectangle in global logical coordinates.
    global: Region,
}

/// Split a global logical rectangle over intersecting outputs.
fn plan(region: Region, outputs: &[(usize, Region)]) -> Result<Vec<Piece>> {
    let pieces: Vec<Piece> = outputs
        .iter()
        .filter_map(|&(output, r)| {
            let global = region.intersect(&r)?;
            let local = Region {
                x: (global.x as i64 - r.x as i64) as i32,
                y: (global.y as i64 - r.y as i64) as i32,
                ..global
            };
            Some(Piece {
                output,
                local,
                global,
            })
        })
        .collect();
    let layout = || {
        outputs
            .iter()
            .map(|(_, r)| r.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    };
    ensure!(
        !pieces.is_empty(),
        "region {region} does not intersect any output (outputs: {})",
        layout()
    );
    Ok(pieces)
}

/// Native pixels per logical pixel of an output: its current (physical) mode,
/// turned upright by the transform, over the `xdg_output` logical size.
fn output_scale(mode: (u32, u32), transform: u32, logical: (u32, u32)) -> f64 {
    let (w, h) = if transform & 1 != 0 {
        (mode.1, mode.0)
    } else {
        mode
    };
    (w as f64 / logical.0 as f64).max(h as f64 / logical.1 as f64)
}

/// Pixel rectangle `(x, y, w, h)` of `piece` on a canvas of `region` drawn at
/// `scale` pixels per logical pixel. Edges are rounded, not sizes, so pieces
/// that touch logically tile the canvas without gaps or overlap.
fn dest_rect(region: Region, piece: Region, scale: f64) -> (u32, u32, u32, u32) {
    let edge = |v: i64| (v as f64 * scale).round() as u32;
    let (x0, y0) = (
        edge(piece.x as i64 - region.x as i64),
        edge(piece.y as i64 - region.y as i64),
    );
    let (x1, y1) = (
        edge(piece.right() - region.x as i64),
        edge(piece.bottom() - region.y as i64),
    );
    (x0, y0, (x1 - x0).max(1), (y1 - y0).max(1))
}

fn crop_upright(image: RgbaImage, local: Region, logical: (u32, u32)) -> Result<RgbaImage> {
    ensure!(
        image.width() > 0
            && image.height() > 0
            && logical.0 > 0
            && logical.1 > 0
            && local.width > 0
            && local.height > 0
            && local.x >= 0
            && local.y >= 0
            && local.right() <= i64::from(logical.0)
            && local.bottom() <= i64::from(logical.1),
        "capture region is outside the output's logical bounds"
    );
    if local.x == 0 && local.y == 0 && (local.width, local.height) == logical {
        return Ok(image);
    }
    let x = |value: i64| {
        (value as f64 * f64::from(image.width()) / f64::from(logical.0)).round() as u32
    };
    let y = |value: i64| {
        (value as f64 * f64::from(image.height()) / f64::from(logical.1)).round() as u32
    };
    let left = x(i64::from(local.x)).min(image.width() - 1);
    let top = y(i64::from(local.y)).min(image.height() - 1);
    let right = x(local.right()).max(left + 1).min(image.width());
    let bottom = y(local.bottom()).max(top + 1).min(image.height());
    Ok(imageops::crop_imm(&image, left, top, right - left, bottom - top).to_image())
}

/// Stitch upright `parts` (global rectangle, pixels) into one image of
/// `region` at `scale`. Parts at a lower scale are upsampled. A lone part that
/// is the whole region is returned untouched at its native size.
fn compose(region: Region, scale: f64, mut parts: Vec<(Region, RgbaImage)>) -> Result<RgbaImage> {
    if parts.len() == 1 && parts[0].0 == region {
        return Ok(parts.remove(0).1);
    }
    let (_, _, w, h) = dest_rect(region, region, scale);
    ensure!(
        w <= MAX_DIMENSION && h <= MAX_DIMENSION && w as u64 * h as u64 * 4 <= i32::MAX as u64,
        "region {region} is {w}x{h} px at scale {scale:.2}, larger than the supported capture size"
    );
    let mut canvas = RgbaImage::new(w, h);
    for (piece, img) in parts {
        let (x, y, pw, ph) = dest_rect(region, piece, scale);
        let img = if img.dimensions() == (pw, ph) {
            img
        } else {
            imageops::resize(&img, pw, ph, imageops::FilterType::Triangle)
        };
        imageops::replace(&mut canvas, &img, x as i64, y as i64);
    }
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// 3x2 image whose pixel value encodes its position: (x, y) -> [x, y, 0, 255].
    fn grid() -> RgbaImage {
        RgbaImage::from_fn(3, 2, |x, y| Rgba([x as u8, y as u8, 0, 255]))
    }

    fn at(img: &RgbaImage, x: u32, y: u32) -> (u8, u8) {
        let p = img.get_pixel(x, y).0;
        (p[0], p[1])
    }

    #[test]
    fn rotate_90_ccw_moves_top_right_to_top_left() {
        let r = apply_transform(grid(), 1);
        assert_eq!(r.dimensions(), (2, 3));
        assert_eq!(at(&r, 0, 0), (2, 0));
        assert_eq!(at(&r, 1, 2), (0, 1));
    }

    #[test]
    fn rotate_270_and_180() {
        let r = apply_transform(grid(), 3);
        assert_eq!(r.dimensions(), (2, 3));
        assert_eq!(at(&r, 1, 0), (0, 0)); // top-left goes to top-right
        let r = apply_transform(grid(), 2);
        assert_eq!(r.dimensions(), (3, 2));
        assert_eq!(at(&r, 0, 0), (2, 1));
    }

    #[test]
    fn flips() {
        assert_eq!(at(&apply_transform(grid(), 4), 0, 0), (2, 0));
        // flipped_180 is a vertical flip.
        assert_eq!(at(&apply_transform(grid(), 6), 0, 0), (0, 1));
        // flipped_90 is a transpose.
        let r = apply_transform(grid(), 5);
        assert_eq!(r.dimensions(), (2, 3));
        assert_eq!(at(&r, 1, 2), (2, 1));
        assert_eq!(at(&r, 1, 0), (0, 1));
    }

    #[test]
    fn invert_undoes_every_transform() {
        for t in 0..8 {
            let back = apply_transform(apply_transform(grid(), t), invert_transform(t));
            assert_eq!(back, grid(), "transform {t}");
        }
    }

    #[test]
    fn physical_vs_upright_buffers() {
        assert!(!buffer_is_physical(
            0,
            (1920, 1080),
            Some((1920, 1080)),
            false
        ));
        // 90° output, native 1920x1080: landscape buffer is physical, portrait is upright.
        assert!(buffer_is_physical(
            1,
            (1920, 1080),
            Some((1920, 1080)),
            true
        ));
        assert!(!buffer_is_physical(
            1,
            (1080, 1920),
            Some((1920, 1080)),
            false
        ));
        // No dimension hint: fall back to compositor identity.
        assert!(buffer_is_physical(
            2,
            (1920, 1080),
            Some((1920, 1080)),
            false
        ));
        assert!(!buffer_is_physical(
            2,
            (1920, 1080),
            Some((1920, 1080)),
            true
        ));
        // Square mode is ambiguous for 90°.
        assert!(buffer_is_physical(
            3,
            (1000, 1000),
            Some((1000, 1000)),
            false
        ));
    }

    #[test]
    fn argb_and_xrgb_decode_bgra_bytes() {
        // Memory B,G,R,A -> RGBA, alpha forced opaque.
        assert_eq!(Layout::Xrgb8.pixel([1, 2, 3, 0]), [3, 2, 1, 255]);
        assert_eq!(Layout::from_fourcc(0), Some(Layout::Xrgb8));
        assert_eq!(Layout::from_fourcc(1), Some(Layout::Xrgb8));
    }

    #[test]
    fn abgr_and_xbgr_decode_rgba_bytes() {
        assert_eq!(Layout::Xbgr8.pixel([1, 2, 3, 0]), [1, 2, 3, 255]);
        assert_eq!(Layout::from_fourcc(0x3432_4241), Some(Layout::Xbgr8));
        assert_eq!(Layout::from_fourcc(0x3432_4258), Some(Layout::Xbgr8));
    }

    #[test]
    fn ten_bit_decodes_extremes() {
        // x:R:G:B = 0:1023:0:512 little endian.
        let v: u32 = (1023 << 20) | 512;
        assert_eq!(Layout::Xrgb10.pixel(v.to_le_bytes()), [255, 0, 128, 255]);
        // Same bits as xBGR: R=512, B=1023.
        assert_eq!(Layout::Xbgr10.pixel(v.to_le_bytes()), [128, 0, 255, 255]);
        assert_eq!(
            Layout::Xrgb10.pixel(0x3fff_ffffu32.to_le_bytes()),
            [255, 255, 255, 255]
        );
        assert_eq!(Layout::from_fourcc(0x3033_5258), Some(Layout::Xrgb10));
        assert_eq!(Layout::from_fourcc(0x3033_4258), Some(Layout::Xbgr10));
        assert_eq!(Layout::from_fourcc(0x5659_5559), None); // YUYV
    }

    #[test]
    fn decode_respects_stride_padding_and_y_invert() {
        // 2x2, stride 12 (4 bytes of row padding). Row 0 = red-ish, row 1 = blue-ish (BGRx memory).
        let raw = [
            0, 0, 255, 0, 0, 0, 255, 0, 9, 9, 9, 9, // row 0: red, red, pad
            255, 0, 0, 0, 255, 0, 0, 0, 9, 9, 9, 9, // row 1: blue, blue, pad
        ];
        let img = decode(&raw, 2, 2, 12, Layout::Xrgb8, false).unwrap();
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 0, 255, 255]);
        let flipped = decode(&raw, 2, 2, 12, Layout::Xrgb8, true).unwrap();
        assert_eq!(flipped.get_pixel(1, 0).0, [0, 0, 255, 255]);
        assert_eq!(flipped.get_pixel(0, 1).0, [255, 0, 0, 255]);
    }

    #[test]
    fn decode_rejects_short_buffers() {
        assert!(decode(&[0; 15], 2, 2, 8, Layout::Xrgb8, false).is_err());
        assert!(decode(&[0; 16], 2, 2, 7, Layout::Xrgb8, false).is_err());
    }

    #[test]
    fn shm_geometry_validation() {
        assert_eq!(validate_shm(1920, 1080, 7680).unwrap(), 7680 * 1080);
        assert_eq!(validate_shm(1, 1, 4).unwrap(), 4);
        assert!(validate_shm(0, 10, 4).is_err());
        assert!(validate_shm(10, 0, 40).is_err());
        assert!(validate_shm(10, 10, 39).is_err()); // stride < width * 4
        assert!(validate_shm(MAX_DIMENSION + 1, 1, u32::MAX).is_err());
        assert!(validate_shm(MAX_DIMENSION, MAX_DIMENSION, MAX_DIMENSION * 4).is_err()); // > i32::MAX
    }

    fn rg(x: i32, y: i32, w: u32, h: u32) -> Region {
        Region {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn parses_slurp_geometry() {
        assert_eq!(
            "10,20 640x480".parse::<Region>().unwrap(),
            rg(10, 20, 640, 480)
        );
        assert_eq!(
            "-1920,-5 1x1\n".parse::<Region>().unwrap(),
            rg(-1920, -5, 1, 1)
        );
        assert_eq!("  3,4   5x6 ".parse::<Region>().unwrap(), rg(3, 4, 5, 6));
        assert_eq!(
            rg(-7, 8, 9, 10).to_string().parse::<Region>().unwrap(),
            rg(-7, 8, 9, 10)
        );
    }

    #[test]
    fn rejects_malformed_geometry() {
        for bad in [
            "",
            "10,20",
            "10 20 30x40",
            "10,20 30",
            "10,20 30x",
            "10,20 x40",
            "a,20 30x40",
            "10,20 -30x40",
            "10,20 30x40 50",
            "10;20 30x40",
            "10,20 30X40",
            "1.5,2 3x4",
            "10,20 0x40",
            "10,20 40x0",
            "2147483648,0 1x1",
            "10,20 4294967296x1",
        ] {
            assert!(bad.parse::<Region>().is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn rejects_far_edge_overflow() {
        assert!("2147483647,0 1x1".parse::<Region>().is_err());
        assert!("0,2147483647 1x1".parse::<Region>().is_err());
        assert!("-5,0 2147483653x1".parse::<Region>().is_err());
        assert!("-2147483648,0 4294967295x1".parse::<Region>().is_ok()); // far edge is exactly i32::MAX
        assert!("2147483646,0 1x1".parse::<Region>().is_ok());
        assert!(
            "-2147483648,-2147483648 2147483647x2147483647"
                .parse::<Region>()
                .is_ok()
        );
    }

    #[test]
    fn intersection_handles_negative_and_touching_rects() {
        let a = rg(-100, -50, 200, 100);
        assert_eq!(a.intersect(&rg(0, 0, 500, 500)), Some(rg(0, 0, 100, 50)));
        assert_eq!(a.intersect(&rg(100, -50, 10, 10)), None); // shares only an edge
        assert_eq!(a.intersect(&rg(-1000, -1000, 10, 10)), None);
        assert_eq!(a.intersect(&rg(-90, -40, 5, 5)), Some(rg(-90, -40, 5, 5)));
        let big = rg(i32::MIN, i32::MIN, u32::MAX, u32::MAX);
        assert_eq!(big.intersect(&rg(0, 0, 3, 3)), Some(rg(0, 0, 3, 3)));
    }

    #[test]
    fn plan_maps_global_to_output_local_across_negative_origin() {
        let outs = [
            (0usize, rg(-1920, 0, 1920, 1080)),
            (1, rg(0, 0, 2560, 1440)),
        ];
        let p = plan(rg(-100, 10, 300, 50), &outs).unwrap();
        assert_eq!(
            p,
            vec![
                Piece {
                    output: 0,
                    local: rg(1820, 10, 100, 50),
                    global: rg(-100, 10, 100, 50)
                },
                Piece {
                    output: 1,
                    local: rg(0, 10, 200, 50),
                    global: rg(0, 10, 200, 50)
                },
            ]
        );
        // Entirely inside one output: a single piece equal to the region.
        let p = plan(rg(-1900, 5, 10, 10), &outs).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].output, p[0].local, p[0].global),
            (0, rg(20, 5, 10, 10), rg(-1900, 5, 10, 10))
        );
    }

    #[test]
    fn a_zone_with_no_visible_pixels_is_rejected() {
        let outputs = [(0usize, rg(0, 0, 100, 100)), (1, rg(200, 0, 100, 100))];
        assert!(plan(rg(120, 0, 50, 50), &outputs).is_err());
    }

    #[test]
    fn a_zone_preserves_transparent_gaps_and_offscreen_edges() {
        let region = rg(-1, -1, 6, 4);
        let outputs = [(0usize, rg(0, 0, 2, 2)), (1, rg(4, 0, 2, 2))];
        let pieces = plan(region, &outputs).unwrap();
        let red = Rgba([255, 0, 0, 255]);
        let blue = Rgba([0, 0, 255, 255]);
        let parts = pieces
            .into_iter()
            .map(|p| {
                let color = if p.output == 0 { red } else { blue };
                (
                    p.global,
                    RgbaImage::from_pixel(p.local.width, p.local.height, color),
                )
            })
            .collect();
        let image = compose(region, 1.0, parts).unwrap();
        let expected = RgbaImage::from_fn(6, 4, |x, y| {
            if (1..3).contains(&y) {
                if (1..3).contains(&x) {
                    return red;
                }
                if x == 5 {
                    return blue;
                }
            }
            Rgba([0, 0, 0, 0])
        });
        assert_eq!(image, expected);
    }

    #[test]
    fn output_scale_covers_integer_fractional_and_rotated() {
        assert_eq!(output_scale((1920, 1080), 0, (1920, 1080)), 1.0);
        assert_eq!(output_scale((3840, 2160), 0, (1920, 1080)), 2.0);
        assert_eq!(output_scale((3840, 2160), 0, (2560, 1440)), 1.5);
        // 90°: logical size is the mode's sides swapped.
        assert_eq!(output_scale((1920, 1080), 1, (1080, 1920)), 1.0);
        assert_eq!(output_scale((3840, 2160), 3, (1080, 1920)), 2.0);
        // Logical sizes are rounded: 2560x1440 at 1.25 -> 2048x1152 exactly, 1.5 -> 1707x960.
        let s = output_scale((2560, 1440), 0, (1707, 960));
        assert!((s - 1.5).abs() < 0.001, "{s}");
    }

    #[test]
    fn dest_rects_tile_without_gaps_at_fractional_scale() {
        let region = rg(0, 0, 7, 3);
        let (a, b) = (rg(0, 0, 3, 3), rg(3, 0, 4, 3));
        let (ax, _, aw, _) = dest_rect(region, a, 1.5);
        let (bx, _, bw, _) = dest_rect(region, b, 1.5);
        assert_eq!((ax, aw, bx, bw), (0, 5, 5, 6)); // 4.5 and 10.5 round up
        assert_eq!(dest_rect(region, region, 1.5), (0, 0, 11, 5));
        assert_eq!(
            dest_rect(rg(-10, -10, 4, 4), rg(-8, -9, 2, 1), 2.0),
            (4, 2, 4, 2)
        );
        assert_eq!(dest_rect(region, rg(0, 0, 1, 1), 0.1), (0, 0, 1, 1)); // never empty
    }

    #[test]
    fn upright_crop_maps_logical_offsets_and_preserves_native_pixels() {
        let image = RgbaImage::from_fn(6, 10, |x, y| Rgba([x as u8, y as u8, 7, 255]));
        let crop = crop_upright(image, rg(1, 2, 2, 2), (3, 5)).unwrap();
        let expected = RgbaImage::from_fn(4, 4, |x, y| Rgba([x as u8 + 2, y as u8 + 4, 7, 255]));
        assert_eq!(crop, expected);
        let fractional = RgbaImage::from_fn(5, 8, |x, y| Rgba([x as u8, y as u8, 9, 255]));
        let crop = crop_upright(fractional, rg(1, 1, 1, 3), (3, 5)).unwrap();
        let expected = RgbaImage::from_fn(1, 4, |_, y| Rgba([2, y as u8 + 2, 9, 255]));
        assert_eq!(crop, expected);
    }

    fn solid(w: u32, h: u32, c: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(c))
    }

    #[test]
    fn compose_mixed_scale_upsamples_to_the_highest_scale() {
        // Left output at scale 1 (100x100 px), right at scale 2 (200x200 px).
        let region = rg(-100, 0, 200, 100);
        let (red, blue) = ([255, 0, 0, 255], [0, 0, 255, 255]);
        let out = compose(
            region,
            2.0,
            vec![
                (rg(-100, 0, 100, 100), solid(100, 100, red)),
                (rg(0, 0, 100, 100), solid(200, 200, blue)),
            ],
        )
        .unwrap();
        assert_eq!(out.dimensions(), (400, 200));
        assert_eq!(out.get_pixel(0, 0).0, red);
        assert_eq!(out.get_pixel(199, 199).0, red);
        assert_eq!(out.get_pixel(200, 0).0, blue);
        assert_eq!(out.get_pixel(399, 199).0, blue);
    }

    #[test]
    fn compose_places_vertical_neighbours_and_keeps_pixels_exact() {
        let region = rg(5, 5, 2, 4);
        let top = RgbaImage::from_fn(2, 1, |x, _| Rgba([x as u8 + 1, 0, 0, 255]));
        let bottom = RgbaImage::from_fn(2, 3, |x, y| Rgba([0, x as u8 + 1, y as u8, 255]));
        let out = compose(
            region,
            1.0,
            vec![(rg(5, 8, 2, 1), top), (rg(5, 5, 2, 3), bottom)],
        )
        .unwrap();
        assert_eq!(out.dimensions(), (2, 4));
        assert_eq!(out.get_pixel(1, 3).0, [2, 0, 0, 255]);
        assert_eq!(out.get_pixel(1, 0).0, [0, 2, 0, 255]);
        assert_eq!(out.get_pixel(0, 2).0, [0, 1, 2, 255]);
    }

    #[test]
    fn compose_single_full_part_keeps_native_pixels() {
        // 3x3 logical at 1.5 would round to 5x5; the compositor gave 4x4 and it is kept.
        let img = RgbaImage::from_fn(4, 4, |x, y| Rgba([x as u8, y as u8, 7, 255]));
        let out = compose(rg(0, 0, 3, 3), 1.5, vec![(rg(0, 0, 3, 3), img.clone())]).unwrap();
        assert_eq!(out, img);
    }

    #[test]
    fn compose_rejects_absurd_canvas() {
        assert!(compose(rg(0, 0, 60000, 60000), 1.0, vec![]).is_err());
        assert!(compose(rg(0, 0, 40000, 2), 2.0, vec![]).is_err()); // > MAX_DIMENSION wide
    }
}
