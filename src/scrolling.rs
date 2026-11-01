//! Guided scrolling capture: the user scrolls the content, SharpShot grabs native screencopy
//! frames of a fixed region and stitches them top to bottom.
//!
//! Overlap detection is deliberately conservative. A frame is only joined without review when
//! exactly one scroll distance explains it; no movement, upward movement, repeating or
//! featureless content and unmatched content are reported as such. Frames that cannot be
//! matched are kept *unverified* until the user checks the seam and accepts it, so content is
//! never duplicated or dropped silently.

use crate::capture::{self, Region};
use crate::presentation::surface_from;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use gtk::{
    cairo::{self, Context, Filter, ImageSurface},
    gdk, glib,
    prelude::*,
};
use image::{RgbaImage, imageops};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};

/// Stitched-output bounds; the height limit equals the editor's side limit and the pixel limit
/// is half of it because finishing briefly holds the frames and the result together.
const MAX_DIM: u32 = 32_767;
const MAX_PIXELS: u64 = 1 << 26;
/// Bounds on one captured frame (`capture_region` itself allows far larger buffers).
const MAX_FRAME_SIDE: u32 = 8192;
const MAX_FRAME_PIXELS: u64 = 1 << 24;
const MAX_FRAMES: usize = 512;
/// Total pixels of all frames kept for undo and seam adjustment (256 MiB of RGBA).
const MAX_RETAINED_PIXELS: u64 = 1 << 26;

// Overlap detection. Scores are mean absolute luma differences (0..=255) of row signatures.
const SIGNATURE_COLS: usize = 64;
/// At most this many rows are compared per candidate shift.
const MAX_COMPARE_ROWS: usize = 1024;
const MAX_FULL_ROWS: usize = 512;
/// A row whose signature differs by more than this on average counts as mismatching.
const BAD_ROW: u32 = 12;
const STILL_MEAN: f32 = 0.5;
const STILL_BAD: f32 = 0.005;
/// Worst rows ignored when ranking shifts, equal to `MATCH_BAD`, so a small overlay (cursor,
/// toast, badge) cannot veto a true alignment. Wholesale mismatches stay far above the limit.
const TRIM_PERCENT: usize = 5;
const MATCH_TRIMMED: f32 = 3.0;
// shortcut: no sticky header/footer trimming. The user selects only the scrolling content, and
// frames where fixed bars exceed MATCH_BAD of the rows are flagged unmatched. Upgrade: per-edge
// trim rows applied to frames 2..n if users cannot select around fixed bars.
const MATCH_BAD: f32 = 0.05;
const FULL_MEAN: f32 = 6.0;
/// Frames shorter than this cannot be matched at all.
const MIN_ROWS: usize = 32;
/// Shifts this close to the best one are the same alignment, not a competitor.
const AMBIGUOUS_RADIUS: usize = 3;

// UI.
const PREVIEW_WIDTH: f64 = 360.0;
const SEAM_CONTEXT: u32 = 80;
const FIRST_DELAY: Duration = Duration::from_millis(700);
const HIDE_DELAY: Duration = Duration::from_millis(350);

/// How a new frame relates to the previous one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlap {
    /// The frames are identical: nothing scrolled.
    NoMovement,
    /// The content moved the other way (scrolled up).
    Upward,
    /// Exactly one scroll distance, in rows, explains the new frame.
    Match { shift: u32 },
    /// Several distances fit equally well (repeating or featureless content).
    Ambiguous { guess: u32 },
    /// Nothing aligns reliably (scrolled too far, sticky bars, changing content).
    Unmatched { guess: u32 },
}

struct Signature {
    cols: usize,
    rows: usize,
    data: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Score {
    /// Mean row difference over all sampled rows.
    mean: f32,
    /// The same after dropping the worst `TRIM_PERCENT` of rows.
    trimmed: f32,
    /// Fraction of sampled rows that differ by more than `BAD_ROW`.
    bad: f32,
}

struct Found {
    shift: usize,
    best: Score,
    second: f32,
}

fn luma(p: &[u8]) -> u32 {
    (77 * u32::from(p[0]) + 150 * u32::from(p[1]) + 29 * u32::from(p[2])) >> 8
}

/// One row vector per image row: the mean luma of `SIGNATURE_COLS` column blocks.
fn signature(img: &RgbaImage) -> Signature {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let cols = SIGNATURE_COLS.min(w);
    let mut data = Vec::with_capacity(h * cols);
    for row in img.as_raw().chunks_exact(w * 4) {
        for c in 0..cols {
            let (x0, x1) = (c * w / cols, (c + 1) * w / cols);
            let sum: u32 = row[x0 * 4..x1 * 4].chunks_exact(4).map(luma).sum();
            data.push((sum / (x1 - x0) as u32) as u8);
        }
    }
    Signature {
        cols,
        rows: h,
        data,
    }
}

/// Compares rows `d..` of `a` with rows `0..` of `b`.
fn score(a: &Signature, b: &Signature, d: usize) -> Score {
    let rows = a.rows - d;
    let width = a.cols as u32;
    let mut diffs = Vec::with_capacity(rows.min(MAX_COMPARE_ROWS) + 1);
    for y in (0..rows).step_by(rows.div_ceil(MAX_COMPARE_ROWS)) {
        let ra = &a.data[(d + y) * a.cols..][..a.cols];
        let rb = &b.data[y * b.cols..][..b.cols];
        diffs.push(
            ra.iter()
                .zip(rb)
                .map(|(&p, &q)| u32::from(p.abs_diff(q)))
                .sum::<u32>(),
        );
    }
    let n = diffs.len() as f32;
    let total: u64 = diffs.iter().map(|&v| u64::from(v)).sum();
    let bad = diffs.iter().filter(|&&v| v > BAD_ROW * width).count();
    Score {
        mean: total as f32 / (n * width as f32),
        trimmed: trimmed_mean(&mut diffs) / width as f32,
        bad: bad as f32 / n,
    }
}

fn min_overlap(rows: usize) -> usize {
    (rows / 10).max(16)
}

/// Scores every shift `1..=rows - min_overlap` of `b` against `a`.
fn search(a: &Signature, b: &Signature) -> Option<Found> {
    if a.rows < MIN_ROWS || a.rows != b.rows || a.cols != b.cols {
        return None;
    }
    let scores: Vec<Score> = (1..=a.rows - min_overlap(a.rows))
        .map(|d| score(a, b, d))
        .collect();
    let (at, best) = scores
        .iter()
        .enumerate()
        .min_by(|x, y| x.1.trimmed.total_cmp(&y.1.trimmed))?;
    let second = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| i.abs_diff(at) > AMBIGUOUS_RADIUS)
        .map(|(_, s)| s.trimmed)
        .min_by(f32::total_cmp)
        .unwrap_or(f32::MAX);
    Some(Found {
        shift: at + 1,
        best: *best,
        second,
    })
}

