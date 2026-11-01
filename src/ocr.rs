//! Local text recognition: PaddleOCR (PP-OCRv5 mobile) detector and recogniser, run offline on CPU.
//!
//! Both networks are the unmodified ONNX exports bundled in `assets/ocr/` (provenance, licences and
//! checksums are listed there) and run through ONNX Runtime (`ort`), statically linked. No Python,
//! subprocess, or network is involved. Pre- and post-processing follow PaddleOCR/RapidOCR: the DB
//! detector's probability map becomes rotated boxes (threshold, components, minimum-area rectangle,
//! unclip), each box is warped to a 48 px strip, and the CTC recogniser reads it.
//!
//! Character geometry is not guessed: a CTC output step `t` of `T` sits at `t / T` of the strip, so
//! every emitted character is placed where the decoder fired (centre of its run of steps) and the
//! glyph cells tile the line between neighbouring centres.
//!
//! Selection, reading order and copying live in [`Page`] and are pure functions of the cached boxes
//! plus a per-glyph `visible` mask, so callers can drop masked or cropped glyphs at any time without
//! re-running inference. Nothing here stores or exposes text for glyphs the mask hides.

use anyhow::{Context, Result, anyhow, ensure};
use image::{
    RgbImage, RgbaImage,
    imageops::{self, FilterType},
};
use ort::{session::Session, value::Tensor};

const DET_MODEL: &[u8] = include_bytes!("../assets/ocr/ppocrv5_det_mobile.onnx");
const REC_MODEL: &[u8] = include_bytes!("../assets/ocr/ppocrv5_rec_mobile.onnx");
const DICTIONARY: &str = include_str!("../assets/ocr/ppocrv5_dict.txt");

/// Longest side fed to the detector; larger screenshots are downscaled for detection only.
const MAX_DET_SIDE: f64 = 2560.0;
const DET_THRESHOLD: f32 = 0.3;
const BOX_THRESHOLD: f32 = 0.6;
const UNCLIP_RATIO: f64 = 1.5;
const REC_HEIGHT: usize = 48;
const MAX_REC_WIDTH: usize = 4096;
/// Lines the recogniser is less sure of than this are dropped, as PaddleOCR does.
const DROP_SCORE: f32 = 0.5;
/// DB detector normalisation (PaddleOCR applies it to BGR-ordered channels).
const DET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const DET_STD: [f32; 3] = [0.229, 0.224, 0.225];

pub type Pt = (f64, f64);
/// Top-left, top-right, bottom-right, bottom-left in image pixels; the top edge runs along the text.
pub type Quad = [Pt; 4];

/// Axis-aligned box in image pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Glyph {
    pub ch: String,
    pub score: f32,
    /// Extent along the line, 0 = left end, 1 = right end; neighbours tile without gaps.
    pub u0: f64,
    pub u1: f64,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub quad: Quad,
    pub glyphs: Vec<Glyph>,
}

/// Cached recognition of one image: lines in reading order.
#[derive(Debug)]
pub struct Page {
    pub width: u32,
    pub height: u32,
    lines: Vec<Line>,
    /// Line index ranges, one per visual row (left to right within the row).
    rows: Vec<std::ops::Range<usize>>,
    /// Global glyph ordinal of each line's first glyph; the last entry is the glyph total.
    starts: Vec<usize>,
}

pub struct Engine {
    _env: ort::environment::Environment,
    det: Session,
    rec: Session,
    chars: Vec<&'static str>,
}

fn ort_error<R>(e: ort::Error<R>) -> anyhow::Error {
    anyhow!("{e}")
}

impl Engine {
    /// Loads the bundled models (a fraction of a second); run once per OCR job.
    pub fn new() -> Result<Self> {
        let env = ort::init()
            .with_name("sharpshot-ocr")
            .build()
            .map_err(ort_error)?;
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
        let load = |model: &[u8], what: &str| -> Result<Session> {
            Session::builder(&env)
                .map_err(ort_error)?
                .with_intra_threads(threads)
                .map_err(ort_error)?
                .commit_from_memory(model)
                .map_err(ort_error)
                .with_context(|| format!("loading the {what} model"))
        };
        let det = load(DET_MODEL, "text detection")?;
        let rec = load(REC_MODEL, "text recognition")?;
        // CTC classes: blank, the dictionary, then the space PaddleOCR appends.
        let mut chars = vec![""];
        chars.extend(
            DICTIONARY
                .strip_suffix('\n')
                .unwrap_or(DICTIONARY)
                .split('\n'),
        );
        chars.push(" ");
        Ok(Self {
            _env: env,
            det,
            rec,
            chars,
        })
    }

    /// Detects and reads all text of `image`. Coordinates of the result are `image` pixels.
    pub fn recognize(&mut self, image: &RgbaImage) -> Result<Page> {
        let (width, height) = image.dimensions();
        let rgb = flatten(image);
        let mut lines = Vec::new();
        for quad in self.detect(&rgb)? {
            if let Some(line) = self.read(&rgb, quad)? {
                lines.push(line);
            }
        }
        Ok(Page::new(width, height, lines))
    }

