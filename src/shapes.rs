//! On-device single-stroke shape recognition: Desert Ant Labs "Shapes" v1.0.0.
//!
//! A small CNN (`assets/shapes/shapes.tflite`, run offline on CPU by Google's LiteRT through the
//! `litert` crate) scores the stroke
//! and a geometric fitter turns it into a clean outline. This is a Rust port of the reference
//! pipeline in `Desert-Ant-Labs/desert-ant-core` `Sources/Shapes/{Preprocess,Model,Fitter,
//! Snapping,ShapeMeta}.swift` (commit pinned in `assets/shapes/PROVENANCE.md`), verified against
//! the upstream Python parity fixture (`tests/fixtures/shapes-parity.json`).
//!
//! Pipeline: drop non-finite points -> arc-length resample to 128 points -> bbox-centre and
//! scale longer side to 1 -> network (`probs[13]`, `head[128]`) -> reject class / confidence
//! gate -> fit the winning kind in the original pixel space -> residual gate -> snapping
//! (15 degree steps, 5 degree axis snap, circle/square/regular polygon).
//!
//! Model licence: Desert Ant Labs Source-Available License 1.0, which requires crediting
//! "Powered by Desert Ant Labs" (https://desertant.com) where users can find it; see
//! `assets/shapes/LICENSE.md`.
//!
//! The scribble (erase gesture) class is deliberately not recognised: it returns `None`, so
//! the ink stays.

use std::collections::{HashMap, HashSet};
use std::f64::consts::PI;
use std::ops::{Add, Div, Mul, Sub};

use anyhow::{Context, Result, anyhow, ensure};
use litert::{CompilationOptions, CompiledModel, ElementType, Environment, Model, TensorBuffer};
use serde::{Deserialize, Serialize};

const MODEL: &[u8] = include_bytes!("../assets/shapes/shapes.tflite");
const META: &str = include_str!("../assets/shapes/shapes_meta.json");

/// Points per resampled stroke; `shapes_meta.json` must agree.
const N: usize = 128;
/// A stroke shorter than this many pixels is a tap (upstream `Model.minimumLength`).
const MIN_LENGTH: f64 = 6.0;
/// Coordinates beyond this are treated as malformed (keeps every f32/f64 step finite).
const MAX_COORD: f64 = 1.0e7;

/// A point in original screenshot pixel space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// A recognised shape as a polyline in the input coordinate space. Closed shapes do not repeat
/// their first point. An arrow's polyline is the shaft with its heads drawn as barbs that
/// retrace onto the tip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recognized {
    /// One of `line`, `curve`, `arrow`, `rectangle`, `ellipse`, `triangle`, `diamond`,
    /// `pentagon`, `hexagon`, `star`, `heart`, `cloud`.
    pub kind: String,
    pub points: Vec<Point>,
    pub closed: bool,
}

/// Result of [`Recognizer::recognize_choices`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recognition {
    /// What `recognize` returns: the shape that may replace the ink automatically.
    pub accepted: Option<Recognized>,
    /// Distinct other shapes (never the accepted kind), best model score first. Offered for
    /// an explicit user choice only.
    pub alternatives: Vec<Recognized>,
}

/// Alternatives offered per stroke.
pub const MAX_ALTERNATIVES: usize = 3;
/// An alternative needs at least this model probability (the fixture's runner-up classes sit
/// at ~0.004, i.e. noise).
const ALT_MIN_PROB: f32 = 0.01;
/// Loosest residual an alternative may fit with; caps classes whose gate is unset or looser.
const ALT_MAX_RESID: f64 = 0.1;
/// An alternative must span at least this many pixels.
const ALT_MIN_EXTENT: f32 = 1.0;

pub struct Recognizer {
    // Buffers are declared first so they drop before the environment owned by `compiled`.
    input: TensorBuffer,
    outputs: Vec<TensorBuffer>,
    compiled: CompiledModel,
    meta: Meta,
}

impl Recognizer {
    /// Loads the bundled model once into Google's LiteRT CPU runtime (XNNPACK); reuse the
    /// recognizer for every stroke. Needs `libLiteRt.so` at run time (see `Cargo.toml`/build.rs).
    pub fn new() -> Result<Self> {
        let meta = Meta::parse(META)?;
        let env = Environment::new().context("creating LiteRT environment")?;
        let model = Model::from_bytes(MODEL.to_vec()).context("loading shapes.tflite")?;
        let sig = model.signature(0)?;
        ensure!(
            sig.input_count()? == 1 && sig.output_count()? == 2,
            "shapes model must have 1 input and 2 outputs"
        );
        let shape = sig.input_shape(0)?;
        ensure!(
            shape.element_type == ElementType::Float32 && shape.dims == [1, N as i32, 2],
            "unexpected shapes model input {shape:?}"
        );
        let input = TensorBuffer::managed_host(&env, &shape)?;
        let outputs = (0..2)
            .map(|i| TensorBuffer::managed_host(&env, &sig.output_shape(i)?))
            .collect::<litert::Result<Vec<_>>>()?;
        let compiled = CompiledModel::new(env, model, &CompilationOptions::new()?)
            .context("compiling shapes.tflite for CPU")?;
        Ok(Self {
            input,
            outputs,
            compiled,
            meta,
        })
    }

    /// `Ok(None)` for taps, malformed strokes, non-shapes, scribbles and anything below the
    /// model's calibrated confidence or fit gates: the caller keeps the ink.
    pub fn recognize(&mut self, points: &[Point]) -> Result<Option<Recognized>> {
        let Some(p) = prepare(points) else {
            return Ok(None);
        };
        let (probs, head) = self.infer(&p)?;
        Ok(self.meta.accept(&p, &probs, &head))
    }

    /// `recognize` plus correction choices from the same single inference: the calibrated
    /// `accepted` result (identical to `recognize`) and up to [`MAX_ALTERNATIVES`] other
    /// shapes the model ranks next that fit the stroke. Alternatives are never applied
    /// automatically, so they skip the confidence gate; they still need a fit inside the
    /// class's residual gate. A tap, malformed stroke or scribble yields no choices at all.
    pub fn recognize_choices(&mut self, points: &[Point]) -> Result<Recognition> {
        let Some(p) = prepare(points) else {
            return Ok(Recognition::default());
        };
        let (probs, head) = self.infer(&p)?;
        let accepted = self.meta.accept(&p, &probs, &head);
        let alternatives = self.meta.alternatives(&p, &probs, &head, accepted.as_ref());
        Ok(Recognition {
            accepted,
            alternatives,
        })
    }

    /// One forward pass: class probabilities and per-point arrowhead probabilities.
    fn infer(&mut self, p: &[V2]) -> Result<(Vec<f32>, Vec<f32>)> {
        let input: Vec<f32> = normalize(p)
            .iter()
            .flat_map(|q| [q.x as f32, q.y as f32])
            .collect();
        self.input.lock_for_write::<f32>()?.copy_from_slice(&input);
        self.compiled
            .run(std::slice::from_mut(&mut self.input), &mut self.outputs)
            .context("running shapes model")?;
        let (mut probs, mut head) = (None, None);
        for o in &self.outputs {
            let s = o.lock_for_read::<f32>()?.to_vec();
            if s.len() == self.meta.kinds.len() {
                probs = Some(s);
            } else if s.len() == N {
                head = Some(s);
            }
        }
        let (probs, head) = probs
            .zip(head)
            .ok_or_else(|| anyhow!("unexpected shapes model output shapes"))?;
        ensure!(
            probs.iter().chain(&head).all(|x| x.is_finite()),
            "shapes model produced non-finite output"
        );
        Ok((probs, head))
    }
}

// ---------------------------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Line,
    Curve,
    Arrow,
    Rectangle,
    Ellipse,
    Triangle,
    Diamond,
    Pentagon,
    Hexagon,
    Star,
    Heart,
    Cloud,
    Scribble,
}

impl Kind {
    fn from_name(s: &str) -> Option<Kind> {
        Some(match s {
            "line" => Kind::Line,
            "curve" => Kind::Curve,
            "arrow" => Kind::Arrow,
            "rectangle" => Kind::Rectangle,
            "ellipse" => Kind::Ellipse,
            "triangle" => Kind::Triangle,
            "diamond" => Kind::Diamond,
            "pentagon" => Kind::Pentagon,
            "hexagon" => Kind::Hexagon,
            "star" => Kind::Star,
            "heart" => Kind::Heart,
            "cloud" => Kind::Cloud,
            "scribble" => Kind::Scribble,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Line => "line",
            Kind::Curve => "curve",
            Kind::Arrow => "arrow",
            Kind::Rectangle => "rectangle",
            Kind::Ellipse => "ellipse",
            Kind::Triangle => "triangle",
            Kind::Diamond => "diamond",
            Kind::Pentagon => "pentagon",
            Kind::Hexagon => "hexagon",
            Kind::Star => "star",
            Kind::Heart => "heart",
            Kind::Cloud => "cloud",
            Kind::Scribble => "scribble",
        }
    }
}

#[derive(Clone, Copy)]
struct Gate {
    conf: f64,
    resid: f64,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            conf: 0.0,
            resid: f64::INFINITY,
        }
    }
}

struct Meta {
    /// Per classifier output; `None` for a class this build does not know (always rejected).
    kinds: Vec<Option<Kind>>,
    reject: Vec<bool>,
    gates: HashMap<Kind, Gate>,
    straight_tolerance: f64,
}

impl Meta {
    fn parse(json: &str) -> Result<Meta> {
        let root: serde_json::Value =
            serde_json::from_str(json).context("parsing shapes_meta.json")?;
        let names: Vec<&str> = root["classes"]
            .as_array()
            .context("meta: classes")?
            .iter()
            .map(|c| c.as_str().context("meta: class name"))
            .collect::<Result<_>>()?;
        ensure!(
            root["n_points"].as_u64() == Some(N as u64),
            "meta: n_points must be {N}"
        );
        let reject_names: Vec<&str> = match root["reject_classes"].as_array() {
            Some(a) => a.iter().filter_map(|x| x.as_str()).collect(),
            None => vec!["none"],
        };
        let mut gates = HashMap::new();
        for (name, g) in root["gates"].as_object().context("meta: gates")? {
            if let Some(kind) = Kind::from_name(name) {
                gates.insert(
                    kind,
                    Gate {
                        conf: g["conf"].as_f64().unwrap_or(0.0),
                        resid: g["resid"].as_f64().unwrap_or(f64::INFINITY),
                    },
                );
            }
        }
        Ok(Meta {
            kinds: names.iter().map(|n| Kind::from_name(n)).collect(),
            reject: names.iter().map(|n| reject_names.contains(n)).collect(),
            gates,
            straight_tolerance: root["fit"]["straight_tolerance"].as_f64().unwrap_or(0.06),
        })
    }
}

/// Finite points -> the 128-point resample the model and fitters work on; `None` for taps and
/// malformed strokes.
fn prepare(points: &[Point]) -> Option<Vec<V2>> {
    let pts: Vec<V2> = points
        .iter()
        .filter(|p| p.x.is_finite() && p.y.is_finite())
        .map(|p| v(p.x as f64, p.y as f64))
        .collect();
    if pts.len() < 2
        || pts
            .iter()
            .any(|p| p.x.abs() > MAX_COORD || p.y.abs() > MAX_COORD)
        || path_length(&pts) < MIN_LENGTH
    {
        return None;
    }
    Some(resample_f32(&pts, N))
}

/// Fits `kind` to the stroke; `None` when the residual exceeds `max_resid` or snapping fails.
fn candidate(
    kind: Kind,
    p: &[V2],
    head: &[f32],
    straight_tolerance: f64,
    max_resid: f64,
) -> Option<Recognized> {
    let fit = fit(kind, p, head, straight_tolerance);
    if !(fit.residual <= max_resid) {
        return None;
    }
    snap(fit.shape).and_then(into_recognized)
}

/// Alternatives skip the confidence gate, so insist the geometry is a real, drawable shape.
fn usable(r: &Recognized) -> bool {
    let (mut lo, mut hi) = (
        (f32::INFINITY, f32::INFINITY),
        (f32::NEG_INFINITY, f32::NEG_INFINITY),
    );
    for q in &r.points {
        lo = (lo.0.min(q.x), lo.1.min(q.y));
        hi = (hi.0.max(q.x), hi.1.max(q.y));
    }
    (!r.closed || r.points.len() >= 3) && (hi.0 - lo.0).max(hi.1 - lo.1) >= ALT_MIN_EXTENT
}