/// Mean luma difference over the real pixels of the overlap implied by `shift`, ignoring the
/// worst `TRIM_PERCENT` of sampled rows so a small animated overlay cannot veto a true match.
fn full_diff(prev: &RgbaImage, next: &RgbaImage, shift: usize) -> f32 {
    let row = prev.width() as usize * 4;
    let rows = prev.height() as usize - shift;
    let mut per_row = Vec::with_capacity(rows.min(MAX_FULL_ROWS) + 1);
    for y in (0..rows).step_by(rows.div_ceil(MAX_FULL_ROWS)) {
        let ra = &prev.as_raw()[(shift + y) * row..][..row];
        let rb = &next.as_raw()[y * row..][..row];
        let (mut total, mut n) = (0u32, 0u32);
        for (p, q) in ra.chunks_exact(4).zip(rb.chunks_exact(4)).step_by(2) {
            total += luma(p).abs_diff(luma(q));
            n += 1;
        }
        per_row.push(total / n.max(1));
    }
    trimmed_mean(&mut per_row)
}

/// Mean of the values after dropping the largest `TRIM_PERCENT` percent of them.
fn trimmed_mean(values: &mut [u32]) -> f32 {
    let keep = values.len() - values.len() * TRIM_PERCENT / 100;
    if keep < values.len() {
        values.select_nth_unstable(keep);
    }
    let sum: u64 = values[..keep].iter().map(|&v| u64::from(v)).sum();
    sum as f32 / keep.max(1) as f32
}

fn aligned(found: &Found, prev: &RgbaImage, next: &RgbaImage) -> bool {
    found.best.trimmed <= MATCH_TRIMMED
        && found.best.bad <= MATCH_BAD
        && full_diff(prev, next, found.shift) <= FULL_MEAN
}

fn unique(found: &Found) -> bool {
    found.second >= found.best.trimmed * 2.0 + 1.0
}

/// Relates `next` to `prev`. Both must have the same size.
pub fn detect(prev: &RgbaImage, next: &RgbaImage) -> Overlap {
    let (a, b) = (signature(prev), signature(next));
    let fallback = (prev.height() / 2).max(1);
    if a.rows != b.rows || a.cols != b.cols {
        return Overlap::Unmatched { guess: fallback };
    }
    let still = score(&a, &b, 0);
    if still.mean <= STILL_MEAN && still.bad <= STILL_BAD {
        return Overlap::NoMovement;
    }
    let Some(found) = search(&a, &b) else {
        return Overlap::Unmatched { guess: fallback };
    };
    let guess = found.shift as u32;
    if !aligned(&found, prev, next) {
        let upward = search(&b, &a).is_some_and(|f| aligned(&f, next, prev) && unique(&f));
        return if upward {
            Overlap::Upward
        } else {
            Overlap::Unmatched { guess }
        };
    }
    if unique(&found) {
        Overlap::Match { shift: guess }
    } else {
        Overlap::Ambiguous { guess }
    }
}

fn check_frame(img: &RgbaImage) -> Result<()> {
    let (w, h) = img.dimensions();
    ensure!(
        (1..=MAX_FRAME_SIDE).contains(&w)
            && (1..=MAX_FRAME_SIDE).contains(&h)
            && u64::from(w) * u64::from(h) <= MAX_FRAME_PIXELS,
        "the captured region is {w}×{h} px; scrolling capture supports at most {MAX_FRAME_SIDE} px per side and {} megapixels",
        MAX_FRAME_PIXELS >> 20
    );
    Ok(())
}

fn fits(width: u32, total_rows: u64) -> bool {
    total_rows <= u64::from(MAX_DIM) && total_rows * u64::from(width) <= MAX_PIXELS
}

fn retained_fits(have: u64, add: u64) -> bool {
    have + add <= MAX_RETAINED_PIXELS
}

#[derive(Clone)]
struct Frame {
    img: Arc<RgbaImage>,
    /// Rows the content moved since the previous frame (0 for the first frame).
    shift: u32,
    verified: bool,
}

/// Frames laid out top to bottom. Frame `i` contributes its first `frames[i + 1].shift` rows;
/// the last frame contributes all of its rows.
///
/// shortcut: frames are kept whole (bounded by `MAX_RETAINED_PIXELS`) so undo restores every
/// earlier seam's full range; upgrade by spilling frames to disk if captures need more.
#[derive(Clone, Default)]
struct Stitch {
    frames: Vec<Frame>,
}

impl Stitch {
    fn shift_sum(&self) -> u64 {
        self.frames.iter().skip(1).map(|f| u64::from(f.shift)).sum()
    }

    fn height(&self) -> u64 {
        self.frames
            .last()
            .map_or(0, |last| self.shift_sum() + u64::from(last.img.height()))
    }