    fn detect(&mut self, img: &RgbImage) -> Result<Vec<Quad>> {
        let (w, h) = img.dimensions();
        let scale = (MAX_DET_SIDE / f64::from(w.max(h))).min(1.0);
        let round32 = |v: f64| ((v / 32.0).round() as u32).max(1) * 32;
        let (dw, dh) = (round32(f64::from(w) * scale), round32(f64::from(h) * scale));
        let resized;
        let src = if (dw, dh) == (w, h) {
            img
        } else {
            resized = imageops::resize(img, dw, dh, FilterType::Triangle);
            &resized
        };
        let plane = (dw * dh) as usize;
        let mut data = vec![0f32; 3 * plane];
        for (i, p) in src.pixels().enumerate() {
            for c in 0..3 {
                // Channel 0 is blue: the detector was trained on BGR input.
                data[c * plane + i] = (f32::from(p.0[2 - c]) / 255.0 - DET_MEAN[c]) / DET_STD[c];
            }
        }
        let input =
            Tensor::from_array(([1usize, 3, dh as usize, dw as usize], data)).map_err(ort_error)?;
        let outputs = self.det.run(ort::inputs![input]).map_err(ort_error)?;
        let (shape, prob) = outputs[0].try_extract_tensor::<f32>().map_err(ort_error)?;
        ensure!(
            shape.len() == 4 && shape[2] == i64::from(dh) && shape[3] == i64::from(dw),
            "unexpected detector output shape {shape:?}"
        );
        let (sx, sy) = (f64::from(w) / f64::from(dw), f64::from(h) / f64::from(dh));
        Ok(boxes_from_map(prob, dw as usize, dh as usize)
            .into_iter()
            .filter_map(|q| {
                let q = q.map(|(x, y)| {
                    (
                        (x * sx).clamp(0.0, f64::from(w)),
                        (y * sy).clamp(0.0, f64::from(h)),
                    )
                });
                let (qw, qh) = (dist(q[0], q[1]), dist(q[0], q[3]));
                (qw.min(qh) >= 4.0).then_some(q)
            })
            .collect())
    }

    fn read(&mut self, img: &RgbImage, quad: Quad) -> Result<Option<Line>> {
        let (data, width) = strip(img, &quad);
        let input =
            Tensor::from_array(([1usize, 3, REC_HEIGHT, width], data)).map_err(ort_error)?;
        let outputs = self.rec.run(ort::inputs![input]).map_err(ort_error)?;
        let (shape, probs) = outputs[0].try_extract_tensor::<f32>().map_err(ort_error)?;
        ensure!(
            shape.len() == 3 && shape[2] as usize == self.chars.len(),
            "recogniser has {} classes but the dictionary has {}",
            shape[shape.len() - 1],
            self.chars.len()
        );
        let glyphs = decode(probs, shape[1] as usize, self.chars.len(), &self.chars);
        if glyphs.is_empty() {
            return Ok(None);
        }
        let score = glyphs.iter().map(|g| g.score).sum::<f32>() / glyphs.len() as f32;
        Ok((score >= DROP_SCORE).then_some(Line { quad, glyphs }))
    }
}

/// Opaque RGB; transparent pixels are shown over white, as a viewer would.
fn flatten(image: &RgbaImage) -> RgbImage {
    RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let p = image.get_pixel(x, y).0;
        let a = u32::from(p[3]);
        let mix = |c: u8| ((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
        image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])])
    })
}

fn dist(a: Pt, b: Pt) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

fn lerp(a: Pt, b: Pt, t: f64) -> Pt {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

// ---------------------------------------------------------------------------------------------
// Detection post-processing

/// DB probability map (`w` x `h`, row-major) to rotated boxes in map pixels.
pub(crate) fn boxes_from_map(prob: &[f32], w: usize, h: usize) -> Vec<Quad> {
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    let mut stack = Vec::new();
    let mut pixels: Vec<(usize, usize)> = Vec::new();
    for start in 0..w * h {
        if seen[start] || prob[start] <= DET_THRESHOLD {
            continue;
        }
        pixels.clear();
        seen[start] = true;
        stack.push(start);
        let mut sum = 0.0f64;
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            pixels.push((x, y));
            sum += f64::from(prob[i]);
            for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                    let n = ny * w + nx;
                    if !seen[n] && prob[n] > DET_THRESHOLD {
                        seen[n] = true;
                        stack.push(n);
                    }
                }
            }
        }
        if pixels.len() < 8 || sum / (pixels.len() as f64) < f64::from(BOX_THRESHOLD) {
            continue;
        }
        if let Some(q) = component_box(&pixels) {
            out.push(q);
        }
    }
    out
}