impl Meta {
    /// The calibrated automatic result for one inference: top class, confidence gate, fit gate.
    fn accept(&self, p: &[V2], probs: &[f32], head: &[f32]) -> Option<Recognized> {
        let mut top = 0;
        for i in 0..probs.len() {
            if probs[i] > probs[top] {
                top = i;
            }
        }
        let kind = self.kinds[top]?;
        if self.reject[top] || kind == Kind::Scribble {
            return None;
        }
        let gate = self.gates.get(&kind).copied().unwrap_or_default();
        if (probs[top] as f64) < gate.conf {
            return None;
        }
        candidate(kind, p, head, self.straight_tolerance, gate.resid)
    }

    /// Up to [`MAX_ALTERNATIVES`] shapes of a kind other than `accepted`, in descending model
    /// probability. A scribble-led stroke is an erase gesture and gets none.
    fn alternatives(
        &self,
        p: &[V2],
        probs: &[f32],
        head: &[f32],
        accepted: Option<&Recognized>,
    ) -> Vec<Recognized> {
        let mut order: Vec<usize> = (0..probs.len()).collect();
        order.sort_by(|&a, &b| probs[b].total_cmp(&probs[a])); // stable: ties keep class order
        let Some(&top) = order.first() else {
            return vec![];
        };
        if self.kinds[top] == Some(Kind::Scribble) {
            return vec![];
        }
        let mut seen: HashSet<String> = accepted.map(|a| a.kind.clone()).into_iter().collect();
        let mut out = Vec::new();
        for i in order {
            if out.len() == MAX_ALTERNATIVES || probs[i] < ALT_MIN_PROB {
                break;
            }
            let Some(kind) = self.kinds[i] else { continue };
            if self.reject[i] || kind == Kind::Scribble || (accepted.is_some() && i == top) {
                continue;
            }
            let gate = self.gates.get(&kind).copied().unwrap_or_default();
            let max_resid = gate.resid.min(ALT_MAX_RESID);
            if let Some(r) = candidate(kind, p, head, self.straight_tolerance, max_resid)
                && usable(&r)
                && seen.insert(r.kind.clone())
            {
                out.push(r);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Vector math (f64, like the reference)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct V2 {
    x: f64,
    y: f64,
}

const fn v(x: f64, y: f64) -> V2 {
    V2 { x, y }
}

impl Add for V2 {
    type Output = V2;
    fn add(self, o: V2) -> V2 {
        v(self.x + o.x, self.y + o.y)
    }
}
impl Sub for V2 {
    type Output = V2;
    fn sub(self, o: V2) -> V2 {
        v(self.x - o.x, self.y - o.y)
    }
}
impl Mul<f64> for V2 {
    type Output = V2;
    fn mul(self, s: f64) -> V2 {
        v(self.x * s, self.y * s)
    }
}
impl Div<f64> for V2 {
    type Output = V2;
    fn div(self, s: f64) -> V2 {
        v(self.x / s, self.y / s)
    }
}

impl V2 {
    fn len(self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
    fn angle(self) -> f64 {
        self.y.atan2(self.x)
    }
}

fn dot(a: V2, b: V2) -> f64 {
    a.x * b.x + a.y * b.y
}

fn cross(a: V2, b: V2) -> f64 {
    a.x * b.y - a.y * b.x
}

fn sum(p: &[V2]) -> V2 {
    p.iter().fold(v(0.0, 0.0), |a, &b| a + b)
}

fn mean(p: &[V2]) -> V2 {
    sum(p) / p.len() as f64
}

fn bounds(p: &[V2]) -> (V2, V2) {
    let mut lo = p[0];
    let mut hi = p[0];
    for q in p {
        lo = v(lo.x.min(q.x), lo.y.min(q.y));
        hi = v(hi.x.max(q.x), hi.y.max(q.y));
    }
    (lo, hi)
}

fn fmax(it: impl IntoIterator<Item = f64>) -> f64 {
    it.into_iter().fold(f64::NEG_INFINITY, f64::max)
}

fn fmin(it: impl IntoIterator<Item = f64>) -> f64 {
    it.into_iter().fold(f64::INFINITY, f64::min)
}

/// Index of the first minimum (strict `<`, as the reference's `min(by:)`).
fn argmin(it: impl IntoIterator<Item = f64>) -> usize {
    let mut best = (0, f64::INFINITY);
    for (i, x) in it.into_iter().enumerate() {
        if i == 0 || x < best.1 {
            best = (i, x);
        }
    }
    best.0
}

fn rotate(ps: &[V2], a: f64, about: V2) -> Vec<V2> {
    let (s, c) = a.sin_cos();
    ps.iter()
        .map(|&p| {
            let d = p - about;
            about + v(c * d.x - s * d.y, s * d.x + c * d.y)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Preprocess
// ---------------------------------------------------------------------------------------------

fn path_length(p: &[V2]) -> f64 {
    p.windows(2).map(|w| (w[1] - w[0]).len()).sum()
}

/// `n` points evenly spaced by arc length, keeping the first and last point.
fn resample(points: &[V2], n: usize) -> Vec<V2> {
    let Some(&first) = points.first() else {
        return vec![];
    };
    if points.len() == 1 {
        return vec![first; n];
    }
    let mut cum = vec![0.0_f64];
    let mut seg = Vec::with_capacity(points.len() - 1);
    for w in points.windows(2) {
        let d = (w[1] - w[0]).len();
        seg.push(d);
        cum.push(cum[cum.len() - 1] + d);
    }
    let total = cum[cum.len() - 1];
    if !(total > 0.0) {
        return vec![points[0]; n];
    }
    let mut out = Vec::with_capacity(n);
    let mut idx = 0;
    for j in 0..n {
        let target = total * j as f64 / (n - 1) as f64;
        while idx + 1 < cum.len() && cum[idx + 1] <= target {
            idx += 1;
        }
        let i = idx.min(points.len() - 2);
        let t = ((target - cum[i]) / seg[i].max(1e-12)).clamp(0.0, 1.0);
        out.push(points[i] + (points[i + 1] - points[i]) * t);
    }
    out
}

/// The resample rounded through f32, as the Python implementation returns float32.
fn resample_f32(points: &[V2], n: usize) -> Vec<V2> {
    resample(points, n)
        .into_iter()
        .map(|q| v(q.x as f32 as f64, q.y as f32 as f64))
        .collect()
}

/// Bounding box centre to the origin, longer side scaled to 1.
fn normalize(p: &[V2]) -> Vec<V2> {
    if p.is_empty() {
        return vec![];
    }
    let (lo, hi) = bounds(p);
    let c = (lo + hi) / 2.0;
    let scale = (hi.x - lo.x).max(hi.y - lo.y).max(1e-9);
    p.iter().map(|&q| (q - c) / scale).collect()
}

// ---------------------------------------------------------------------------------------------
// Fitter
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Shape {
    Line(V2, V2),
    Curve([V2; 4]),
    Arrow {
        ctrl: [V2; 4],
        start: bool,
        end: bool,
    },
    /// Rectangle, triangle, diamond, pentagon, hexagon vertices in perimeter order. An
    /// incomplete fit has fewer vertices than the kind requires and is dropped.
    Poly(Kind, Vec<V2>),
    Ellipse {
        c: V2,
        a: f64,
        b: f64,
        rot: f64,
    },
    /// `rot` is the angle of a tip from +x here, as the fitter computes it; see `star_shape`.
    Star {
        c: V2,
        outer: f64,
        inner: f64,
        rot: f64,
        k: usize,
    },
    Heart {
        c: V2,
        w: f64,
        h: f64,
    },
    Cloud(Vec<V2>),
}

struct Fit {
    shape: Shape,
    /// The polyline the residual is measured against.
    outline: Vec<V2>,
    residual: f64,
}

fn fit(kind: Kind, p: &[V2], head: &[f32], straight_tolerance: f64) -> Fit {
    match kind {
        Kind::Line | Kind::Curve => connector(p, straight_tolerance),
        Kind::Arrow => arrow(p, head, straight_tolerance),
        Kind::Ellipse => ellipse(p),
        Kind::Rectangle => rectangle(p),
        Kind::Triangle => convex_polygon(p, 3, kind),
        Kind::Diamond => convex_polygon(p, 4, kind),
        Kind::Pentagon => convex_polygon(p, 5, kind),
        Kind::Hexagon => convex_polygon(p, 6, kind),
        Kind::Star => star(p),
        Kind::Heart => heart(p),
        Kind::Cloud => cloud(p),
        Kind::Scribble => unreachable!("scribble is rejected before fitting"),
    }
}

fn size(p: &[V2]) -> f64 {
    let (lo, hi) = bounds(p);
    (hi.x - lo.x).max(hi.y - lo.y).max(1e-9)
}

fn seg_dist(q: V2, a: V2, b: V2) -> f64 {
    let ab = b - a;
    let t = (dot(q - a, ab) / dot(ab, ab).max(1e-18)).clamp(0.0, 1.0);
    (q - (a + ab * t)).len()
}

fn dist_to_poly(q: V2, poly: &[V2]) -> f64 {
    fmin(poly.windows(2).map(|w| seg_dist(q, w[0], w[1])))
}

/// Mean distance from the stroke to the outline divided by the stroke's size.
fn residual(p: &[V2], outline: &[V2]) -> f64 {
    let total: f64 = p.iter().map(|&q| dist_to_poly(q, outline)).sum();
    total / p.len() as f64 / size(p)
}

/// Total least squares line: a point on it and its unit direction.
fn tls_line(p: &[V2]) -> (V2, V2) {
    let c = mean(p);
    let (mut sxx, mut sxy, mut syy) = (0.0_f64, 0.0_f64, 0.0_f64);
    for &q in p {
        let d = q - c;
        sxx += d.x * d.x;
        sxy += d.x * d.y;
        syy += d.y * d.y;
    }
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    (c, v(theta.cos(), theta.sin()))
}

fn intersect(c1: V2, d1: V2, c2: V2, d2: V2) -> Option<V2> {
    let den = cross(d1, d2);
    if den.abs() < 0.17 {
        return None; // within ~10 degrees of parallel
    }
    let t = cross(c2 - c1, d2) / den;
    Some(c1 + d1 * t)
}

fn closed(p: &[V2]) -> Vec<V2> {
    let mut out = p.to_vec();
    if let Some(&f) = p.first() {
        out.push(f);
    }
    out
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round_ties_even()
}

/// Convex hull by monotone chain. Points equal to 1e-6 count as one, as in the reference.
fn convex_hull(p: &[V2]) -> Vec<V2> {
    let mut seen = HashSet::new();
    let mut pts = Vec::new();
    for q in p {
        let (kx, ky) = (round6(q.x), round6(q.y));
        if seen.insert((kx as i64, ky as i64)) {
            pts.push(v(kx / 1e6, ky / 1e6));
        }
    }
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    if pts.len() < 3 {
        return pts;
    }
    fn half(seq: impl Iterator<Item = V2>) -> Vec<V2> {
        let mut h: Vec<V2> = Vec::new();
        for q in seq {
            while h.len() >= 2 && cross(h[h.len() - 1] - h[h.len() - 2], q - h[h.len() - 2]) <= 0.0
            {
                h.pop();
            }
            h.push(q);
        }
        h
    }
    let mut lower = half(pts.iter().copied());
    let mut upper = half(pts.iter().rev().copied());
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

fn polygon_centroid(poly: &[V2]) -> V2 {
    let (mut a, mut cx, mut cy) = (0.0_f64, 0.0_f64, 0.0_f64);
    for i in 0..poly.len() {
        let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
        let cr = p.x * q.y - q.x * p.y;
        a += cr;
        cx += (p.x + q.x) * cr;
        cy += (p.y + q.y) * cr;
    }
    a /= 2.0;
    if a.abs() < 1e-12 {
        return mean(poly);
    }
    v(cx, cy) / (6.0 * a)
}

/// `np.interp` over a polyline at arc length `t`, searching from `j`.
fn interp(t: f64, s: &[f64], c: &[V2], j: usize) -> V2 {
    let mut k = j;
    while k + 2 < s.len() && s[k + 1] <= t {
        k += 1;
    }
    let span = s[k + 1] - s[k];
    if span <= 0.0 {
        return c[k + 1];
    }
    c[k] + (c[k + 1] - c[k]) * ((t - s[k]) / span)
}

/// `n` points evenly spaced around a closed loop, from its first point.
fn resample_closed(lp: &[V2], n: usize) -> Vec<V2> {
    let c = closed(lp);
    if c.len() < 2 {
        return vec![v(0.0, 0.0); n];
    }
    let mut s = vec![0.0_f64];
    for i in 1..c.len() {
        s.push(s[i - 1] + (c[i] - c[i - 1]).len());
    }
    let total = s[s.len() - 1];
    let mut j = 0;
    (0..n)
        .map(|i| {
            let t = total * i as f64 / n as f64;
            while j + 2 < c.len() && s[j + 1] < t {
                j += 1;
            }
            interp(t, &s, &c, j)
        })
        .collect()
}

/// The `k` largest values of a circular signal at least `min_sep` apart, ascending by index,
/// picked greedily from the largest down.
fn circular_peaks(signal: &[f64], k: usize, min_sep: usize) -> Vec<usize> {
    let m = signal.len();
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|&a, &b| signal[b].total_cmp(&signal[a]).then(a.cmp(&b)));
    let mut out: Vec<usize> = Vec::new();
    for i in order {
        if out.iter().all(|&o| {
            let d = i.abs_diff(o);
            d.min(m - d) >= min_sep
        }) {
            out.push(i);
            if out.len() == k {
                break;
            }
        }
    }
    out.sort_unstable();
    out
}

/// Box filter over a circular signal (`np.convolve(..., "same")` on the padded signal).
fn smooth_circular(x: &[f64], w: usize) -> Vec<f64> {
    if w <= 1 {
        return x.to_vec();
    }
    let m = x.len();
    let mut padded = x[m - w..].to_vec();
    padded.extend_from_slice(x);
    padded.extend_from_slice(&x[..w]);
    let half = (w - 1) / 2 + (w - 1) % 2; // where numpy centres an even kernel
    (0..m)
        .map(|i| {
            let mut acc = 0.0_f64;
            for k in 0..w {
                let idx = (i + w + k) as isize - half as isize;
                if idx >= 0 && (idx as usize) < padded.len() {
                    acc += padded[idx as usize];
                }
            }
            acc / w as f64
        })
        .collect()
}

/// Linear-interpolated percentile (numpy's default).
fn percentile(vals: &[f64], q: f64) -> f64 {
    let mut s = vals.to_vec();
    s.sort_by(f64::total_cmp);
    let pos = q / 100.0 * (s.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(s.len() - 1);
    s[lo] + (s[hi] - s[lo]) * (pos - lo as f64)
}

fn median(vals: &[f64]) -> f64 {
    percentile(vals, 50.0)
}

/// Absolute turn at each vertex of a closed loop.
fn turning(p: &[V2]) -> Vec<f64> {
    let m = p.len();
    let a: Vec<f64> = (0..m).map(|i| (p[(i + 1) % m] - p[i]).angle()).collect();
    (0..m)
        .map(|i| wrap(a[i] - a[(i + m - 1) % m]).abs())
        .collect()
}

/// `np.angle(np.exp(1j * x))`
fn wrap(x: f64) -> f64 {
    x.sin().atan2(x.cos())
}

fn bezier_at(c: &[V2; 4], t: f64) -> V2 {
    let u = 1.0 - t;
    c[0] * (u * u * u) + c[1] * (3.0 * u * u * t) + c[2] * (3.0 * u * t * t) + c[3] * (t * t * t)
}

fn bezier(c: &[V2; 4], n: usize) -> Vec<V2> {
    (0..n)
        .map(|i| bezier_at(c, i as f64 / (n - 1) as f64))
        .collect()
}

/// Least-squares cubic with fixed end points; chord-length parameters refined by moving each
/// point's parameter to its nearest point on the curve.
fn cubic(p: &[V2], iterations: usize) -> [V2; 4] {
    let (p0, p3) = (p[0], p[p.len() - 1]);
    let mut cum = vec![0.0_f64];
    for i in 1..p.len() {
        cum.push(cum[i - 1] + (p[i] - p[i - 1]).len());
    }
    let total = cum[cum.len() - 1].max(1e-12);
    let mut t: Vec<f64> = cum.iter().map(|c| c / total).collect();
    let mut ctrl = [p0, p0 + (p3 - p0) / 3.0, p0 + (p3 - p0) * (2.0 / 3.0), p3];
    for _ in 0..=iterations {
        let (mut a11, mut a12, mut a22) = (0.0_f64, 0.0_f64, 0.0_f64);
        let (mut r1, mut r2) = (v(0.0, 0.0), v(0.0, 0.0));
        for (i, &q) in p.iter().enumerate() {
            let u = 1.0 - t[i];
            let (b0, b1, b2, b3) = (
                u * u * u,
                3.0 * u * u * t[i],
                3.0 * u * t[i] * t[i],
                t[i] * t[i] * t[i],
            );
            let rhs = q - p0 * b0 - p3 * b3;
            a11 += b1 * b1;
            a12 += b1 * b2;
            a22 += b2 * b2;
            r1 = r1 + rhs * b1;
            r2 = r2 + rhs * b2;
        }
        let det = a11 * a22 - a12 * a12;
        if det.abs() > 1e-18 {
            ctrl = [
                p0,
                (r1 * a22 - r2 * a12) / det,
                (r2 * a11 - r1 * a12) / det,
                p3,
            ];
        }
        let dense = bezier(&ctrl, 200);
        t = p
            .iter()
            .map(|&q| {
                argmin(
                    dense
                        .iter()
                        .map(|&d| (q - d).x * (q - d).x + (q - d).y * (q - d).y),
                ) as f64
                    / 199.0
            })
            .collect();
        t[0] = 0.0;
        let last = t.len() - 1;
        t[last] = 1.0;
        for i in 1..t.len() {
            t[i] = t[i].max(t[i - 1]);
        }
    }
    ctrl
}

fn straight_ctrl(a: V2, b: V2) -> [V2; 4] {
    [a, a + (b - a) / 3.0, a + (b - a) * (2.0 / 3.0), b]
}

fn connector(p: &[V2], tolerance: f64) -> Fit {
    let (c, d) = tls_line(p);
    let proj: Vec<f64> = p.iter().map(|&q| dot(q - c, d)).collect();
    let (mut a, mut b) = (
        c + d * fmin(proj.iter().copied()),
        c + d * fmax(proj.iter().copied()),
    );
    if (a - p[0]).len() > (b - p[0]).len() {
        std::mem::swap(&mut a, &mut b);
    }
    let length = (b - a).len().max(1e-9);
    let normal = v(-d.y, d.x);
    let dev = fmax(p.iter().map(|&q| dot(q - c, normal).abs())) / length;
    if dev <= tolerance {
        let outline = vec![a, b];
        let residual = residual(p, &outline);
        return Fit {
            shape: Shape::Line(a, b),
            outline,
            residual,
        };
    }
    let ctrl = cubic(p, 3);
    let outline = bezier(&ctrl, 48);
    let residual = residual(p, &outline);
    Fit {
        shape: Shape::Curve(ctrl),
        outline,
        residual,
    }
}

/// The shaft is the longest run of points the model does not mark as arrowhead. Each end with
/// two or more arrowhead points gets a head, and the tip moves to the head point furthest along
/// the shaft's direction.
fn arrow(p: &[V2], head: &[f32], tolerance: f64) -> Fit {
    let n = p.len();
    let mask: Vec<bool> = (0..n).map(|i| i < head.len() && head[i] > 0.5).collect();
    let (mut best, mut run, mut span) = (0, 0, (0, n));
    for i in 0..=n {
        if i < n && !mask[i] {
            run += 1;
            continue;
        }
        if run > best {
            best = run;
            span = (i - run, i);
        }
        run = 0;
    }
    let (mut s0, mut s1) = span;
    if s1 - s0 < 4 {
        (s0, s1) = (0, n);
    }
    let shaft_points = &p[s0..s1];
    let shaft = connector(shaft_points, tolerance);
    let start_head = s0 > 1 && mask[..s0].iter().filter(|&&m| m).count() >= 2;
    let mut end_head = s1 + 1 < n && mask[s1..].iter().filter(|&&m| m).count() >= 2;
    if !(start_head || end_head) {
        end_head = true;
    }

    let straight = matches!(shaft.shape, Shape::Line(..));
    let mut ctrl = match shaft.shape {
        Shape::Curve(c) => c,
        Shape::Line(a, b) => straight_ctrl(a, b),
        _ => [p[0], p[0], p[n - 1], p[n - 1]],
    };
    let mut sp = shaft.outline;
    let sz = size(p);
    for at_end in [false, true] {
        if !(if at_end { end_head } else { start_head }) {
            continue;
        }
        let region = if at_end { &p[s1 - 1..] } else { &p[..=s0] };
        let tip = if at_end { sp[sp.len() - 1] } else { sp[0] };
        let nb = if at_end {
            sp[sp.len() - sp.len().min(4)]
        } else {
            sp[(sp.len() - 1).min(3)]
        };
        let mut dir = tip - nb;
        dir = dir / dir.len().max(1e-9);
        if region.len() <= 1 {
            continue;
        }
        let reach = fmax(region.iter().map(|&q| dot(q - tip, dir)));
        if reach > 0.0 && reach < 0.25 * sz {
            let moved = tip + dir * reach;
            if at_end {
                let l = sp.len() - 1;
                sp[l] = moved;
                ctrl[3] = moved;
            } else {
                sp[0] = moved;
                ctrl[0] = moved;
            }
        }
    }
    if straight {
        ctrl = straight_ctrl(ctrl[0], ctrl[3]);
    }
    let residual = residual(shaft_points, &sp);
    Fit {
        shape: Shape::Arrow {
            ctrl,
            start: start_head,
            end: end_head,
        },
        outline: sp,
        residual,
    }
}

fn ellipse_outline(c: V2, a: f64, b: f64, theta: f64, n: usize) -> Vec<V2> {
    let (st, ct) = theta.sin_cos();
    (0..=n)
        .map(|i| {
            let t = 2.0 * PI * i as f64 / n as f64;
            let (x, y) = (a * t.cos(), b * t.sin());
            v(c.x + x * ct - y * st, c.y + x * st + y * ct)
        })
        .collect()
}

type M3 = [[f64; 3]; 3];

fn m3_mul(a: &M3, b: &M3) -> M3 {
    let mut o = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            o[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    o
}

fn m3_vec(a: &M3, x: &[f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| (0..3).map(|k| a[i][k] * x[k]).sum())
}

fn m3_t(a: &M3) -> M3 {
    let mut o = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            o[i][j] = a[j][i];
        }
    }
    o
}

fn m3_det(a: &M3) -> f64 {
    a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
}

fn m3_inverse(a: &M3) -> Option<M3> {
    let det = m3_det(a);
    if !(det.abs() > 1e-300) {
        return None;
    }
    let c: M3 = [
        [
            a[1][1] * a[2][2] - a[1][2] * a[2][1],
            a[0][2] * a[2][1] - a[0][1] * a[2][2],
            a[0][1] * a[1][2] - a[0][2] * a[1][1],
        ],
        [
            a[1][2] * a[2][0] - a[1][0] * a[2][2],
            a[0][0] * a[2][2] - a[0][2] * a[2][0],
            a[0][2] * a[1][0] - a[0][0] * a[1][2],
        ],
        [
            a[1][0] * a[2][1] - a[1][1] * a[2][0],
            a[0][1] * a[2][0] - a[0][0] * a[2][1],
            a[0][0] * a[1][1] - a[0][1] * a[1][0],
        ],
    ];
    Some(c.map(|r| r.map(|x| x / det)))
}

/// Real roots of x^3 + b x^2 + c x + d, polished by Newton.
fn cubic_roots(b: f64, c: f64, d: f64) -> Vec<f64> {
    let p = c - b * b / 3.0;
    let q = 2.0 * b * b * b / 27.0 - b * c / 3.0 + d;
    let disc = q * q / 4.0 + p * p * p / 27.0;
    let roots: Vec<f64> = if disc > 0.0 {
        let s = disc.sqrt();
        vec![(-q / 2.0 + s).cbrt() + (-q / 2.0 - s).cbrt()]
    } else {
        let r = (-p / 3.0).sqrt();
        let phi = (-q / (2.0 * r * r * r + 1e-300)).clamp(-1.0, 1.0).acos();
        (0..3)
            .map(|k| 2.0 * r * ((phi + 2.0 * PI * k as f64) / 3.0).cos())
            .collect()
    };
    roots
        .into_iter()
        .map(|y| {
            let mut x = y - b / 3.0;
            for _ in 0..3 {
                let f = ((x + b) * x + c) * x + d;
                let df = (3.0 * x + 2.0 * b) * x + c;
                if df != 0.0 {
                    x -= f / df;
                }
            }
            x
        })
        .collect()
}

/// Eigenvectors of a real 3x3 matrix for its real eigenvalues, each the null vector of `A - lI`.
fn real_eigenvectors(a: &M3) -> Vec<[f64; 3]> {
    let tr = a[0][0] + a[1][1] + a[2][2];
    let c2 = a[0][0] * a[1][1] - a[0][1] * a[1][0] + a[0][0] * a[2][2] - a[0][2] * a[2][0]
        + a[1][1] * a[2][2]
        - a[1][2] * a[2][1];
    let det = m3_det(a);
    let mut vectors = Vec::new();
    for l in cubic_roots(-tr, c2, -det) {
        let mut b = *a;
        for i in 0..3 {
            b[i][i] -= l;
        }
        let (mut best, mut best_norm) = ([0.0; 3], 0.0);
        for (i, j) in [(0, 1), (0, 2), (1, 2)] {
            let (r, s) = (b[i], b[j]);
            let c = [
                r[1] * s[2] - r[2] * s[1],
                r[2] * s[0] - r[0] * s[2],
                r[0] * s[1] - r[1] * s[0],
            ];
            let norm = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt();
            if norm > best_norm {
                best = c;
                best_norm = norm;
            }
        }
        if best_norm > 0.0 {
            vectors.push(best.map(|x| x / best_norm));
        }
    }
    vectors
}

/// Direct least-squares ellipse (Halir and Flusser), falling back to second moments when the
/// conic is not an ellipse.
fn ellipse(p: &[V2]) -> Fit {
    let n = p.len() as f64;
    let mu = mean(p);
    let sc = size(p);
    let q: Vec<V2> = p.iter().map(|&x| (x - mu) / sc).collect();
    let (mut s1, mut s2, mut s3) = ([[0.0; 3]; 3], [[0.0; 3]; 3], [[0.0; 3]; 3]);
    for w in &q {
        let d1 = [w.x * w.x, w.x * w.y, w.y * w.y];
        let d2 = [w.x, w.y, 1.0];
        for i in 0..3 {
            for j in 0..3 {
                s1[i][j] += d1[i] * d1[j];
                s2[i][j] += d1[i] * d2[j];
                s3[i][j] += d2[i] * d2[j];
            }
        }
    }
    let (mut center, mut a, mut b, mut theta, mut ok) = (mu, 0.0_f64, 0.0_f64, 0.0_f64, false);
    if let Some(s3inv) = m3_inverse(&s3) {
        // T = -S3^-1 S2^T, M = S1 + S2 T, then the constraint matrix applied
        let neg = s3inv.map(|r| r.map(|x| -x));
        let t = m3_mul(&neg, &m3_t(&s2));
        let s2t = m3_mul(&s2, &t);
        let mut m = s1;
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += s2t[i][j];
            }
        }
        let mc: M3 = [
            m[2].map(|x| x / 2.0),
            m[1].map(|x| -x),
            m[0].map(|x| x / 2.0),
        ];
        if let Some(e) = real_eigenvectors(&mc)
            .into_iter()
            .find(|e| 4.0 * e[0] * e[2] - e[1] * e[1] > 0.0)
        {
            let (ca, cb, cc) = (e[0], e[1], e[2]);
            let [d, ee, f] = m3_vec(&t, &e);
            let den = cb * cb - 4.0 * ca * cc;
            let (cx, cy) = (
                (2.0 * cc * d - cb * ee) / den,
                (2.0 * ca * ee - cb * d) / den,
            );
            let num = 2.0 * (ca * ee * ee + cc * d * d - cb * d * ee + den * f);
            let root = ((ca - cc) * (ca - cc) + cb * cb).sqrt();
            let ra = -(num * (ca + cc + root)).max(0.0).sqrt() / den;
            let rb = -(num * (ca + cc - root)).max(0.0).sqrt() / den;
            theta = if cb != 0.0 || ca > cc {
                0.5 * (-cb).atan2(cc - ca)
            } else {
                0.0
            };
            if ca > cc && cb == 0.0 {
                theta = PI / 2.0;
            }
            if ra > 0.0 && rb > 0.0 && (ra + rb).is_finite() {
                center = v(cx, cy) * sc + mu;
                a = ra * sc;
                b = rb * sc;
                ok = true;
            }
        }
    }
    if !ok || a.max(b) > 3.0 * sc {
        let (mut sxx, mut sxy, mut syy) = (0.0_f64, 0.0_f64, 0.0_f64);
        for &x in p {
            let d = x - mu;
            sxx += d.x * d.x;
            sxy += d.x * d.y;
            syy += d.y * d.y;
        }
        let k = n - 1.0;
        (sxx, sxy, syy) = (sxx / k, sxy / k, syy / k);
        let (tr, det) = (sxx + syy, sxx * syy - sxy * sxy);
        let disc = (tr * tr / 4.0 - det).max(0.0).sqrt();
        let (l1, l2) = (tr / 2.0 + disc, tr / 2.0 - disc);
        center = mu;
        a = (2.0 * l1.max(1e-12)).sqrt();
        b = (2.0 * l2.max(1e-12)).sqrt();
        theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    }
    if b > a {
        std::mem::swap(&mut a, &mut b);
        theta += PI / 2.0;
    }
    theta = (theta + PI / 2.0) % PI;
    if theta < 0.0 {
        theta += PI;
    }
    theta -= PI / 2.0;
    let outline = ellipse_outline(center, a, b, theta, 96);
    let residual = residual(p, &outline);
    Fit {
        shape: Shape::Ellipse {
            c: center,
            a,
            b,
            rot: theta,
        },
        outline,
        residual,
    }
}

/// An oriented rectangle: orientation from the tangents' four-fold mean, then each side at the
/// median of the points nearest it, iterated.
fn rectangle(p: &[V2]) -> Fit {
    let (mut sx, mut sy) = (0.0_f64, 0.0_f64);
    for w in p.windows(2) {
        let d = w[1] - w[0];
        let (wt, phi) = (d.len(), d.angle());
        sx += wt * (4.0 * phi).sin();
        sy += wt * (4.0 * phi).cos();
    }
    let theta = 0.25 * sx.atan2(sy);
    let (s, c) = theta.sin_cos();
    let q: Vec<V2> = p
        .iter()
        .map(|w| v(c * w.x + s * w.y, -s * w.x + c * w.y))
        .collect();
    let xs: Vec<f64> = q.iter().map(|w| w.x).collect();
    let ys: Vec<f64> = q.iter().map(|w| w.y).collect();
    let mut e = [
        percentile(&xs, 3.0),
        percentile(&xs, 97.0),
        percentile(&ys, 3.0),
        percentile(&ys, 97.0),
    ];
    for _ in 0..3 {
        let side: Vec<usize> = q
            .iter()
            .map(|w| {
                argmin([
                    (w.x - e[0]).abs(),
                    (w.x - e[1]).abs(),
                    (w.y - e[2]).abs(),
                    (w.y - e[3]).abs(),
                ])
            })
            .collect();
        let mut next = e;
        for k in 0..4 {
            let vals: Vec<f64> = (0..q.len())
                .filter(|&i| side[i] == k)
                .map(|i| if k < 2 { q[i].x } else { q[i].y })
                .collect();
            if vals.len() >= 3 {
                next[k] = median(&vals);
            }
        }
        e = next;
    }
    let local = [v(e[0], e[2]), v(e[1], e[2]), v(e[1], e[3]), v(e[0], e[3])];
    let corners: Vec<V2> = local
        .iter()
        .map(|w| v(c * w.x - s * w.y, s * w.x + c * w.y))
        .collect();
    let outline = closed(&corners);
    let residual = residual(p, &outline);
    Fit {
        shape: Shape::Poly(Kind::Rectangle, corners),
        outline,
        residual,
    }
}

/// A convex k-gon: corners where the convex hull turns most, then each side refit through the
/// stroke points nearest it and adjacent sides intersected. An incomplete result (degenerate
/// hull, fewer than `k` corners) has fewer than `k` vertices and residual 1.
fn polygon_core(p: &[V2], k: usize, refine: bool) -> (Vec<V2>, Vec<V2>, f64) {
    let hull = convex_hull(p);
    if hull.len() < 3 {
        return (vec![], p.to_vec(), 1.0);
    }
    let m = 180;
    let h = resample_closed(&hull, m);
    let turn = smooth_circular(&turning(&h), 5);
    let corners = circular_peaks(&turn, k, (m / (3 * k)).max(2));
    let mut verts: Vec<V2> = corners.iter().map(|&i| h[i]).collect();
    if verts.len() < k {
        let outline = closed(&verts);
        return (verts, outline, 1.0);
    }
    let sz = size(p);
    for _ in 0..(if refine { 2 } else { 0 }) {
        let poly = closed(&verts);
        let side: Vec<usize> = p
            .iter()
            .map(|&q| argmin((0..k).map(|i| seg_dist(q, poly[i], poly[i + 1]))))
            .collect();
        let mut lines: Vec<(V2, V2)> = Vec::with_capacity(k);
        for i in 0..k {
            let mut pts: Vec<V2> = (0..p.len())
                .filter(|&j| side[j] == i)
                .map(|j| p[j])
                .collect();
            let (a, b) = (poly[i], poly[i + 1]);
            if pts.len() >= 3 {
                let ab = b - a;
                let l2 = dot(ab, ab).max(1e-12);
                let kept: Vec<V2> = pts
                    .iter()
                    .copied()
                    .filter(|&q| {
                        let t = dot(q - a, ab) / l2;
                        t > 0.12 && t < 0.88
                    })
                    .collect();
                if kept.len() >= 3 {
                    pts = kept;
                }
                lines.push(tls_line(&pts));
            } else {
                let d = b - a;
                lines.push(((a + b) / 2.0, d / d.len().max(1e-12)));
            }
        }
        verts = (0..k)
            .map(|i| {
                let (prev, cur) = (lines[(i + k - 1) % k], lines[i]);
                match intersect(prev.0, prev.1, cur.0, cur.1) {
                    Some(x) if (x - verts[i]).len() <= 0.5 * sz => x,
                    _ => verts[i],
                }
            })
            .collect();
    }
    let outline = closed(&verts);
    let residual = residual(p, &outline);
    (verts, outline, residual)
}

fn convex_polygon(p: &[V2], k: usize, kind: Kind) -> Fit {
    let (verts, outline, residual) = polygon_core(p, k, true);
    Fit {
        shape: Shape::Poly(kind, verts),
        outline,
        residual,
    }
}

fn star_outline(c: V2, r_out: f64, r_in: f64, rot: f64, k: usize) -> Vec<V2> {
    let pts: Vec<V2> = (0..2 * k)
        .map(|i| {
            let a = rot + PI * i as f64 / k as f64;
            let r = if i % 2 == 0 { r_out } else { r_in };
            v(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect();
    closed(&pts)
}

/// The fitter's `rot` is the angle of a tip; this normalises it to the tip-up convention used
/// by the final shape (rotation 0 = a tip straight up), in `[0, 2 pi / k)`.
fn star_shape(c: V2, outer: f64, inner: f64, rot: f64, k: usize) -> Shape {
    let period = 2.0 * PI / k as f64;
    let mut r = (rot + PI / 2.0) % period;
    if r < 0.0 {
        r += period;
    }
    Shape::Star {
        c,
        outer,
        inner,
        rot: r,
        k,
    }
}

fn densify(p: &[V2]) -> Vec<V2> {
    let mut out = Vec::with_capacity(8 * p.len());
    for w in p.windows(2) {
        for j in 0..8 {
            out.push(w[0] + (w[1] - w[0]) * (j as f64 / 8.0));
        }
    }
    out.push(p[p.len() - 1]);
    out
}

fn tip_rotation(angles: impl Iterator<Item = f64>, k: usize) -> f64 {
    let (mut cs, mut ss) = (0.0_f64, 0.0_f64);
    for a in angles {
        cs += (k as f64 * a).cos();
        ss += (k as f64 * a).sin();
    }
    (ss / k as f64).atan2(cs / k as f64) / k as f64
}

fn better(best: &mut Option<Fit>, f: Fit) {
    if best
        .as_ref()
        .map_or(true, |b| f.residual < b.residual * 0.85)
    {
        *best = Some(f);
    }
}

fn star_envelope(p: &[V2]) -> Option<Fit> {
    let hull = convex_hull(p);
    let center = if hull.len() >= 3 {
        polygon_centroid(&hull)
    } else {
        mean(p)
    };
    let bins = 180usize;
    let mut env = vec![0.0f64; bins];
    for q in densify(p) {
        let d = q - center;
        let b = ((d.angle() + PI) / (2.0 * PI) * bins as f64) as i64;
        let b = b.rem_euclid(bins as i64) as usize;
        env[b] = env[b].max(d.len());
    }
    for _ in 0..bins {
        let zero: Vec<usize> = (0..bins).filter(|&i| env[i] == 0.0).collect();
        if zero.is_empty() {
            break;
        }
        let snap = env.clone();
        for i in zero {
            env[i] = snap[(i + bins - 1) % bins].max(snap[(i + 1) % bins]);
        }
    }
    let sm = smooth_circular(&env, 3);
    let mut best: Option<Fit> = None;
    for k in [5usize, 6] {
        let tips = circular_peaks(&sm, k, bins / (2 * k));
        if tips.len() < k {
            continue;
        }
        let r_out = tips.iter().map(|&t| env[t]).sum::<f64>() / k as f64;
        let rot = tip_rotation(
            tips.iter()
                .map(|&t| (t as f64 + 0.5) / bins as f64 * 2.0 * PI - PI),
            k,
        );
        let valleys: Vec<f64> = (0..k)
            .map(|i| {
                let (a, b) = (tips[i], tips[(i + 1) % k]);
                let seg: Vec<f64> = if b > a {
                    sm[a..b].to_vec()
                } else {
                    [&sm[a..], &sm[..b]].concat()
                };
                let m = fmin(seg);
                if m.is_finite() { m } else { r_out * 0.4 }
            })
            .collect();
        let r_in = median(&valleys).max(0.15 * r_out).min(0.8 * r_out);
        let outline = star_outline(center, r_out, r_in, rot, k);
        let residual = residual(p, &outline);
        better(
            &mut best,
            Fit {
                shape: star_shape(center, r_out, r_in, rot, k),
                outline,
                residual,
            },
        );
    }
    best
}

fn star_hull(p: &[V2]) -> Option<Fit> {
    let dense = densify(p);
    let mut best: Option<Fit> = None;
    for k in [5usize, 6] {
        let (tips, ..) = polygon_core(p, k, false);
        if tips.len() < k {
            continue;
        }
        let center = mean(&tips);
        let r_out = tips.iter().map(|&t| (t - center).len()).sum::<f64>() / k as f64;
        let rot = tip_rotation(tips.iter().map(|&t| (t - center).angle()), k);
        let mut valleys = Vec::new();
        for i in 0..k {
            let mid = rot + (2 * i + 1) as f64 * PI / k as f64;
            let m = fmax(
                dense
                    .iter()
                    .filter(|&&q| wrap((q - center).angle() - mid).abs() < PI / (3 * k) as f64)
                    .map(|&q| (q - center).len()),
            );
            if m.is_finite() {
                valleys.push(m);
            }
        }
        let valley = if valleys.is_empty() {
            0.4 * r_out
        } else {
            median(&valleys)
        };
        let r_in = valley.max(0.15 * r_out).min(0.8 * r_out);
        let outline = star_outline(center, r_out, r_in, rot, k);
        let residual = residual(p, &outline);
        better(
            &mut best,
            Fit {
                shape: star_shape(center, r_out, r_in, rot, k),
                outline,
                residual,
            },
        );
    }
    best
}

/// The better (lower residual) of the envelope and hull fits.
fn star(p: &[V2]) -> Fit {
    [star_envelope(p), star_hull(p)]
        .into_iter()
        .flatten()
        .min_by(|a, b| a.residual.total_cmp(&b.residual))
        .unwrap_or_else(|| Fit {
            shape: star_shape(mean(p), 1.0, 0.4, 0.0, 5),
            outline: closed(&convex_hull(p)),
            residual: 1.0,
        })
}

/// The parametric heart x = 16 sin^3 t, y = -(13 cos t - 5 cos 2t - 2 cos 3t - cos 4t), scaled
/// to the box, point down in y-down space.
fn heart_template(center: V2, width: f64, height: f64, n: usize, closed: bool) -> Vec<V2> {
    let raw: Vec<V2> = (0..if closed { n + 1 } else { n })
        .map(|i| {
            let t = 2.0 * PI * i as f64 / n as f64;
            let s = t.sin();
            v(
                16.0 * s * s * s,
                -(13.0 * t.cos() - 5.0 * (2.0 * t).cos() - 2.0 * (3.0 * t).cos() - (4.0 * t).cos()),
            )
        })
        .collect();
    let (lo, hi) = bounds(&raw);
    let mid = (lo + hi) / 2.0;
    raw.iter()
        .map(|q| {
            v(
                center.x + (q.x - mid.x) / (hi.x - lo.x) * width,
                center.y + (q.y - mid.y) / (hi.y - lo.y) * height,
            )
        })
        .collect()
}

fn heart(p: &[V2]) -> Fit {
    let xs: Vec<f64> = p.iter().map(|q| q.x).collect();
    let ys: Vec<f64> = p.iter().map(|q| q.y).collect();
    let lo = v(percentile(&xs, 1.0), percentile(&ys, 1.0));
    let hi = v(percentile(&xs, 99.0), percentile(&ys, 99.0));
    let (center, bx) = ((lo + hi) / 2.0, hi - lo);
    let outline = heart_template(center, bx.x, bx.y, 96, true);
    let residual = residual(p, &outline);
    Fit {
        shape: Shape::Heart {
            c: center,
            w: bx.x,
            h: bx.y,
        },
        outline,
        residual,
    }
}

/// Cuts a closed stroke's overlap where its cumulative turning completes a turn.
fn one_lap(p: &[V2]) -> Vec<V2> {
    let a: Vec<f64> = p.windows(2).map(|w| (w[1] - w[0]).angle()).collect();
    let mut cum = 0.0_f64;
    for i in 0..a.len().saturating_sub(1) {
        cum += wrap(a[i + 1] - a[i]);
        if cum.abs() >= 2.0 * PI {
            return p[..i + 2].to_vec();
        }
    }
    p.to_vec()
}

fn circle3(a: V2, b: V2, c: V2) -> Option<(V2, f64)> {
    let d = 2.0 * (a.x * (b.y - c.y) + b.x * (c.y - a.y) + c.x * (a.y - b.y));
    if d.abs() < 1e-12 {
        return None;
    }
    let (aa, bb, cc) = (dot(a, a), dot(b, b), dot(c, c));
    let o = v(
        (aa * (b.y - c.y) + bb * (c.y - a.y) + cc * (a.y - b.y)) / d,
        (aa * (c.x - b.x) + bb * (a.x - c.x) + cc * (b.x - a.x)) / d,
    );
    Some((o, (a - o).len()))
}

/// The circular arc a -> m -> b, `n` samples (a polyline when collinear).
fn arc3(a: V2, m: V2, b: V2, n: usize) -> Vec<V2> {
    let Some((o, r)) = circle3(a, m, b) else {
        return vec![a, m, b];
    };
    let t0 = (a - o).angle();
    let tm = (m - o).angle();
    let t1 = (b - o).angle();
    let modp = |x: f64| x.rem_euclid(2.0 * PI);
    let mut span = modp(t1 - t0);
    if modp(tm - t0) > span {
        span -= 2.0 * PI;
    }
    (0..n)
        .map(|i| {
            let t = t0 + span * i as f64 / (n - 1) as f64;
            v(o.x + r * t.cos(), o.y + r * t.sin())
        })
        .collect()
}

/// The cloud as drawn: one lap of the stroke, split at the concave cusps between bumps, with
/// each bump replaced by the circular arc through its two cusps and its outermost point.
fn cloud_arcs(p: &[V2]) -> Fit {
    let lap = resample_closed(&one_lap(p), 192);
    let m = lap.len();
    let ang: Vec<f64> = (0..m)
        .map(|i| (lap[(i + 1) % m] - lap[i]).angle())
        .collect();
    let turn: Vec<f64> = (0..m)
        .map(|i| wrap(ang[i] - ang[(i + m - 1) % m]))
        .collect();
    // The winding comes from the signed area: with loops the turning sum is rounding noise.
    let area: f64 = (0..m)
        .map(|i| lap[i].x * lap[(i + 1) % m].y - lap[i].y * lap[(i + 1) % m].x)
        .sum();
    let orient = if area >= 0.0 { 1.0 } else { -1.0 };
    let concave = smooth_circular(
        &turn
            .iter()
            .map(|t| (-orient * t).max(0.0))
            .collect::<Vec<_>>(),
        3,
    );
    let cusps: Vec<usize> = circular_peaks(&concave, 24, 6)
        .into_iter()
        .filter(|&i| concave[i] > 0.12)
        .collect();
    if cusps.len() < 3 {
        let ef = ellipse(p);
        let pts = ef.outline[..ef.outline.len() - 1].to_vec();
        return Fit {
            shape: Shape::Cloud(pts),
            outline: ef.outline,
            residual: ef.residual,
        };
    }
    let mut pts: Vec<V2> = Vec::new();
    for j in 0..cusps.len() {
        let (i0, i1) = (cusps[j], cusps[(j + 1) % cusps.len()]);
        let end = if i1 > i0 { i1 } else { i1 + m };
        let seg: Vec<V2> = (i0..end).map(|i| lap[i % m]).collect();
        let (a, b) = (lap[i0], lap[i1]);
        let chord = b - a;
        let normal = v(-chord.y, chord.x) / chord.len().max(1e-9);
        let dev: Vec<f64> = seg.iter().map(|&q| dot(q - a, normal).abs()).collect();
        let dev_max = fmax(dev.iter().copied());
        if dev_max < 0.06 * chord.len() {
            pts.push(a); // a flat stretch, such as a cloud's base, stays straight
            continue;
        }
        let far = seg[dev.iter().position(|&d| d == dev_max).unwrap_or(0)];
        let arc = arc3(a, far, b, 16);
        pts.extend_from_slice(&arc[..arc.len() - 1]);
    }
    let outline = closed(&pts);
    let residual = residual(p, &outline);
    Fit {
        shape: Shape::Cloud(pts),
        outline,
        residual,
    }
}

/// The ellipse under the cloud and its bump count, drawn as a regular scallop.
fn cloud_scallop(p: &[V2]) -> Fit {
    let ef = ellipse(p);
    let Shape::Ellipse { c, a, b, rot } = ef.shape else {
        return ef;
    };
    let (st, ct) = rot.sin_cos();
    let r: Vec<f64> = p
        .iter()
        .map(|&q| {
            let d = q - c;
            v((d.x * ct + d.y * st) / a, (-d.x * st + d.y * ct) / b).len()
        })
        .collect();
    let mu = r.iter().sum::<f64>() / r.len() as f64;
    let d: Vec<f64> = r.iter().map(|x| x - mu).collect();
    // np.convolve(d, ones(3)/3, "same"), zero-padded
    let rs: Vec<f64> = (0..d.len())
        .map(|i| {
            ((if i > 0 { d[i - 1] } else { 0.0 })
                + d[i]
                + (if i + 1 < d.len() { d[i + 1] } else { 0.0 }))
                / 3.0
        })
        .collect();
    let peaks = (1..rs.len() - 1)
        .filter(|&i| rs[i] > rs[i - 1] && rs[i] >= rs[i + 1] && rs[i] > 0.02)
        .count();
    let bumps = peaks.clamp(4, 16);
    let n = bumps * 16;
    let outline: Vec<V2> = (0..=n)
        .map(|i| {
            let t = 2.0 * PI * i as f64 / n as f64;
            let scallop = 1.0 + 0.12 * (t * bumps as f64 / 2.0).sin().abs();
            let (x, y) = (a * scallop * t.cos() * 0.9, b * scallop * t.sin() * 0.9);
            v(c.x + x * ct - y * st, c.y + x * st + y * ct)
        })
        .collect();
    let residual = residual(p, &outline);
    Fit {
        shape: Shape::Cloud(outline[..outline.len() - 1].to_vec()),
        outline,
        residual,
    }
}

fn cloud(p: &[V2]) -> Fit {
    let (a, s) = (cloud_arcs(p), cloud_scallop(p));
    if a.residual <= s.residual { a } else { s }
}

// ---------------------------------------------------------------------------------------------
// Snapping (`Rotation.snapped`)
// ---------------------------------------------------------------------------------------------

/// Lines and edges within this many degrees of horizontal or vertical snap onto the axis.
const AXIS: f64 = 5.0 * PI / 180.0;
/// Rotation step.
const STEP: f64 = 15.0 * PI / 180.0;
/// `min axis / max axis` at or above which an ellipse becomes a circle.
const CIRCLE_RATIO: f64 = 0.85;
/// `min side / max side` at or above which a rectangle becomes a square.
const SQUARE_RATIO: f64 = 0.9;
/// `(max side - min side) / max side` at or below which a polygon becomes regular.
const REGULAR_SPREAD: f64 = 0.15;
/// Leg spread at or below which a triangle becomes isosceles.
const ISOSCELES_SPREAD: f64 = 0.1;

fn nearest(angle: f64, period: f64) -> f64 {
    (angle / period).round() * period
}

fn step(angle: f64) -> f64 {
    nearest(angle, STEP)
}

/// The turn from `angle` onto the nearest axis, or `None` when larger than 5 degrees.
fn to_axis(angle: f64) -> Option<f64> {
    let target = nearest(angle, PI / 2.0);
    ((angle - target).abs() <= AXIS).then_some(target - angle)
}

fn snap(shape: Shape) -> Option<Shape> {
    Some(match shape {
        Shape::Line(a, b) => match to_axis((b - a).angle()) {
            Some(turn) => {
                let r = rotate(&[a, b], turn, (a + b) / 2.0);
                Shape::Line(r[0], r[1])
            }
            None => Shape::Line(a, b),
        },
        Shape::Poly(kind, vs) => {
            let want = match kind {
                Kind::Triangle => 3,
                Kind::Rectangle | Kind::Diamond => 4,
                Kind::Pentagon => 5,
                _ => 6,
            };
            if vs.len() != want {
                return None;
            }
            Shape::Poly(
                kind,
                match kind {
                    Kind::Rectangle => snap_rectangle(&vs),
                    Kind::Triangle => snap_triangle(&vs),
                    Kind::Diamond => snap_diamond(&vs),
                    _ => snap_polygon(&vs),
                },
            )
        }
        Shape::Ellipse { c, a, b, rot } => {
            if a > 0.0 && a.min(b) / a.max(b) >= CIRCLE_RATIO {
                let r = (a + b) / 2.0;
                Shape::Ellipse {
                    c,
                    a: r,
                    b: r,
                    rot: 0.0,
                }
            } else {
                let mut r = step(rot);
                while r <= -PI / 2.0 {
                    r += PI;
                }
                while r > PI / 2.0 {
                    r -= PI;
                }
                Shape::Ellipse { c, a, b, rot: r }
            }
        }
        Shape::Star {
            c,
            outer,
            inner,
            rot,
            k,
        } => {
            let period = 2.0 * PI / k as f64;
            Shape::Star {
                c,
                outer,
                inner,
                rot: (step(rot) % period + period) % period,
                k,
            }
        }
        other => other,
    })
}

fn snap_rectangle(p: &[V2]) -> Vec<V2> {
    let c = mean(p);
    let (mut w, mut h) = ((p[1] - p[0]).len(), (p[3] - p[0]).len());
    let a = (p[1] - p[0]).angle();
    if w.max(h) > 0.0 && w.min(h) / w.max(h) >= SQUARE_RATIO {
        w = (w + h) / 2.0;
        h = w;
    }
    let local = [
        v(-w / 2.0, -h / 2.0),
        v(w / 2.0, -h / 2.0),
        v(w / 2.0, h / 2.0),
        v(-w / 2.0, h / 2.0),
    ];
    rotate(&local, step(a), v(0.0, 0.0))
        .into_iter()
        .map(|q| q + c)
        .collect()
}

fn spread(sides: &[f64]) -> f64 {
    let (hi, lo) = (fmax(sides.iter().copied()), fmin(sides.iter().copied()));
    if hi > 0.0 {
        (hi - lo) / hi
    } else {
        f64::INFINITY
    }
}

/// A regular polygon around the centroid, with the mean radius, the vertices' circular-mean
/// phase, and the original vertex order and winding.
fn regular(p: &[V2]) -> Vec<V2> {
    let k = p.len();
    let c = mean(p);
    let r = p.iter().map(|&q| (q - c).len()).sum::<f64>() / k as f64;
    let wind = if cross(p[1] - p[0], p[2] - p[1]) >= 0.0 {
        1.0
    } else {
        -1.0
    };
    let (mut sx, mut sy) = (0.0_f64, 0.0_f64);
    for (i, &q) in p.iter().enumerate() {
        let a = (q - c).angle() - wind * i as f64 * 2.0 * PI / k as f64;
        sx += a.cos();
        sy += a.sin();
    }
    let base = sy.atan2(sx);
    (0..k)
        .map(|i| {
            let a = base + wind * i as f64 * 2.0 * PI / k as f64;
            c + v(a.cos(), a.sin()) * r
        })
        .collect()
}

/// Makes the two most similar legs equal, keeping their shared apex.
fn isosceles(p: &[V2]) -> Option<Vec<V2>> {
    let mut best: Option<(usize, f64)> = None;
    for apex in 0..3 {
        let l1 = (p[(apex + 1) % 3] - p[apex]).len();
        let l2 = (p[(apex + 2) % 3] - p[apex]).len();
        let sp = (l1 - l2).abs() / l1.max(l2).max(1e-12);
        if best.map_or(true, |b| sp < b.1) {
            best = Some((apex, sp));
        }
    }
    let (apex, sp) = best?;
    if sp > ISOSCELES_SPREAD {
        return None;
    }
    let a = p[apex];
    let l = ((p[(apex + 1) % 3] - a).len() + (p[(apex + 2) % 3] - a).len()) / 2.0;
    let mut out = p.to_vec();
    for k in [(apex + 1) % 3, (apex + 2) % 3] {
        let d = p[k] - a;
        out[k] = a + d / d.len().max(1e-12) * l;
    }
    Some(out)
}

fn snap_triangle(p: &[V2]) -> Vec<V2> {
    let sides = [
        (p[1] - p[0]).len(),
        (p[2] - p[1]).len(),
        (p[0] - p[2]).len(),
    ];
    let out = if spread(&sides) <= REGULAR_SPREAD {
        regular(p)
    } else {
        isosceles(p).unwrap_or_else(|| p.to_vec())
    };
    let edges: Vec<V2> = (0..3).map(|i| out[(i + 1) % 3] - out[i]).collect();
    let turns: Vec<f64> = edges
        .iter()
        .map(|e| nearest(e.angle(), PI / 2.0) - e.angle())
        .collect();
    let best = argmin(turns.iter().map(|t| t.abs()));
    if turns[best].abs() <= AXIS {
        rotate(&out, turns[best], mean(&out))
    } else {
        out
    }
}

/// The phase of a k-gon's vertices (any vertex's angle, modulo 2 pi / k).
fn phase(p: &[V2]) -> f64 {
    let k = p.len() as f64;
    let c = mean(p);
    let (mut sx, mut sy) = (0.0_f64, 0.0_f64);
    for &q in p {
        let a = (q - c).angle() * k;
        sx += a.cos();
        sy += a.sin();
    }
    sy.atan2(sx) / k
}

fn snap_polygon(p: &[V2]) -> Vec<V2> {
    let k = p.len();
    let sides: Vec<f64> = (0..k).map(|i| (p[(i + 1) % k] - p[i]).len()).collect();
    let out = if spread(&sides) <= REGULAR_SPREAD {
        regular(p)
    } else {
        p.to_vec()
    };
    let ph = phase(&out);
    rotate(&out, step(ph) - ph, mean(&out))
}

/// A rhombus with perpendicular diagonals, the diagonal angle rounded to a 15 degree step.
fn snap_diamond(p: &[V2]) -> Vec<V2> {
    let c = mean(p);
    let d1 = (p[0] - p[2]) / 2.0;
    let d2 = (p[1] - p[3]) / 2.0;
    let n = v(-d1.y, d1.x) / d1.len().max(1e-12);
    let d2 = n * if dot(d2, n) >= 0.0 {
        d2.len()
    } else {
        -d2.len()
    };
    let out = [c + d1, c + d2, c - d1, c - d2];
    let a = d1.angle();
    rotate(&out, step(a) - a, c)
}

// ---------------------------------------------------------------------------------------------
// Output polylines
// ---------------------------------------------------------------------------------------------

fn pt(q: V2) -> Point {
    Point {
        x: q.x as f32,
        y: q.y as f32,
    }
}

/// Barbs of an arrow head at `tip`, pointing back along the shaft that arrives from `from`.
fn barbs(tip: V2, from: V2, length: f64) -> [V2; 2] {
    let d = tip - from;
    let u = if d.len() > 0.0 {
        d / d.len()
    } else {
        v(1.0, 0.0)
    };
    let back = v(-u.x, -u.y);
    let spread = 0.45;
    [
        rotate(&[back], spread, v(0.0, 0.0))[0],
        rotate(&[back], -spread, v(0.0, 0.0))[0],
    ]
    .map(|b| tip + b * length)
}

fn into_recognized(shape: Shape) -> Option<Recognized> {
    let (kind, pts, closed): (Kind, Vec<V2>, bool) = match shape {
        Shape::Line(a, b) => (Kind::Line, vec![a, b], false),
        Shape::Curve(c) => (Kind::Curve, bezier(&c, 64), false),
        Shape::Arrow { ctrl, start, end } => {
            let shaft = bezier(&ctrl, 64);
            let chord = (ctrl[3] - ctrl[0]).len();
            let length = (0.2 * chord).clamp(10.0, 48.0).min(0.5 * chord);
            let samples = bezier(&ctrl, 33);
            let mut out = Vec::with_capacity(shaft.len() + 6);
            if start {
                let [l, r] = barbs(ctrl[0], samples[2], length);
                out.extend([l, ctrl[0], r]);
            }
            out.extend(&shaft);
            if end {
                let [l, r] = barbs(ctrl[3], samples[30], length);
                out.extend([l, ctrl[3], r, ctrl[3]]);
            }
            (Kind::Arrow, out, false)
        }
        Shape::Poly(kind, vs) => (kind, vs, true),
        Shape::Ellipse { c, a, b, rot } => {
            // Sagitta of at most ~0.25 px so large circles do not facet.
            let r = a.max(b);
            let n = if r > 0.25 {
                (PI / (1.0 - 0.25 / r).acos()).ceil() as usize
            } else {
                48
            };
            let n = n.clamp(48, 720);
            let mut pts = ellipse_outline(c, a, b, rot, n);
            pts.pop();
            (Kind::Ellipse, pts, true)
        }
        Shape::Star {
            c,
            outer,
            inner,
            rot,
            k,
        } => {
            let pts = (0..2 * k)
                .map(|i| {
                    let a = rot - PI / 2.0 + i as f64 * PI / k as f64;
                    let r = if i % 2 == 0 { outer } else { inner };
                    v(c.x + r * a.cos(), c.y + r * a.sin())
                })
                .collect();
            (Kind::Star, pts, true)
        }
        Shape::Heart { c, w, h } => (Kind::Heart, heart_template(c, w, h, 96, false), true),
        Shape::Cloud(pts) => (Kind::Cloud, pts, true),
    };
    if pts.len() < 2 || pts.iter().any(|q| !q.x.is_finite() || !q.y.is_finite()) {
        return None;
    }
    Some(Recognized {
        kind: kind.name().to_string(),
        points: pts.into_iter().map(pt).collect(),
        closed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/shapes-parity.json");

    struct Case {
        name: String,
        label: String,
        points: Vec<V2>,
        model_input: Vec<V2>,
        probs: Vec<f64>,
        head: Vec<f32>,
        fit: Option<serde_json::Value>,
    }

    fn pair(x: &serde_json::Value) -> V2 {
        v(x[0].as_f64().unwrap(), x[1].as_f64().unwrap())
    }

    fn cases() -> Vec<Case> {
        let root: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        root["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| Case {
                name: c["name"].as_str().unwrap().into(),
                label: c["label"].as_str().unwrap().into(),
                points: c["points"].as_array().unwrap().iter().map(pair).collect(),
                model_input: c["model_input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(pair)
                    .collect(),
                probs: c["probs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_f64().unwrap())
                    .collect(),
                head: c["head"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_f64().unwrap() as f32)
                    .collect(),
                fit: c.get("fit").cloned(),
            })
            .collect()
    }

    fn to_points(p: &[V2]) -> Vec<Point> {
        p.iter().map(|&q| pt(q)).collect()
    }

    /// Deterministic noise in [-1, 1).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 52) as f64 - 1.0
        }
    }

    fn jitter(path: &[V2], amp: f64, seed: u64) -> Vec<Point> {
        let mut r = Lcg(seed);
        path.iter()
            .map(|&q| pt(v(q.x + amp * r.next(), q.y + amp * r.next())))
            .collect()
    }

    /// Densely sampled closed polygon through `corners`, starting mid-edge like a hand does.
    fn polygon_stroke(corners: &[V2], per_edge: usize) -> Vec<V2> {
        let mut out = Vec::new();
        for i in 0..corners.len() {
            let (a, b) = (corners[i], corners[(i + 1) % corners.len()]);
            for j in 0..per_edge {
                out.push(a + (b - a) * (j as f64 / per_edge as f64));
            }
        }
        out.push(corners[0]);
        out
    }

    #[test]
    fn resample_is_evenly_spaced_and_keeps_ends() {
        let path = [v(0.0, 0.0), v(10.0, 0.0), v(10.0, 30.0)];
        let r = resample(&path, 9);
        assert_eq!((r[0], r[8]), (path[0], path[2]));
        for w in r.windows(2) {
            assert!(((w[1] - w[0]).len() - 5.0).abs() < 1e-9);
        }
        assert_eq!(resample(&[v(3.0, 4.0)], 4), vec![v(3.0, 4.0); 4]);
        assert_eq!(
            resample(&[v(1.0, 1.0), v(1.0, 1.0)], 3),
            vec![v(1.0, 1.0); 3]
        );
        assert!(resample(&[], 3).is_empty());
    }

    #[test]
    fn normalize_centres_bbox_and_scales_longer_side() {
        let n = normalize(&[v(10.0, 20.0), v(50.0, 30.0)]);
        assert_eq!(n, vec![v(-0.5, -0.125), v(0.5, 0.125)]);
    }

    #[test]
    fn preprocessing_matches_python_reference() {
        for c in cases() {
            let got = normalize(&resample_f32(&c.points, N));
            let worst = got
                .iter()
                .zip(&c.model_input)
                .map(|(a, b)| (a.x - b.x).abs().max((a.y - b.y).abs()))
                .fold(0.0, f64::max);
            assert!(worst <= 1e-5, "{}: model input off by {worst}", c.name);
        }
    }

    #[test]
    fn fitters_match_python_reference() {
        for c in cases()
            .iter()
            .filter(|c| c.fit.is_some() && c.label != "scribble")
        {
            let r = c.fit.as_ref().unwrap();
            let params = &r["params"];
            let kind = Kind::from_name(&c.label).unwrap();
            let p = resample_f32(&c.points, N);
            let got = fit(kind, &p, &c.head, 0.06);
            let tol = 0.005 * size(&c.points);
            let name = &c.name;
            let want_resid = r["residual"].as_f64().unwrap();
            assert!(
                (got.residual - want_resid).abs() <= 5e-4,
                "{name}: residual {} vs {want_resid}",
                got.residual
            );
            let close = |a: V2, b: V2, what: &str| {
                assert!((a - b).len() <= tol, "{name}: {what} {a:?} vs {b:?}")
            };
            match (&got.shape, r["kind"].as_str().unwrap()) {
                (Shape::Line(a, b), "line") => {
                    close(*a, pair(&params["from"]), "from");
                    close(*b, pair(&params["to"]), "to");
                }
                (Shape::Curve(c4), "curve") => {
                    for (i, key) in ["from", "c1", "c2", "to"].iter().enumerate() {
                        close(c4[i], pair(&params[key]), key);
                    }
                }
                (Shape::Arrow { start, end, .. }, "arrow") => {
                    let heads = params["heads"].as_array().unwrap();
                    let at = |s: &str| heads.iter().any(|h| h["at"] == s);
                    assert_eq!((*start, *end), (at("start"), at("end")), "{name}: heads");
                    for h in heads {
                        let tip = if h["at"] == "end" {
                            got.outline[got.outline.len() - 1]
                        } else {
                            got.outline[0]
                        };
                        close(tip, pair(&h["tip"]), "tip");
                    }
                }
                (Shape::Ellipse { c: ctr, a, b, .. }, "ellipse") => {
                    close(*ctr, pair(&params["center"]), "center");
                    assert!(
                        (a - params["semi_major"].as_f64().unwrap()).abs() <= tol,
                        "{name}: semi_major"
                    );
                    assert!(
                        (b - params["semi_minor"].as_f64().unwrap()).abs() <= tol,
                        "{name}: semi_minor"
                    );
                }
                (
                    Shape::Poly(_, vs),
                    "rectangle" | "triangle" | "diamond" | "pentagon" | "hexagon",
                ) => {
                    let key = if r["kind"] == "rectangle" {
                        "corners"
                    } else {
                        "vertices"
                    };
                    let want: Vec<V2> = params[key].as_array().unwrap().iter().map(pair).collect();
                    assert_eq!(vs.len(), want.len(), "{name}: vertex count");
                    for (a, b) in vs.iter().zip(&want) {
                        close(*a, *b, "vertex");
                    }
                }
                (
                    Shape::Star {
                        c: ctr,
                        outer,
                        inner,
                        k,
                        ..
                    },
                    "star",
                ) => {
                    close(*ctr, pair(&params["center"]), "center");
                    assert_eq!(
                        *k as u64,
                        params["point_count"].as_u64().unwrap(),
                        "{name}: points"
                    );
                    assert!(
                        (outer - params["outer_radius"].as_f64().unwrap()).abs() <= tol,
                        "{name}: outer"
                    );
                    assert!(
                        (inner - params["inner_radius"].as_f64().unwrap()).abs() <= tol,
                        "{name}: inner"
                    );
                }
                (Shape::Heart { c: ctr, w, h }, "heart") => {
                    close(*ctr, pair(&params["center"]), "center");
                    assert!(
                        (w - params["width"].as_f64().unwrap()).abs() <= tol,
                        "{name}: width"
                    );
                    assert!(
                        (h - params["height"].as_f64().unwrap()).abs() <= tol,
                        "{name}: height"
                    );
                }
                (Shape::Cloud(_), "cloud") => {}
                (s, k) => panic!("{name}: fitted {s:?}, reference {k}"),
            }
        }
    }

    #[test]
    fn inference_matches_python_reference() {
        let mut rec = Recognizer::new().unwrap();
        for c in cases() {
            let (probs, head) = rec.infer(&resample_f32(&c.points, N)).unwrap();
            let worst = probs
                .iter()
                .zip(&c.probs)
                .map(|(a, b)| (*a as f64 - b).abs())
                .fold(0.0, f64::max);
            assert!(
                worst <= 0.05,
                "{}: probs off by {worst}: {probs:?} vs {:?}",
                c.name,
                c.probs
            );
            let top = |p: &[f64]| (0..p.len()).max_by(|&a, &b| p[a].total_cmp(&p[b])).unwrap();
            let got: Vec<f64> = probs.iter().map(|&x| x as f64).collect();
            assert_eq!(top(&got), top(&c.probs), "{}: class", c.name);
            let head_worst = head
                .iter()
                .zip(&c.head)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f32::max);
            assert!(head_worst <= 0.1, "{}: head off by {head_worst}", c.name);
        }
    }
    /// Each reference stroke is accepted exactly when the upstream pipeline accepts it: the
    /// reference probabilities and Python fit residual checked against the calibrated gates of
    /// `shapes_meta.json`. A stroke within the parity tolerance of a gate may land either side.
    #[test]
    fn recognizes_reference_strokes_end_to_end() {
        let meta = Meta::parse(META).unwrap();
        let mut rec = Recognizer::new().unwrap();
        let (mut accepted, mut rejected) = (0, 0);
        for c in cases() {
            let top = (0..c.probs.len())
                .max_by(|&a, &b| c.probs[a].total_cmp(&c.probs[b]))
                .unwrap();
            let out = rec.recognize(&to_points(&c.points)).unwrap();
            let (expected, borderline): (Option<&str>, bool) = match meta.kinds[top] {
                None | Some(Kind::Scribble) => (None, false),
                Some(_) if meta.reject[top] => (None, false),
                Some(kind) => match (&c.fit, Kind::from_name(&c.label)) {
                    (Some(f), Some(label)) if label == kind => {
                        let gate = meta.gates.get(&kind).copied().unwrap_or_default();
                        let resid = f["residual"].as_f64().unwrap();
                        let ok = c.probs[top] >= gate.conf && resid <= gate.resid;
                        let near = (c.probs[top] - gate.conf).abs() < 0.01
                            || (resid - gate.resid).abs() < 5e-4;
                        (ok.then(|| f["kind"].as_str().unwrap()), near)
                    }
                    // the fixture only carries the Python fit of the labelled kind
                    _ => continue,
                },
            };
            match (expected, &out) {
                (Some(kind), Some(r)) => {
                    assert_eq!(r.kind, kind, "{}", c.name);
                    assert_eq!(
                        r.closed,
                        !matches!(kind, "line" | "curve" | "arrow"),
                        "{}",
                        c.name
                    );
                    accepted += 1;
                }
                (None, None) => rejected += 1,
                _ if borderline => {}
                (Some(kind), None) => panic!("{}: expected {kind}, ink was kept", c.name),
                (None, Some(r)) => panic!("{}: expected ink kept, recognised {}", c.name, r.kind),
            }
        }
        assert!(
            accepted > 0 && rejected > 0,
            "{accepted} accepted, {rejected} rejected"
        );
    }

    #[test]
    fn rough_circle_snaps_to_circle() {
        let mut rec = Recognizer::new().unwrap();
        let ring: Vec<V2> = (0..=64)
            .map(|i| {
                let t = 2.0 * PI * i as f64 / 64.0 * 1.04;
                v(400.0 + 150.0 * t.cos(), 300.0 + 140.0 * t.sin())
            })
            .collect();
        let r = rec
            .recognize(&jitter(&ring, 2.0, 7))
            .unwrap()
            .expect("circle");
        assert_eq!(r.kind, "ellipse");
        assert!(r.closed);
        let c = (
            r.points.iter().map(|p| p.x as f64).sum::<f64>() / r.points.len() as f64,
            r.points.iter().map(|p| p.y as f64).sum::<f64>() / r.points.len() as f64,
        );
        let radii: Vec<f64> = r
            .points
            .iter()
            .map(|p| ((p.x as f64 - c.0).powi(2) + (p.y as f64 - c.1).powi(2)).sqrt())
            .collect();
        let (lo, hi) = (fmin(radii.iter().copied()), fmax(radii.iter().copied()));
        assert!(
            hi - lo < 1e-2,
            "snapped circle must have a single radius, got {lo}..{hi}"
        );
        assert!((hi - 145.0).abs() < 8.0);
        assert!((c.0 - 400.0).abs() < 8.0 && (c.1 - 300.0).abs() < 8.0);
    }

    #[test]
    fn rough_rectangle_snaps_to_axis_aligned_rectangle() {
        let mut rec = Recognizer::new().unwrap();
        let corners = [
            v(100.0, 100.0),
            v(500.0, 108.0),
            v(497.0, 330.0),
            v(96.0, 322.0),
        ];
        let r = rec
            .recognize(&jitter(&polygon_stroke(&corners, 30), 1.5, 3))
            .unwrap()
            .expect("rectangle");
        assert_eq!(r.kind, "rectangle");
        assert_eq!(r.points.len(), 4);
        for i in 0..4 {
            let (a, b) = (r.points[i], r.points[(i + 1) % 4]);
            let ang = ((b.y - a.y) as f64)
                .atan2((b.x - a.x) as f64)
                .to_degrees()
                .rem_euclid(90.0);
            assert!(
                ang < 0.01 || ang > 89.99,
                "edge {i} not axis aligned: {ang}"
            );
        }
    }

    #[test]
    fn rough_triangle_star_and_line() {
        let mut rec = Recognizer::new().unwrap();
        let tri = [v(300.0, 100.0), v(520.0, 460.0), v(80.0, 440.0)];
        let r = rec
            .recognize(&jitter(&polygon_stroke(&tri, 40), 1.5, 5))
            .unwrap()
            .expect("triangle");
        assert_eq!((r.kind.as_str(), r.points.len()), ("triangle", 3));

        let star: Vec<V2> = (0..5)
            .map(|i| {
                let a = -PI / 2.0 + i as f64 * 4.0 * PI / 5.0;
                v(300.0 + 200.0 * a.cos(), 300.0 + 200.0 * a.sin())
            })
            .collect();
        let r = rec
            .recognize(&jitter(&polygon_stroke(&star, 30), 1.5, 9))
            .unwrap()
            .expect("star");
        assert_eq!((r.kind.as_str(), r.points.len()), ("star", 10));

        let line: Vec<V2> = (0..60)
            .map(|i| v(100.0 + 6.0 * i as f64, 200.0 + 0.3 * i as f64))
            .collect();
        let r = rec
            .recognize(&jitter(&line, 1.0, 11))
            .unwrap()
            .expect("line");
        assert_eq!(r.kind, "line");
        assert_eq!(r.points.len(), 2);
        assert_eq!(
            r.points[0].y, r.points[1].y,
            "nearly horizontal lines snap to the axis"
        );
        assert!(!r.closed);
    }

    #[test]
    fn rejects_scribbles_taps_and_malformed_strokes() {
        let mut rec = Recognizer::new().unwrap();
        let mut r = Lcg(42);
        let scribble: Vec<Point> = (0..200)
            .map(|i| {
                pt(v(
                    100.0 + 3.0 * i as f64 + 40.0 * r.next(),
                    200.0 + 80.0 * r.next(),
                ))
            })
            .collect();
        assert!(rec.recognize(&scribble).unwrap().is_none());
        assert!(rec.recognize(&[]).unwrap().is_none());
        assert!(
            rec.recognize(&[Point { x: 1.0, y: 1.0 }])
                .unwrap()
                .is_none()
        );
        let tap = [Point { x: 1.0, y: 1.0 }, Point { x: 3.0, y: 2.0 }];
        assert!(rec.recognize(&tap).unwrap().is_none());
        let same = vec![Point { x: 5.0, y: 5.0 }; 50];
        assert!(rec.recognize(&same).unwrap().is_none());
        let nan = [
            Point {
                x: f32::NAN,
                y: 0.0,
            },
            Point {
                x: f32::INFINITY,
                y: 1.0,
            },
            Point {
                x: 0.0,
                y: f32::NEG_INFINITY,
            },
        ];
        assert!(rec.recognize(&nan).unwrap().is_none());
        let huge = [
            Point { x: 0.0, y: 0.0 },
            Point {
                x: 3.0e38,
                y: 1.0e38,
            },
            Point { x: -3.0e38, y: 2.0 },
        ];
        assert!(rec.recognize(&huge).unwrap().is_none());
        // NaN points inside an otherwise good stroke are skipped, not fatal
        let ring: Vec<V2> = (0..=64)
            .map(|i| {
                let t = 2.0 * PI * i as f64 / 64.0;
                v(400.0 + 150.0 * t.cos(), 300.0 + 150.0 * t.sin())
            })
            .collect();
        let mut pts = jitter(&ring, 1.0, 1);
        pts.insert(
            10,
            Point {
                x: f32::NAN,
                y: f32::NAN,
            },
        );
        assert_eq!(rec.recognize(&pts).unwrap().unwrap().kind, "ellipse");
    }

    /// Class scores for the loaded model: `none` (always reject) and the given kinds, rest 0.
    fn scores(meta: &Meta, none: f32, kinds: &[(Kind, f32)]) -> Vec<f32> {
        let mut probs = vec![0.0; meta.kinds.len()];
        probs[0] = none;
        for &(kind, s) in kinds {
            probs[meta.kinds.iter().position(|k| *k == Some(kind)).unwrap()] = s;
        }
        probs
    }

    fn assert_sane(name: &str, accepted: Option<&Recognized>, alts: &[Recognized]) {
        assert!(
            alts.len() <= MAX_ALTERNATIVES,
            "{name}: {} alternatives",
            alts.len()
        );
        let mut seen: HashSet<&str> = accepted.map(|a| a.kind.as_str()).into_iter().collect();
        for a in alts {
            assert!(seen.insert(&a.kind), "{name}: duplicate kind {}", a.kind);
            assert!(usable(a), "{name}: unusable {}", a.kind);
            assert!(a.points.iter().all(|q| q.x.is_finite() && q.y.is_finite()));
            assert_eq!(
                a.closed,
                !matches!(a.kind.as_str(), "line" | "curve" | "arrow")
            );
        }
    }

    /// A low-confidence class must be offered but never applied, and its geometry must be
    /// exactly what the same class yields once the model is confident.
    #[test]
    fn low_confidence_candidate_is_an_alternative_with_accepted_geometry() {
        let meta = Meta::parse(META).unwrap();
        for c in cases()
            .iter()
            .filter(|c| c.fit.is_some() && c.label != "scribble")
        {
            let kind = Kind::from_name(&c.label).unwrap();
            let p = resample_f32(&c.points, N);
            let low = scores(&meta, 0.6, &[(kind, 0.3)]);
            assert!(meta.accept(&p, &low, &c.head).is_none(), "{}", c.name);
            let alts = meta.alternatives(&p, &low, &c.head, None);
            assert_sane(&c.name, None, &alts);
            assert_eq!(alts.len(), 1, "{}: {alts:?}", c.name);
            assert_eq!(alts[0].kind, c.fit.as_ref().unwrap()["kind"], "{}", c.name);
            let high = scores(&meta, 0.0, &[(kind, 0.95)]);
            let accepted = meta.accept(&p, &high, &c.head);
            assert_eq!(accepted.as_ref(), Some(&alts[0]), "{}", c.name);
            // the accepted kind is never repeated as a choice
            assert!(
                meta.alternatives(&p, &high, &c.head, accepted.as_ref())
                    .is_empty()
            );
        }
    }

    #[test]
    fn alternatives_follow_ranking_and_never_follow_a_scribble() {
        let meta = Meta::parse(META).unwrap();
        let all = |lead: f32| -> Vec<f32> {
            (0..meta.kinds.len())
                .map(|i| lead - 0.01 * i as f32)
                .collect()
        };
        for c in cases().iter().filter(|c| c.fit.is_some()) {
            let p = resample_f32(&c.points, N);
            // every class plausible: whatever fits, in at most three distinct kinds
            let probs = all(0.2);
            let alts = meta.alternatives(&p, &probs, &c.head, None);
            assert_sane(&c.name, None, &alts);
            // a scribble-led stroke is an erase gesture, never a shape
            let mut scribble = all(0.2);
            *scribble.last_mut().unwrap() = 0.5;
            assert!(meta.accept(&p, &scribble, &c.head).is_none(), "{}", c.name);
            assert!(
                meta.alternatives(&p, &scribble, &c.head, None).is_empty(),
                "{}",
                c.name
            );
            // noise-level scores are not worth offering
            let flat = vec![0.004; meta.kinds.len()];
            assert!(
                meta.alternatives(&p, &flat, &c.head, None).is_empty(),
                "{}",
                c.name
            );
        }
        // ranking: the ellipse stroke is scored below `rectangle`, so it can only come after it
        let c = cases().into_iter().find(|c| c.name == "ellipse_0").unwrap();
        let p = resample_f32(&c.points, N);
        let probs = scores(&meta, 0.5, &[(Kind::Rectangle, 0.3), (Kind::Ellipse, 0.2)]);
        let alts = meta.alternatives(&p, &probs, &c.head, None);
        assert_eq!(alts.last().map(|a| a.kind.as_str()), Some("ellipse"));
        if alts.len() == 2 {
            assert_eq!(alts[0].kind, "rectangle", "higher model score lists first");
        }
    }

    /// `recognize_choices` accepts exactly what `recognize` accepts, for every reference stroke.
    #[test]
    fn recognize_choices_matches_recognize_on_reference_strokes() {
        let mut rec = Recognizer::new().unwrap();
        for c in cases() {
            let pts = to_points(&c.points);
            let want = rec.recognize(&pts).unwrap();
            let got = rec.recognize_choices(&pts).unwrap();
            assert_eq!(got.accepted, want, "{}", c.name);
            assert_sane(&c.name, got.accepted.as_ref(), &got.alternatives);
            if c.label == "scribble" {
                assert_eq!(
                    got,
                    Recognition::default(),
                    "{}: scribble offered shapes",
                    c.name
                );
            }
        }
    }

    #[test]
    fn recognize_choices_offers_nothing_for_taps_and_malformed_strokes() {
        let mut rec = Recognizer::new().unwrap();
        let p = |x: f32, y: f32| Point { x, y };
        let strokes: [Vec<Point>; 5] = [
            vec![],
            vec![p(1.0, 1.0)],
            vec![p(1.0, 1.0), p(3.0, 2.0)],
            vec![p(5.0, 5.0); 50],
            vec![p(f32::NAN, 0.0), p(0.0, f32::INFINITY), p(3.0e38, 1.0)],
        ];
        for s in &strokes {
            assert_eq!(rec.recognize_choices(s).unwrap(), Recognition::default());
        }
    }

    #[test]
    fn point_and_recognized_round_trip_through_serde() {
        let r = Recognized {
            kind: "line".into(),
            points: vec![Point { x: 1.5, y: -2.0 }, Point { x: 3.0, y: 4.25 }],
            closed: false,
        };
        let back: Recognized = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn snapping_rules() {
        // ellipse within 0.85 axis ratio becomes a circle; otherwise rotation snaps to 15 degrees
        let c = v(0.0, 0.0);
        match snap(Shape::Ellipse {
            c,
            a: 100.0,
            b: 90.0,
            rot: 0.3,
        })
        .unwrap()
        {
            Shape::Ellipse { a, b, rot, .. } => assert_eq!((a, b, rot), (95.0, 95.0, 0.0)),
            _ => unreachable!(),
        }
        match snap(Shape::Ellipse {
            c,
            a: 100.0,
            b: 40.0,
            rot: 20f64.to_radians(),
        })
        .unwrap()
        {
            Shape::Ellipse { rot, .. } => assert!((rot - 15f64.to_radians()).abs() < 1e-12),
            _ => unreachable!(),
        }
        // near-square rectangle becomes a square
        let rect = vec![v(0.0, 0.0), v(100.0, 0.0), v(100.0, 95.0), v(0.0, 95.0)];
        let Shape::Poly(_, sq) = snap(Shape::Poly(Kind::Rectangle, rect)).unwrap() else {
            unreachable!()
        };
        assert!(((sq[1] - sq[0]).len() - (sq[3] - sq[0]).len()).abs() < 1e-9);
        // a line 4 degrees off horizontal snaps flat, 8 degrees stays
        let off = |deg: f64| {
            snap(Shape::Line(
                v(0.0, 0.0),
                v(
                    100.0 * deg.to_radians().cos(),
                    100.0 * deg.to_radians().sin(),
                ),
            ))
            .unwrap()
        };
        let Shape::Line(a, b) = off(4.0) else {
            unreachable!()
        };
        assert!((a.y - b.y).abs() < 1e-9);
        let Shape::Line(a, b) = off(8.0) else {
            unreachable!()
        };
        assert!((a.y - b.y).abs() > 1.0);
        // an incomplete polygon is dropped rather than drawn
        assert!(snap(Shape::Poly(Kind::Hexagon, vec![v(0.0, 0.0); 4])).is_none());
    }

    #[test]
    fn arrow_polyline_has_shaft_and_heads() {
        let ctrl = straight_ctrl(v(0.0, 0.0), v(200.0, 0.0));
        let r = into_recognized(Shape::Arrow {
            ctrl,
            start: false,
            end: true,
        })
        .unwrap();
        assert_eq!(r.kind, "arrow");
        assert!(!r.closed);
        let n = r.points.len();
        assert_eq!(n, 64 + 4);
        let tip = r.points[63];
        assert_eq!((tip.x, tip.y), (200.0, 0.0));
        // barbs lie behind the tip, symmetric about the shaft
        assert!(r.points[64].x < 200.0 && r.points[66].x < 200.0);
        assert!((r.points[64].y + r.points[66].y).abs() < 1e-3);
    }
}