    fn pending(&self) -> bool {
        self.frames.last().is_some_and(|f| !f.verified)
    }

    /// Pixels held by all frames.
    fn retained(&self) -> u64 {
        self.frames
            .iter()
            .map(|f| u64::from(f.img.width()) * u64::from(f.img.height()))
            .sum()
    }

    fn push(&mut self, img: Arc<RgbaImage>, shift: u32, verified: bool) -> Result<()> {
        check_frame(&img)?;
        ensure!(
            self.frames.len() < MAX_FRAMES,
            "at most {MAX_FRAMES} frames per capture; finish or undo"
        );
        let (w, h) = img.dimensions();
        let Some(last) = self.frames.last() else {
            self.frames.push(Frame {
                img,
                shift: 0,
                verified: true,
            });
            return Ok(());
        };
        ensure!(
            last.img.dimensions() == (w, h),
            "the capture size changed from {}×{} to {w}×{h}",
            last.img.width(),
            last.img.height()
        );
        ensure!(
            (1..=last.img.height()).contains(&shift),
            "the seam offset {shift} is outside 1..={}",
            last.img.height()
        );
        ensure!(
            fits(w, self.shift_sum() + u64::from(shift) + u64::from(h)),
            "the stitched image would exceed {MAX_DIM} px tall or {} megapixels; press Finish",
            MAX_PIXELS >> 20
        );
        ensure!(
            retained_fits(self.retained(), u64::from(w) * u64::from(h)),
            "the captured frames use the memory limit of {} megapixels; press Finish",
            MAX_RETAINED_PIXELS >> 20
        );
        self.frames.push(Frame {
            img,
            shift,
            verified,
        });
        Ok(())
    }

    fn pop(&mut self) {
        self.frames.pop();
    }

    fn confirm_last(&mut self) {
        if let Some(last) = self.frames.last_mut() {
            last.verified = true;
        }
    }

    /// Valid seam offsets of the newest frame: any row of the previous frame, which is kept
    /// whole, limited by the stitched-size bounds.
    fn last_shift_range(&self) -> Option<(u32, u32)> {
        let [.., prev, last] = self.frames.as_slice() else {
            return None;
        };
        let base = self.shift_sum() - u64::from(last.shift) + u64::from(last.img.height());
        let rows = u64::from(MAX_DIM).min(MAX_PIXELS / u64::from(last.img.width()));
        let hi = rows.saturating_sub(base).min(u64::from(prev.img.height()));
        (hi >= 1).then_some((1, hi as u32))
    }

    fn set_last_shift(&mut self, shift: u32) -> Result<()> {
        let (lo, hi) = self
            .last_shift_range()
            .context("there is no seam to adjust")?;
        ensure!(
            (lo..=hi).contains(&shift),
            "the seam offset must be between {lo} and {hi}"
        );
        if let Some(last) = self.frames.last_mut() {
            last.shift = shift;
        }
        Ok(())
    }

    fn ready(&self) -> Result<()> {
        ensure!(!self.frames.is_empty(), "capture at least one frame first");
        ensure!(
            !self.pending(),
            "review the newest seam and accept it, or undo the frame"
        );
        Ok(())
    }

    /// Builds the stitched image; runs on a worker thread.
    fn finish(self) -> Result<RgbaImage> {
        self.ready()?;
        let width = self.frames[0].img.width();
        let total = self.height();
        ensure!(fits(width, total), "the stitched image is too large");
        let row = width as usize * 4;
        let mut buf = Vec::with_capacity(row * total as usize);
        let n = self.frames.len();
        let rows: Vec<usize> = (0..n)
            .map(|i| match self.frames.get(i + 1) {
                Some(next) => next.shift as usize,
                None => self.frames[i].img.height() as usize,
            })
            .collect();
        for (frame, rows) in self.frames.into_iter().zip(rows) {
            buf.extend_from_slice(&frame.img.as_raw()[..rows * row]);
        }
        RgbaImage::from_raw(width, total as u32, buf).context("stitched buffer has the wrong size")
    }
}

/// The rows around a seam at native resolution: the end of the previous frame's contribution
/// above, the start of the new frame below. Returns the image and the seam's row in it.
fn seam_view(prev: &RgbaImage, next: &RgbaImage, shift: u32) -> (RgbaImage, u32) {
    let row = prev.width() as usize * 4;
    let above = SEAM_CONTEXT.min(shift);
    let below = SEAM_CONTEXT.min(next.height());
    let mut buf = Vec::with_capacity((above + below) as usize * row);
    buf.extend_from_slice(&prev.as_raw()[(shift - above) as usize * row..shift as usize * row]);
    buf.extend_from_slice(&next.as_raw()[..below as usize * row]);
    let view = RgbaImage::from_raw(prev.width(), above + below, buf)
        .expect("buffer length follows from its own dimensions");
    (view, above)
}

/// The seam preview of the newest two frames. Frames are tracked by identity (their `Arc`
/// allocations, which a `Weak` keeps from being reused), so undo + recapture or a changed shift
/// rebuilds it while status-only syncs reuse the surface. Holds only the current seam.
#[derive(Default)]
struct SeamCache {
    prev: std::sync::Weak<RgbaImage>,
    last: std::sync::Weak<RgbaImage>,
    shift: u32,
    seam: Option<(ImageSurface, u32)>,
}

impl SeamCache {
    fn refresh(&mut self, frames: &[Frame]) -> Option<&(ImageSurface, u32)> {
        let [.., prev, last] = frames else {
            *self = Self::default();
            return None;
        };
        if !(std::ptr::eq(self.prev.as_ptr(), Arc::as_ptr(&prev.img))
            && std::ptr::eq(self.last.as_ptr(), Arc::as_ptr(&last.img))
            && self.shift == last.shift)
        {
            let (view, row) = seam_view(&prev.img, &last.img, last.shift);
            self.seam = surface_from(&view).ok().map(|s| (s, row));
            self.prev = Arc::downgrade(&prev.img);
            self.last = Arc::downgrade(&last.img);
            self.shift = last.shift;
        }
        self.seam.as_ref()
    }
}