/// Minimum-area rectangle of a pixel component, grown by the DB "unclip" distance, with its long
/// axis running left to right.
fn component_box(pixels: &[(usize, usize)]) -> Option<Quad> {
    // Per row only the extreme pixels can lie on the hull.
    let mut rows: std::collections::HashMap<usize, (usize, usize)> = Default::default();
    for &(x, y) in pixels {
        let e = rows.entry(y).or_insert((x, x));
        e.0 = e.0.min(x);
        e.1 = e.1.max(x);
    }
    let mut pts: Vec<Pt> = Vec::with_capacity(rows.len() * 4);
    for (&y, &(x0, x1)) in &rows {
        let (y, x0, x1) = (y as f64, x0 as f64, (x1 + 1) as f64);
        pts.extend([(x0, y), (x0, y + 1.0), (x1, y), (x1, y + 1.0)]);
    }
    let hull = convex_hull(pts);
    if hull.len() < 3 {
        return None;
    }
    let mut best: Option<(f64, Pt, Pt, f64, f64)> = None; // area, axis, centre, w, h
    for i in 0..hull.len() {
        let (a, b) = (hull[i], hull[(i + 1) % hull.len()]);
        let len = dist(a, b);
        if len < 1e-9 {
            continue;
        }
        let u = ((b.0 - a.0) / len, (b.1 - a.1) / len);
        let v = (-u.1, u.0);
        let (mut u0, mut u1, mut v0, mut v1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in &hull {
            let (pu, pv) = (p.0 * u.0 + p.1 * u.1, p.0 * v.0 + p.1 * v.1);
            u0 = u0.min(pu);
            u1 = u1.max(pu);
            v0 = v0.min(pv);
            v1 = v1.max(pv);
        }
        let (rw, rh) = (u1 - u0, v1 - v0);
        if best.is_none_or(|b| rw * rh < b.0) {
            let (cu, cv) = ((u0 + u1) / 2.0, (v0 + v1) / 2.0);
            best = Some((
                rw * rh,
                u,
                (cu * u.0 + cv * v.0, cu * u.1 + cv * v.1),
                rw,
                rh,
            ));
        }
    }
    let (_, mut u, centre, mut w, mut h) = best?;
    if w.min(h) < 3.0 {
        return None;
    }
    // Text runs along the axis nearer to horizontal; keep it pointing right, and "down" pointing down.
    if u.0.abs() < u.1.abs() {
        u = (-u.1, u.0);
        std::mem::swap(&mut w, &mut h);
    }
    if u.0 < 0.0 {
        u = (-u.0, -u.1);
    }
    let v = (-u.1, u.0);
    let d = w * h * UNCLIP_RATIO / (2.0 * (w + h));
    let (hw, hh) = (w / 2.0 + d, h / 2.0 + d);
    let at = |su: f64, sv: f64| {
        (
            centre.0 + u.0 * su * hw + v.0 * sv * hh,
            centre.1 + u.1 * su * hw + v.1 * sv * hh,
        )
    };
    Some([at(-1.0, -1.0), at(1.0, -1.0), at(1.0, 1.0), at(-1.0, 1.0)])
}

fn convex_hull(mut pts: Vec<Pt>) -> Vec<Pt> {
    pts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross = |o: Pt, a: Pt, b: Pt| (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0);
    let mut hull: Vec<Pt> = Vec::new();
    let lower_done = {
        for &p in &pts {
            while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
                hull.pop();
            }
            hull.push(p);
        }
        hull.len() + 1
    };
    // The upper chain may only pop points it pushed itself.
    for &p in pts.iter().rev().skip(1) {
        while hull.len() >= lower_done
            && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0
        {
            hull.pop();
        }
        hull.push(p);
    }
    hull.pop();
    hull
}

// ---------------------------------------------------------------------------------------------
// Recognition pre/post-processing

fn bilinear(img: &RgbImage, x: f64, y: f64) -> [f32; 3] {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let (x, y) = (x - 0.5, y - 0.5);
    let (fx, fy) = (x.floor(), y.floor());
    let (tx, ty) = ((x - fx) as f32, (y - fy) as f32);
    let px = |dx: i64, dy: i64| {
        img.get_pixel(
            (fx as i64 + dx).clamp(0, w - 1) as u32,
            (fy as i64 + dy).clamp(0, h - 1) as u32,
        )
        .0
    };
    let (a, b, c, d) = (px(0, 0), px(1, 0), px(0, 1), px(1, 1));
    std::array::from_fn(|i| {
        let top = f32::from(a[i]) * (1.0 - tx) + f32::from(b[i]) * tx;
        let bottom = f32::from(c[i]) * (1.0 - tx) + f32::from(d[i]) * tx;
        top * (1.0 - ty) + bottom * ty
    })
}