fn preview_scale(width: u32) -> f64 {
    (PREVIEW_WIDTH / f64::from(width)).min(1.0)
}

fn thumbnail(img: &RgbaImage) -> RgbaImage {
    let s = preview_scale(img.width());
    let side = |v: u32| ((f64::from(v) * s).round() as u32).max(1);
    imageops::thumbnail(img, side(img.width()), side(img.height()))
}

struct Captured {
    img: Arc<RgbaImage>,
    thumb: RgbaImage,
    overlap: Option<Overlap>,
}

/// Worker body: native screencopy, then overlap detection against the previous frame.
fn grab(region: Region, cursor: bool, prev: Option<Arc<RgbaImage>>) -> Result<Captured> {
    let img = capture::capture_region(region, cursor)?;
    check_frame(&img)?;
    let overlap = match &prev {
        Some(prev) => {
            ensure!(
                prev.dimensions() == img.dimensions(),
                "the capture size changed from {}×{} to {}×{}",
                prev.width(),
                prev.height(),
                img.width(),
                img.height()
            );
            Some(detect(prev, &img))
        }
        None => None,
    };
    Ok(Captured {
        thumb: thumbnail(&img),
        img: Arc::new(img),
        overlap,
    })
}

type Outcome = Rc<RefCell<Option<Result<Option<RgbaImage>>>>>;

/// Runs the guided capture of `region` and returns the stitched image, or `None` when the
/// user cancelled. Owns its own GTK application; call before the editor starts.
pub fn run(region: Region, cursor: bool) -> Result<Option<RgbaImage>> {
    let app = gtk::Application::builder()
        .application_id("io.github.sharpshot.SharpShot.Scroll")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let outcome: Outcome = Rc::default();
    let keep: Rc<RefCell<Option<Rc<Ui>>>> = Rc::default();
    app.connect_activate({
        let (outcome, keep) = (outcome.clone(), keep.clone());
        move |app| {
            if keep.borrow().is_some() {
                return;
            }
            crate::chrome::install();
            let ui = Ui::new(app, region, cursor, outcome.clone());
            ui.capture(FIRST_DELAY);
            *keep.borrow_mut() = Some(ui);
        }
    });
    let code = app.run_with_args::<&str>(&[]);
    keep.borrow_mut().take();
    if code != glib::ExitCode::SUCCESS && outcome.borrow().is_none() {
        bail!("GTK exited with {code:?}");
    }
    outcome.take().unwrap_or(Ok(None))
}

struct Ui {
    app: gtk::Application,
    _hold: gtk::gio::ApplicationHoldGuard,
    window: gtk::ApplicationWindow,
    region: Region,
    cursor: bool,
    outcome: Outcome,
    stitch: RefCell<Stitch>,
    /// One preview thumbnail per frame, aligned with `stitch.frames`.
    thumbs: RefCell<Vec<ImageSurface>>,
    seam: RefCell<SeamCache>,
    busy: Cell<bool>,
    /// Set while the worker stitches; `busy` is also set so editing controls stay disabled.
    finishing: Cell<bool>,
    syncing: Cell<bool>,
    shown_frames: Cell<usize>,
    preview: gtk::DrawingArea,
    preview_scroll: gtk::ScrolledWindow,
    seam_area: gtk::DrawingArea,
    shift: gtk::SpinButton,
    status: gtk::Label,
    info: gtk::Label,
    undo: gtk::Button,
    capture: gtk::Button,
    accept: gtk::Button,
    finish: gtk::Button,
    cancel: gtk::Button,
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
    label.add_css_class(class);
    label
}

impl Ui {
    fn new(app: &gtk::Application, region: Region, cursor: bool, outcome: Outcome) -> Rc<Self> {
        let (width, height) = gtk::gdk::Display::default()
            .and_then(|d| d.monitors().item(0))
            .and_then(|m| m.downcast::<gtk::gdk::Monitor>().ok())
            .map_or((980, 700), |m| {
                let g = m.geometry();
                (
                    (g.width() - 48).clamp(760, 980),
                    (g.height() - 96).clamp(440, 700),
                )
            });
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("SharpShot — Scrolling capture")
            .default_width(width)
            .default_height(height)
            // Fixed-size utility windows float instead of retiling the source viewport on wlroots.
            .resizable(false)
            .build();
        let preview = gtk::DrawingArea::new();
        preview.add_css_class("canvas");
        preview.set_halign(gtk::Align::Start);
        let preview_scroll = gtk::ScrolledWindow::builder()
            .child(&preview)
            .min_content_width(PREVIEW_WIDTH as i32 + 12)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        preview_scroll.add_css_class("canvas");
        let seam_area = gtk::DrawingArea::new();
        seam_area.add_css_class("canvas");
        seam_area.set_hexpand(true);
        seam_area.set_vexpand(true);
        seam_area.set_content_width(480);
        seam_area.set_content_height(240);
        let shift = gtk::SpinButton::with_range(1.0, 2.0, 1.0);
        shift.set_numeric(true);
        shift.set_increments(1.0, 10.0);
        named(
            &shift,
            "Scrolled distance",
            "Pixels the content moved between the last two frames. The seam is cut this far down the previous frame; larger means less overlap",
        );
        let status = gtk::Label::new(None);
        status.set_wrap(true);
        status.set_xalign(0.0);
        status.set_selectable(false);
        let info = caption("No frames yet", "dimension-label");
        info.set_hexpand(true);
        let undo = button("Undo last frame", "Remove the newest frame (Ctrl+Z)");
        let capture = button(
            "Capture next frame",
            "Hide this window, capture the region and add it below the previous frame",
        );
        let accept = button(
            "Accept seam",
            "Keep the newest frame at the offset shown after checking the seam preview",
        );
        let finish = button("Finish", "Open the stitched image in the editor");
        finish.add_css_class("export-button");
        let cancel = button("Cancel", "Discard the capture (Escape)");

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let intro = caption(
            "Use a non-fullscreen source window. Select only scrolling content: fixed headers, toolbars and sidebars inside the region cannot be stitched. Move these controls away from the source, scroll down by less than one page, then capture the next frame. Frames stack top to bottom.",
            "status-bar",
        );
        intro.set_wrap(true);
        root.append(&intro);
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.set_vexpand(true);
        body.append(&preview_scroll);
        body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        let side = gtk::Box::new(gtk::Orientation::Vertical, 8);
        side.set_hexpand(true);
        side.set_margin_start(12);
        side.set_margin_end(12);
        side.set_margin_top(8);
        side.set_margin_bottom(8);
        side.append(&caption("Seam review (native pixels)", "title"));
        side.append(&seam_area);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&caption("Scrolled distance (px)", "field-label"));
        row.append(&shift);
        side.append(&row);
        side.append(&status);
        body.append(&side);
        root.append(&body);
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.add_css_class("context-bar");
        bar.append(&info);
        for b in [&undo, &accept, &capture, &finish, &cancel] {
            bar.append(b);
        }
        root.append(&bar);
        window.set_child(Some(&root));
        window.set_default_widget(Some(&capture));

        let ui = Rc::new(Self {
            app: app.clone(),
            _hold: app.hold(),
            window,
            region,
            cursor,
            outcome,
            stitch: RefCell::default(),
            thumbs: RefCell::default(),
            seam: RefCell::default(),
            busy: Cell::new(false),
            finishing: Cell::new(false),
            syncing: Cell::new(false),
            shown_frames: Cell::new(0),
            preview,
            preview_scroll,
            seam_area,
            shift,
            status,
            info,
            undo,
            capture,
            accept,
            finish,
            cancel,
        });
        ui.wire();
        ui.sync();
        ui
    }

    fn wire(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let on = |button: &gtk::Button, f: fn(&Ui)| {
            let weak = weak.clone();
            button.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    f(&ui);
                }
            });
        };
        on(&self.undo, Ui::undo);
        on(&self.accept, Ui::accept);
        let w = weak.clone();
        self.finish.connect_clicked(move |_| {
            if let Some(ui) = w.upgrade() {
                ui.finish();
            }
        });
        on(&self.cancel, Ui::cancel);
        let w = weak.clone();
        self.capture.connect_clicked(move |_| {
            if let Some(ui) = w.upgrade() {
                ui.capture(HIDE_DELAY);
            }
        });

        let w: Weak<Ui> = weak.clone();
        self.shift.connect_value_changed(move |spin| {
            let Some(ui) = w.upgrade() else { return };
            if ui.syncing.get() || ui.busy.get() {
                return;
            }
            if let Err(error) = ui
                .stitch
                .borrow_mut()
                .set_last_shift(spin.value_as_int() as u32)
            {
                ui.status.set_text(&format!("{error:#}"));
            }
            ui.sync();
        });
        let w = weak.clone();
        self.window.connect_close_request(move |_| {
            if let Some(ui) = w.upgrade() {
                ui.cancel();
            }
            glib::Propagation::Stop
        });
        let keys = gtk::EventControllerKey::new();
        let w = weak.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(ui) = w.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape => ui.cancel(),
                gdk::Key::z if state.contains(gdk::ModifierType::CONTROL_MASK) => ui.undo(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);

        let w = weak.clone();
        self.preview.set_draw_func(move |_, cr, _, _| {
            if let Some(Err(error)) = w.upgrade().map(|ui| ui.draw_preview(cr)) {
                eprintln!("Cannot draw scrolling preview: {error}");
            }
        });
        let w = weak;
        self.seam_area.set_draw_func(move |_, cr, width, height| {
            if let Some(Err(error)) = w.upgrade().map(|ui| ui.draw_seam(cr, width, height)) {
                eprintln!("Cannot draw seam preview: {error}");
            }
        });
    }

    fn draw_preview(&self, cr: &Context) -> Result<(), cairo::Error> {
        let (stitch, thumbs) = (self.stitch.borrow(), self.thumbs.borrow());
        let Some(first) = stitch.frames.first() else {
            return Ok(());
        };
        let scale = preview_scale(first.img.width());
        let width = f64::from(first.img.width()) * scale;
        let n = stitch.frames.len();
        let mut pos = 0.0;
        for (i, frame) in stitch.frames.iter().enumerate() {
            let rows = f64::from(
                stitch
                    .frames
                    .get(i + 1)
                    .map_or(frame.img.height(), |f| f.shift),
            );
            let thumb = &thumbs[i];
            cr.save()?;
            cr.rectangle(0.0, pos * scale, width, rows * scale);
            cr.clip();
            cr.translate(0.0, pos * scale);
            cr.scale(
                width / f64::from(thumb.width()),
                f64::from(frame.img.height()) * scale / f64::from(thumb.height()),
            );
            cr.set_source_surface(thumb, 0.0, 0.0)?;
            cr.paint()?;
            cr.restore()?;
            if i > 0 {
                let last = i == n - 1;
                match (last, frame.verified) {
                    (true, true) => cr.set_source_rgb(0.388, 0.867, 0.835),
                    (true, false) => cr.set_source_rgb(1.0, 0.48, 0.42),
                    _ => cr.set_source_rgba(0.376, 0.439, 0.47, 0.8),
                }
                cr.set_line_width(if last { 2.0 } else { 1.0 });
                cr.move_to(0.0, pos * scale);
                cr.line_to(width, pos * scale);
                cr.stroke()?;
            }
            pos += rows;
        }
        Ok(())
    }

    fn draw_seam(&self, cr: &Context, width: i32, height: i32) -> Result<(), cairo::Error> {
        cr.set_source_rgb(0.65, 0.72, 0.75);
        let seam = self.seam.borrow();
        let Some((surface, row)) = seam.seam.as_ref() else {
            cr.move_to(12.0, 24.0);
            return cr.show_text("Capture a second frame to review its seam.");
        };
        let (sw, sh) = (f64::from(surface.width()), f64::from(surface.height()));
        let scale = (f64::from(width) / sw).min(f64::from(height) / sh).min(3.0);
        cr.translate(
            (f64::from(width) - sw * scale) / 2.0,
            (f64::from(height) - sh * scale) / 2.0,
        );
        cr.scale(scale, scale);
        cr.set_source_surface(surface, 0.0, 0.0)?;
        cr.source().set_filter(if scale >= 1.0 {
            Filter::Nearest
        } else {
            Filter::Good
        });
        cr.paint()?;
        cr.set_source_rgb(0.388, 0.867, 0.835);
        cr.set_line_width(1.5 / scale);
        cr.set_dash(&[6.0 / scale, 4.0 / scale], 0.0);
        cr.move_to(0.0, f64::from(*row));
        cr.line_to(sw, f64::from(*row));
        cr.stroke()
    }

    /// Refreshes every widget from the stitch state.
    fn sync(&self) {
        let stitch = self.stitch.borrow();
        let n = stitch.frames.len();
        let (busy, pending) = (self.busy.get(), stitch.pending());
        self.info.set_text(&match stitch.frames.first() {
            None => "No frames yet".to_owned(),
            Some(first) => format!(
                "{n} frame{} · {} × {} px",
                if n == 1 { "" } else { "s" },
                first.img.width(),
                stitch.height()
            ),
        });
        let range = stitch.last_shift_range();
        self.syncing.set(true);
        if let (Some((lo, hi)), Some(last)) = (range, stitch.frames.last()) {
            self.shift.set_range(f64::from(lo), f64::from(hi));
            self.shift.set_value(f64::from(last.shift));
        }
        self.syncing.set(false);
        self.shift.set_sensitive(range.is_some() && !busy);
        self.undo.set_sensitive(n > 0 && !busy);
        self.capture.set_sensitive(!busy && !pending);
        self.accept.set_sensitive(pending && !busy);
        self.finish.set_sensitive(n > 0 && !pending && !busy);
        self.cancel.set_sensitive(!busy || self.finishing.get());
        self.seam.borrow_mut().refresh(&stitch.frames);
        let (width, height) = stitch.frames.first().map_or((0, 0), |first| {
            let s = preview_scale(first.img.width());
            (
                (f64::from(first.img.width()) * s).round() as i32,
                (stitch.height() as f64 * s).ceil() as i32,
            )
        });
        self.preview.set_content_width(width);
        self.preview.set_content_height(height);
        self.preview.queue_draw();
        self.seam_area.queue_draw();
        if self.shown_frames.replace(n) < n {
            let adjustment = self.preview_scroll.vadjustment();
            glib::timeout_add_local_once(Duration::from_millis(60), move || {
                adjustment.set_value(adjustment.upper() - adjustment.page_size());
            });
        }
    }

    fn note(&self, text: &str) {
        self.status.set_text(text);
        self.sync();
    }

    /// Hides the window so it cannot appear in the frame, captures on a worker, shows it again.
    fn capture(self: &Rc<Self>, delay: Duration) {
        if self.busy.replace(true) {
            return;
        }
        self.sync();
        self.window.set_visible(false);
        let (region, cursor) = (self.region, self.cursor);
        let prev = self.stitch.borrow().frames.last().map(|f| f.img.clone());
        let ui = self.clone();
        glib::spawn_future_local(async move {
            glib::timeout_future(delay).await;
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(grab(region, cursor, prev));
            });
            let result = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err(anyhow!("the capture worker stopped unexpectedly")));
            ui.captured(result);
        });
    }

    fn captured(&self, result: Result<Captured>) {
        self.busy.set(false);
        self.window.present();
        let captured = match result {
            Ok(captured) => captured,
            Err(error) if self.stitch.borrow().frames.is_empty() => return self.quit(Err(error)),
            Err(error) => return self.note(&format!("Capture failed: {error:#}")),
        };
        let n = self.stitch.borrow().frames.len() + 1;
        let overlap = |shift: u32| captured.img.height() - shift;
        let (shift, verified, message) = match captured.overlap {
            None => (
                0,
                true,
                "Frame 1 captured. Scroll the content down by less than one page, then capture the next frame. Undo removes it if the content was not at the top.".to_owned(),
            ),
            Some(Overlap::NoMovement) => {
                return self.note("Nothing moved since the last frame, so no frame was added. Scroll the content down, then capture again.");
            }
            Some(Overlap::Upward) => {
                return self.note("The content moved up; no frame was added. Scroll downward: frames are stitched top to bottom.");
            }
            Some(Overlap::Match { shift }) => (
                shift,
                true,
                format!(
                    "Frame {n} added: {shift} px of new content, {} px overlap matched. Check the seam, adjust it if needed, or keep scrolling.",
                    overlap(shift)
                ),
            ),
            Some(Overlap::Ambiguous { guess }) => (
                guess,
                false,
                format!("Frame {n}: the overlap is ambiguous (repeating or featureless content). The preview uses the best guess of {guess} px. Set the distance so the seam looks continuous, then Accept seam; or Undo and scroll less."),
            ),
            Some(Overlap::Unmatched { guess }) => (
                guess,
                false,
                format!("Frame {n}: no reliable overlap found (scrolled too far, a fixed bar inside the region, or changing content). The preview uses {guess} px. Set the distance so the seam looks continuous, then Accept seam; or Undo and scroll less. Fixed bars cannot be stitched: capture only the scrolling content."),
            ),
        };
        match self.add_frame(captured, shift, verified) {
            Ok(()) => self.note(&message),
            Err(error) => self.note(&format!("Frame not added: {error:#}")),
        }
    }

    fn add_frame(&self, captured: Captured, shift: u32, verified: bool) -> Result<()> {
        let thumb = surface_from(&captured.thumb).context("cannot build a preview surface")?;
        self.stitch
            .borrow_mut()
            .push(captured.img, shift, verified)?;
        self.thumbs.borrow_mut().push(thumb);
        Ok(())
    }

    fn undo(&self) {
        if self.busy.get() || self.stitch.borrow().frames.is_empty() {
            return;
        }
        self.stitch.borrow_mut().pop();
        self.thumbs.borrow_mut().pop();
        self.note("Removed the newest frame.");
    }

    fn accept(&self) {
        if !self.busy.get() {
            self.stitch.borrow_mut().confirm_last();
            self.note("Seam accepted.");
        }
    }

    /// Stitching runs on a worker over cheap `Arc` clones of the frames.
    fn finish(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        if let Err(error) = self.stitch.borrow().ready() {
            return self.note(&format!("{error:#}"));
        }
        self.busy.set(true);
        self.finishing.set(true);
        self.status.set_text("Stitching the final image…");
        self.sync();
        let stitch = self.stitch.borrow().clone();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(stitch.finish());
            });
            let result = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err(anyhow!("the stitching worker stopped unexpectedly")));
            ui.stitched(result);
        });
    }

    fn stitched(&self, result: Result<RgbaImage>) {
        if self.outcome.borrow().is_some() {
            return; // cancelled while stitching
        }
        match result {
            Ok(image) => self.quit(Ok(Some(image))),
            Err(error) => {
                self.busy.set(false);
                self.finishing.set(false);
                self.note(&format!("Cannot finish: {error:#}"));
            }
        }
    }

    /// Cancelling is allowed while stitching: the worker's result is then discarded.
    fn cancel(&self) {
        if !self.busy.get() || self.finishing.get() {
            self.quit(Ok(None));
        }
    }

    fn quit(&self, result: Result<Option<RgbaImage>>) {
        self.outcome.borrow_mut().get_or_insert(result);
        *self.stitch.borrow_mut() = Stitch::default();
        self.thumbs.borrow_mut().clear();
        *self.seam.borrow_mut() = SeamCache::default();
        self.window.destroy();
        self.app.quit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// Reproducible noise that differs on every row, like text and images do.
    fn content(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let mut v = u64::from(x / 6).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ u64::from(y).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
            v ^= v >> 29;
            v = v.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            v ^= v >> 32;
            Rgba([v as u8, (v >> 8) as u8, (v >> 16) as u8, 255])
        })
    }

    fn window(src: &RgbaImage, y: u32, h: u32) -> RgbaImage {
        imageops::crop_imm(src, 0, y, src.width(), h).to_image()
    }

    #[test]
    fn finds_the_exact_scroll_distance() {
        let page = content(256, 1200);
        let first = window(&page, 0, 400);
        for shift in [1, 37, 150, 300, 360] {
            assert_eq!(
                detect(&first, &window(&page, shift, 400)),
                Overlap::Match { shift },
                "shift {shift}"
            );
        }
    }

    #[test]
    fn identical_frames_mean_no_movement() {
        let first = window(&content(256, 800), 0, 400);
        assert_eq!(detect(&first, &first.clone()), Overlap::NoMovement);
    }

    #[test]
    fn upward_scroll_is_recognised() {
        let page = content(256, 1200);
        assert_eq!(
            detect(&window(&page, 150, 400), &window(&page, 0, 400)),
            Overlap::Upward
        );
    }

    #[test]
    fn too_little_overlap_or_unrelated_content_never_matches() {
        let page = content(256, 1600);
        let first = window(&page, 0, 400);
        for y in [380, 400, 900] {
            assert!(
                matches!(
                    detect(&first, &window(&page, y, 400)),
                    Overlap::Unmatched { .. }
                ),
                "scrolled by {y}"
            );
        }
        assert!(matches!(
            detect(&window(&page, 0, 20), &window(&page, 5, 20)),
            Overlap::Unmatched { .. }
        ));
    }

    #[test]
    fn repeating_content_is_ambiguous() {
        let tile = content(256, 24);
        let stripes = RgbaImage::from_fn(256, 800, |x, y| *tile.get_pixel(x, y % 24));
        // A 12 px scroll of period-24 content fits at 12, 36, 60 … px.
        assert!(matches!(
            detect(&window(&stripes, 0, 400), &window(&stripes, 12, 400)),
            Overlap::Ambiguous { .. }
        ));
    }

    #[test]
    fn small_overlays_do_not_prevent_a_match() {
        let page = content(256, 1200);
        let first = window(&page, 0, 400);
        let mut next = window(&page, 150, 400);
        for y in 0..8 {
            for x in 0..256 {
                next.put_pixel(x, y, Rgba([255, 255, 255, 255])); // sticky strip, 2% of rows
            }
        }
        assert_eq!(detect(&first, &next), Overlap::Match { shift: 150 });
    }

    #[test]
    fn a_big_sticky_header_is_not_matched_silently() {
        let page = content(256, 1200);
        let first = window(&page, 0, 400);
        let mut next = window(&page, 150, 400);
        for y in 0..120 {
            for x in 0..256 {
                next.put_pixel(x, y, *first.get_pixel(x, y));
            }
        }
        assert!(!matches!(detect(&first, &next), Overlap::Match { .. }));
    }

    fn stitched(page: &RgbaImage, h: u32, tops: &[u32]) -> Stitch {
        let mut s = Stitch::default();
        for (i, &top) in tops.iter().enumerate() {
            let shift = if i == 0 { 0 } else { top - tops[i - 1] };
            s.push(Arc::new(window(page, top, h)), shift, true).unwrap();
        }
        s
    }

    #[test]
    fn stitching_reproduces_the_scrolled_content() {
        let page = content(64, 1400);
        let tops = [0, 150, 320, 500, 640];
        let s = stitched(&page, 400, &tops);
        assert_eq!(s.height(), 1040);
        // Frames stay whole so every earlier seam remains adjustable after an undo.
        assert!(s.frames.iter().all(|f| f.img.height() == 400));
        assert_eq!(s.retained(), 5 * 64 * 400);
        assert!(retained_fits(MAX_RETAINED_PIXELS - 1, 1));
        assert!(!retained_fits(MAX_RETAINED_PIXELS, 1));
        assert_eq!(s.finish().unwrap(), window(&page, 0, 1040));
    }

    #[test]
    fn undo_and_seam_adjustment_stay_consistent() {
        let page = content(64, 1400);
        let mut s = stitched(&page, 400, &[0, 150, 320, 500]);
        assert_eq!(s.last_shift_range(), Some((1, 400)));
        s.set_last_shift(160).unwrap();
        assert_eq!(s.height(), 150 + 170 + 160 + 400);
        assert!(s.set_last_shift(0).is_err());
        assert!(s.set_last_shift(401).is_err());
        s.pop();
        assert_eq!(s.frames.len(), 3);
        assert_eq!(s.height(), 150 + 170 + 400);
        // Frames are kept whole, so undo restores the previous seam's full adjustment range.
        assert_eq!(s.last_shift_range(), Some((1, 400)));
        s.set_last_shift(390).unwrap();
        assert_eq!(s.height(), 150 + 390 + 400);
        s.pop();
        s.pop();
        s.pop();
        assert!(s.frames.is_empty() && s.last_shift_range().is_none());
        assert!(s.clone().finish().is_err());
    }

    #[test]
    fn unverified_frames_block_finishing_until_accepted() {
        let page = content(64, 1000);
        let mut s = stitched(&page, 400, &[0]);
        s.push(Arc::new(window(&page, 200, 400)), 200, false)
            .unwrap();
        assert!(s.pending() && s.clone().finish().is_err());
        s.confirm_last();
        assert_eq!(s.finish().unwrap(), window(&page, 0, 600));
    }

    #[test]
    fn invalid_frames_and_oversized_results_are_rejected() {
        let page = content(64, 600);
        let mut s = stitched(&page, 400, &[0]);
        assert!(
            s.push(Arc::new(window(&page, 0, 399)), 10, true).is_err(),
            "size change"
        );
        assert!(
            s.push(Arc::new(window(&page, 0, 400)), 0, true).is_err(),
            "zero shift"
        );
        assert!(
            s.push(Arc::new(window(&page, 0, 400)), 401, true).is_err(),
            "gap"
        );
        assert_eq!(s.frames.len(), 1);

        let mut tall = Stitch::default();
        let frame = Arc::new(RgbaImage::new(8, 1000));
        tall.push(frame.clone(), 0, true).unwrap();
        let mut error = None;
        for _ in 0..40 {
            if let Err(e) = tall.push(frame.clone(), 1000, true) {
                error = Some(e);
                break;
            }
        }
        assert!(error.is_some(), "height beyond {MAX_DIM} must be refused");
        assert!(tall.height() <= u64::from(MAX_DIM));
        assert!(check_frame(&RgbaImage::new(MAX_FRAME_SIDE + 1, 1)).is_err());
        assert!(check_frame(&RgbaImage::new(5000, 4000)).is_err());
    }

    #[test]
    fn seam_view_shows_both_sides_of_the_cut() {
        let page = content(32, 600);
        let (prev, next) = (window(&page, 0, 300), window(&page, 100, 300));
        let (view, row) = seam_view(&prev, &next, 100);
        assert_eq!((view.height(), row), (2 * SEAM_CONTEXT, SEAM_CONTEXT));
        assert_eq!(view, window(&page, 20, 160));
        let (short, row) = seam_view(&prev, &next, 10);
        assert_eq!((short.height(), row), (10 + SEAM_CONTEXT, 10));
    }

    #[test]
    fn seam_cache_follows_frame_identity_and_reuses_for_status_syncs() {
        let page = content(64, 1400);
        let mut s = stitched(&page, 400, &[0, 150]);
        let mut cache = SeamCache::default();
        assert!(cache.refresh(&s.frames[..1]).is_none());

        let first = cache.refresh(&s.frames).unwrap().0.to_raw_none();
        let same = cache.refresh(&s.frames).unwrap().0.to_raw_none();
        assert_eq!(first, same, "a status-only sync must reuse the surface");

        // Undo and recapture different pixels with the same frame count, size and shift.
        s.pop();
        let other = imageops::rotate180(&content(64, 1400));
        s.push(Arc::new(window(&other, 150, 400)), 150, true)
            .unwrap();
        let (surface, row) = cache.refresh(&s.frames).unwrap();
        let [prev, last] = s.frames.as_slice() else {
            unreachable!()
        };
        let (view, view_row) = seam_view(&prev.img, &last.img, last.shift);
        assert_eq!(*row, view_row);
        assert_eq!(
            crate::presentation::rgba_from(surface).unwrap(),
            crate::presentation::rgba_from(&surface_from(&view).unwrap()).unwrap()
        );
        assert_ne!(window(&page, 150, 400), *last.img);

        // A shift change on the same frames also rebuilds.
        let before = cache.refresh(&s.frames).unwrap().0.to_raw_none();
        s.set_last_shift(100).unwrap();
        let after = cache.refresh(&s.frames).unwrap().0.to_raw_none();
        assert_ne!(before, after);
    }
}