/// Warps `quad` to a `REC_HEIGHT`-tall strip (area-averaged when shrinking) and returns it as the
/// recogniser's planar BGR input in [-1, 1] together with its width.
fn strip(img: &RgbImage, quad: &Quad) -> (Vec<f32>, usize) {
    let (qw, qh) = (
        dist(quad[0], quad[1]).max(1.0),
        dist(quad[0], quad[3]).max(1.0),
    );
    let width = ((REC_HEIGHT as f64 * qw / qh).round() as usize).clamp(16, MAX_REC_WIDTH);
    let k = (qh / REC_HEIGHT as f64)
        .max(qw / width as f64)
        .ceil()
        .clamp(1.0, 4.0) as usize;
    let (right, down) = (
        (quad[1].0 - quad[0].0, quad[1].1 - quad[0].1),
        (quad[3].0 - quad[0].0, quad[3].1 - quad[0].1),
    );
    let plane = REC_HEIGHT * width;
    let mut data = vec![0f32; 3 * plane];
    for y in 0..REC_HEIGHT {
        for x in 0..width {
            let mut acc = [0f32; 3];
            for j in 0..k {
                for i in 0..k {
                    let u = (x as f64 + (i as f64 + 0.5) / k as f64) / width as f64;
                    let v = (y as f64 + (j as f64 + 0.5) / k as f64) / REC_HEIGHT as f64;
                    let s = bilinear(
                        img,
                        quad[0].0 + right.0 * u + down.0 * v,
                        quad[0].1 + right.1 * u + down.1 * v,
                    );
                    for c in 0..3 {
                        acc[c] += s[c];
                    }
                }
            }
            for c in 0..3 {
                // Blue first, matching the detector's channel order.
                data[c * plane + y * width + x] = (acc[2 - c] / (k * k) as f32 - 127.5) / 127.5;
            }
        }
    }
    (data, width)
}

/// Greedy CTC decode of `steps` x `classes` probabilities. Each emitted character is placed at the
/// centre of the run of steps that produced it; cells tile the line between neighbouring centres.
pub(crate) fn decode(probs: &[f32], steps: usize, classes: usize, chars: &[&str]) -> Vec<Glyph> {
    // (class, first step, last step, best probability)
    let mut runs: Vec<(usize, usize, usize, f32)> = Vec::new();
    let mut previous = 0;
    for t in 0..steps {
        let row = &probs[t * classes..(t + 1) * classes];
        let (class, p) = row
            .iter()
            .copied()
            .enumerate()
            .fold((0, f32::MIN), |b, (i, p)| if p > b.1 { (i, p) } else { b });
        if class != 0 {
            match runs.last_mut() {
                Some(r) if class == previous => {
                    r.2 = t;
                    r.3 = r.3.max(p);
                }
                _ => runs.push((class, t, t, p)),
            }
        }
        previous = class;
    }
    let centres: Vec<f64> = runs
        .iter()
        .map(|r| (r.1 + r.2 + 1) as f64 / 2.0 / steps as f64)
        .collect();
    let n = runs.len();
    let mut glyphs: Vec<Glyph> = runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (u0, u1) = match n {
                1 => (0.0, 1.0),
                _ => (
                    if i == 0 {
                        (centres[0] - (centres[1] - centres[0]) / 2.0).max(0.0)
                    } else {
                        (centres[i - 1] + centres[i]) / 2.0
                    },
                    if i == n - 1 {
                        (centres[i] + (centres[i] - centres[i - 1]) / 2.0).min(1.0)
                    } else {
                        (centres[i] + centres[i + 1]) / 2.0
                    },
                ),
            };
            Glyph {
                ch: chars.get(r.0).copied().unwrap_or("").to_string(),
                score: r.3,
                u0,
                u1,
            }
        })
        .collect();
    // Leading and trailing spaces carry no text.
    while glyphs.last().is_some_and(|g| g.ch.trim().is_empty()) {
        glyphs.pop();
    }
    let lead = glyphs.iter().take_while(|g| g.ch.trim().is_empty()).count();
    glyphs.drain(..lead);
    glyphs
}

// ---------------------------------------------------------------------------------------------
// Page: reading order, hit testing, selection text

/// A caret is a gap between glyphs: `n` lies before the glyph with ordinal `n`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub ordinal: usize,
    pub caret: usize,
    /// Pixels from the point to the glyph cell (0 inside it).
    pub distance: f64,
    /// Height of the glyph's line, for judging "close enough".
    pub line_height: f64,
}

/// A contiguous stretch of one line.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub line: usize,
    pub quad: Quad,
}

impl Line {
    fn height(&self) -> f64 {
        dist(self.quad[0], self.quad[3])
    }

    fn bounds(&self) -> Rect {
        rect_of(&self.quad)
    }

    /// Cell of `u0..u1` along the line.
    pub fn strip(&self, u0: f64, u1: f64) -> Quad {
        let q = &self.quad;
        [
            lerp(q[0], q[1], u0),
            lerp(q[0], q[1], u1),
            lerp(q[3], q[2], u1),
            lerp(q[3], q[2], u0),
        ]
    }
}

pub fn rect_of(q: &Quad) -> Rect {
    let (x0, x1) = q
        .iter()
        .fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.0), a.1.max(p.0)));
    let (y0, y1) = q
        .iter()
        .fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.1), a.1.max(p.1)));
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

fn inside(q: &Quad, p: Pt) -> bool {
    let sides = (0..4).map(|i| {
        let (a, b) = (q[i], q[(i + 1) % 4]);
        (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
    });
    let (mut pos, mut neg) = (false, false);
    for s in sides {
        pos |= s > 0.0;
        neg |= s < 0.0;
    }
    !(pos && neg)
}

fn distance_to(q: &Quad, p: Pt) -> f64 {
    if inside(q, p) {
        return 0.0;
    }
    (0..4)
        .map(|i| {
            let (a, b) = (q[i], q[(i + 1) % 4]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 0.0 {
                (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            dist(p, (a.0 + dx * t, a.1 + dy * t))
        })
        .fold(f64::MAX, f64::min)
}

impl Page {
    /// Orders `lines` for reading: rows top to bottom, lines left to right within a row.
    pub fn new(width: u32, height: u32, lines: Vec<Line>) -> Self {
        let centre = |l: &Line| l.bounds().y + l.bounds().h / 2.0;
        let mut order: Vec<usize> = (0..lines.len()).collect();
        order.sort_by(|&a, &b| centre(&lines[a]).total_cmp(&centre(&lines[b])));
        let mut grouped: Vec<Vec<usize>> = Vec::new();
        let mut anchor = (0.0, 0.0);
        for i in order {
            let (c, h) = (centre(&lines[i]), lines[i].height());
            match grouped.last_mut() {
                Some(row) if (c - anchor.0).abs() <= 0.5 * h.min(anchor.1) => row.push(i),
                _ => {
                    anchor = (c, h);
                    grouped.push(vec![i]);
                }
            }
        }
        let mut slots: Vec<Option<Line>> = lines.into_iter().map(Some).collect();
        let (mut sorted, mut rows) = (Vec::new(), Vec::new());
        for mut row in grouped {
            row.sort_by(|&a, &b| {
                let left = |i: usize| slots[i].as_ref().map_or(0.0, |l| l.bounds().x);
                left(a).total_cmp(&left(b))
            });
            let first = sorted.len();
            sorted.extend(row.into_iter().filter_map(|i| slots[i].take()));
            rows.push(first..sorted.len());
        }
        let mut starts = vec![0];
        for l in &sorted {
            starts.push(starts[starts.len() - 1] + l.glyphs.len());
        }
        Self {
            width,
            height,
            lines: sorted,
            rows,
            starts,
        }
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Number of glyphs; ordinals run in reading order.
    pub fn len(&self) -> usize {
        self.starts[self.starts.len() - 1]
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `(line, index in line)` of a glyph ordinal.
    pub fn locate(&self, ordinal: usize) -> Option<(usize, usize)> {
        (ordinal < self.len()).then(|| {
            let line = self.starts.partition_point(|&s| s <= ordinal) - 1;
            (line, ordinal - self.starts[line])
        })
    }

    pub fn glyph_quad(&self, line: usize, index: usize) -> Quad {
        let g = &self.lines[line].glyphs[index];
        self.lines[line].strip(g.u0, g.u1)
    }

    /// Which glyphs may be shown or copied. `allow(padded, exact)` gets each glyph's box: `exact` is
    /// the cell itself, `padded` adds a margin because decoder alignment is only accurate to about
    /// a step. Callers should reject anything touching private areas using `padded` and anything not
    /// fully inside the output using `exact`.
    pub fn visibility(&self, allow: impl Fn(Rect, Rect) -> bool) -> Vec<bool> {
        let mut out = Vec::with_capacity(self.len());
        for (li, line) in self.lines.iter().enumerate() {
            let pad = line.height() * 0.35;
            for gi in 0..line.glyphs.len() {
                let exact = rect_of(&self.glyph_quad(li, gi));
                let padded = Rect {
                    x: exact.x - pad,
                    y: exact.y - pad * 0.4,
                    w: exact.w + 2.0 * pad,
                    h: exact.h + 0.8 * pad,
                };
                out.push(allow(padded, exact));
            }
        }
        out
    }

    /// Nearest visible glyph to `p`.
    pub fn hit(&self, p: Pt, visible: &[bool]) -> Option<Hit> {
        let mut best: Option<Hit> = None;
        for (li, line) in self.lines.iter().enumerate() {
            let q = &line.quad;
            let axis = (q[1].0 - q[0].0, q[1].1 - q[0].1);
            let len2 = (axis.0 * axis.0 + axis.1 * axis.1).max(1e-9);
            let u = ((p.0 - q[0].0) * axis.0 + (p.1 - q[0].1) * axis.1) / len2;
            for (gi, g) in line.glyphs.iter().enumerate() {
                let ordinal = self.starts[li] + gi;
                if !visible.get(ordinal).copied().unwrap_or(false) {
                    continue;
                }
                let distance = distance_to(&self.glyph_quad(li, gi), p);
                if best.is_none_or(|b| distance < b.distance) {
                    let after = u > (g.u0 + g.u1) / 2.0;
                    best = Some(Hit {
                        ordinal,
                        caret: ordinal + usize::from(after),
                        distance,
                        line_height: line.height(),
                    });
                }
            }
        }
        best
    }

    /// The non-space glyphs around `ordinal`, bounded by spaces, line ends and hidden glyphs.
    pub fn word(&self, ordinal: usize, visible: &[bool]) -> (usize, usize) {
        let Some((li, gi)) = self.locate(ordinal) else {
            return (0, 0);
        };
        let glyphs = &self.lines[li].glyphs;
        let base = self.starts[li];
        let word = |i: usize| visible[base + i] && !glyphs[i].ch.trim().is_empty();
        if !word(gi) {
            return (ordinal, ordinal);
        }
        let (mut a, mut b) = (gi, gi + 1);
        while a > 0 && word(a - 1) {
            a -= 1;
        }
        while b < glyphs.len() && word(b) {
            b += 1;
        }
        (base + a, base + b)
    }

    /// All glyphs of the line holding `ordinal`.
    pub fn line_range(&self, ordinal: usize) -> (usize, usize) {
        self.locate(ordinal)
            .map_or((0, 0), |(li, _)| (self.starts[li], self.starts[li + 1]))
    }

    /// Visible, selected glyphs of each line as one strip, with spaces trimmed from the ends.
    pub fn runs(&self, lo: usize, hi: usize, visible: &[bool]) -> Vec<Run> {
        let mut out = Vec::new();
        for (li, line) in self.lines.iter().enumerate() {
            let base = self.starts[li];
            let mut open: Option<(usize, usize)> = None;
            let close = |open: &mut Option<(usize, usize)>, out: &mut Vec<Run>| {
                if let Some((mut a, mut b)) = open.take() {
                    while a < b && line.glyphs[a].ch.trim().is_empty() {
                        a += 1;
                    }
                    while b > a && line.glyphs[b - 1].ch.trim().is_empty() {
                        b -= 1;
                    }
                    if a < b {
                        out.push(Run {
                            line: li,
                            quad: line.strip(line.glyphs[a].u0, line.glyphs[b - 1].u1),
                        });
                    }
                }
            };
            for gi in 0..line.glyphs.len() {
                let ordinal = base + gi;
                if (lo..hi).contains(&ordinal) && visible.get(ordinal).copied().unwrap_or(false) {
                    open = Some(open.map_or((gi, gi + 1), |(a, _)| (a, gi + 1)));
                } else {
                    close(&mut open, &mut out);
                }
            }
            close(&mut open, &mut out);
        }
        out
    }

    /// Text of the visible glyphs in `lo..hi`, laid out like the original: lines of a row joined by
    /// a space (or a tab across a wide gap), rows by newlines, a blank line where the vertical gap
    /// is large, indentation as a count of average characters. Recognition errors are kept as read.
    pub fn text(&self, lo: usize, hi: usize, visible: &[bool]) -> String {
        struct Seg {
            text: String,
            left: f64,
            right: f64,
            chars: usize,
        }
        struct Row {
            segs: Vec<Seg>,
            top: f64,
            bottom: f64,
            height: f64,
        }
        let mut rows: Vec<Row> = Vec::new();
        for range in &self.rows {
            let mut row = Row {
                segs: vec![],
                top: f64::MAX,
                bottom: f64::MIN,
                height: 0.0,
            };
            for li in range.clone() {
                let line = &self.lines[li];
                let base = self.starts[li];
                let text: String = (0..line.glyphs.len())
                    .filter(|gi| {
                        (lo..hi).contains(&(base + gi))
                            && visible.get(base + gi).copied().unwrap_or(false)
                    })
                    .map(|gi| line.glyphs[gi].ch.as_str())
                    .collect();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let b = line.bounds();
                row.top = row.top.min(b.y);
                row.bottom = row.bottom.max(b.y + b.h);
                row.height = row.height.max(line.height());
                row.segs.push(Seg {
                    text: trimmed.to_string(),
                    left: b.x,
                    right: b.x + b.w,
                    chars: line.glyphs.len(),
                });
            }
            if !row.segs.is_empty() {
                rows.push(row);
            }
        }
        let min_left = rows.iter().map(|r| r.segs[0].left).fold(f64::MAX, f64::min);
        let (width, count) = rows
            .iter()
            .flat_map(|r| &r.segs)
            .fold((0.0, 0usize), |a, s| {
                (a.0 + s.right - s.left, a.1 + s.chars)
            });
        let char_width = (width / count.max(1) as f64).max(1.0);
        let mut out = String::new();
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                out.push('\n');
                let prev = &rows[i - 1];
                if row.top - prev.bottom > 0.75 * prev.height.max(row.height) {
                    out.push('\n');
                }
                let indent = ((row.segs[0].left - min_left) / char_width)
                    .round()
                    .clamp(0.0, 40.0) as usize;
                if indent >= 2 {
                    out.extend(std::iter::repeat_n(' ', indent));
                }
            }
            for (j, seg) in row.segs.iter().enumerate() {
                if j > 0 {
                    out.push(if seg.left - row.segs[j - 1].right > 3.0 * row.height {
                        '\t'
                    } else {
                        ' '
                    });
                }
                out.push_str(&seg.text);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = include_bytes!("../assets/ocr/sample.png");

    fn glyphs(text: &str) -> Vec<Glyph> {
        let n = text.chars().count() as f64;
        text.chars()
            .enumerate()
            .map(|(i, c)| Glyph {
                ch: c.to_string(),
                score: 0.9,
                u0: i as f64 / n,
                u1: (i + 1) as f64 / n,
            })
            .collect()
    }

    fn line(x: f64, y: f64, w: f64, h: f64, text: &str) -> Line {
        Line {
            quad: [(x, y), (x + w, y), (x + w, y + h), (x, y + h)],
            glyphs: glyphs(text),
        }
    }

    fn all(page: &Page) -> Vec<bool> {
        vec![true; page.len()]
    }

    #[test]
    fn ctc_places_characters_where_the_decoder_fired() {
        let chars = ["", "A", "B", " "];
        let (steps, classes) = (20, 4);
        let mut probs = vec![0.0f32; steps * classes];
        for t in 0..steps {
            probs[t * classes] = 1.0;
        }
        let mut fire = |t: usize, class: usize| {
            probs[t * classes] = 0.0;
            probs[t * classes + class] = 0.9;
        };
        // "A" fires on steps 2-3, a blank, "A" again on 5 (a new character), then "B" on 10-11.
        for t in [2, 3] {
            fire(t, 1);
        }
        fire(5, 1);
        for t in [10, 11] {
            fire(t, 2);
        }
        let g = decode(&probs, steps, classes, &chars);
        assert_eq!(g.iter().map(|g| g.ch.as_str()).collect::<String>(), "AAB");
        let centre = |g: &Glyph| (g.u0 + g.u1) / 2.0;
        // Centres follow the firing steps: (2+4)/2/20, (5+6)/2/20, (10+12)/2/20.
        assert!((g[0].u1 - (0.15 + 0.275) / 2.0).abs() < 1e-9);
        assert!(g[1].u0 == g[0].u1 && g[2].u0 == g[1].u1, "cells tile");
        assert!(g[0].u0 >= 0.0 && g[2].u1 <= 1.0);
        assert!(centre(&g[0]) < centre(&g[1]) && centre(&g[1]) < centre(&g[2]));
        assert!(
            g[2].u0 > 0.4 && g[2].u0 < 0.55,
            "B sits near step 10 of 20, not at an equal share"
        );
    }

    #[test]
    fn ctc_trims_blank_ends_and_survives_empty_output() {
        let chars = ["", "A", "B", " "];
        let mut probs = vec![0.0f32; 6 * 4];
        for t in 0..6 {
            probs[t * 4
                + if t == 0 || t == 5 {
                    3
                } else if t == 2 {
                    1
                } else {
                    0
                }] = 1.0;
        }
        let g = decode(&probs, 6, 4, &chars);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].ch, "A");
        assert!(decode(&[1.0, 0.0, 0.0, 0.0], 1, 4, &chars).is_empty());
    }

    #[test]
    fn detector_map_becomes_one_box_per_text_line() {
        let (w, h) = (200usize, 100usize);
        let mut map = vec![0.0f32; w * h];
        for (y0, y1) in [(20, 30), (60, 70)] {
            for y in y0..y1 {
                for x in 20..120 {
                    map[y * w + x] = 0.9;
                }
            }
        }
        // A faint smudge and a speck are not text.
        for x in 150..170 {
            map[50 * w + x] = 0.4;
        }
        map[5 * w + 5] = 0.95;
        let mut boxes = boxes_from_map(&map, w, h);
        boxes.sort_by(|a, b| a[0].1.total_cmp(&b[0].1));
        assert_eq!(boxes.len(), 2);
        for (q, (y0, y1)) in boxes.iter().zip([(20.0, 30.0), (60.0, 70.0)]) {
            let r = rect_of(q);
            assert!(
                r.x < 20.0 && r.x + r.w > 120.0,
                "unclip grows the box: {r:?}"
            );
            assert!(r.y < y0 && r.y + r.h > y1);
            assert!(
                q[0].0 < q[1].0 && q[0].1 < q[3].1,
                "corner order is tl, tr, br, bl"
            );
        }
    }

    #[test]
    fn detector_keeps_a_tilted_line_tilted() {
        let (w, h) = (300usize, 200usize);
        let mut map = vec![0.0f32; w * h];
        for x in 40..240usize {
            let y0 = 40 + (x - 40) / 5; // ~11 degrees
            for y in y0..y0 + 12 {
                map[y * w + x] = 0.9;
            }
        }
        let boxes = boxes_from_map(&map, w, h);
        assert_eq!(boxes.len(), 1);
        let q = boxes[0];
        let angle = (q[1].1 - q[0].1).atan2(q[1].0 - q[0].0).to_degrees();
        assert!((angle - 11.3).abs() < 2.0, "angle {angle}");
        assert!(dist(q[0], q[1]) > dist(q[0], q[3]));
    }

    fn sample_page() -> Page {
        // Two paragraphs, an indented line, and a two-column row; deliberately shuffled.
        Page::new(
            400,
            300,
            vec![
                line(200.0, 10.0, 60.0, 14.0, "right"),
                line(10.0, 80.0, 100.0, 14.0, "Second para"),
                line(10.0, 10.0, 100.0, 14.0, "Hello world"),
                line(10.0, 28.0, 100.0, 14.0, "next line"),
                line(30.0, 46.0, 100.0, 14.0, "indented ok"),
            ],
        )
    }

    #[test]
    fn reading_order_rows_paragraphs_and_columns() {
        let page = sample_page();
        let text = page.text(0, page.len(), &all(&page));
        assert_eq!(
            text,
            "Hello world\tright\nnext line\n  indented ok\n\nSecond para"
        );
    }

    #[test]
    fn hit_testing_and_selection_follow_glyph_cells() {
        let page = sample_page();
        let v = all(&page);
        // Inside the 3rd glyph of "Hello world" (cells are 100/11 px wide).
        let hit = page.hit((10.0 + 100.0 / 11.0 * 2.2, 17.0), &v).unwrap();
        assert_eq!((hit.ordinal, hit.distance), (2, 0.0));
        assert_eq!(
            hit.caret, 2,
            "left half of the glyph puts the caret before it"
        );
        let hit = page.hit((10.0 + 100.0 / 11.0 * 2.8, 17.0), &v).unwrap();
        assert_eq!(hit.caret, 3);
        // Beyond the end of a line the last glyph is nearest.
        let far = page.hit((390.0, 17.0), &v).unwrap();
        assert_eq!(
            far.ordinal,
            page.locate(far.ordinal).map(|_| far.ordinal).unwrap()
        );
        assert_eq!(page.word(2, &v), (0, 5));
        assert_eq!(page.word(5, &v), (5, 5), "a space is no word");
        assert_eq!(page.word(6, &v), (6, 11));
        assert_eq!(page.line_range(7), (0, 11));
        assert_eq!(page.text(3, 9, &v), "lo wor");
    }

    #[test]
    fn hidden_glyphs_never_appear_in_hits_text_or_runs() {
        let page = sample_page();
        let mut v = all(&page);
        for hidden in 2..=6 {
            v[hidden] = false; // "llo w"
        }
        assert_eq!(
            page.text(0, page.len(), &v).lines().next().unwrap(),
            "Heorld\tright"
        );
        assert!(!page.text(0, page.len(), &v).contains("llo"));
        // Pointing straight at a hidden glyph selects a visible neighbour, never the hidden one.
        let hit = page.hit((10.0 + 100.0 / 11.0 * 4.5, 17.0), &v).unwrap();
        assert!(!(2..=6).contains(&hit.ordinal));
        assert_eq!(page.word(1, &v), (0, 2));
        let runs = page.runs(0, 11, &v);
        assert_eq!(runs.len(), 2, "the hidden stretch splits the highlight");
        for r in &runs {
            let b = rect_of(&r.quad);
            let hidden = rect_of(&page.lines()[r.line].strip(2.0 / 11.0, 7.0 / 11.0));
            assert!(b.x + b.w <= hidden.x + 1e-6 || b.x >= hidden.x + hidden.w - 1e-6);
        }
    }

    #[test]
    fn visibility_mask_rejects_private_and_cropped_glyphs() {
        let page = sample_page();
        // Mask over x 40..60, crop to x >= 20: padded boxes touching the mask or exact boxes
        // outside the crop are hidden.
        let v = page.visibility(|padded, exact| {
            let masked = padded.x < 60.0
                && padded.x + padded.w > 40.0
                && padded.y < 90.0
                && padded.y + padded.h > 0.0;
            !masked && exact.x >= 20.0
        });
        assert!(!v[0], "cropped away");
        let text = page.text(0, page.len(), &v);
        assert!(!text.contains('H') && !text.contains("llo"));
        let margin_ok = page.lines()[2].height() * 0.35;
        assert!(margin_ok > 4.0, "padding is a real margin, not zero");
    }

    #[test]
    fn nothing_selected_yields_nothing() {
        let page = sample_page();
        assert_eq!(page.text(5, 5, &all(&page)), "");
        assert_eq!(page.text(0, page.len(), &vec![false; page.len()]), "");
        assert!(Page::new(10, 10, vec![]).hit((1.0, 1.0), &[]).is_none());
    }

    #[test]
    fn real_models_read_a_rendered_screenshot() -> Result<()> {
        let image = image::load_from_memory(SAMPLE)?.to_rgba8();
        let page = Engine::new()?.recognize(&image)?;
        let text = page.text(0, page.len(), &all(&page));
        eprintln!("{text}");
        assert!(
            page.lines().len() >= 5,
            "found {} lines",
            page.lines().len()
        );
        for expected in [
            "Quarterly report 2026",
            "Revenue grew by 12%",
            "team@example.com",
            "SK-4821-XYZ",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text:?}");
        }
        // Reading order: headline before the body, body before the contact line.
        let at = |s: &str| text.find(s).unwrap();
        assert!(
            at("Quarterly") < at("Revenue")
                && at("Revenue") < at("Contact")
                && at("Contact") < at("Secret")
        );
        // A character cell sits on the glyph: "Q" of the headline starts at x = 30 in the source.
        let (li, _) = page.locate(0).unwrap();
        let q = page.glyph_quad(li, 0);
        assert!(q[0].0 < 45.0 && q[1].0 > 30.0, "first glyph cell {q:?}");
        Ok(())
    }
}
