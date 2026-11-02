use crate::presentation::{Presentation, rgba_from, surface_from};
use crate::shapes::Point;
use anyhow::{Context, Result, bail, ensure};
use gtk::cairo::{
    Context as Cairo, Extend, Filter, Format, ImageSurface, LineCap, LineJoin, Matrix, Operator,
    SurfacePattern,
};
use gtk::pango::prelude::FontMapExt;
use gtk::pango::{Alignment, FontDescription, Layout, SCALE, Style, Weight, WrapMode};
use image::{ImageEncoder, RgbaImage};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::BufWriter,
    path::Path,
    rc::Rc,
};

/// Resource bounds shared by `Document::from_parts` and the mutators; external project data is untrusted.
pub const MAX_MARKS: usize = 10_000;
pub const MAX_POINTS: usize = 200_000;
const MAX_TOTAL_POINTS: usize = 2_000_000;
const MAX_DIM: u32 = 32_767;
const MAX_PIXELS: u64 = 1 << 27;
const MAX_COORD: f32 = 1.0e7;
const MAX_ID: u64 = 1 << 48;
pub const MAX_TEXT: usize = 20_000;
const MAX_STEP: u32 = 1_000_000;
const MAX_SCALE: f32 = 1.0e4;
const MAX_TRANSLATE: f32 = 1.0e8;
/// Line widths and arrowhead multipliers a mark may carry.
pub const MAX_WIDTH: f64 = 128.0;
/// Thinnest line a mark may carry; widths are decimal.
pub const MIN_WIDTH: f64 = 0.1;
pub const MAX_HEAD_SIZE: f64 = 16.0;
/// Text sizes in local units; `DEFAULT_FONT` is what constructors assign.
pub const MAX_FONT: f64 = 2_000.0;
pub const DEFAULT_FONT: f64 = 24.0;
/// Arrowhead length in pixels at `head_size` 1.
pub const HEAD_BASE: f64 = 12.0;
/// Smallest wrapping width of a text box in local units; `0` means unwrapped.
pub const MIN_BOX_WIDTH: f32 = 16.0;
/// Copies made by `duplicate_marks` are shifted by this much on both axes.
pub const DUPLICATE_OFFSET: f32 = 16.0;
const TEXT_PAD: f32 = 4.0;
/// Outward pad on x spans interpolated along slanted edges, so f64 rounding can never shave a
/// partially covered pixel off the redaction. Vertex coordinates are used exactly.
const COVER_PAD: f64 = 1e-6;
/// Family every mark starts with; Pango resolves it to the desktop's default sans.
pub const DEFAULT_FAMILY: &str = "Sans";
/// Pango style a mark starts with, and the longest style string it may carry.
pub const DEFAULT_FONT_STYLE: &str = "Regular";
pub const MAX_FONT_STYLE: usize = 64;
/// Longest family list (`"A, B, C"`: first match wins) a mark or preset may carry.
pub const MAX_FAMILY: usize = 256;
/// Largest rectangle corner radius a mark or preset may carry, in local units.
pub const MAX_RADIUS: f64 = MAX_COORD as f64;
/// Stroke miter limit of the square-ended style: a corner tip reaches at most one line width.
const SHARP_MITER: f64 = 2.0;
/// Bézier handle length, as a fraction of the radius, of a quarter circle.
const KAPPA: f64 = 0.552_284_749_830_793_4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Crop {
    pub fn between(a: Point, b: Point, width: u32, height: u32) -> Option<Self> {
        let x = a.x.min(b.x).floor().clamp(0.0, width as f32) as u32;
        let y = a.y.min(b.y).floor().clamp(0.0, height as f32) as u32;
        let right = a.x.max(b.x).ceil().clamp(0.0, width as f32) as u32;
        let bottom = a.y.max(b.y).ceil().clamp(0.0, height as f32) as u32;
        (right > x && bottom > y).then_some(Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Bounds {
    /// Top-left, top-right, bottom-right, bottom-left.
    pub fn corners(&self) -> [Point; 4] {
        let (r, b) = (self.x + self.width, self.y + self.height);
        [
            Point {
                x: self.x,
                y: self.y,
            },
            Point { x: r, y: self.y },
            Point { x: r, y: b },
            Point { x: self.x, y: b },
        ]
    }

    pub fn union(self, o: Bounds) -> Bounds {
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        Bounds {
            x,
            y,
            width: (self.x + self.width).max(o.x + o.width) - x,
            height: (self.y + self.height).max(o.y + o.height) - y,
        }
    }

    fn finite(self) -> Option<Self> {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            .then_some(self)
    }
}

/// `x' = a*x + c*y + tx`, `y' = b*x + d*y + ty` (Cairo's xx, yx, xy, yy, x0, y0).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Default for Affine {
    fn default() -> Self {
        Self::identity()
    }
}

impl From<Affine> for Matrix {
    fn from(m: Affine) -> Matrix {
        Matrix::new(
            m.a.into(),
            m.b.into(),
            m.c.into(),
            m.d.into(),
            m.tx.into(),
            m.ty.into(),
        )
    }
}

impl Affine {
    pub const fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: 0.0,
            ty: 0.0,
        }
    }
    pub fn translation(p: Point) -> Self {
        Self {
            tx: p.x,
            ty: p.y,
            ..Self::identity()
        }
    }
    pub fn scale_about(sx: f32, sy: f32, center: Point) -> Self {
        Self {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            tx: center.x - sx * center.x,
            ty: center.y - sy * center.y,
        }
    }
    pub fn rotation_about(radians: f32, center: Point) -> Self {
        let (s, k) = radians.sin_cos();
        Self {
            a: k,
            b: s,
            c: -s,
            d: k,
            tx: center.x - (k * center.x - s * center.y),
            ty: center.y - (s * center.x + k * center.y),
        }
    }
    pub fn apply(&self, p: Point) -> Point {
        let (x, y) = (f64::from(p.x), f64::from(p.y));
        Point {
            x: (f64::from(self.a) * x + f64::from(self.c) * y + f64::from(self.tx)) as f32,
            y: (f64::from(self.b) * x + f64::from(self.d) * y + f64::from(self.ty)) as f32,
        }
    }
    /// `self * other`: `other` is applied first. Document-wide transforms pre-multiply a mark's own
    /// transform: `new = matrix.multiply(mark.transform)`.
    pub fn multiply(self, o: Affine) -> Affine {
        let f = |v: f32| f64::from(v);
        Affine {
            a: (f(self.a) * f(o.a) + f(self.c) * f(o.b)) as f32,
            b: (f(self.b) * f(o.a) + f(self.d) * f(o.b)) as f32,
            c: (f(self.a) * f(o.c) + f(self.c) * f(o.d)) as f32,
            d: (f(self.b) * f(o.c) + f(self.d) * f(o.d)) as f32,
            tx: (f(self.a) * f(o.tx) + f(self.c) * f(o.ty) + f(self.tx)) as f32,
            ty: (f(self.b) * f(o.tx) + f(self.d) * f(o.ty) + f(self.ty)) as f32,
        }
    }
    /// `self` first, then `next`.
    pub fn then(self, next: Affine) -> Affine {
        next.multiply(self)
    }
    pub fn det(&self) -> f64 {
        f64::from(self.a) * f64::from(self.d) - f64::from(self.b) * f64::from(self.c)
    }
    pub fn inverse(&self) -> Option<Affine> {
        let det = self.det();
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let f = |v: f32| f64::from(v);
        let inv = Affine {
            a: (f(self.d) / det) as f32,
            b: (-f(self.b) / det) as f32,
            c: (-f(self.c) / det) as f32,
            d: (f(self.a) / det) as f32,
            tx: ((f(self.c) * f(self.ty) - f(self.d) * f(self.tx)) / det) as f32,
            ty: ((f(self.b) * f(self.tx) - f(self.a) * f(self.ty)) / det) as f32,
        };
        [inv.a, inv.b, inv.c, inv.d, inv.tx, inv.ty]
            .iter()
            .all(|v| v.is_finite())
            .then_some(inv)
    }
    /// Finite, invertible and within the bounds a drawable, hit-testable transform may have.
    pub fn is_sane(&self) -> bool {
        [self.a, self.b, self.c, self.d]
            .iter()
            .all(|v| v.is_finite() && v.abs() <= MAX_SCALE)
            && [self.tx, self.ty]
                .iter()
                .all(|v| v.is_finite() && v.abs() <= MAX_TRANSLATE)
            && self.det().abs() >= 1e-8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub mark: u64,
    /// Normalised position in the target's untransformed local bounds.
    pub u: f32,
    pub v: f32,
}

/// `point` is local to the connector (under its transform); it is the live position of a free end and
/// the last resolved position of an attached one. Attached ends follow the target's current transform.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub point: Point,
    pub anchor: Option<Anchor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MarkKind {
    Path,
    /// Origin is `points[0]`; `box_width` wraps the text (0 = unwrapped).
    Text {
        value: String,
        box_width: f32,
        background: bool,
    },
    /// Geometry lives in the endpoints; `Mark::points` is ignored.
    Connector {
        start: Endpoint,
        end: Endpoint,
    },
    /// `points` = [top-left, bottom-right].
    Redaction,
    /// Same box as `Redaction`, covered with an opaque grey-blue mosaic of crisp blocks instead of
    /// black. The mosaic is a fixed pattern: it is never computed from the screenshot, so it
    /// reveals nothing of it. (The serialized name is historical: it was a blur once.)
    PrivacyBlur,
    /// Centre is `points[0]`; `width` sets the radius.
    Step {
        number: u32,
    },
    /// `points` = [top-left, bottom-right] of the lens. It shows the screenshot around the centre of
    /// `source` at `zoom`; `source` itself stays in screenshot space when the lens is transformed.
    Magnifier {
        source: Bounds,
        zoom: f32,
    },
    /// `points` = [top-left, bottom-right]. The scene outside the union of all spotlights is dimmed
    /// by `dim` (0..=1); `radius` rounds the corners. Painted below the privacy masks.
    Spotlight {
        radius: f64,
        dim: f64,
    },
    /// `points` = [top-left, bottom-right]: a filled rectangle in `color` (alpha included) drawn in
    /// z-order like any annotation, e.g. a text highlight. Privacy masks stay above it.
    Highlight,
    /// Freehand marker: `points` is a polyline stroked at `width` in `color` (alpha included) as
    /// one shape, so a stroke crossing itself never darkens. Never closed and never recognised.
    Highlighter,
}

/// Horizontal alignment of the lines of a text mark inside its box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Decoration painted behind the glyphs of a caption, which stay in the mark colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextEffect {
    #[default]
    None,
    /// Outline in the inverse of the text colour.
    InvertedOutline,
    /// Outline in black or white, whichever contrasts with the text colour.
    ContrastOutline,
    /// Black copy at half opacity, offset down and to the right.
    Shadow,
}

impl TextEffect {
    pub const ALL: [TextEffect; 4] = [
        Self::None,
        Self::InvertedOutline,
        Self::ContrastOutline,
        Self::Shadow,
    ];

    /// Next effect in the cycle None, inverted outline, contrast outline, shadow, None.
    pub fn next(self) -> Self {
        let at = Self::ALL.iter().position(|e| *e == self).unwrap_or(0);
        Self::ALL[(at + 1) % Self::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No effect",
            Self::InvertedOutline => "Inverted outline",
            Self::ContrastOutline => "Contrast outline",
            Self::Shadow => "Drop shadow",
        }
    }

    fn outline_width(size: f64) -> f64 {
        (size * 0.12).max(1.0)
    }

    fn shadow_offset(size: f64) -> f64 {
        (size / 16.0).max(1.0)
    }

    /// How far the effect paints beyond the glyph box of text `size` pixels tall.
    fn reach(self, size: f64) -> f32 {
        (match self {
            Self::None => 0.0,
            Self::InvertedOutline | Self::ContrastOutline => Self::outline_width(size) / 2.0,
            Self::Shadow => Self::shadow_offset(size),
        }) as f32
    }

    /// Paints the effect of `layout` placed at `origin`, under the glyphs the caller draws next.
    fn paint(
        self,
        cr: &Cairo,
        layout: &Layout,
        origin: &Point,
        color: &[f64; 4],
        size: f64,
    ) -> Result<()> {
        let contrast = if luminance(color) > 0.5 { 0.0 } else { 1.0 };
        let (x, y) = (f64::from(origin.x), f64::from(origin.y));
        match self {
            Self::None => {}
            Self::Shadow => {
                let d = Self::shadow_offset(size);
                cr.set_source_rgba(0.0, 0.0, 0.0, color[3] * 0.5);
                cr.move_to(x + d, y + d);
                pangocairo::functions::show_layout(cr, layout);
            }
            Self::InvertedOutline | Self::ContrastOutline => {
                let rgb = if self == Self::InvertedOutline {
                    [1.0 - color[0], 1.0 - color[1], 1.0 - color[2]]
                } else {
                    [contrast; 3]
                };
                cr.set_source_rgba(rgb[0], rgb[1], rgb[2], color[3]);
                cr.set_line_width(Self::outline_width(size));
                cr.set_line_join(LineJoin::Round);
                cr.move_to(x, y);
                pangocairo::functions::layout_path(cr, layout);
                cr.stroke()?;
            }
        }
        cr.set_source_rgba(color[0], color[1], color[2], color[3]);
        Ok(())
    }
}

impl Mark {
    /// Radius and stroke width of the outer ring of a step marker.
    fn ring(&self) -> (f64, f64) {
        let r = step_radius(self.width);
        (r * 1.3, (r * 0.12).max(1.0))
    }

    /// Outer radius of a step marker, ring included.
    fn step_outer(&self) -> f64 {
        if self.step_ring {
            let (r, w) = self.ring();
            r + w / 2.0
        } else {
            step_radius(self.width)
        }
    }
}

/// Rich style of a text mark. Marks stored before it existed load as plain left-aligned text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStyle {
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub align: TextAlign,
}

/// Edge or centre of the selection box that `Document::align_marks` lines objects up on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Absolute document-space box values; `None` keeps each object's own value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Geometry {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
}

/// Partial style change. A `None` field leaves every mark's own value alone, so changing one
/// field never flattens another that differs across the selection (width keeps mixed colours).
/// Fields a mark has no use for (head size of a rectangle, colour of a privacy mask) are skipped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MarkEdit {
    pub rgb: Option<[f64; 3]>,
    pub alpha: Option<f64>,
    pub width: Option<f64>,
    pub head_size: Option<f64>,
    /// Document-space pixels (see `Document::text_size`).
    pub font_px: Option<f64>,
    pub background: Option<bool>,
    pub box_width: Option<f32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub align: Option<TextAlign>,
    pub radius: Option<f64>,
    pub dim: Option<f64>,
    pub fill: Option<bool>,
    pub round_caps: Option<bool>,
    pub corner_radius: Option<f64>,
    pub font_family: Option<String>,
    pub font_style: Option<String>,
    pub text_effect: Option<TextEffect>,
    pub step_ring: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub id: u64,
    /// Local coordinates: recognition rewrites them, transforms never do.
    pub points: Vec<Point>,
    pub closed: bool,
    pub color: [f64; 4],
    pub width: f64,
    pub kind: MarkKind,
    #[serde(default)]
    pub transform: Affine,
    #[serde(default)]
    pub group: Option<u64>,
    /// Arrowhead length multiplier of a connector (0 = plain line); independent of `width`.
    #[serde(default = "one")]
    pub head_size: f64,
    /// Text size in local units; 0 only in files written before it existed (migrated on load).
    #[serde(default)]
    pub font_size: f64,
    #[serde(default)]
    pub text_style: TextStyle,
    /// Closed paths are filled with `color`; ignored by every other kind.
    #[serde(default)]
    pub fill: bool,
    /// Round caps and joins (the original look); off gives butt caps and mitred corners.
    #[serde(default = "yes")]
    pub round_caps: bool,
    /// Corner radius in local units of a rectangle path; 0 keeps square corners.
    #[serde(default)]
    pub corner_radius: f64,
    /// Pango family list of text and step numbers.
    #[serde(default = "sans")]
    pub font_family: String,
    /// Pango style string of text and step numbers ("Regular", "Heavy", "Light Italic").
    #[serde(default = "regular")]
    pub font_style: String,
    /// Decoration of caption glyphs.
    #[serde(default)]
    pub text_effect: TextEffect,
    /// Outer ring of a step marker.
    #[serde(default)]
    pub step_ring: bool,
}

fn one() -> f64 {
    1.0
}

fn yes() -> bool {
    true
}

fn sans() -> String {
    DEFAULT_FAMILY.to_owned()
}

fn regular() -> String {
    DEFAULT_FONT_STYLE.to_owned()
}

/// Whether `style` can be stored: a non-blank Pango style string, bounded, no control characters.
pub fn valid_font_style(style: &str) -> bool {
    !style.trim().is_empty()
        && style.len() <= MAX_FONT_STYLE
        && !style.chars().any(char::is_control)
}

/// Upgrades marks read from a version-1 project, which stored neither head nor font size: text was
/// drawn at 6 × its line width and arrowheads at 4 × width (at least 10 px). Gives them those sizes.
pub fn migrate_v1(marks: &mut [Mark]) {
    for m in marks {
        match m.kind {
            MarkKind::Text { .. } => m.font_size = (m.width * 6.0).clamp(1.0, MAX_FONT),
            MarkKind::Connector { .. } => {
                m.head_size = ((m.width * 4.0).max(10.0) / HEAD_BASE).min(MAX_HEAD_SIZE);
            }
            _ => {}
        }
    }
}

thread_local! {
    /// The texture of `PrivacyBlur`: a fixed field of grey-blue mosaic blocks, built once per thread
    /// from a formula and shared by every paint. No screenshot pixel ever enters it.
    static MOSAIC: Option<SurfacePattern> = mosaic_pattern();
}

fn mosaic_pattern() -> Option<SurfacePattern> {
    const BLOCK: u32 = 8;
    const TILE: u32 = 256;
    const PALETTE: [[u8; 3]; 6] = [
        [96, 104, 120],
        [118, 128, 148],
        [138, 150, 172],
        [84, 96, 124],
        [108, 122, 156],
        [150, 158, 176],
    ];
    let tile = RgbaImage::from_fn(TILE, TILE, |x, y| {
        let (bx, by) = (x / BLOCK, y / BLOCK);
        let mut h = bx.wrapping_mul(0x9E37_79B1) ^ by.wrapping_mul(0x85EB_CA6B);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        let [r, g, b] = PALETTE[h as usize % PALETTE.len()];
        image::Rgba([r, g, b, 255])
    });
    let pattern = SurfacePattern::create(&surface_from(&tile).ok()?);
    pattern.set_extend(Extend::Repeat);
    pattern.set_filter(Filter::Nearest);
    Some(pattern)
}

/// Whether `family` can be stored on a mark or preset: a non-blank Pango family list that is
/// bounded and free of control characters.
pub fn valid_family(family: &str) -> bool {
    !family.trim().is_empty() && family.len() <= MAX_FAMILY && !family.chars().any(char::is_control)
}

/// Installed family names, sorted, for font pickers. A name holding a comma could not be told
/// apart from a list of families, so it is left out.
pub fn installed_families() -> Vec<String> {
    use gtk::pango::prelude::FontFamilyExt;
    let mut names: Vec<String> = pangocairo::FontMap::default()
        .list_families()
        .iter()
        .map(|f| f.name().to_string())
        .filter(|n| valid_family(n) && !n.contains(','))
        .collect();
    names.sort_by_cached_key(|n| n.to_lowercase());
    names.dedup();
    names.truncate(4096);
    names
}

/// Pango weight at which a face counts as bold (Semibold).
const BOLD_WEIGHT: i32 = 600;

fn parsed_face(font_style: &str) -> FontDescription {
    FontDescription::from_string(&format!("{DEFAULT_FAMILY} {}", font_style.trim()))
}

/// The rich bold and italic toggles a Pango style string stands for: bold when its weight is
/// Semibold or heavier, italic for any slant (Italic or Oblique). A caption that starts from
/// these shows exactly the face the style names, and switching a toggle off or on then really
/// changes it. Alignment is left.
pub fn font_style_defaults(font_style: &str) -> TextStyle {
    use gtk::glib::translate::IntoGlib;
    let face = parsed_face(font_style);
    TextStyle {
        bold: face.weight().into_glib() >= BOLD_WEIGHT,
        italic: face.style() != Style::Normal,
        align: TextAlign::Left,
    }
}

/// The one font description every text surface (paint, metrics, live caption editor) uses, so a
/// caption looks and measures the same while it is typed and once it is saved. `size` is in
/// absolute pixels; `family` is a Pango family list, so a missing first choice falls through.
/// `font_style` is a Pango style string ("Regular", "Heavy", "Light Italic", "Condensed Bold"):
/// its weight, slant, variant and stretch are taken. The rich toggles are then authoritative:
/// `bold` keeps a configured Semibold-or-heavier weight or else uses Bold, and `bold == false`
/// turns such a weight into Normal while keeping a lighter one (Light, Medium); `italic` keeps a
/// configured Italic or Oblique or else uses Italic, and `italic == false` makes it upright.
/// An unrecognised word is ignored, so a bad style reads as Regular.
pub fn font_description(
    family: &str,
    font_style: &str,
    size: f64,
    style: TextStyle,
) -> FontDescription {
    use gtk::glib::translate::IntoGlib;
    let family = family.trim();
    let mut font = parsed_face(font_style);
    let configured = (font.weight(), font.style());
    font.set_family(if family.is_empty() {
        DEFAULT_FAMILY
    } else {
        family
    });
    font.set_absolute_size(size * f64::from(SCALE));
    let heavy = configured.0.into_glib() >= BOLD_WEIGHT;
    font.set_weight(match (style.bold, heavy) {
        (true, true) => configured.0,
        (true, false) => Weight::Bold,
        (false, true) => Weight::Normal,
        (false, false) => configured.0,
    });
    font.set_style(match (style.italic, configured.1) {
        (true, Style::Normal) => Style::Italic,
        (true, slant) => slant,
        (false, _) => Style::Normal,
    });
    font
}

/// Pango layout for a text mark; shared by paint and measurement so both shape identically.
fn text_layout(
    cr: &Cairo,
    text: &str,
    family: &str,
    font_style: &str,
    size: f64,
    wrap: f64,
    style: TextStyle,
) -> Layout {
    // Pango handles Unicode and shaping; Cairo's toy text API does not.
    let layout = pangocairo::functions::create_layout(cr);
    // Metrics must not depend on the target surface or CTM (image: hinted, PDF/SVG: not), or the
    // same text would wrap differently on screen, in PNG and in vector exports.
    if let Ok(mut options) = gtk::cairo::FontOptions::new() {
        options.set_hint_metrics(gtk::cairo::HintMetrics::Off);
        pangocairo::functions::context_set_font_options(&layout.context(), Some(&options));
        layout.context_changed();
    }
    layout.set_font_description(Some(&font_description(family, font_style, size, style)));
    layout.set_alignment(match style.align {
        TextAlign::Left => Alignment::Left,
        TextAlign::Center => Alignment::Center,
        TextAlign::Right => Alignment::Right,
    });
    if wrap > 0.0 {
        layout.set_width((wrap * f64::from(SCALE)).min(f64::from(i32::MAX)) as i32);
        layout.set_wrap(WrapMode::WordChar);
    }
    layout.set_text(text);
    layout
}

fn logical(layout: &Layout) -> Bounds {
    let (_, r) = layout.extents();
    let s = f64::from(SCALE);
    Bounds {
        x: (f64::from(r.x()) / s) as f32,
        y: (f64::from(r.y()) / s) as f32,
        width: (f64::from(r.width()) / s) as f32,
        height: (f64::from(r.height()) / s) as f32,
    }
}

thread_local! {
    static MEASURE: Option<Cairo> = ImageSurface::create(Format::ARgb32, 1, 1)
        .ok()
        .and_then(|s| Cairo::new(&s).ok());
}

/// Measured logical rectangles of text, keyed by every input that shapes them. They come from
/// the thread's measurement context (hint metrics off), so they never depend on a paint target;
/// only those numbers are kept, never layouts, which stay per render context.
#[derive(Default)]
struct MetricsCache {
    entries: HashMap<u64, Metrics>,
    text_bytes: usize,
    /// Serial of the Pango font map the entries were measured with; a change drops them all.
    fonts: u32,
}

struct Metrics {
    key: MetricsKey<Box<str>>,
    bounds: Bounds,
}

/// Exact inputs of one measurement (floats by bit pattern, so equality is total).
struct MetricsKey<T> {
    text: T,
    family: T,
    font_style: T,
    size: u64,
    wrap: u64,
    style: TextStyle,
}

impl<'a> MetricsKey<&'a str> {
    fn new(
        text: &'a str,
        family: &'a str,
        font_style: &'a str,
        size: f64,
        wrap: f64,
        style: TextStyle,
    ) -> Self {
        Self {
            text,
            family,
            font_style,
            size: size.to_bits(),
            wrap: wrap.to_bits(),
            style,
        }
    }

    fn hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.text.hash(&mut h);
        self.family.hash(&mut h);
        self.font_style.hash(&mut h);
        (self.size, self.wrap, self.style.bold, self.style.italic).hash(&mut h);
        (self.style.align as u8).hash(&mut h);
        h.finish()
    }
}

impl MetricsCache {
    /// Entries beyond this, or holding more text than this, are dropped wholesale: a scene's
    /// working set fits, and an editing session cannot grow the cache without bound.
    const MAX_ENTRIES: usize = 4096;
    const MAX_TEXT_BYTES: usize = 4 << 20;

    /// `hash` is only a bucket: the stored key must equal `key`, so a collision is a miss.
    fn get(&mut self, hash: u64, key: &MetricsKey<&str>, fonts: u32) -> Option<Bounds> {
        if self.fonts != fonts {
            self.entries.clear();
            self.text_bytes = 0;
            self.fonts = fonts;
            return None;
        }
        let m = self.entries.get(&hash)?;
        let k = &m.key;
        (*k.text == *key.text
            && *k.family == *key.family
            && *k.font_style == *key.font_style
            && k.size == key.size
            && k.wrap == key.wrap
            && k.style == key.style)
            .then_some(m.bounds)
    }

    fn put(&mut self, hash: u64, key: &MetricsKey<&str>, bounds: Bounds) {
        if let Some(old) = self.entries.remove(&hash) {
            self.text_bytes -= old.key.text.len();
        }
        if self.entries.len() >= Self::MAX_ENTRIES
            || self.text_bytes + key.text.len() > Self::MAX_TEXT_BYTES
        {
            self.entries.clear();
            self.text_bytes = 0;
        }
        self.text_bytes += key.text.len();
        let key = MetricsKey {
            text: Box::from(key.text),
            family: Box::from(key.family),
            font_style: Box::from(key.font_style),
            size: key.size,
            wrap: key.wrap,
            style: key.style,
        };
        self.entries.insert(hash, Metrics { key, bounds });
    }
}

thread_local! {
    static METRICS: RefCell<MetricsCache> = RefCell::default();
}

/// Logical Pango rectangle of `text` relative to its origin, in the units `paint` draws it in.
fn text_extents(
    text: &str,
    family: &str,
    font_style: &str,
    size: f64,
    wrap: f64,
    style: TextStyle,
) -> Option<Bounds> {
    let key = MetricsKey::new(text, family, font_style, size, wrap, style);
    let (hash, fonts) = (key.hash(), pangocairo::FontMap::default().serial());
    if let Some(b) = METRICS.with_borrow_mut(|c| c.get(hash, &key, fonts)) {
        return Some(b);
    }
    let b = MEASURE.with(|cr| {
        Some(logical(&text_layout(
            cr.as_ref()?,
            text,
            family,
            font_style,
            size,
            wrap,
            style,
        )))
    })?;
    METRICS.with_borrow_mut(|c| c.put(hash, &key, b));
    Some(b)
}

fn seg_nearest(p: Point, a: Point, b: Point) -> Point {
    let (abx, aby) = (b.x - a.x, b.y - a.y);
    let len2 = abx * abx + aby * aby;
    let t = if len2 > 0.0 {
        (((p.x - a.x) * abx + (p.y - a.y) * aby) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Point {
        x: a.x + abx * t,
        y: a.y + aby * t,
    }
}

fn seg_dist2(p: Point, a: Point, b: Point) -> f32 {
    let n = seg_nearest(p, a, b);
    (p.x - n.x).powi(2) + (p.y - n.y).powi(2)
}

fn bbox(points: &[Point], r: f32) -> Option<Bounds> {
    let first = points.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x, first.y);
    for p in &points[1..] {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Bounds {
        x: x0 - r,
        y: y0 - r,
        width: x1 - x0 + 2.0 * r,
        height: y1 - y0 + 2.0 * r,
    }
    .finite()
}

fn transformed(b: Bounds, tf: Affine) -> Option<Bounds> {
    bbox(&b.corners().map(|c| tf.apply(c)), 0.0)
}

fn luminance(c: &[f64; 4]) -> f64 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Translation by `(dx, dy)`; `None` for a shift too small to be a real move.
fn shift(dx: f32, dy: f32) -> Option<Affine> {
    (dx.abs() > 1e-4 || dy.abs() > 1e-4).then(|| Affine::translation(Point { x: dx, y: dy }))
}

/// Radius of a step marker: at least readable at the smallest width.
pub fn step_radius(width: f64) -> f64 {
    (width * 2.5).max(11.0)
}

/// Arrowhead length in pixels; never depends on the shaft width.
fn head_px(head_size: f64) -> f64 {
    HEAD_BASE * head_size
}

/// Rotation and uniform scale (sqrt|det|) of `tf` about `c`: what a drawable that must not be
/// stretched, sheared or mirrored (text) is drawn with. A transform that already is a rotation
/// with uniform scale comes back unchanged.
fn conformal(tf: Affine, c: Point) -> Affine {
    let [a, b, k, d] = [tf.a, tf.b, tf.c, tf.d].map(f64::from);
    let det = a * d - b * k;
    let s = det.abs().sqrt();
    if s < 1e-9 || (det > 0.0 && (a - d).abs() + (b + k).abs() <= 1e-4 * s) {
        return tf;
    }
    let (sin, cos) = (b - k).atan2(a + d).sin_cos();
    let n = Affine {
        a: (s * cos) as f32,
        b: (s * sin) as f32,
        c: (-s * sin) as f32,
        d: (s * cos) as f32,
        tx: 0.0,
        ty: 0.0,
    };
    let (m, q) = (tf.apply(c), n.apply(c));
    Affine {
        tx: m.x - q.x,
        ty: m.y - q.y,
        ..n
    }
}

/// Local font size that shows as `px` in document space under `m`'s uniform scale; `None` when
/// unusable. Keeps the stored value when it already shows as `px`, so no-op edits stay no-ops.
fn local_font(m: &Mark, px: f64) -> Option<f64> {
    let scale = m.transform.det().abs().sqrt();
    let local = px / scale;
    if scale < 1e-9 || !(1.0..=MAX_FONT).contains(&local) {
        return None;
    }
    Some(
        if (local - m.font_size).abs() <= 1e-6 * m.font_size.max(1.0) {
            m.font_size
        } else {
            local
        },
    )
}

/// Rounded rectangle of a spotlight in its local space.
fn rounded_rect(cr: &Cairo, m: &Mark, radius: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let Some(b) = bbox(&m.points[..m.points.len().min(2)], 0.0) else {
        return;
    };
    let (x, y, w, h) = (
        f64::from(b.x),
        f64::from(b.y),
        f64::from(b.width),
        f64::from(b.height),
    );
    let r = radius.clamp(0.0, w.min(h) / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, PI + FRAC_PI_2);
    cr.close_path();
}

/// A closed four-point path that is a rectangle (right angles, any rotation): corner `o`, unit
/// edge directions `ux` (towards the second point) and `uy` (towards the last), and side lengths.
/// Only these may have rounded corners; every other polygon keeps its own.
struct Frame {
    o: Point,
    ux: (f64, f64),
    uy: (f64, f64),
    w: f64,
    h: f64,
}

impl Frame {
    fn of(points: &[Point]) -> Option<Self> {
        let [a, b, c, d] = points else {
            return None;
        };
        let v = |p: &Point, q: &Point| (f64::from(q.x - p.x), f64::from(q.y - p.y));
        let (e1, e2, diag) = (v(a, b), v(a, d), v(a, c));
        let (w, h) = (e1.0.hypot(e1.1), e2.0.hypot(e2.1));
        let slack = 1e-3 * w.max(h);
        (w > 1e-3
            && h > 1e-3
            && (e1.0 * e2.0 + e1.1 * e2.1).abs() <= 1e-3 * w * h
            && (diag.0 - e1.0 - e2.0).abs() <= slack
            && (diag.1 - e1.1 - e2.1).abs() <= slack)
            .then(|| Self {
                o: *a,
                ux: (e1.0 / w, e1.1 / w),
                uy: (e2.0 / h, e2.1 / h),
                w,
                h,
            })
    }

    /// Local position of the frame coordinates `(u, v)`.
    fn at(&self, u: f64, v: f64) -> (f64, f64) {
        (
            f64::from(self.o.x) + self.ux.0 * u + self.uy.0 * v,
            f64::from(self.o.y) + self.ux.1 * u + self.uy.1 * v,
        )
    }

    /// Radius limited so neighbouring corners never overlap.
    fn limit(&self, radius: f64) -> f64 {
        radius.clamp(0.0, self.w.min(self.h) / 2.0)
    }

    /// Signed distance from `q` to the rounded rectangle, negative inside.
    fn distance(&self, q: Point, radius: f64) -> f64 {
        let r = self.limit(radius);
        let (dx, dy) = (f64::from(q.x - self.o.x), f64::from(q.y - self.o.y));
        let (u, v) = (
            dx * self.ux.0 + dy * self.ux.1,
            dx * self.uy.0 + dy * self.uy.1,
        );
        let px = (u - self.w / 2.0).abs() - (self.w / 2.0 - r);
        let py = (v - self.h / 2.0).abs() - (self.h / 2.0 - r);
        px.max(0.0).hypot(py.max(0.0)) + px.max(py).min(0.0) - r
    }

    /// Adds the closed outline, in the order of the original points, with quarter-circle corners.
    fn trace(&self, cr: &Cairo, radius: f64) {
        let r = self.limit(radius);
        let k = r * (1.0 - KAPPA);
        let (w, h) = (self.w, self.h);
        let line = |(x, y): (f64, f64)| cr.line_to(x, y);
        let curve =
            |a: (f64, f64), b: (f64, f64), c: (f64, f64)| cr.curve_to(a.0, a.1, b.0, b.1, c.0, c.1);
        let (x, y) = self.at(r, 0.0);
        cr.move_to(x, y);
        line(self.at(w - r, 0.0));
        curve(self.at(w - k, 0.0), self.at(w, k), self.at(w, r));
        line(self.at(w, h - r));
        curve(self.at(w, h - k), self.at(w - k, h), self.at(w - r, h));
        line(self.at(r, h));
        curve(self.at(k, h), self.at(0.0, h - k), self.at(0.0, h - r));
        line(self.at(0.0, r));
        curve(self.at(0.0, k), self.at(k, 0.0), self.at(r, 0.0));
        cr.close_path();
    }
}

/// Non-zero winding test, the rule Cairo fills with, of `q` against the closed polygon `points`.
fn inside(points: &[Point], q: Point) -> bool {
    let mut winding = 0i32;
    for (i, a) in points.iter().enumerate() {
        let b = &points[(i + 1) % points.len()];
        let cross = (b.x - a.x) * (q.y - a.y) - (q.x - a.x) * (b.y - a.y);
        if a.y <= q.y {
            winding += i32::from(b.y > q.y && cross > 0.0);
        } else {
            winding -= i32::from(b.y <= q.y && cross < 0.0);
        }
    }
    winding != 0
}

/// White rounded outline of one spotlight.
fn spot_outline(cr: &Cairo, m: &Mark, tf: Affine) -> Result<()> {
    let MarkKind::Spotlight { radius, .. } = m.kind else {
        return Ok(());
    };
    if !tf.is_sane() {
        return Ok(());
    }
    cr.save()?;
    cr.transform(tf.into());
    rounded_rect(cr, m, radius);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.set_line_width(m.width);
    cr.set_line_join(LineJoin::Round);
    let stroked = cr.stroke();
    cr.new_path();
    cr.restore()?;
    Ok(stroked?)
}

/// Dims everything outside the union of the spotlights (the strongest `dim` wins), then outlines
/// each. Cut out of one group, so overlapping spotlights do not dim or double-clear each other.
fn paint_spots(cr: &Cairo, spots: &[(&Mark, Affine)]) -> Result<()> {
    let spots: Vec<_> = spots.iter().filter(|(_, tf)| tf.is_sane()).collect();
    let dim = spots
        .iter()
        .filter_map(|(m, _)| match m.kind {
            MarkKind::Spotlight { dim, .. } => Some(dim),
            _ => None,
        })
        .fold(0.0, f64::max);
    if spots.is_empty() {
        return Ok(());
    }
    cr.save()?;
    cr.push_group();
    let cut = (|| -> Result<()> {
        cr.set_source_rgba(0.0, 0.0, 0.0, dim);
        cr.paint()?;
        cr.set_operator(Operator::DestOut);
        cr.set_source_rgba(0.0, 0.0, 0.0, 1.0);
        for (m, tf) in &spots {
            if let MarkKind::Spotlight { radius, .. } = m.kind {
                cr.save()?;
                cr.transform((*tf).into());
                rounded_rect(cr, m, radius);
                let filled = cr.fill();
                cr.new_path();
                cr.restore()?;
                filled?;
            }
        }
        Ok(())
    })();
    let group = cr.pop_group_to_source();
    let painted = cut
        .and(group.map_err(Into::into))
        .and_then(|()| Ok(cr.paint()?));
    cr.restore()?;
    painted?;
    for (m, tf) in spots {
        spot_outline(cr, m, *tf)?;
    }
    Ok(())
}

fn quad_path(cr: &Cairo, q: &[Point; 4]) {
    cr.move_to(q[0].x.into(), q[0].y.into());
    for p in &q[1..] {
        cr.line_to(p.x.into(), p.y.into());
    }
    cr.close_path();
}

/// Where the shaft of an arrow from `a` to `b` ends and the two barbs of its head, `head` pixels
/// long. The head shrinks on short arrows, so shaft end and barbs stay between the endpoints along
/// the line; the tip is `b`.
fn arrow_head(a: Point, b: Point, head: f64) -> Option<(Point, [Point; 2])> {
    let (dx, dy) = (f64::from(b.x - a.x), f64::from(b.y - a.y));
    let len = dx.hypot(dy);
    if len < 1e-3 {
        return None;
    }
    let head = head.min(len);
    let (ux, uy) = (dx / len, dy / len);
    let (bx, by) = (
        f64::from(b.x) - ux * head * 0.9,
        f64::from(b.y) - uy * head * 0.9,
    );
    let at = |x: f64, y: f64| Point {
        x: x as f32,
        y: y as f32,
    };
    Some((
        at(bx, by),
        [
            at(bx - uy * head * 0.4, by + ux * head * 0.4),
            at(bx + uy * head * 0.4, by - ux * head * 0.4),
        ],
    ))
}

fn paint_connector(cr: &Cairo, a: Point, b: Point, width: f64, head_size: f64) -> Result<()> {
    let head = head_px(head_size);
    if f64::from(b.x - a.x).hypot(f64::from(b.y - a.y)) < 1e-3 {
        cr.arc(
            a.x.into(),
            a.y.into(),
            width / 2.0,
            0.0,
            std::f64::consts::TAU,
        );
        return Ok(cr.fill()?);
    }
    cr.move_to(a.x.into(), a.y.into());
    let Some((base, barbs)) = arrow_head(a, b, head).filter(|_| head > 0.0) else {
        cr.line_to(b.x.into(), b.y.into());
        return Ok(cr.stroke()?);
    };
    cr.line_to(base.x.into(), base.y.into());
    cr.stroke()?;
    cr.move_to(b.x.into(), b.y.into());
    cr.line_to(barbs[0].x.into(), barbs[0].y.into());
    cr.line_to(barbs[1].x.into(), barbs[1].y.into());
    cr.close_path();
    Ok(cr.fill()?)
}

/// Calls `emit(x0, y0, x1, y1)` for every pixel-aligned span of the document grid the convex quad
/// overlaps by any amount, rounded outwards, so no partially covered source pixel is missed.
/// Contact along a pixel boundary alone (an integer-aligned edge) is not overlap.
fn pixel_spans(q: &[Point; 4], width: u32, height: u32, mut emit: impl FnMut(f64, f64, f64, f64)) {
    if !q.iter().all(|p| p.x.is_finite() && p.y.is_finite()) {
        return;
    }
    let ys = q.iter().map(|p| f64::from(p.y));
    let top = ys.clone().fold(f64::INFINITY, f64::min);
    let bottom = ys.fold(f64::NEG_INFINITY, f64::max);
    let y0 = top.floor().max(0.0) as i64;
    let y1 = bottom.ceil().min(f64::from(height)) as i64;
    let mut run: Option<(f64, f64, i64, i64)> = None;
    let flush = |run: Option<(f64, f64, i64, i64)>, emit: &mut dyn FnMut(f64, f64, f64, f64)| {
        if let Some((x0, x1, y, n)) = run {
            emit(x0, y as f64, x1, (y + n) as f64);
        }
    };
    for y in y0..y1 {
        let (lo, hi) = (y as f64, (y + 1) as f64);
        let (mut xmin, mut xmax) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut take = |x: f64, exact: bool| {
            let pad = if exact { 0.0 } else { COVER_PAD };
            xmin = xmin.min(x - pad);
            xmax = xmax.max(x + pad);
        };
        for i in 0..4 {
            let (p, r) = (q[i], q[(i + 1) % 4]);
            let (px, py) = (f64::from(p.x), f64::from(p.y));
            let (dx, dy) = (f64::from(r.x) - px, f64::from(r.y) - py);
            if dy == 0.0 {
                if py >= lo && py <= hi {
                    take(px, true);
                    take(f64::from(r.x), true);
                }
            } else {
                let (ta, tb) = ((lo - py) / dy, (hi - py) / dy);
                let (t0, t1) = (ta.min(tb).max(0.0), ta.max(tb).min(1.0));
                if t0 <= t1 {
                    for t in [t0, t1] {
                        if t <= 0.0 {
                            take(px, true);
                        } else if t >= 1.0 {
                            take(f64::from(r.x), true);
                        } else {
                            take(px + dx * t, dx == 0.0);
                        }
                    }
                }
            }
        }
        if xmin > xmax {
            continue;
        }
        let x0 = xmin.floor().max(0.0);
        let x1 = xmax.ceil().min(f64::from(width));
        if x1 <= x0 {
            continue;
        }
        run = match run {
            Some((rx0, rx1, ry, n)) if rx0 == x0 && rx1 == x1 && ry + n == y => {
                Some((rx0, rx1, ry, n + 1))
            }
            other => {
                flush(other, &mut emit);
                Some((x0, x1, y, 1))
            }
        };
    }
    flush(run, &mut emit);
}

/// Adds the covered spans to the current path, in the document pixel grid.
fn cover_quad(cr: &Cairo, q: &[Point; 4], width: u32, height: u32) {
    pixel_spans(q, width, height, |x0, y0, x1, y1| {
        cr.rectangle(x0, y0, x1 - x0, y1 - y0);
    });
}

/// Adds the covered source pixels to the current path after `map`, for drawing into a magnifier.
/// Filled without antialiasing, this blacks exactly the lens pixels whose nearest-neighbour sample
/// would come from a redacted screenshot pixel.
fn cover_quad_mapped(cr: &Cairo, q: &[Point; 4], width: u32, height: u32, map: Affine) {
    pixel_spans(q, width, height, |x0, y0, x1, y1| {
        let (x0, y0, x1, y1) = (
            x0 - COVER_PAD,
            y0 - COVER_PAD,
            x1 + COVER_PAD,
            y1 + COVER_PAD,
        );
        let corner = |x: f64, y: f64| {
            map.apply(Point {
                x: x as f32,
                y: y as f32,
            })
        };
        quad_path(
            cr,
            &[
                corner(x0, y0),
                corner(x1, y0),
                corner(x1, y1),
                corner(x0, y1),
            ],
        );
    });
}

impl Mark {
    pub fn path(id: u64, points: Vec<Point>, closed: bool, color: [f64; 4], width: f64) -> Self {
        Self {
            id,
            points,
            closed,
            color,
            width,
            kind: MarkKind::Path,
            transform: Affine::identity(),
            group: None,
            head_size: 1.0,
            font_size: DEFAULT_FONT,
            text_style: TextStyle::default(),
            fill: false,
            round_caps: true,
            corner_radius: 0.0,
            font_family: sans(),
            font_style: regular(),
            text_effect: TextEffect::None,
            step_ring: false,
        }
    }

    pub fn text(
        id: u64,
        origin: Point,
        text: impl Into<String>,
        color: [f64; 4],
        width: f64,
    ) -> Self {
        Self {
            kind: MarkKind::Text {
                value: text.into(),
                box_width: 0.0,
                background: false,
            },
            font_size: (width * 6.0).clamp(1.0, MAX_FONT),
            ..Self::path(id, vec![origin], false, color, width)
        }
    }

    /// Endpoints are document-space, as returned by `Document::endpoint`.
    pub fn connector(id: u64, start: Endpoint, end: Endpoint, color: [f64; 4], width: f64) -> Self {
        Self {
            kind: MarkKind::Connector { start, end },
            ..Self::path(id, vec![start.point, end.point], false, color, width)
        }
    }

    pub fn step(id: u64, center: Point, number: u32, color: [f64; 4], width: f64) -> Self {
        Self {
            kind: MarkKind::Step { number },
            ..Self::path(id, vec![center], false, color, width)
        }
    }

    /// Opaque black whatever the style: the colour fields are fixed.
    pub fn redaction(id: u64, a: Point, b: Point) -> Self {
        Self {
            kind: MarkKind::Redaction,
            ..Self::path(id, Self::rect(a, b), false, [0., 0., 0., 1.], 1.0)
        }
    }

    /// Like `redaction`, covered with the neutral blur texture.
    pub fn privacy_blur(id: u64, a: Point, b: Point) -> Self {
        Self {
            kind: MarkKind::PrivacyBlur,
            ..Self::redaction(id, a, b)
        }
    }

    /// Translucent filled rectangle; `color` carries the alpha.
    pub fn highlight(id: u64, a: Point, b: Point, color: [f64; 4]) -> Self {
        Self {
            kind: MarkKind::Highlight,
            ..Self::path(id, Self::rect(a, b), false, color, 1.0)
        }
    }

    /// Freehand marker through `points`; `color` carries the alpha.
    pub fn highlighter(id: u64, points: Vec<Point>, color: [f64; 4], width: f64) -> Self {
        Self {
            kind: MarkKind::Highlighter,
            ..Self::path(id, points, false, color, width)
        }
    }

    /// Dims everything outside the union of all spotlights; `a`/`b` are opposite corners.
    pub fn spotlight(id: u64, a: Point, b: Point) -> Self {
        Self {
            kind: MarkKind::Spotlight {
                radius: 12.0,
                dim: 0.55,
            },
            ..Self::path(id, Self::rect(a, b), false, [1., 1., 1., 1.], 2.0)
        }
    }

    /// Lens `[a, b]` showing `source`'s centre at `zoom`; see `MarkKind::Magnifier`.
    pub fn magnifier(
        id: u64,
        source: Bounds,
        zoom: f32,
        a: Point,
        b: Point,
        color: [f64; 4],
        width: f64,
    ) -> Self {
        Self {
            kind: MarkKind::Magnifier { source, zoom },
            ..Self::path(id, Self::rect(a, b), false, color, width)
        }
    }

    fn rect(a: Point, b: Point) -> Vec<Point> {
        vec![
            Point {
                x: a.x.min(b.x),
                y: a.y.min(b.y),
            },
            Point {
                x: a.x.max(b.x),
                y: a.y.max(b.y),
            },
        ]
    }

    /// Privacy masks (black or blur): painted above every other mark, whatever their position, and
    /// kept out of every magnifier.
    pub fn is_redaction(&self) -> bool {
        matches!(self.kind, MarkKind::Redaction | MarkKind::PrivacyBlur)
    }

    /// Connector ends may attach only to marks that can never be part of a cycle or hide content.
    fn attachable(&self) -> bool {
        matches!(
            self.kind,
            MarkKind::Path | MarkKind::Text { .. } | MarkKind::Step { .. }
        )
    }

    /// Kinds whose strokes follow `round_caps`.
    pub fn stroked(&self) -> bool {
        matches!(
            self.kind,
            MarkKind::Path | MarkKind::Connector { .. } | MarkKind::Highlighter
        )
    }

    /// Butt caps and mitred corners instead of round ones.
    fn sharp(&self) -> bool {
        !self.round_caps && self.stroked()
    }

    /// How far the stroke reaches past its centre line: half the width, or a whole width where a
    /// mitred corner has a tip. Every bound and export decision includes it.
    fn reach(&self) -> f32 {
        (if self.sharp() {
            self.width
        } else {
            self.width / 2.0
        }) as f32
    }

    /// A closed path with at least three corners: the only thing `fill` applies to.
    pub fn fillable(&self) -> bool {
        matches!(self.kind, MarkKind::Path) && self.closed && self.points.len() >= 3
    }

    fn fills(&self) -> bool {
        self.fill && self.fillable()
    }

    /// A rectangle path (any rotation), the only shape `corner_radius` rounds.
    pub fn is_rectangle(&self) -> bool {
        self.fillable() && Frame::of(&self.points).is_some()
    }

    /// Outline of a path: the rounded rectangle when it has a radius and is one, else its polygon.
    fn trace(&self, cr: &Cairo, first: &Point) {
        if self.corner_radius > 0.0
            && self.fillable()
            && let Some(frame) = Frame::of(&self.points)
        {
            frame.trace(cr, self.corner_radius);
            return;
        }
        cr.move_to(first.x.into(), first.y.into());
        for p in &self.points[1..] {
            cr.line_to(p.x.into(), p.y.into());
        }
        if self.closed {
            cr.close_path();
        }
    }

    /// Fills the traced outline and strokes it. A translucent shape is composed as one flat layer
    /// first, so the stroke over its own fill never shows as a darker rim.
    fn paint_shape(&self, cr: &Cairo, first: &Point) -> Result<()> {
        if !self.fills() {
            self.trace(cr, first);
            return Ok(cr.stroke()?);
        }
        let flat = self.color[3] < 1.0;
        if flat {
            cr.push_group();
            cr.set_source_rgb(self.color[0], self.color[1], self.color[2]);
        }
        self.trace(cr, first);
        let drawn = cr.fill_preserve().and_then(|()| cr.stroke());
        if flat {
            let group = cr.pop_group_to_source();
            drawn?;
            group?;
            cr.paint_with_alpha(self.color[3])?;
            return Ok(());
        }
        Ok(drawn?)
    }

    /// Whether `q` (local space) is in the filled interior of a visible filled shape.
    fn fill_hit(&self, q: Point) -> bool {
        if !self.fills() || self.color[3] <= 0.0 {
            return false;
        }
        match Frame::of(&self.points).filter(|_| self.corner_radius > 0.0) {
            Some(frame) => frame.distance(q, self.corner_radius) <= 0.0,
            None => inside(&self.points, q),
        }
    }

    /// Transform the mark is actually drawn, measured and hit with for a stored/preview transform
    /// `tf`: text keeps only the rotation and uniform scale of it, so no gesture can distort glyphs.
    pub fn drawn_transform(&self, tf: Affine) -> Affine {
        if !matches!(self.kind, MarkKind::Text { .. }) {
            return tf;
        }
        let Some(lb) = self.local_bounds() else {
            return tf;
        };
        conformal(
            tf,
            Point {
                x: lb.x + lb.width / 2.0,
                y: lb.y + lb.height / 2.0,
            },
        )
    }

    /// Document-space corners of the rectangle kinds (privacy masks, magnifier lens) under `tf`.
    fn quad(&self, tf: Affine) -> Option<[Point; 4]> {
        if !(self.is_redaction()
            || matches!(self.kind, MarkKind::Magnifier { .. } | MarkKind::Highlight))
        {
            return None;
        }
        let b = bbox(&self.points[..self.points.len().min(2)], 0.0)?;
        Some(b.corners().map(|c| tf.apply(c)))
    }

    /// Untransformed box: stroke thickness included, text by its Pango logical rectangle. A connector
    /// reports its stored (unresolved) endpoints; use `Document::mark_bounds` for attachments.
    pub fn local_bounds(&self) -> Option<Bounds> {
        match &self.kind {
            MarkKind::Connector { start, end } => bbox(
                &[start.point, end.point],
                head_px(self.head_size).max(self.width / 2.0) as f32,
            ),
            MarkKind::Text {
                value,
                box_width,
                background,
            } => {
                let first = self.points.first()?;
                let e = text_extents(
                    value,
                    &self.font_family,
                    &self.font_style,
                    self.font_size,
                    f64::from(*box_width),
                    self.text_style,
                )?;
                let pad = if *background { TEXT_PAD } else { 0.0 }
                    .max(self.text_effect.reach(self.font_size));
                Bounds {
                    x: first.x + e.x - pad,
                    y: first.y + e.y - pad,
                    width: e.width + 2.0 * pad,
                    height: e.height + 2.0 * pad,
                }
                .finite()
            }
            MarkKind::Step { .. } => {
                let c = self.points.first()?;
                let r = self.step_outer() as f32;
                Bounds {
                    x: c.x - r,
                    y: c.y - r,
                    width: 2.0 * r,
                    height: 2.0 * r,
                }
                .finite()
            }
            MarkKind::Redaction | MarkKind::PrivacyBlur | MarkKind::Highlight => {
                bbox(&self.points[..self.points.len().min(2)], 0.0)
            }
            MarkKind::Spotlight { .. } => bbox(
                &self.points[..self.points.len().min(2)],
                (self.width / 2.0) as f32,
            ),
            MarkKind::Magnifier { .. } => bbox(
                &self.points[..self.points.len().min(2)],
                (self.width / 2.0) as f32,
            ),
            MarkKind::Path | MarkKind::Highlighter => bbox(&self.points, self.reach()),
        }
    }

    /// Document-space box (axis-aligned hull of the transformed local box).
    pub fn bounds(&self) -> Option<Bounds> {
        transformed(self.local_bounds()?, self.drawn_transform(self.transform))
    }

    /// `p` is in document space. Paths are unfilled: only within half the stroke width of the ink
    /// (the closing segment of a closed path included), never in the interior. Text, privacy masks
    /// and lenses are hit anywhere in their rectangle; steps in their disc.
    pub fn hit_test(&self, p: Point, tolerance: f32) -> bool {
        if !(p.x.is_finite() && p.y.is_finite() && tolerance.is_finite()) {
            return false;
        }
        let tf = self.drawn_transform(self.transform);
        let Some(inv) = tf.inverse() else {
            return false;
        };
        let scale = (tf.det().abs().sqrt() as f32).max(1e-6);
        self.local_hit(inv.apply(p), tolerance.max(0.0) / scale)
    }

    fn local_hit(&self, q: Point, tol: f32) -> bool {
        match &self.kind {
            MarkKind::Path => self.path_hit(q, tol) || self.fill_hit(q),
            MarkKind::Highlighter => self.path_hit(q, tol),
            MarkKind::Step { .. } => self.points.first().is_some_and(|&c| {
                let reach = self.step_outer() as f32 + tol;
                seg_dist2(q, c, c) <= reach * reach
            }),
            MarkKind::Connector { start, end } => {
                let reach = (self.width / 2.0) as f32 + tol;
                seg_dist2(q, start.point, end.point) <= reach * reach
            }
            _ => self.local_bounds().is_some_and(|b| {
                q.x >= b.x - tol
                    && q.x <= b.x + b.width + tol
                    && q.y >= b.y - tol
                    && q.y <= b.y + b.height + tol
            }),
        }
    }

    fn path_hit(&self, q: Point, tol: f32) -> bool {
        // A rounded rectangle is stroked along its arcs, not along the square polygon.
        if self.corner_radius > 0.0
            && self.fillable()
            && let Some(frame) = Frame::of(&self.points)
        {
            let reach = self.width / 2.0 + f64::from(tol);
            return frame.distance(q, self.corner_radius).abs() <= reach;
        }
        let Some(&first) = self.points.first() else {
            return false;
        };
        let reach = (self.width / 2.0) as f32 + tol;
        let n = self.points.len();
        if n == 1 {
            return seg_dist2(q, first, first) <= reach * reach;
        }
        let segs = n - 1 + usize::from(self.closed && n > 2);
        (0..segs).any(|i| seg_dist2(q, self.points[i], self.points[(i + 1) % n]) <= reach * reach)
    }

    fn draw(&self, cr: &Cairo, tf: Affine, ends: Option<[Point; 2]>) -> Result<()> {
        if !tf.is_sane() {
            return Ok(());
        }
        cr.save()?;
        let drawn = self.draw_inner(cr, tf, ends);
        cr.new_path();
        cr.restore()?;
        drawn
    }

    fn draw_inner(&self, cr: &Cairo, tf: Affine, ends: Option<[Point; 2]>) -> Result<()> {
        cr.set_source_rgba(self.color[0], self.color[1], self.color[2], self.color[3]);
        cr.set_line_width(self.width);
        if self.sharp() {
            cr.set_line_cap(LineCap::Butt);
            cr.set_line_join(LineJoin::Miter);
            cr.set_miter_limit(SHARP_MITER);
        } else {
            cr.set_line_cap(LineCap::Round);
            cr.set_line_join(LineJoin::Round);
        }
        match &self.kind {
            MarkKind::Connector { start, end } => {
                let [a, b] = ends.unwrap_or_else(|| [tf.apply(start.point), tf.apply(end.point)]);
                paint_connector(cr, a, b, self.width, self.head_size)
            }
            // Painted only by `Document::paint_one`, which snaps them to whole pixels.
            MarkKind::Redaction | MarkKind::PrivacyBlur | MarkKind::Spotlight { .. } => Ok(()),
            MarkKind::Highlight => {
                if let Some(q) = self.quad(tf) {
                    quad_path(cr, &q);
                    cr.fill()?;
                }
                Ok(())
            }
            MarkKind::Magnifier { .. } => {
                if let Some(q) = self.quad(tf) {
                    quad_path(cr, &q);
                    cr.stroke()?;
                }
                Ok(())
            }
            _ => {
                cr.transform(tf.into());
                self.draw_local(cr)
            }
        }
    }

    fn draw_local(&self, cr: &Cairo) -> Result<()> {
        let Some(first) = self.points.first() else {
            return Ok(());
        };
        match &self.kind {
            MarkKind::Text {
                value,
                box_width,
                background,
            } => {
                let layout = text_layout(
                    cr,
                    value,
                    &self.font_family,
                    &self.font_style,
                    self.font_size,
                    f64::from(*box_width),
                    self.text_style,
                );
                if *background {
                    let e = logical(&layout);
                    let shade = if luminance(&self.color) > 0.5 {
                        0.1
                    } else {
                        0.98
                    };
                    cr.set_source_rgb(shade, shade, shade);
                    cr.rectangle(
                        f64::from(first.x + e.x - TEXT_PAD),
                        f64::from(first.y + e.y - TEXT_PAD),
                        f64::from(e.width + 2.0 * TEXT_PAD),
                        f64::from(e.height + 2.0 * TEXT_PAD),
                    );
                    cr.fill()?;
                    cr.set_source_rgba(self.color[0], self.color[1], self.color[2], self.color[3]);
                }
                self.text_effect
                    .paint(cr, &layout, first, &self.color, self.font_size)?;
                cr.move_to(first.x.into(), first.y.into());
                pangocairo::functions::show_layout(cr, &layout);
            }
            MarkKind::Step { number } => {
                let r = step_radius(self.width);
                cr.arc(
                    first.x.into(),
                    first.y.into(),
                    r,
                    0.0,
                    std::f64::consts::TAU,
                );
                cr.fill()?;
                let shade = if luminance(&self.color) > 0.6 {
                    0.0
                } else {
                    1.0
                };
                cr.set_source_rgb(shade, shade, shade);
                let layout = text_layout(
                    cr,
                    &number.to_string(),
                    &self.font_family,
                    &self.font_style,
                    r * 1.1,
                    0.0,
                    font_style_defaults(&self.font_style),
                );
                let e = logical(&layout);
                cr.move_to(
                    f64::from(first.x) - f64::from(e.x) - f64::from(e.width) / 2.0,
                    f64::from(first.y) - f64::from(e.y) - f64::from(e.height) / 2.0,
                );
                pangocairo::functions::show_layout(cr, &layout);
                if self.step_ring {
                    let (ring, stroke) = self.ring();
                    cr.set_source_rgba(self.color[0], self.color[1], self.color[2], self.color[3]);
                    cr.set_line_width(stroke);
                    // The number's move_to left a current point: without this the arc would be
                    // joined to it by a stroked line across the marker.
                    cr.new_path();
                    cr.arc(
                        first.x.into(),
                        first.y.into(),
                        ring,
                        0.0,
                        std::f64::consts::TAU,
                    );
                    cr.stroke()?;
                }
            }
            _ if self.points.len() == 1 => {
                cr.arc(
                    first.x.into(),
                    first.y.into(),
                    self.width / 2.0,
                    0.0,
                    std::f64::consts::TAU,
                );
                cr.fill()?;
            }
            _ => self.paint_shape(cr, first)?,
        }
        Ok(())
    }
}

/// One reversible step of history. A user-visible edit is a list of these, applied in order and
/// undone in reverse, so batches restore exactly.
enum Op {
    /// Replaces the live marks having these ids by the given values (exact, in place).
    Set(Vec<Mark>),
    Remove(Vec<u64>),
    /// Ascending original indices.
    Insert(Vec<(usize, Mark)>),
    Crop(Option<Crop>),
    Presentation(Presentation),
    /// Full z-order as mark ids, bottom first; applying it returns the previous order.
    Reorder(Vec<u64>),
}

type Edit = Vec<Op>;

/// Optional preview: `ids` are drawn as if `matrix` had been applied to them.
struct View<'a> {
    ids: &'a [u64],
    matrix: Affine,
    /// A text mark drawn in place of the live mark with the given id (or on top when `None`).
    text: Option<(&'a Mark, Option<u64>)>,
}

impl View<'_> {
    const NONE: View<'static> = View {
        ids: &[],
        matrix: Affine::identity(),
        text: None,
    };
    fn tf(&self, m: &Mark) -> Affine {
        m.drawn_transform(if self.ids.contains(&m.id) {
            self.matrix.multiply(m.transform)
        } else {
            m.transform
        })
    }
}

/// Where every privacy mask can be, one hull per mask; see `Document::privacy_snapshot`.
#[derive(Debug, PartialEq)]
pub struct PrivacySnapshot {
    /// Axis-aligned hull of each mask's transformed rectangle, in z-order. `None`: the mask's
    /// geometry is unusable, so nothing can be proven clear of it.
    hulls: Vec<Option<Bounds>>,
}

impl PrivacySnapshot {
    /// Conservative: true when `bounds` (document space) may overlap any privacy mask, measured
    /// on the hull of each mask with a pixel of slack. A non-finite `bounds`, or any mask with
    /// unusable geometry, blocks everything. Over-blocking is safe.
    pub fn is_private(&self, bounds: Bounds) -> bool {
        let Some(b) = bounds.finite() else {
            return true;
        };
        self.hulls.iter().any(|h| {
            let Some(h) = h else {
                return true;
            };
            h.x - 1.0 <= b.x + b.width
                && b.x <= h.x + h.width + 1.0
                && h.y - 1.0 <= b.y + b.height
                && b.y <= h.y + h.height + 1.0
        })
    }
}

pub struct Document {
    pub background: ImageSurface,
    pub width: u32,
    pub height: u32,
    pub marks: Vec<Mark>,
    pub crop: Option<Crop>,
    pub presentation: Presentation,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    next_id: u64,
    /// Counts every visible change of marks, crop or presentation, undo and redo included.
    revision: u64,
    /// Last seen z-order index of each mark id; see `position`. Never trusted unchecked.
    slots: RefCell<HashMap<u64, usize>>,
    /// See `privacy_snapshot`. Never trusted unchecked.
    privacy: RefCell<Option<Rc<PrivacySnapshot>>>,
}

fn valid_points(points: &[Point]) -> bool {
    !points.is_empty()
        && points.len() <= MAX_POINTS
        && points.iter().all(|p| {
            p.x.is_finite() && p.y.is_finite() && p.x.abs() <= MAX_COORD && p.y.abs() <= MAX_COORD
        })
}

fn valid_style(color: &[f64; 4], width: f64) -> bool {
    color.iter().all(|c| (0.0..=1.0).contains(c)) && (MIN_WIDTH..=MAX_WIDTH).contains(&width)
}

fn valid_crop(c: &Crop, width: u32, height: u32) -> bool {
    c.width > 0
        && c.height > 0
        && c.x.checked_add(c.width).is_some_and(|r| r <= width)
        && c.y.checked_add(c.height).is_some_and(|b| b <= height)
}

fn valid_box_width(w: f32) -> bool {
    w == 0.0 || (w.is_finite() && (MIN_BOX_WIDTH..=MAX_COORD).contains(&w))
}

fn valid_source(source: &Bounds, zoom: f32) -> bool {
    [source.x, source.y, source.width, source.height]
        .iter()
        .all(|v| v.is_finite() && v.abs() <= MAX_COORD)
        && source.width > 0.0
        && source.height > 0.0
        && zoom.is_finite()
        && (1.0..=32.0).contains(&zoom)
}

fn valid_endpoint(e: &Endpoint) -> bool {
    e.point.x.is_finite()
        && e.point.y.is_finite()
        && e.point.x.abs() <= MAX_COORD
        && e.point.y.abs() <= MAX_COORD
        && e.anchor
            .is_none_or(|a| a.mark != 0 && (0.0..=1.0).contains(&a.u) && (0.0..=1.0).contains(&a.v))
}

fn validate_mark(m: &Mark) -> Result<()> {
    let id = m.id;
    ensure!(m.transform.is_sane(), "mark {id}: invalid transform");
    ensure!(valid_style(&m.color, m.width), "mark {id}: invalid style");
    ensure!(
        (0.0..=MAX_HEAD_SIZE).contains(&m.head_size) && (0.0..=MAX_FONT).contains(&m.font_size),
        "mark {id}: invalid head or font size"
    );
    ensure!(
        m.points.len() <= MAX_POINTS
            && m.points.iter().all(|p| {
                p.x.is_finite()
                    && p.y.is_finite()
                    && p.x.abs() <= MAX_COORD
                    && p.y.abs() <= MAX_COORD
            }),
        "mark {id}: invalid geometry"
    );
    let n = m.points.len();
    match &m.kind {
        MarkKind::Path | MarkKind::Highlighter => ensure!(n >= 1, "mark {id}: empty path"),
        MarkKind::Text {
            value, box_width, ..
        } => ensure!(
            n == 1
                && !value.trim().is_empty()
                && value.len() <= MAX_TEXT
                && valid_box_width(*box_width)
                && m.font_size >= 1.0,
            "mark {id}: invalid text"
        ),
        MarkKind::Connector { start, end } => ensure!(
            valid_endpoint(start) && valid_endpoint(end),
            "mark {id}: invalid connector"
        ),
        MarkKind::Redaction | MarkKind::PrivacyBlur => {
            ensure!(n == 2, "mark {id}: invalid privacy mask")
        }
        MarkKind::Highlight => ensure!(n == 2, "mark {id}: invalid highlight"),
        MarkKind::Step { number } => ensure!(
            n == 1 && (1..=MAX_STEP).contains(number),
            "mark {id}: invalid step"
        ),
        MarkKind::Magnifier { source, zoom } => ensure!(
            n == 2 && valid_source(source, *zoom),
            "mark {id}: invalid magnifier"
        ),
        MarkKind::Spotlight { radius, dim } => ensure!(
            n == 2
                && radius.is_finite()
                && (0.0..=f64::from(MAX_COORD)).contains(radius)
                && (0.0..=1.0).contains(dim),
            "mark {id}: invalid spotlight"
        ),
    }
    ensure!(
        matches!(m.kind, MarkKind::Text { .. }) || m.text_style == TextStyle::default(),
        "mark {id}: only text carries a text style"
    );
    ensure!(
        valid_family(&m.font_family)
            && valid_font_style(&m.font_style)
            && m.corner_radius.is_finite()
            && (0.0..=MAX_RADIUS).contains(&m.corner_radius),
        "mark {id}: invalid font family or corner radius"
    );
    Ok(())
}

fn validate_scene(marks: &[Mark], width: u32, height: u32, crop: Option<Crop>) -> Result<()> {
    ensure!(marks.len() <= MAX_MARKS, "too many marks");
    let mut total = 0usize;
    let mut kinds: HashMap<u64, bool> = HashMap::with_capacity(marks.len());
    let mut groups: HashMap<u64, usize> = HashMap::new();
    for m in marks {
        ensure!(
            (1..=MAX_ID).contains(&m.id),
            "mark id {} out of range",
            m.id
        );
        ensure!(
            kinds.insert(m.id, m.attachable()).is_none(),
            "duplicate mark id {}",
            m.id
        );
        total += m.points.len();
        ensure!(total <= MAX_TOTAL_POINTS, "too many points");
        validate_mark(m)?;
        if let Some(g) = m.group {
            ensure!((1..=MAX_ID).contains(&g), "mark {}: invalid group", m.id);
            *groups.entry(g).or_default() += 1;
        }
    }
    ensure!(
        groups.values().all(|&n| n >= 2),
        "a group has fewer than two members"
    );
    for m in marks {
        if let MarkKind::Connector { start, end } = &m.kind {
            for a in [start, end].iter().filter_map(|e| e.anchor) {
                ensure!(
                    a.mark != m.id && kinds.get(&a.mark) == Some(&true),
                    "mark {}: connector attached to a missing or ineligible mark",
                    m.id
                );
            }
        }
    }
    if let Some(c) = crop {
        ensure!(valid_crop(&c, width, height), "crop outside the image");
    }
    Ok(())
}

impl Document {
    pub fn new(image: RgbaImage) -> Result<Self> {
        let (width, height) = image.dimensions();
        let background = surface_from(&image)?;
        Ok(Self {
            background,
            width,
            height,
            marks: vec![],
            crop: None,
            undo: vec![],
            redo: vec![],
            next_id: 0,
            revision: 0,
            presentation: Presentation::default(),
            slots: RefCell::default(),
            privacy: RefCell::default(),
        })
    }

    /// Builds a document from untrusted parts (e.g. a project file): validates every mark, link,
    /// group and the crop, then starts with empty history and a collision-free id counter.
    pub fn from_parts(image: RgbaImage, marks: Vec<Mark>, crop: Option<Crop>) -> Result<Self> {
        let (w, h) = image.dimensions();
        ensure!(
            (1..=MAX_DIM).contains(&w)
                && (1..=MAX_DIM).contains(&h)
                && u64::from(w) * u64::from(h) <= MAX_PIXELS,
            "image size {w}x{h} is unsupported"
        );
        validate_scene(&marks, w, h, crop)?;
        let mut doc = Self::new(image)?;
        doc.next_id = marks
            .iter()
            .flat_map(|m| [m.id, m.group.unwrap_or(0)])
            .max()
            .unwrap_or(0);
        doc.marks = marks;
        doc.crop = crop;
        Ok(doc)
    }

    /// Applies a stored presentation, rejecting invalid ones and any that would exceed the output
    /// limits for the current crop. Not an undo step: used while building a document.
    pub fn with_presentation(mut self, presentation: Presentation) -> Result<Self> {
        presentation.validate()?;
        let (w, h) = self
            .crop
            .map_or((self.width, self.height), |c| (c.width, c.height));
        presentation.dimensions(w, h)?;
        self.presentation = presentation;
        Ok(self)
    }

    /// The screenshot pixels without any annotation (exact for opaque pixels), for project saving.
    /// Converted straight from the stored surface; no second surface is allocated.
    pub fn background_image(&self) -> Result<RgbaImage> {
        rgba_from(&self.background)
    }

    /// Fresh id for a mark or a group.
    pub fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    /// Monotonic change counter: bumped by every committed edit, undo, redo and recognition
    /// snap, never reused, so equal values mean an unchanged scene. Costs nothing to read.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn mark(&self, id: u64) -> Option<&Mark> {
        self.marks.get(self.position(id)?)
    }

    /// Z-order index of mark `id`. `slots` only remembers where ids were last seen; a slot is
    /// trusted after checking that the mark there still has the id, so undo, reordering and edits
    /// made straight to `marks` cannot return a wrong mark. A stale or missing slot costs one scan
    /// and a rebuild. Ids are unique (`validate_scene`).
    fn position(&self, id: u64) -> Option<usize> {
        let mut slots = self.slots.borrow_mut();
        if let Some(&i) = slots.get(&id)
            && self.marks.get(i).is_some_and(|m| m.id == id)
        {
            return Some(i);
        }
        let at = self.marks.iter().position(|m| m.id == id)?;
        slots.clear();
        // Reversed so a duplicated id (invalid, but possible through `marks`) maps to its first mark.
        slots.extend(self.marks.iter().enumerate().rev().map(|(i, m)| (m.id, i)));
        Some(at)
    }

    /// Number a new step marker should take.
    pub fn next_step_number(&self) -> u32 {
        self.marks
            .iter()
            .filter_map(|m| match m.kind {
                MarkKind::Step { number } => Some(number),
                _ => None,
            })
            .max()
            .map_or(1, |n| n + 1)
    }

    fn commit(&mut self, edit: Edit) {
        self.undo.push(edit);
        self.redo.clear();
        self.revision += 1;
    }

    pub fn add(&mut self, mark: Mark) {
        self.next_id = self.next_id.max(mark.id).max(mark.group.unwrap_or(0));
        self.commit(vec![Op::Remove(vec![mark.id])]);
        self.marks.push(mark);
    }

    /// Adds several marks (e.g. OCR highlights) as one undo step; false (nothing added) when any is
    /// invalid or the scene would exceed its limits.
    pub fn add_many(&mut self, marks: Vec<Mark>) -> bool {
        if marks.is_empty() || self.marks.len() + marks.len() > MAX_MARKS {
            return false;
        }
        let at = self.marks.len();
        self.marks.extend(marks);
        if validate_scene(&self.marks, self.width, self.height, self.crop).is_err() {
            self.marks.truncate(at);
            return false;
        }
        let top = self.marks[at..]
            .iter()
            .flat_map(|m| [m.id, m.group.unwrap_or(0)])
            .max()
            .unwrap_or(0);
        self.next_id = self.next_id.max(top);
        let ids = self.marks[at..].iter().map(|m| m.id).collect();
        self.commit(vec![Op::Remove(ids)]);
        true
    }

    /// Applies `f` to every copy of mark `id`: live and held in history, so late work reaches it.
    fn patch(&mut self, id: u64, mut f: impl FnMut(&mut Mark)) -> bool {
        let mut found = false;
        let mut hit = |m: &mut Mark| {
            if m.id == id && matches!(m.kind, MarkKind::Path) {
                f(m);
                found = true;
            }
        };
        self.marks.iter_mut().for_each(&mut hit);
        for edit in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            for op in edit.iter_mut() {
                match op {
                    Op::Set(marks) => marks.iter_mut().for_each(&mut hit),
                    Op::Insert(items) => items.iter_mut().for_each(|(_, m)| hit(m)),
                    Op::Remove(_) | Op::Crop(_) | Op::Reorder(_) | Op::Presentation(_) => {}
                }
            }
        }
        if found {
            self.revision += 1;
        }
        found
    }

    /// Asynchronous recognition result: replaces path geometry only (never transform, style or
    /// group), also for marks undone or deleted but still in history. Not an undo step.
    pub fn snap(&mut self, id: u64, points: Vec<Point>, closed: bool) -> bool {
        valid_points(&points)
            && self.patch(id, |m| {
                m.points = points.clone();
                m.closed = closed;
            })
    }

    /// Explicit, undoable geometry replacement of a live path (e.g. choosing a recognition alternative).
    pub fn replace_geometry(&mut self, id: u64, points: Vec<Point>, closed: bool) -> bool {
        let Some(m) = self.mark(id) else {
            return false;
        };
        if !valid_points(&points)
            || !matches!(m.kind, MarkKind::Path)
            || (m.points == points && m.closed == closed)
        {
            return false;
        }
        let mut new = m.clone();
        new.points = points;
        new.closed = closed;
        self.replace_marks(vec![new])
    }

    /// Topmost mark under `p` (document space); redactions are drawn last, so they win first.
    pub fn hit_test(&self, p: Point, tolerance: f32) -> Option<u64> {
        let hit = |m: &&Mark| match self.ends(m, &View::NONE) {
            Some([a, b]) => {
                p.x.is_finite()
                    && p.y.is_finite()
                    && tolerance.is_finite()
                    && seg_dist2(p, a, b) <= ((m.width / 2.0) as f32 + tolerance.max(0.0)).powi(2)
            }
            None => m.hit_test(p, tolerance),
        };
        let layer = |m: &Mark| match m.kind {
            _ if m.is_redaction() => 0,
            MarkKind::Spotlight { .. } => 2,
            _ => 1,
        };
        // Masks first, then ordinary marks, spotlights last: a filled spotlight never hides what
        // it surrounds.
        (0..3)
            .find_map(|want| {
                self.marks
                    .iter()
                    .rev()
                    .filter(|m| layer(*m) == want)
                    .find(hit)
            })
            .map(|m| m.id)
    }

    // --- attachments -------------------------------------------------------------------------

    fn resolve(&self, e: &Endpoint, tf: Affine, view: &View) -> Point {
        if let Some(a) = e.anchor
            && let Some(t) = self.mark(a.mark)
            && let Some(lb) = t.local_bounds()
        {
            return view.tf(t).apply(Point {
                x: lb.x + a.u * lb.width,
                y: lb.y + a.v * lb.height,
            });
        }
        tf.apply(e.point)
    }

    fn ends(&self, m: &Mark, view: &View) -> Option<[Point; 2]> {
        let MarkKind::Connector { start, end } = &m.kind else {
            return None;
        };
        let tf = view.tf(m);
        Some([self.resolve(start, tf, view), self.resolve(end, tf, view)])
    }

    /// Resolved document-space [start, end] of a connector.
    pub fn connector_ends(&self, id: u64) -> Option<[Point; 2]> {
        self.ends(self.mark(id)?, &View::NONE)
    }

    /// Endpoint for a connector end dropped at `p`: attached to the nearest edge of the topmost
    /// eligible mark (paths, text, steps) within `tolerance`, otherwise free. The point is in
    /// document space.
    pub fn endpoint(&self, p: Point, tolerance: f32) -> Endpoint {
        let free = Endpoint {
            point: p,
            anchor: None,
        };
        if !(p.x.is_finite() && p.y.is_finite() && tolerance.is_finite()) {
            return free;
        }
        let reach2 = tolerance.max(0.0).powi(2);
        let mut best: Option<(f32, u64, Point)> = None;
        for m in self.marks.iter().rev().filter(|m| m.attachable()) {
            let Some(lb) = m.local_bounds() else { continue };
            let c = lb
                .corners()
                .map(|c| m.drawn_transform(m.transform).apply(c));
            for i in 0..4 {
                let n = seg_nearest(p, c[i], c[(i + 1) % 4]);
                let d = (n.x - p.x).powi(2) + (n.y - p.y).powi(2);
                if d <= reach2 && !best.is_some_and(|b| b.0 <= d) {
                    best = Some((d, m.id, n));
                }
            }
        }
        let Some((_, id, n)) = best else { return free };
        let anchor = self.mark(id).and_then(|m| {
            let lb = m.local_bounds()?;
            let q = m.drawn_transform(m.transform).inverse()?.apply(n);
            Some(Anchor {
                mark: id,
                u: ((q.x - lb.x) / lb.width).clamp(0.0, 1.0),
                v: ((q.y - lb.y) / lb.height).clamp(0.0, 1.0),
            })
        });
        Endpoint { point: n, anchor }
    }

    /// Re-targets both ends of a connector (document-space endpoints from `endpoint`); one undo.
    pub fn set_connector_ends(&mut self, id: u64, start: Endpoint, end: Endpoint) -> bool {
        let Some(m) = self.mark(id) else {
            return false;
        };
        let (MarkKind::Connector { .. }, Some(inv)) = (&m.kind, m.transform.inverse()) else {
            return false;
        };
        let eligible = |e: &Endpoint| {
            valid_endpoint(e)
                && e.anchor
                    .is_none_or(|a| a.mark != id && self.mark(a.mark).is_some_and(Mark::attachable))
        };
        if !(eligible(&start) && eligible(&end)) {
            return false;
        }
        let local = |e: Endpoint| Endpoint {
            point: inv.apply(e.point),
            anchor: e.anchor,
        };
        let (start, end) = (local(start), local(end));
        let mut new = m.clone();
        new.points = vec![start.point, end.point];
        new.kind = MarkKind::Connector { start, end };
        self.replace_marks(vec![new])
    }

    // --- selection and batch edits ------------------------------------------------------------

    /// `ids` plus every member of their groups, unknown ids dropped, in z-order. All batch methods
    /// below act on this expansion.
    pub fn expand_groups(&self, ids: &[u64]) -> Vec<u64> {
        let groups: Vec<u64> = ids.iter().filter_map(|&id| self.mark(id)?.group).collect();
        if groups.is_empty() {
            // No group involved: the answer is the known ids in z-order, no scan of the scene.
            let mut at: Vec<usize> = ids.iter().filter_map(|&id| self.position(id)).collect();
            at.sort_unstable();
            at.dedup();
            return at.into_iter().map(|i| self.marks[i].id).collect();
        }
        self.marks
            .iter()
            .filter(|m| ids.contains(&m.id) || m.group.is_some_and(|g| groups.contains(&g)))
            .map(|m| m.id)
            .collect()
    }

    fn bounds_in(&self, m: &Mark, view: &View) -> Option<Bounds> {
        match self.ends(m, view) {
            Some([a, b]) => bbox(&[a, b], head_px(m.head_size).max(m.width / 2.0) as f32),
            None => transformed(m.local_bounds()?, view.tf(m)),
        }
    }

    /// Document-space box of one mark with connector attachments resolved.
    pub fn mark_bounds(&self, id: u64) -> Option<Bounds> {
        self.bounds_in(self.mark(id)?, &View::NONE)
    }

    pub fn selection_bounds(&self, ids: &[u64]) -> Option<Bounds> {
        self.expand_groups(ids)
            .into_iter()
            .filter_map(|id| self.bounds_in(self.mark(id)?, &View::NONE))
            .reduce(Bounds::union)
    }

    /// Clones of the (expanded) selection that `f` actually changes.
    fn modified(&self, ids: &[u64], mut f: impl FnMut(&mut Mark)) -> Vec<Mark> {
        let ids = self.expand_groups(ids);
        self.marks
            .iter()
            .filter(|m| ids.contains(&m.id))
            .filter_map(|m| {
                let mut n = m.clone();
                f(&mut n);
                (n != *m).then_some(n)
            })
            .collect()
    }

    /// One undo step replacing the live marks having these ids.
    fn replace_marks(&mut self, news: Vec<Mark>) -> bool {
        if news.is_empty() {
            return false;
        }
        let inverse = self.apply_op(Op::Set(news));
        self.commit(vec![inverse]);
        true
    }

    /// Pre-multiplies every selected mark's transform by `matrix` (one undo). False for singular,
    /// non-finite, absurd or identity matrices and when any result would be unusable.
    pub fn transform_marks(&mut self, ids: &[u64], matrix: Affine) -> bool {
        if !matrix.is_sane() || matrix == Affine::identity() {
            return false;
        }
        let news = self.modified(ids, |m| m.transform = matrix.multiply(m.transform));
        !news.iter().any(|m| !m.transform.is_sane()) && self.replace_marks(news)
    }

    pub fn move_marks(&mut self, ids: &[u64], delta: Point) -> bool {
        delta.x.is_finite()
            && delta.y.is_finite()
            && self.transform_marks(ids, Affine::translation(delta))
    }

    /// One undoable colour/width change. Channels must be in 0..=1 and width in 0.1..=128.
    pub fn set_style_many(&mut self, ids: &[u64], color: [f64; 4], width: f64) -> bool {
        valid_style(&color, width) && {
            let news = self.modified(ids, |m| {
                m.color = color;
                m.width = width;
            });
            self.replace_marks(news)
        }
    }

    /// Edits a text mark in place as one undo; `box_width` 0 = unwrapped, else at least
    /// `MIN_BOX_WIDTH`. `font_size` is the size shown in document space (see `text_size`).
    pub fn set_text(
        &mut self,
        id: u64,
        value: &str,
        box_width: f32,
        background: bool,
        font_size: f64,
    ) -> bool {
        let Some((style, family, font_style, effect)) = self.mark(id).map(|m| {
            (
                m.text_style,
                m.font_family.clone(),
                m.font_style.clone(),
                m.text_effect,
            )
        }) else {
            return false;
        };
        self.set_text_styled(
            id,
            value,
            box_width,
            background,
            font_size,
            style,
            &family,
            &font_style,
            effect,
            None,
        )
    }

    /// `set_text` that also sets the rich style, font family, effect and position (see
    /// `retexted`), as the same single undo.
    pub fn set_text_styled(
        &mut self,
        id: u64,
        value: &str,
        box_width: f32,
        background: bool,
        font_size: f64,
        style: TextStyle,
        family: &str,
        font_style: &str,
        effect: TextEffect,
        at: Option<Point>,
    ) -> bool {
        if value.trim().is_empty() {
            return false;
        }
        let Some(new) = self.retexted(
            id, value, box_width, background, font_size, style, family, font_style, effect, at,
        ) else {
            return false;
        };
        self.mark(id) != Some(&new) && self.replace_marks(vec![new])
    }

    /// What text mark `id` would become with these values, without touching the document; the
    /// exact mark `set_text_styled` would store. For live previews. A blank value is allowed here
    /// (it measures as nothing) but `set_text_styled` refuses it. `at` is the document-space
    /// position of the caption's origin (as `Document::transform` of its first point gives it):
    /// when it differs from the current one the mark's transform is shifted by the difference, so
    /// rotation and scale are kept; `None` keeps the position.
    pub fn retexted(
        &self,
        id: u64,
        value: &str,
        box_width: f32,
        background: bool,
        font_size: f64,
        style: TextStyle,
        family: &str,
        font_style: &str,
        effect: TextEffect,
        at: Option<Point>,
    ) -> Option<Mark> {
        let m = self.mark(id)?;
        if !matches!(m.kind, MarkKind::Text { .. })
            || value.len() > MAX_TEXT
            || !valid_box_width(box_width)
            || !valid_family(family)
            || !valid_font_style(font_style)
        {
            return None;
        }
        let mut new = m.clone();
        new.kind = MarkKind::Text {
            value: value.to_owned(),
            box_width,
            background,
        };
        new.font_size = local_font(m, font_size)?;
        new.text_style = style;
        new.font_family = family.to_owned();
        new.font_style = font_style.to_owned();
        new.text_effect = effect;
        if let Some(at) = at {
            if !(at.x.is_finite()
                && at.y.is_finite()
                && at.x.abs() <= MAX_COORD
                && at.y.abs() <= MAX_COORD)
            {
                return None;
            }
            let origin = m.transform.apply(*m.points.first()?);
            if let Some(t) = shift(at.x - origin.x, at.y - origin.y) {
                new.transform = t.multiply(m.transform);
                if !new.transform.is_sane() {
                    return None;
                }
            }
        }
        Some(new)
    }

    /// Font size a text mark shows in document space: its own size times the uniform scale of its
    /// transform (the scale text is drawn with however the transform distorts).
    pub fn text_size(&self, id: u64) -> Option<f64> {
        let m = self.mark(id)?;
        matches!(m.kind, MarkKind::Text { .. })
            .then(|| m.font_size * m.transform.det().abs().sqrt())
    }

    /// One undoable font size, in document-space pixels, for the text marks of the selection.
    pub fn set_font_size(&mut self, ids: &[u64], pixels: f64) -> bool {
        if !pixels.is_finite() {
            return false;
        }
        let news = self.modified(ids, |m| {
            if matches!(m.kind, MarkKind::Text { .. })
                && let Some(local) = local_font(m, pixels)
            {
                m.font_size = local;
            }
        });
        self.replace_marks(news)
    }

    /// One undoable arrowhead multiplier (0 = no head) for the connectors of the selection.
    pub fn set_head_size(&mut self, ids: &[u64], size: f64) -> bool {
        (0.0..=MAX_HEAD_SIZE).contains(&size) && {
            let news = self.modified(ids, |m| {
                if matches!(m.kind, MarkKind::Connector { .. }) {
                    m.head_size = size;
                }
            });
            self.replace_marks(news)
        }
    }

    /// One undoable partial style edit; false when nothing is valid or changes. See `MarkEdit`.
    pub fn edit_marks(&mut self, ids: &[u64], e: &MarkEdit) -> bool {
        let unit = |v: &f64| (0.0..=1.0).contains(v);
        if !(e.rgb.is_none_or(|c| c.iter().all(unit))
            && e.alpha.is_none_or(|a| unit(&a))
            && e.width.is_none_or(|w| (MIN_WIDTH..=MAX_WIDTH).contains(&w))
            && e.head_size
                .is_none_or(|h| (0.0..=MAX_HEAD_SIZE).contains(&h))
            && e.font_px.is_none_or(f64::is_finite)
            && e.box_width.is_none_or(valid_box_width)
            && e.radius
                .is_none_or(|r| (0.0..=f64::from(MAX_COORD)).contains(&r))
            && e.dim.is_none_or(|d| unit(&d))
            && e.corner_radius
                .is_none_or(|r| (0.0..=MAX_RADIUS).contains(&r))
            && e.font_family.as_deref().is_none_or(valid_family)
            && e.font_style.as_deref().is_none_or(valid_font_style))
        {
            return false;
        }
        let news = self.modified(ids, |m| {
            let text = matches!(m.kind, MarkKind::Text { .. });
            if !m.is_redaction() {
                if let Some(c) = e.rgb {
                    m.color[..3].copy_from_slice(&c);
                }
                if let Some(a) = e.alpha {
                    m.color[3] = a;
                }
                if let Some(w) = e.width.filter(|_| !text) {
                    m.width = w;
                }
            }
            if let Some(on) = e.fill.filter(|_| m.fillable()) {
                m.fill = on;
            }
            if let Some(on) = e.round_caps.filter(|_| m.stroked()) {
                m.round_caps = on;
            }
            if let Some(r) = e.corner_radius.filter(|_| m.is_rectangle()) {
                m.corner_radius = r;
            }
            if let Some(f) = e
                .font_family
                .as_ref()
                .filter(|_| matches!(m.kind, MarkKind::Text { .. } | MarkKind::Step { .. }))
            {
                m.font_family.clone_from(f);
            }
            if let Some(f) = e
                .font_style
                .as_ref()
                .filter(|_| matches!(m.kind, MarkKind::Text { .. } | MarkKind::Step { .. }))
            {
                m.font_style.clone_from(f);
                // A caption takes the bold and italic the new style names, so its toggles show
                // and can undo it; explicit `bold`/`italic` in the same edit still win below.
                if text {
                    let d = font_style_defaults(f);
                    (m.text_style.bold, m.text_style.italic) = (d.bold, d.italic);
                }
            }
            let local = e.font_px.filter(|_| text).and_then(|px| local_font(m, px));
            match &mut m.kind {
                MarkKind::Connector { .. } => {
                    if let Some(h) = e.head_size {
                        m.head_size = h;
                    }
                }
                MarkKind::Text {
                    box_width,
                    background,
                    ..
                } => {
                    if let Some(l) = local {
                        m.font_size = l;
                    }
                    if let Some(b) = e.background {
                        *background = b;
                    }
                    if let Some(w) = e.box_width {
                        *box_width = w;
                    }
                    let s = &mut m.text_style;
                    s.bold = e.bold.unwrap_or(s.bold);
                    s.italic = e.italic.unwrap_or(s.italic);
                    s.align = e.align.unwrap_or(s.align);
                    m.text_effect = e.text_effect.unwrap_or(m.text_effect);
                }
                MarkKind::Step { .. } => {
                    m.step_ring = e.step_ring.unwrap_or(m.step_ring);
                }
                MarkKind::Spotlight { radius, dim } => {
                    *radius = e.radius.unwrap_or(*radius);
                    *dim = e.dim.unwrap_or(*dim);
                }
                _ => {}
            }
        });
        self.replace_marks(news)
    }

    // --- object layout (inspector) -----------------------------------------------------------

    /// Moving units of a selection in z-order: each group is one unit, every other mark its own.
    pub fn units(&self, ids: &[u64]) -> Vec<Vec<u64>> {
        let mut units: Vec<Vec<u64>> = vec![];
        let mut groups: HashMap<u64, usize> = HashMap::new();
        for id in self.expand_groups(ids) {
            let group = self.mark(id).and_then(|m| m.group);
            match group.and_then(|g| groups.get(&g).copied()) {
                Some(i) => units[i].push(id),
                None => {
                    if let Some(g) = group {
                        groups.insert(g, units.len());
                    }
                    units.push(vec![id]);
                }
            }
        }
        units
    }

    /// Applies one matrix per unit as a single undo. False when nothing moves or any result is
    /// unusable (nothing is applied then).
    fn transform_units(&mut self, moves: Vec<(Vec<u64>, Affine)>) -> bool {
        let mut news = vec![];
        for (ids, matrix) in moves {
            if matrix == Affine::identity() {
                continue;
            }
            for m in self.marks.iter().filter(|m| ids.contains(&m.id)) {
                let mut n = m.clone();
                n.transform = matrix.multiply(n.transform);
                if !n.transform.is_sane() {
                    return false;
                }
                news.push(n);
            }
        }
        self.replace_marks(news)
    }

    /// Per-unit boxes, or `None` when any unit has no finite box.
    fn unit_boxes(&self, units: &[Vec<u64>]) -> Option<Vec<Bounds>> {
        units.iter().map(|u| self.selection_bounds(u)).collect()
    }

    /// Sets each unit's own box values (absolute); width/height scale it about its top-left corner.
    /// Text only ever scales uniformly, following the width when both are given. One undo.
    pub fn set_geometry(&mut self, ids: &[u64], g: Geometry) -> bool {
        let ok = |v: Option<f64>, min: f64| {
            v.is_none_or(|v| v.is_finite() && v >= min && v.abs() <= f64::from(MAX_COORD))
        };
        if !(ok(g.x, f64::MIN) && ok(g.y, f64::MIN) && ok(g.width, 1.0) && ok(g.height, 1.0)) {
            return false;
        }
        let units = self.units(ids);
        let Some(boxes) = self.unit_boxes(&units) else {
            return false;
        };
        let moves = units
            .into_iter()
            .zip(boxes)
            .map(|(unit, b)| {
                let text = unit.iter().any(|&id| {
                    matches!(self.mark(id).map(|m| &m.kind), Some(MarkKind::Text { .. }))
                });
                let ratio = |to: Option<f64>, from: f32| {
                    to.filter(|_| from > 1e-6)
                        .map(|t| (t / f64::from(from)) as f32)
                };
                let (sx, sy) = (ratio(g.width, b.width), ratio(g.height, b.height));
                let (sx, sy) = if text {
                    let s = sx.or(sy).unwrap_or(1.0);
                    (s, s)
                } else {
                    (sx.unwrap_or(1.0), sy.unwrap_or(1.0))
                };
                let corner = Point { x: b.x, y: b.y };
                let to = Point {
                    x: g.x.map_or(b.x, |x| x as f32) - b.x,
                    y: g.y.map_or(b.y, |y| y as f32) - b.y,
                };
                (
                    unit,
                    Affine::scale_about(sx, sy, corner).then(Affine::translation(to)),
                )
            })
            .collect();
        self.transform_units(moves)
    }

    /// Rotation of a unit's first mark in degrees, (-180, 180].
    pub fn rotation_degrees(&self, id: u64) -> Option<f64> {
        let t = self.mark(id)?.transform;
        Some(f64::from(t.b).atan2(f64::from(t.a)).to_degrees())
    }

    /// Rotates each unit about the centre of its box so that it shows `degrees` (one undo).
    pub fn set_rotation(&mut self, ids: &[u64], degrees: f64) -> bool {
        if !degrees.is_finite() {
            return false;
        }
        let units = self.units(ids);
        let Some(boxes) = self.unit_boxes(&units) else {
            return false;
        };
        let moves = units
            .into_iter()
            .zip(boxes)
            .filter_map(|(unit, b)| {
                let delta = (degrees - self.rotation_degrees(*unit.first()?)?).rem_euclid(360.0);
                let delta = if delta > 180.0 { delta - 360.0 } else { delta };
                // f32 trigonometry leaves ~1e-5° of noise; under a thousandth of a degree is "there".
                (delta.abs() > 1e-3).then(|| {
                    let centre = Point {
                        x: b.x + b.width / 2.0,
                        y: b.y + b.height / 2.0,
                    };
                    (
                        unit,
                        Affine::rotation_about(delta.to_radians() as f32, centre),
                    )
                })
            })
            .collect();
        self.transform_units(moves)
    }

    /// Lines two or more units up on an edge or centre of their common box (one undo).
    pub fn align_marks(&mut self, ids: &[u64], how: Align) -> bool {
        let units = self.units(ids);
        let Some(boxes) = self.unit_boxes(&units).filter(|_| units.len() >= 2) else {
            return false;
        };
        let Some(all) = boxes.iter().copied().reduce(Bounds::union) else {
            return false;
        };
        let (cx, cy) = (all.x + all.width / 2.0, all.y + all.height / 2.0);
        let moves = units
            .into_iter()
            .zip(boxes)
            .filter_map(|(unit, b)| {
                let (dx, dy) = match how {
                    Align::Left => (all.x - b.x, 0.0),
                    Align::Center => (cx - (b.x + b.width / 2.0), 0.0),
                    Align::Right => (all.x + all.width - (b.x + b.width), 0.0),
                    Align::Top => (0.0, all.y - b.y),
                    Align::Middle => (0.0, cy - (b.y + b.height / 2.0)),
                    Align::Bottom => (0.0, all.y + all.height - (b.y + b.height)),
                };
                shift(dx, dy).map(|m| (unit, m))
            })
            .collect();
        self.transform_units(moves)
    }

    /// Spreads three or more units so the gaps between neighbours are equal; the outermost two
    /// stay where they are (one undo).
    pub fn distribute_marks(&mut self, ids: &[u64], axis: Axis) -> bool {
        let units = self.units(ids);
        let Some(boxes) = self.unit_boxes(&units).filter(|_| units.len() >= 3) else {
            return false;
        };
        let span = |b: &Bounds| match axis {
            Axis::Horizontal => (b.x, b.width),
            Axis::Vertical => (b.y, b.height),
        };
        let mut items: Vec<(Vec<u64>, f32, f32)> = units
            .into_iter()
            .zip(&boxes)
            .map(|(u, b)| (u, span(b).0, span(b).1))
            .collect();
        items.sort_by(|a, b| a.1.total_cmp(&b.1));
        let start = items[0].1;
        let end = items.iter().map(|i| i.1 + i.2).fold(f32::MIN, f32::max);
        let used: f32 = items.iter().map(|i| i.2).sum();
        let gap = (end - start - used) / (items.len() - 1) as f32;
        let mut at = start;
        let moves = items
            .into_iter()
            .filter_map(|(unit, lo, size)| {
                let d = at - lo;
                at += size + gap;
                match axis {
                    Axis::Horizontal => shift(d, 0.0),
                    Axis::Vertical => shift(0.0, d),
                }
                .map(|m| (unit, m))
            })
            .collect();
        self.transform_units(moves)
    }

    /// The privacy masks' hulls as they are now. The same `Rc` comes back for as long as the
    /// hulls are unchanged (so `Rc::ptr_eq` tells a cache its answers still hold) and a new one
    /// once any mask moves, changes, appears or goes (undo and redo included); an old snapshot
    /// never changes. Every call re-derives the hulls from `marks` itself, so edits made straight
    /// through the public fields are seen too.
    pub fn privacy_snapshot(&self) -> Rc<PrivacySnapshot> {
        let hulls = || {
            self.marks
                .iter()
                .filter(|m| m.is_redaction())
                .map(|m| m.quad(m.transform).and_then(|q| bbox(&q, 0.0)))
        };
        let mut cached = self.privacy.borrow_mut();
        if let Some(s) = cached
            .as_ref()
            .filter(|s| s.hulls.iter().copied().eq(hulls()))
        {
            return Rc::clone(s);
        }
        let fresh = Rc::new(PrivacySnapshot {
            hulls: hulls().collect(),
        });
        *cached = Some(Rc::clone(&fresh));
        fresh
    }

    /// Re-aims a lens: new source rectangle and zoom (one undo).
    pub fn set_magnifier_source(&mut self, id: u64, source: Bounds, zoom: f32) -> bool {
        let Some(m) = self.mark(id) else {
            return false;
        };
        if !matches!(m.kind, MarkKind::Magnifier { .. }) || !valid_source(&source, zoom) {
            return false;
        }
        let mut new = m.clone();
        new.kind = MarkKind::Magnifier { source, zoom };
        new != *m && self.replace_marks(vec![new])
    }

    /// Changes the zoom keeping the lens rectangle, its transform and the source centre; the source
    /// size becomes the lens' local size divided by `zoom`.
    pub fn set_magnifier_zoom(&mut self, id: u64, zoom: f32) -> bool {
        let Some(m) = self.mark(id) else {
            return false;
        };
        let (MarkKind::Magnifier { source, .. }, Some(lens)) = (&m.kind, m.local_bounds()) else {
            return false;
        };
        let lens = Bounds {
            x: 0.0,
            y: 0.0,
            width: lens.width - m.width as f32,
            height: lens.height - m.width as f32,
        };
        let (cx, cy) = (
            source.x + source.width / 2.0,
            source.y + source.height / 2.0,
        );
        let (w, h) = (lens.width / zoom, lens.height / zoom);
        self.set_magnifier_source(
            id,
            Bounds {
                x: cx - w / 2.0,
                y: cy - h / 2.0,
                width: w,
                height: h,
            },
            zoom,
        )
    }

    pub fn group_marks(&mut self, ids: &[u64]) -> bool {
        let ids = self.expand_groups(ids);
        let first = self
            .mark(ids.first().copied().unwrap_or(0))
            .and_then(|m| m.group);
        let one_group = first.is_some()
            && ids
                .iter()
                .all(|&id| self.mark(id).is_some_and(|m| m.group == first));
        if ids.len() < 2 || one_group {
            return false;
        }
        let group = self.id();
        let news = self.modified(&ids, |m| m.group = Some(group));
        self.replace_marks(news)
    }

    pub fn ungroup_marks(&mut self, ids: &[u64]) -> bool {
        let news = self.modified(ids, |m| m.group = None);
        self.replace_marks(news)
    }

    /// Turns the privacy masks among the (expanded) selection black (`false`) or blurred (`true`);
    /// other marks are untouched. One undo; false when nothing changes.
    pub fn set_privacy_blur(&mut self, ids: &[u64], blurred: bool) -> bool {
        let kind = if blurred {
            MarkKind::PrivacyBlur
        } else {
            MarkKind::Redaction
        };
        let news = self.modified(ids, |m| {
            if m.is_redaction() {
                m.kind = kind.clone();
            }
        });
        self.replace_marks(news)
    }

    /// Moves the (expanded) selection to the front (`true`) or back of the stack, keeping its own
    /// relative order and that of every other mark. One undo; false when the stacking would not
    /// change.
    ///
    /// Privacy masks are painted above every other mark whatever their position, so a mask can
    /// never be buried under an annotation and the two layers are ordered independently: a mask
    /// moves only among masks, an annotation only among annotations.
    pub fn reorder_marks(&mut self, ids: &[u64], front: bool) -> bool {
        let selected: HashSet<u64> = self.expand_groups(ids).into_iter().collect();
        let before: Vec<u64> = self.marks.iter().map(|m| m.id).collect();
        let mut after = before.clone();
        for mask in [false, true] {
            let slots: Vec<usize> = (0..before.len())
                .filter(|&i| self.marks[i].is_redaction() == mask)
                .collect();
            let (mut picked, mut rest): (Vec<u64>, Vec<u64>) = slots
                .iter()
                .map(|&i| before[i])
                .partition(|id| selected.contains(id));
            let layer = if front {
                rest.append(&mut picked);
                rest
            } else {
                picked.append(&mut rest);
                picked
            };
            for (&slot, id) in slots.iter().zip(layer) {
                after[slot] = id;
            }
        }
        if after == before {
            return false;
        }
        let inverse = self.apply_op(Op::Reorder(after));
        self.commit(vec![inverse]);
        true
    }

    /// Step marks other than `skip`, in step order with `reorder` applied, renumbered 1..n; returns
    /// the clones whose number changed.
    fn renumbered(&self, skip: &[u64], reorder: impl FnOnce(&mut Vec<usize>)) -> Vec<Mark> {
        let mut steps: Vec<(u32, usize)> = self
            .marks
            .iter()
            .enumerate()
            .filter(|(_, m)| !skip.contains(&m.id))
            .filter_map(|(i, m)| match m.kind {
                MarkKind::Step { number } => Some((number, i)),
                _ => None,
            })
            .collect();
        steps.sort_unstable();
        let mut order: Vec<usize> = steps.into_iter().map(|(_, i)| i).collect();
        reorder(&mut order);
        order
            .iter()
            .enumerate()
            .filter_map(|(n, &i)| {
                let (m, n) = (&self.marks[i], n as u32 + 1);
                match m.kind {
                    MarkKind::Step { number } if number != n => {
                        let mut c = m.clone();
                        c.kind = MarkKind::Step { number: n };
                        Some(c)
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// Moves a step `direction` places earlier (negative) or later in the numbering (one undo).
    pub fn reorder_step(&mut self, id: u64, direction: i32) -> bool {
        let Some(at) = self
            .marks
            .iter()
            .position(|m| m.id == id && matches!(m.kind, MarkKind::Step { .. }))
        else {
            return false;
        };
        let mut moved = false;
        let news = self.renumbered(&[], |order| {
            if let Some(pos) = order.iter().position(|&i| i == at) {
                let to =
                    (pos as i64 + i64::from(direction)).clamp(0, order.len() as i64 - 1) as usize;
                if to != pos {
                    let i = order.remove(pos);
                    order.insert(to, i);
                    moved = true;
                }
            }
        });
        moved && self.replace_marks(news)
    }

    /// Deletes the selection as one undo. Connectors attached to a deleted mark stay where they were
    /// drawn (detached); steps renumber. Undo restores all of it.
    pub fn remove_many(&mut self, ids: &[u64]) -> bool {
        let removed = self.expand_groups(ids);
        if removed.is_empty() {
            return false;
        }
        let mut changed: HashMap<u64, Mark> = HashMap::new();
        let left = || self.marks.iter().filter(|m| !removed.contains(&m.id));
        for m in left() {
            let MarkKind::Connector { start, end } = &m.kind else {
                continue;
            };
            let gone = |e: &Endpoint| e.anchor.is_some_and(|a| removed.contains(&a.mark));
            let Some(inv) = m.transform.inverse() else {
                continue;
            };
            if gone(start) || gone(end) {
                let free = |e: &Endpoint| {
                    if gone(e) {
                        Endpoint {
                            point: inv.apply(self.resolve(e, m.transform, &View::NONE)),
                            anchor: None,
                        }
                    } else {
                        *e
                    }
                };
                let (start, end) = (free(start), free(end));
                let mut n = m.clone();
                n.points = vec![start.point, end.point];
                n.kind = MarkKind::Connector { start, end };
                changed.insert(m.id, n);
            }
        }
        for n in self.renumbered(&removed, |_| {}) {
            let kind = n.kind.clone();
            changed.entry(n.id).or_insert(n).kind = kind;
        }
        let set: Vec<Mark> = self
            .marks
            .iter()
            .filter_map(|m| changed.remove(&m.id))
            .collect();
        let mut inverse = vec![];
        if !set.is_empty() {
            inverse.push(self.apply_op(Op::Set(set)));
        }
        inverse.push(self.apply_op(Op::Remove(removed)));
        inverse.reverse();
        self.commit(inverse);
        true
    }

    /// Copies the (expanded) selection on top, shifted by `DUPLICATE_OFFSET`; groups and links
    /// between copied marks are remapped to the copies, steps continue the numbering. Returns the
    /// new ids (empty when nothing was copied). One undo.
    pub fn duplicate_marks(&mut self, ids: &[u64]) -> Vec<u64> {
        let ids = self.expand_groups(ids);
        if ids.is_empty() || self.marks.len() + ids.len() > MAX_MARKS {
            return vec![];
        }
        let sources: Vec<Mark> = self
            .marks
            .iter()
            .filter(|m| ids.contains(&m.id))
            .cloned()
            .collect();
        let shift = Affine::translation(Point {
            x: DUPLICATE_OFFSET,
            y: DUPLICATE_OFFSET,
        });
        if sources
            .iter()
            .any(|m| !shift.multiply(m.transform).is_sane())
        {
            return vec![];
        }
        let mut remap = HashMap::new();
        for m in &sources {
            remap.insert(m.id, self.id());
        }
        let mut groups = HashMap::new();
        for g in sources.iter().filter_map(|m| m.group) {
            groups.entry(g).or_insert_with(|| self.id());
        }
        let mut steps: Vec<(u32, usize)> = sources
            .iter()
            .enumerate()
            .filter_map(|(i, m)| match m.kind {
                MarkKind::Step { number } => Some((number, i)),
                _ => None,
            })
            .collect();
        steps.sort_unstable();
        let first = self.next_step_number();
        let numbers: HashMap<usize, u32> = steps
            .into_iter()
            .enumerate()
            .map(|(n, (_, i))| (i, first + n as u32))
            .collect();
        let copies: Vec<Mark> = sources
            .into_iter()
            .enumerate()
            .map(|(i, mut m)| {
                m.id = remap[&m.id];
                m.group = m.group.map(|g| groups[&g]);
                m.transform = shift.multiply(m.transform);
                match &mut m.kind {
                    MarkKind::Step { number } => *number = numbers[&i],
                    MarkKind::Connector { start, end } => {
                        for e in [start, end] {
                            if let Some(a) = &mut e.anchor
                                && let Some(&n) = remap.get(&a.mark)
                            {
                                a.mark = n;
                            }
                        }
                    }
                    _ => {}
                }
                m
            })
            .collect();
        let new_ids: Vec<u64> = copies.iter().map(|m| m.id).collect();
        self.marks.extend(copies);
        self.commit(vec![Op::Remove(new_ids.clone())]);
        new_ids
    }

    pub fn set_crop(&mut self, crop: Option<Crop>) {
        if self.crop != crop {
            let inverse = self.apply_op(Op::Crop(crop));
            self.commit(vec![inverse]);
        }
    }

    /// Applies a presentation (background, padding, border, shadow, output size) as one undo.
    pub fn set_presentation(&mut self, presentation: Presentation) -> Result<()> {
        self.set_presentation_and_crop(presentation, self.crop)
    }

    /// Presentation and crop together as one undo; validated against each other before anything
    /// changes, a no-op when neither differs.
    pub fn set_presentation_and_crop(
        &mut self,
        presentation: Presentation,
        crop: Option<Crop>,
    ) -> Result<()> {
        presentation.validate()?;
        if let Some(c) = &crop {
            ensure!(
                valid_crop(c, self.width, self.height),
                "crop outside the image"
            );
        }
        let (w, h) = crop.map_or((self.width, self.height), |c| (c.width, c.height));
        presentation.dimensions(w, h)?;
        let mut edit = vec![];
        if presentation != self.presentation {
            edit.push(self.apply_op(Op::Presentation(presentation)));
        }
        if crop != self.crop {
            edit.push(self.apply_op(Op::Crop(crop)));
        }
        if !edit.is_empty() {
            edit.reverse();
            self.commit(edit);
        }
        Ok(())
    }

    // --- history ------------------------------------------------------------------------------

    /// Performs `op` and returns the op that reverses it.
    fn apply_op(&mut self, op: Op) -> Op {
        match op {
            Op::Set(news) => Op::Set(
                news.into_iter()
                    .map(|new| {
                        let slot = self
                            .marks
                            .iter_mut()
                            .find(|m| m.id == new.id)
                            .expect("history mark exists");
                        std::mem::replace(slot, new)
                    })
                    .collect(),
            ),
            Op::Remove(ids) => {
                let mut at: Vec<usize> = self
                    .marks
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| ids.contains(&m.id))
                    .map(|(i, _)| i)
                    .collect();
                at.sort_unstable();
                let mut removed: Vec<(usize, Mark)> = at
                    .into_iter()
                    .rev()
                    .map(|i| (i, self.marks.remove(i)))
                    .collect();
                removed.reverse();
                Op::Insert(removed)
            }
            Op::Insert(items) => {
                let ids = items.iter().map(|(_, m)| m.id).collect();
                for (i, m) in items {
                    self.marks.insert(i, m);
                }
                Op::Remove(ids)
            }
            Op::Crop(crop) => Op::Crop(std::mem::replace(&mut self.crop, crop)),
            Op::Presentation(p) => Op::Presentation(std::mem::replace(&mut self.presentation, p)),
            Op::Reorder(order) => {
                let previous = self.marks.iter().map(|m| m.id).collect();
                let rank: HashMap<u64, usize> = order
                    .into_iter()
                    .enumerate()
                    .map(|(i, id)| (id, i))
                    .collect();
                self.marks.sort_by_key(|m| rank[&m.id]);
                Op::Reorder(previous)
            }
        }
    }

    fn apply(&mut self, edit: Edit) -> Edit {
        let mut inverse: Vec<Op> = edit.into_iter().map(|op| self.apply_op(op)).collect();
        inverse.reverse();
        inverse
    }

    pub fn undo(&mut self) {
        if let Some(edit) = self.undo.pop() {
            let inverse = self.apply(edit);
            self.redo.push(inverse);
            self.revision += 1;
        }
    }
    pub fn redo(&mut self) {
        if let Some(edit) = self.redo.pop() {
            let inverse = self.apply(edit);
            self.undo.push(inverse);
            self.revision += 1;
        }
    }

    // --- rendering ----------------------------------------------------------------------------

    /// Screenshot, annotations in z-order, then every redaction on top.
    pub fn paint(&self, cr: &Cairo) -> Result<()> {
        self.paint_with(cr, &View::NONE)
    }

    /// Like `paint`, with `ids` (and their groups) drawn as if `matrix` were applied; nothing is cloned
    /// or mutated, attached connectors and lenses follow the preview.
    pub fn paint_preview(&self, cr: &Cairo, ids: &[u64], matrix: Affine) -> Result<()> {
        if !matrix.is_sane() {
            return self.paint(cr);
        }
        let ids = self.expand_groups(ids);
        self.paint_with(
            cr,
            &View {
                ids: &ids,
                matrix,
                text: None,
            },
        )
    }

    /// Draws one mark that may not be in the document yet (a tool preview) against the current
    /// scene: connector attachments resolve, lenses sample the sanitised screenshot, redactions cover.
    pub fn paint_mark(&self, cr: &Cairo, mark: &Mark) -> Result<()> {
        if !matches!(mark.kind, MarkKind::Spotlight { .. }) {
            return self.paint_one(cr, mark, &View::NONE);
        }
        // An existing dim layer cannot be undone, so then a draft shows only its outline.
        if self
            .marks
            .iter()
            .any(|m| matches!(m.kind, MarkKind::Spotlight { .. }))
        {
            return spot_outline(cr, mark, mark.transform);
        }
        paint_spots(cr, &[(mark, mark.transform)])
    }

    /// The scene as `paint` draws it, but with `preview` standing in for the live text mark
    /// `original` (same layer and z-order, so nothing ghosts) or, for a new caption, on top of the
    /// annotations, still under spotlights' dimming and privacy masks. Nothing is cloned or mutated.
    pub fn paint_text_preview(
        &self,
        cr: &Cairo,
        original: Option<u64>,
        preview: &Mark,
    ) -> Result<()> {
        self.paint_with(
            cr,
            &View {
                ids: &[],
                matrix: Affine::identity(),
                text: Some((preview, original)),
            },
        )
    }

    fn paint_with(&self, cr: &Cairo, view: &View) -> Result<()> {
        cr.set_source_surface(&self.background, 0.0, 0.0)?;
        cr.paint()?;
        let mut placed = false;
        for m in self.marks.iter().filter(|m| !m.is_redaction()) {
            match view.text {
                Some((p, Some(id))) if id == m.id => {
                    self.paint_one(cr, p, view)?;
                    placed = true;
                }
                _ => self.paint_one(cr, m, view)?,
            }
        }
        if let Some((p, _)) = view.text.filter(|_| !placed) {
            self.paint_one(cr, p, view)?;
        }
        let spots: Vec<(&Mark, Affine)> = self
            .marks
            .iter()
            .filter(|m| matches!(m.kind, MarkKind::Spotlight { .. }))
            .map(|m| (m, view.tf(m)))
            .collect();
        paint_spots(cr, &spots)?;
        for m in self.marks.iter().filter(|m| m.is_redaction()) {
            self.paint_one(cr, m, view)?;
        }
        Ok(())
    }

    fn paint_one(&self, cr: &Cairo, m: &Mark, view: &View) -> Result<()> {
        let tf = view.tf(m);
        match &m.kind {
            MarkKind::Redaction | MarkKind::PrivacyBlur => {
                if let Some(q) = m.quad(tf).filter(|_| tf.is_sane()) {
                    cr.save()?;
                    // Fail closed: without the texture the mask is flat grey, never the screenshot.
                    let blur = matches!(m.kind, MarkKind::PrivacyBlur);
                    let textured = blur
                        && MOSAIC.with(|p| p.as_ref().is_some_and(|p| cr.set_source(p).is_ok()));
                    if !textured {
                        let grey = if blur { 0.55 } else { 0.0 };
                        cr.set_source_rgb(grey, grey, grey);
                    }
                    cover_quad(cr, &q, self.width, self.height);
                    let filled = cr.fill();
                    cr.new_path();
                    cr.restore()?;
                    filled?;
                }
                Ok(())
            }
            MarkKind::Spotlight { .. } => Ok(()),
            MarkKind::Magnifier { source, zoom } => {
                self.paint_lens(cr, m, tf, *source, *zoom, view)?;
                m.draw(cr, tf, None)
            }
            _ => m.draw(cr, tf, self.ends(m, view)),
        }
    }

    /// Nearest-neighbour sample of the screenshot into the lens, then the pixels any redaction
    /// touches blacked out in lens space: a redacted screenshot pixel can never be magnified,
    /// whichever layer the redaction is on.
    fn paint_lens(
        &self,
        cr: &Cairo,
        m: &Mark,
        tf: Affine,
        source: Bounds,
        zoom: f32,
        view: &View,
    ) -> Result<()> {
        use gtk::cairo::Antialias;
        let Some(lens) = m.quad(tf).filter(|_| tf.is_sane()) else {
            return Ok(());
        };
        let (a, b) = (m.points[0], m.points[1]);
        let map = tf
            .multiply(Affine::translation(Point {
                x: (a.x + b.x) / 2.0,
                y: (a.y + b.y) / 2.0,
            }))
            .multiply(Affine::scale_about(zoom, zoom, Point { x: 0.0, y: 0.0 }))
            .multiply(Affine::translation(Point {
                x: -(source.x + source.width / 2.0),
                y: -(source.y + source.height / 2.0),
            }));
        let Some(inv) = map.inverse() else {
            return Ok(());
        };
        let pattern = SurfacePattern::create(&self.background);
        pattern.set_filter(Filter::Nearest);
        pattern.set_extend(Extend::None);
        pattern.set_matrix(inv.into());
        cr.save()?;
        let drawn = (|| -> Result<()> {
            quad_path(cr, &lens);
            cr.clip();
            cr.set_source(&pattern)?;
            cr.paint()?;
            // Pixel-exact: the lens samples nearest-neighbour, so black every lens pixel whose
            // sample would come from a screenshot pixel a redaction touches, on any layer.
            cr.set_source_rgb(0.0, 0.0, 0.0);
            cr.set_antialias(Antialias::None);
            for r in self.marks.iter().filter(|r| r.is_redaction()) {
                let rt = view.tf(r);
                if let Some(q) = r.quad(rt).filter(|_| rt.is_sane()) {
                    cover_quad_mapped(cr, &q, self.width, self.height, map);
                }
            }
            cr.fill()?;
            cr.set_antialias(Antialias::Default);
            Ok(())
        })();
        cr.new_path();
        cr.restore()?;
        drawn
    }

    /// The scene as pixels: cropped, then framed by the presentation. The scene stays a Cairo
    /// surface until here, so framing never takes an RGBA round trip.
    pub fn render(&self) -> Result<RgbaImage> {
        rgba_from(&self.presentation.apply_surface(self.scene_surface()?)?)
    }

    /// The cropped scene, before the presentation frames it.
    pub(crate) fn scene_surface(&self) -> Result<ImageSurface> {
        let crop = self.crop.unwrap_or(Crop {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        });
        self.render_view(crop, &View::NONE)
    }

    fn render_view(&self, crop: Crop, view: &View) -> Result<ImageSurface> {
        let surface = ImageSurface::create(
            Format::ARgb32,
            crop.width.try_into()?,
            crop.height.try_into()?,
        )?;
        {
            let cr = Cairo::new(&surface)?;
            cr.translate(-f64::from(crop.x), -f64::from(crop.y));
            self.paint_with(&cr, view)?;
        }
        Ok(surface)
    }
}

pub fn save_new_png(image: &RgbaImage, path: &Path) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; choose a new filename", path.display());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .context("Cannot create file in destination folder")?;
    {
        let mut writer = BufWriter::new(temp.as_file_mut());
        image::codecs::png::PngEncoder::new(&mut writer).write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )?;
        std::io::Write::flush(&mut writer)?;
    }
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|e| e.error)
        .with_context(|| format!("Cannot save {}", path.display()))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::f32::consts::FRAC_PI_2;

    const RED: [f64; 4] = [1., 0., 0., 1.];

    fn point(x: f32, y: f32) -> Point {
        Point { x, y }
    }
    fn near(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3
    }
    fn shape(id: u64, points: &[(f32, f32)], closed: bool) -> Mark {
        Mark::path(
            id,
            points.iter().map(|&(x, y)| point(x, y)).collect(),
            closed,
            RED,
            2.,
        )
    }
    fn mark(id: u64) -> Mark {
        shape(id, &[(2., 2.), (8., 2.)], false)
    }
    fn text_mark(id: u64, width: f64) -> Mark {
        Mark::text(id, point(10., 10.), "Hello", RED, width)
    }
    fn background() -> Result<Document> {
        Document::new(RgbaImage::from_pixel(
            16,
            16,
            image::Rgba([20, 40, 60, 255]),
        ))
    }
    fn big(w: u32, h: u32) -> Result<Document> {
        Document::new(RgbaImage::from_pixel(w, h, image::Rgba([20, 40, 60, 255])))
    }
    fn ids(d: &Document) -> Vec<u64> {
        d.marks.iter().map(|m| m.id).collect()
    }
    fn render_preview(doc: &Document, ids: &[u64], matrix: Affine) -> Result<RgbaImage> {
        let ids = doc.expand_groups(ids);
        rgba_from(&doc.render_view(
            Crop {
                x: 0,
                y: 0,
                width: doc.width,
                height: doc.height,
            },
            &View {
                ids: &ids,
                matrix,
                text: None,
            },
        )?)
    }

    #[test]
    fn undo_crop_and_ink_then_branch_discards_redo() -> Result<()> {
        let mut doc = Document::new(RgbaImage::new(12, 10))?;
        let id = doc.id();
        doc.add(mark(id));
        let crop = Crop::between(point(10.2, 8.2), point(-4., 1.2), 12, 10).unwrap();
        doc.set_crop(Some(crop));
        doc.undo();
        assert_eq!(doc.crop, None);
        doc.undo();
        assert!(doc.marks.is_empty());
        doc.redo();
        assert_eq!(doc.marks[0].id, id);
        doc.redo();
        assert_eq!(
            doc.crop,
            Some(Crop {
                x: 0,
                y: 1,
                width: 11,
                height: 8
            })
        );
        doc.undo();
        let next = doc.id();
        doc.add(mark(next));
        assert!(!doc.can_redo());
        Ok(())
    }

    #[test]
    fn recognition_completes_while_stroke_is_undone() -> Result<()> {
        let mut doc = Document::new(RgbaImage::new(12, 10))?;
        let id = doc.id();
        doc.add(mark(id));
        doc.undo();
        assert!(doc.snap(id, vec![point(1., 1.), point(9., 9.)], true));
        doc.redo();
        assert!(doc.marks[0].closed);
        assert_eq!(doc.marks[0].points[1].y, 9.);
        doc.undo();
        let next = doc.id();
        doc.add(mark(next));
        assert!(!doc.snap(id, vec![point(0., 0.)], false));
        Ok(())
    }

    #[test]
    fn export_preserves_pixels_and_renders_ink_in_crop_coordinates() -> Result<()> {
        let mut doc = Document::new(RgbaImage::from_pixel(
            12,
            10,
            image::Rgba([20, 40, 60, 255]),
        ))?;
        doc.add(mark(1));
        doc.set_crop(Some(Crop {
            x: 2,
            y: 1,
            width: 8,
            height: 6,
        }));
        let image = doc.render()?;
        assert_eq!(image.dimensions(), (8, 6));
        assert_eq!(image.get_pixel(4, 1).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(7, 5).0, [20, 40, 60, 255]);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("export.png");
        save_new_png(&image, &path)?;
        assert!(save_new_png(&image, &path).is_err());
        assert_eq!(image::open(path)?.into_rgba8(), image);
        Ok(())
    }

    #[test]
    fn affine_algebra_composes_and_inverts() {
        let origin = point(0., 0.);
        let r = Affine::rotation_about(FRAC_PI_2, point(10., 10.));
        assert!(near(r.apply(point(20., 10.)), point(10., 20.)));
        assert!(near(
            r.inverse().unwrap().apply(point(10., 20.)),
            point(20., 10.)
        ));
        let t = Affine::translation(point(5., 0.));
        let s = Affine::scale_about(2., 2., origin);
        assert!(near(t.multiply(s).apply(point(1., 1.)), point(7., 2.)));
        assert!(near(s.then(t).apply(point(1., 1.)), point(7., 2.)));
        assert!(near(
            Affine::scale_about(3., 3., point(1., 1.)).apply(point(1., 1.)),
            point(1., 1.)
        ));
        let singular = Affine {
            a: 0.,
            b: 0.,
            c: 0.,
            d: 0.,
            tx: 1.,
            ty: 1.,
        };
        assert!(singular.inverse().is_none() && !singular.is_sane());
        assert!(Affine::identity().is_sane());
    }

    #[test]
    fn hit_testing_uses_ink_not_bounding_boxes() {
        let mut diagonal = shape(1, &[(0., 0.), (100., 100.)], false);
        assert!(diagonal.hit_test(point(50., 50.), 0.));
        assert!(!diagonal.hit_test(point(90., 10.), 3.));
        diagonal.transform = Affine::translation(point(10., 0.));
        assert!(diagonal.hit_test(point(60., 50.), 0.));
        assert!(!diagonal.hit_test(point(50., 50.), 2.));

        // Unfilled: the outline (closing edge included) is hit, the interior is not.
        let square = shape(2, &[(10., 10.), (50., 10.), (50., 50.), (10., 50.)], true);
        assert!(!square.hit_test(point(30., 30.), 3.));
        assert!(square.hit_test(point(10., 30.), 0.));
        assert!(square.hit_test(point(30., 50.), 0.));
        assert!(square.hit_test(point(11., 30.), 2.));
        assert!(!square.hit_test(point(70., 30.), 3.));
        let mut open = square.clone();
        open.closed = false;
        assert!(open.hit_test(point(30., 10.), 0.));
        assert!(
            !open.hit_test(point(10., 30.), 0.),
            "an open path has no closing edge"
        );
        assert!(!open.hit_test(point(30., 30.), 3.));

        let mut dot = shape(3, &[(20., 20.)], false);
        dot.width = 8.;
        assert!(dot.hit_test(point(23., 20.), 0.));
        assert!(!dot.hit_test(point(25., 20.), 0.));
        assert!(dot.hit_test(point(25., 20.), 2.));
        assert!(!dot.hit_test(point(f32::NAN, 20.), 2.));
    }

    #[test]
    fn document_hit_test_returns_topmost_mark() -> Result<()> {
        let mut doc = Document::new(RgbaImage::new(100, 100))?;
        let corners = |x: f32| [(x, 10.), (x + 40., 10.), (x + 40., 50.), (x, 50.)];
        doc.add(shape(1, &corners(10.), true));
        doc.add(shape(2, &corners(30.), true));
        assert_eq!(doc.hit_test(point(40., 10.), 0.), Some(2));
        assert_eq!(doc.hit_test(point(20., 10.), 0.), Some(1));
        // Inside the top shape but off its outline: the click falls through to the outline beneath.
        assert_eq!(doc.hit_test(point(50., 30.), 0.), Some(1));
        assert_eq!(doc.hit_test(point(90., 90.), 0.), None);
        Ok(())
    }

    #[test]
    fn text_bounds_follow_pango_layout_and_transform() {
        let mut text = text_mark(1, 4.);
        let b = text.bounds().unwrap();
        assert!(b.width > 0. && b.height > 0.);
        assert!((b.x - 10.).abs() < 1. && (b.y - 10.).abs() < 1.);
        assert!(text_mark(1, 8.).bounds().unwrap().width > b.width);
        let centre = point(b.x + b.width / 2., b.y + b.height / 2.);
        assert!(text.hit_test(centre, 0.));
        assert!(!text.hit_test(point(b.x + b.width + 20., centre.y), 5.));
        text.transform = Affine::translation(point(100., 0.));
        assert!(!text.hit_test(centre, 0.));
        assert!(text.hit_test(point(centre.x + 100., centre.y), 0.));
        assert_eq!(text.bounds().unwrap().x, b.x + 100.);

        let mut line = mark(2);
        assert_eq!(
            line.bounds(),
            Some(Bounds {
                x: 1.,
                y: 1.,
                width: 8.,
                height: 2.
            })
        );
        line.transform = Affine::translation(point(3., 4.));
        assert_eq!(line.bounds().unwrap().x, 4.);
        assert_eq!(line.bounds().unwrap().y, 5.);
        line.points.clear();
        assert_eq!(line.bounds(), None);
    }

    #[test]
    fn move_is_one_exact_undoable_edit() -> Result<()> {
        let mut doc = background()?;
        doc.add(mark(1));
        let before = doc.render()?;
        assert!(doc.move_marks(&[1], point(0., 6.)));
        assert_eq!(doc.marks[0].points[0], point(2., 2.));
        let moved = doc.render()?;
        assert_ne!(moved, before);
        let mut expected = background()?;
        expected.add(shape(1, &[(2., 8.), (8., 8.)], false));
        assert_eq!(moved, expected.render()?);
        for delta in [point(0., 0.), point(f32::NAN, 1.), point(f32::INFINITY, 0.)] {
            assert!(!doc.move_marks(&[1], delta));
        }
        assert!(!doc.move_marks(&[9], point(1., 1.)));
        doc.undo();
        assert_eq!(doc.render()?, before);
        assert_eq!(doc.marks[0].transform, Affine::identity());
        doc.redo();
        assert_eq!(doc.render()?, moved);
        doc.undo();
        doc.undo();
        assert!(doc.marks.is_empty(), "one undo per move, then the add");
        Ok(())
    }

    #[test]
    fn affine_batch_is_one_exact_undo_and_hits_through_rotation() -> Result<()> {
        let mut doc = big(120, 120)?;
        doc.add(shape(1, &[(0., 0.), (100., 0.)], false));
        doc.add(shape(2, &[(2., 12.), (8., 12.)], false));
        let before = doc.marks.clone();
        let quarter = Affine::rotation_about(FRAC_PI_2, point(0., 0.));
        assert!(doc.transform_marks(&[1], quarter));
        assert_eq!(doc.hit_test(point(0., 50.), 1.), Some(1));
        assert_eq!(doc.hit_test(point(50., 0.), 1.), None);
        let b = doc.selection_bounds(&[1]).unwrap();
        assert!(b.width < 3. && (b.height - 102.).abs() < 0.1, "{b:?}");
        doc.undo();
        assert_eq!(doc.marks, before);
        doc.redo();
        let turned = doc.marks.clone();

        let spin = Affine::rotation_about(0.5, point(5., 5.));
        assert!(doc.transform_marks(&[1, 2], spin));
        assert!(doc.transform_marks(&[2], Affine::scale_about(2., 2., point(0., 0.))));
        doc.undo();
        doc.undo();
        assert_eq!(doc.marks, turned, "each batch undoes whole and exactly");

        let nan = Affine {
            a: f32::NAN,
            ..Affine::identity()
        };
        let flat = Affine {
            d: 0.,
            ..Affine::identity()
        };
        let huge = Affine::scale_about(1e9, 1e9, point(0., 0.));
        for bad in [nan, flat, huge, Affine::identity()] {
            assert!(!doc.transform_marks(&[1, 2], bad));
        }
        assert_eq!(doc.marks, turned);
        doc.undo();
        assert_eq!(doc.marks, before, "rejected matrices left no history");
        Ok(())
    }

    #[test]
    fn groups_move_together_and_duplicate_with_remapped_links() -> Result<()> {
        let mut doc = big(100, 100)?;
        doc.add(shape(
            1,
            &[(10., 10.), (30., 10.), (30., 30.), (10., 30.)],
            true,
        ));
        doc.add(shape(2, &[(50., 50.), (70., 50.)], false));
        doc.add(shape(3, &[(5., 80.), (25., 80.)], false));
        assert!(!doc.group_marks(&[1]), "a single mark is not a group");
        assert!(doc.group_marks(&[1, 2]));
        assert!(!doc.group_marks(&[1]), "already one group");
        assert_eq!(doc.expand_groups(&[2]), [1, 2]);
        assert_eq!(doc.expand_groups(&[3, 99]), [3]);
        let g = doc.mark(1).unwrap().group;
        assert!(g.is_some() && g == doc.mark(2).unwrap().group);

        let start = doc.endpoint(point(32., 20.), 3.);
        assert_eq!(start.anchor.map(|a| a.mark), Some(1));
        let id = doc.id();
        doc.add(Mark::connector(
            id,
            start,
            Endpoint {
                point: point(60., 20.),
                anchor: None,
            },
            RED,
            2.,
        ));
        assert!(doc.ungroup_marks(&[1]));
        assert_eq!(doc.mark(2).unwrap().group, None);
        assert!(doc.group_marks(&[1, 2, id]));

        let before = doc.marks.clone();
        assert!(doc.move_marks(&[2], point(0., 4.)));
        for m in &doc.marks {
            let moved = m.transform.ty == 4.;
            assert_eq!(moved, m.id != 3, "mark {}", m.id);
        }
        doc.undo();
        assert_eq!(doc.marks, before);

        let copies = doc.duplicate_marks(&[1]);
        assert_eq!(copies.len(), 3);
        assert_eq!(doc.marks.len(), 7);
        let new_group = doc.mark(copies[0]).unwrap().group;
        assert!(new_group.is_some() && new_group != g);
        assert!(
            copies
                .iter()
                .all(|&c| doc.mark(c).unwrap().group == new_group)
        );
        let copy_conn = doc.mark(copies[2]).unwrap();
        assert_eq!(copy_conn.transform.tx, DUPLICATE_OFFSET);
        let MarkKind::Connector { start, .. } = &copy_conn.kind else {
            panic!("connector copy");
        };
        assert_eq!(
            start.anchor.unwrap().mark,
            copies[0],
            "link remapped to the copy"
        );
        let ends = doc.connector_ends(copies[2]).unwrap();
        assert!(near(
            ends[0],
            point(31. + DUPLICATE_OFFSET, 20. + DUPLICATE_OFFSET)
        ));
        // The original is untouched.
        assert_eq!(
            doc.mark(id).unwrap(),
            before.iter().find(|m| m.id == id).unwrap()
        );
        doc.undo();
        assert_eq!(doc.marks, before);
        doc.redo();
        assert_eq!(doc.marks.len(), 7);
        Ok(())
    }

    #[test]
    fn deleting_a_target_detaches_connectors_and_undo_relinks() -> Result<()> {
        let mut doc = big(100, 80)?;
        doc.add(shape(
            1,
            &[(10., 10.), (30., 10.), (30., 30.), (10., 30.)],
            true,
        ));
        doc.add(shape(5, &[(70., 60.), (90., 60.)], false));
        let start = doc.endpoint(point(32., 20.), 3.);
        assert!(start.anchor.is_some());
        let far = doc.endpoint(point(90., 30.), 3.);
        assert!(far.anchor.is_none());
        doc.add(Mark::connector(2, start, far, RED, 2.));
        assert!(near(doc.connector_ends(2).unwrap()[0], point(31., 20.)));

        let before = doc.marks.clone();
        assert!(doc.move_marks(&[1], point(0., 10.)));
        assert!(
            near(doc.connector_ends(2).unwrap()[0], point(31., 30.)),
            "end follows its target"
        );
        let moved = doc.marks.clone();

        // The preview draws the same pixels the committed move will.
        let mut twin = big(100, 80)?;
        twin.marks = before.clone();
        let preview = render_preview(&twin, &[1], Affine::translation(point(0., 10.)))?;
        assert_eq!(twin.marks, before, "preview does not touch the document");
        assert_eq!(preview, doc.render()?);

        let seen = doc.connector_ends(2).unwrap();
        assert!(doc.remove_many(&[1]));
        assert!(doc.mark(1).is_none());
        let MarkKind::Connector { start, .. } = &doc.mark(2).unwrap().kind else {
            panic!("connector");
        };
        assert_eq!(start.anchor, None);
        assert_eq!(
            doc.connector_ends(2).unwrap(),
            seen,
            "stays where it was drawn"
        );
        doc.undo();
        assert_eq!(doc.marks, moved, "undo restores the link exactly");
        doc.redo();
        assert_eq!(doc.connector_ends(2).unwrap(), seen);
        doc.undo();
        doc.undo();
        assert_eq!(doc.marks, before);

        // Redactions, lenses and connectors are never attachment targets.
        doc.add(Mark::redaction(9, point(60., 5.), point(80., 25.)));
        assert!(doc.endpoint(point(60., 15.), 3.).anchor.is_none());
        assert!(!doc.set_connector_ends(
            2,
            doc.endpoint(point(60., 15.), 3.),
            Endpoint {
                point: point(1., 1.),
                anchor: Some(Anchor {
                    mark: 9,
                    u: 0.,
                    v: 0.
                })
            }
        ));
        Ok(())
    }

    #[test]
    fn late_recognition_keeps_transforms_through_undo_and_delete() -> Result<()> {
        let mut doc = background()?;
        doc.add(mark(1));
        let turn = Affine::rotation_about(0.4, point(5., 5.));
        assert!(doc.transform_marks(&[1], turn));
        assert!(doc.snap(1, vec![point(1., 1.), point(9., 9.)], true));
        assert_eq!(doc.marks[0].transform, turn);
        assert!(doc.marks[0].closed);
        doc.undo();
        assert_eq!(doc.marks[0].transform, Affine::identity());
        assert_eq!(doc.marks[0].points[1], point(9., 9.));
        doc.redo();
        assert_eq!(doc.marks[0].transform, turn);

        assert!(doc.remove_many(&[1]));
        assert!(doc.snap(1, vec![point(0., 0.), point(4., 4.)], false));
        doc.undo();
        assert_eq!(doc.marks[0].points[1], point(4., 4.));
        assert_eq!(doc.marks[0].transform, turn);
        assert!(!doc.marks[0].closed);

        // Explicit replacement is a normal undo step; non-paths and junk are refused.
        assert!(doc.replace_geometry(1, vec![point(2., 2.), point(6., 6.), point(2., 6.)], true));
        assert!(doc.marks[0].closed && doc.marks[0].transform == turn);
        doc.undo();
        assert_eq!(doc.marks[0].points.len(), 2);
        assert!(!doc.replace_geometry(1, vec![point(f32::NAN, 0.)], false));
        assert!(!doc.replace_geometry(1, vec![], false));
        doc.add(text_mark(2, 3.));
        assert!(!doc.snap(2, vec![point(0., 0.)], false));
        assert!(!doc.replace_geometry(2, vec![point(0., 0.)], false));
        Ok(())
    }

    #[test]
    fn delete_undo_restores_z_order() -> Result<()> {
        let mut doc = background()?;
        for id in 1..=3 {
            doc.add(mark(id));
        }
        assert!(doc.remove_many(&[2]));
        assert!(!doc.remove_many(&[2]));
        assert_eq!(ids(&doc), [1, 3]);
        doc.undo();
        assert_eq!(ids(&doc), [1, 2, 3]);
        doc.redo();
        assert_eq!(ids(&doc), [1, 3]);
        doc.undo();
        assert!(doc.hit_test(point(5., 2.), 0.).is_some());

        assert!(doc.remove_many(&[1, 3]));
        assert_eq!(ids(&doc), [2]);
        doc.undo();
        assert_eq!(ids(&doc), [1, 2, 3]);
        Ok(())
    }

    #[test]
    fn deleting_a_group_member_deletes_the_group() -> Result<()> {
        let mut doc = background()?;
        for id in 1..=3 {
            doc.add(mark(id));
        }
        assert!(doc.group_marks(&[1, 2]));
        assert!(doc.remove_many(&[2]));
        assert_eq!(ids(&doc), [3], "no group is left with a lone member");
        doc.undo();
        assert!(doc.group_marks(&[2, 3]), "merges the existing group with 3");
        assert!(doc.mark(1).unwrap().group == doc.mark(3).unwrap().group);
        assert!(doc.remove_many(&[3]));
        assert!(doc.marks.is_empty());
        Ok(())
    }

    #[test]
    fn short_fat_arrow_head_stays_between_its_endpoints() {
        let (a, b) = (point(10., 10.), point(14., 10.));
        let (base, barbs) = arrow_head(a, b, 20.).unwrap();
        for p in [base, barbs[0], barbs[1]] {
            assert!(p.x > a.x && p.x < b.x, "{p:?} escapes the 4px shaft");
        }
        assert!(
            barbs[0].y != barbs[1].y,
            "barbs sit on both sides of the shaft"
        );
        // Any direction: shaft end and barbs project strictly inside the line a..b; tip is the end.
        let (a, b) = (point(50., 50.), point(40., 40.));
        let (base, barbs) = arrow_head(a, b, 3.).unwrap();
        let len2 = (b.x - a.x).powi(2) + (b.y - a.y).powi(2);
        for p in [base, barbs[0], barbs[1]] {
            let t = ((p.x - a.x) * (b.x - a.x) + (p.y - a.y) * (b.y - a.y)) / len2;
            assert!(t > 0. && t < 1., "t={t}");
        }
        assert!(arrow_head(a, a, 3.).is_none());

        // A 4px connector of width 20 paints nothing beyond its round cap past the tip.
        let mut doc = Document::new(RgbaImage::from_pixel(
            40,
            30,
            image::Rgba([20, 40, 60, 255]),
        ))
        .unwrap();
        let free = |x| Endpoint {
            point: point(x, 15.),
            anchor: None,
        };
        doc.add(Mark::connector(
            1,
            free(10.),
            free(14.),
            [0., 1., 0., 1.],
            20.,
        ));
        let image = doc.render().unwrap();
        assert_eq!(
            image.get_pixel(30, 15).0,
            [20, 40, 60, 255],
            "nothing beyond the tip"
        );
        assert_eq!(image.get_pixel(12, 15).0, [0, 255, 0, 255]);
    }

    #[test]
    fn style_change_applies_to_the_whole_selection_in_one_edit() -> Result<()> {
        let mut doc = background()?;
        doc.add(mark(1));
        doc.add(shape(2, &[(2., 12.), (8., 12.)], false));
        doc.add(shape(3, &[(2., 14.), (8., 14.)], false));
        assert!(doc.group_marks(&[2, 3]));
        let before = doc.render()?;
        let marks = doc.marks.clone();
        assert!(doc.set_style_many(&[1], [0., 0., 1., 1.], 4.));
        let styled = doc.render()?;
        assert_eq!(styled.get_pixel(5, 0).0, [0, 0, 255, 255]);
        assert_eq!(styled.get_pixel(5, 11).0, [255, 0, 0, 255]);
        assert!(doc.set_style_many(&[3], [0., 1., 0., 1.], 2.));
        assert_eq!(
            doc.mark(2).unwrap().color,
            [0., 1., 0., 1.],
            "group styled together"
        );
        doc.undo();
        let invalid = [
            ([f64::NAN, 0., 0., 1.], 2.),
            ([1.5, 0., 0., 1.], 2.),
            ([0., 0., 0., -0.1], 2.),
            ([0., 0., 1., 1.], 0.05),
            ([0., 0., 1., 1.], 129.),
            ([0., 0., 1., 1.], f64::NAN),
            ([0., 0., 1., 1.], 4.),
        ];
        for (color, width) in invalid {
            assert!(!doc.set_style_many(&[1], color, width));
        }
        assert!(!doc.set_style_many(&[9], [0., 0., 1., 1.], 3.));
        doc.undo();
        assert_eq!(doc.render()?, before, "rejected styles left no history");
        assert_eq!(doc.marks, marks);
        doc.redo();
        assert_eq!(doc.render()?, styled);
        Ok(())
    }

    /// Every pixel is a distinct colour, so a rendered colour identifies the exact source pixel.
    fn tagged(size: u32) -> RgbaImage {
        RgbaImage::from_fn(size, size, |x, y| image::Rgba([x as u8, y as u8, 200, 255]))
    }

    /// Source pixels whose centre lies inside the (transformed) rectangle.
    fn secret_colors(size: u32, rect: (f32, f32, f32, f32), tf: Affine) -> HashSet<[u8; 4]> {
        let inv = tf.inverse().unwrap();
        let mut set = HashSet::new();
        for y in 0..size {
            for x in 0..size {
                let q = inv.apply(point(x as f32 + 0.5, y as f32 + 0.5));
                if q.x >= rect.0 && q.x <= rect.2 && q.y >= rect.1 && q.y <= rect.3 {
                    set.insert([x as u8, y as u8, 200, 255]);
                }
            }
        }
        set
    }

    fn leaked(image: &RgbaImage, secret: &HashSet<[u8; 4]>) -> usize {
        let seen: HashSet<[u8; 4]> = image.pixels().map(|p| p.0).collect();
        secret.intersection(&seen).count()
    }

    #[test]
    fn redaction_is_opaque_black_regardless_of_style_and_layer() -> Result<()> {
        let mut doc = big(40, 40)?;
        let mut r = Mark::redaction(1, point(10., 10.), point(30., 30.));
        r.color = [1., 1., 1., 0.1];
        doc.add(r);
        doc.add(Mark::path(
            2,
            vec![point(0., 20.), point(40., 20.)],
            false,
            [0., 1., 0., 1.],
            6.,
        ));
        doc.add(Mark::text(
            3,
            point(12., 12.),
            "secret",
            [1., 1., 1., 1.],
            3.,
        ));
        assert!(doc.set_style_many(&[1], [1., 1., 0., 0.2], 24.));
        let image = doc.render()?;
        for (x, y) in [(10, 10), (15, 20), (29, 29), (12, 12), (20, 20)] {
            assert_eq!(
                image.get_pixel(x, y).0,
                [0, 0, 0, 255],
                "({x},{y}) under annotations"
            );
        }
        assert_eq!(image.get_pixel(9, 9).0, [20, 40, 60, 255]);
        assert_eq!(image.get_pixel(30, 30).0, [20, 40, 60, 255]);
        // Annotations outside the box are intact.
        assert_eq!(image.get_pixel(3, 20).0, [0, 255, 0, 255]);

        // Fractional edges round outwards to whole pixels with no antialiased seam.
        let mut doc = big(40, 40)?;
        doc.add(Mark::redaction(1, point(10.5, 10.5), point(20.2, 20.2)));
        let image = doc.render()?;
        for x in 10..=20 {
            assert_eq!(image.get_pixel(x, 15).0, [0, 0, 0, 255], "x={x}");
        }
        assert_eq!(image.get_pixel(9, 15).0, [20, 40, 60, 255]);
        assert_eq!(image.get_pixel(21, 15).0, [20, 40, 60, 255]);
        Ok(())
    }

    #[test]
    fn rotated_and_scaled_redaction_covers_every_touched_pixel() -> Result<()> {
        let mut doc = Document::new(tagged(100))?;
        doc.add(Mark::redaction(1, point(30., 40.), point(70., 60.)));
        let tf = Affine::rotation_about(0.6, point(50., 50.)).multiply(Affine::scale_about(
            1.3,
            0.9,
            point(50., 50.),
        ));
        assert!(doc.transform_marks(&[1], tf));
        let image = doc.render()?;
        let secret = secret_colors(100, (30., 40., 70., 60.), tf);
        assert!(secret.len() > 500);
        assert_eq!(
            leaked(&image, &secret),
            0,
            "source pixels inside the box survived"
        );
        // Something outside the box is still the screenshot.
        assert_eq!(image.get_pixel(2, 2).0, [2, 2, 200, 255]);
        Ok(())
    }

    #[test]
    fn redaction_blacks_pixels_overlapped_by_any_sliver() -> Result<()> {
        // Edges 1e-5 inside a pixel still black it out; exact boundaries do not spill.
        let mut doc = big(20, 20)?;
        doc.add(Mark::redaction(
            1,
            point(4.99999, 4.99999),
            point(10.00001, 10.00001),
        ));
        let image = doc.render()?;
        for (x, y) in [(4, 4), (4, 10), (10, 4), (10, 10), (7, 7)] {
            assert_eq!(image.get_pixel(x, y).0, [0, 0, 0, 255], "({x},{y})");
        }
        for (x, y) in [(3, 7), (11, 7), (7, 3), (7, 11)] {
            assert_eq!(image.get_pixel(x, y).0, [20, 40, 60, 255], "({x},{y})");
        }
        // Rotated: every pixel any sampled part of which lies in the quad, and every pixel holding
        // a corner, is fully black.
        let (rect, tf) = (
            (10.3_f32, 12.7_f32, 25.9_f32, 20.1_f32),
            Affine::rotation_about(0.5, point(18., 16.)),
        );
        let mut doc = big(40, 40)?;
        doc.add(Mark::redaction(
            1,
            point(rect.0, rect.1),
            point(rect.2, rect.3),
        ));
        assert!(doc.transform_marks(&[1], tf));
        let image = doc.render()?;
        let inv = tf.inverse().unwrap();
        let inside = |p: Point| {
            let q = inv.apply(p);
            q.x >= rect.0 && q.x <= rect.2 && q.y >= rect.1 && q.y <= rect.3
        };
        let mut black = 0;
        for y in 0..40 {
            for x in 0..40 {
                let hit = (0..=8).any(|j| {
                    (0..=8)
                        .any(|i| inside(point(x as f32 + i as f32 / 8., y as f32 + j as f32 / 8.)))
                });
                if hit {
                    assert_eq!(image.get_pixel(x, y).0, [0, 0, 0, 255], "({x},{y})");
                    black += 1;
                }
            }
        }
        assert!(black > 100);
        for c in [
            (rect.0, rect.1),
            (rect.2, rect.1),
            (rect.2, rect.3),
            (rect.0, rect.3),
        ] {
            let p = tf.apply(point(c.0, c.1));
            assert_eq!(
                image.get_pixel(p.x as u32, p.y as u32).0,
                [0, 0, 0, 255],
                "corner {p:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn magnifier_never_shows_redacted_pixels_on_any_layer() -> Result<()> {
        let rect = (45., 45., 55., 55.);
        for redaction_first in [true, false] {
            let mut doc = Document::new(tagged(100))?;
            let red = Mark::redaction(1, point(rect.0, rect.1), point(rect.2, rect.3));
            let lens = Mark::magnifier(
                2,
                Bounds {
                    x: 42.,
                    y: 42.,
                    width: 16.,
                    height: 16.,
                },
                2.5,
                point(60., 5.),
                point(98., 43.),
                [0., 1., 0., 1.],
                2.,
            );
            if redaction_first {
                doc.add(red);
                doc.add(lens);
            } else {
                doc.add(lens);
                doc.add(red);
            }
            let tf = Affine::rotation_about(0.4, point(50., 50.));
            assert!(doc.transform_marks(&[1], tf));
            let image = doc.render()?;
            let secret = secret_colors(100, rect, tf);
            assert_eq!(leaked(&image, &secret), 0, "first={redaction_first}");
            // The lens really magnifies: its interior is mostly screenshot pixels, not black.
            let shown = (8..40)
                .flat_map(|y| (62..96).map(move |x| (x, y)))
                .filter(|&(x, y)| image.get_pixel(x, y).0[2] == 200)
                .count();
            assert!(shown > 300, "lens shows {shown} screenshot pixels");
            // And it follows the redaction when that moves afterwards.
            let slide = Affine::translation(point(5., -4.));
            assert!(doc.transform_marks(&[1], slide));
            let secret = secret_colors(100, rect, slide.multiply(tf));
            assert_eq!(leaked(&doc.render()?, &secret), 0);
        }

        // A lens rotated and moved by a group still samples the sanitised screenshot.
        let mut doc = Document::new(tagged(100))?;
        doc.add(Mark::redaction(1, point(40., 40.), point(60., 60.)));
        doc.add(Mark::magnifier(
            2,
            Bounds {
                x: 45.,
                y: 45.,
                width: 10.,
                height: 10.,
            },
            3.,
            point(10., 70.),
            point(40., 95.),
            [1., 1., 0., 1.],
            2.,
        ));
        assert!(doc.transform_marks(&[2], Affine::rotation_about(0.9, point(25., 82.))));
        assert_eq!(
            leaked(
                &doc.render()?,
                &secret_colors(100, (40., 40., 60., 60.), Affine::identity())
            ),
            0
        );
        Ok(())
    }

    fn stripe(id: u64, color: [f64; 4]) -> Mark {
        Mark::path(id, vec![point(5., 20.), point(55., 20.)], false, color, 10.)
    }

    #[test]
    fn stacking_is_stable_group_aware_and_one_undo_each() -> Result<()> {
        let mut doc = big(60, 40)?;
        let colors = [
            [1., 0., 0., 1.],
            [0., 1., 0., 1.],
            [0., 0., 1., 1.],
            [1., 1., 1., 1.],
        ];
        for (i, c) in colors.iter().enumerate() {
            doc.add(stripe(i as u64 + 1, *c));
        }
        assert!(doc.group_marks(&[2, 3]));
        let base = doc.marks.clone();
        let top = |d: &Document| d.render().unwrap().get_pixel(30, 20).0;
        let hit = |d: &Document| d.hit_test(point(30., 20.), 0.);
        assert_eq!((top(&doc), hit(&doc)), ([255, 255, 255, 255], Some(4)));

        assert!(doc.reorder_marks(&[1], true));
        assert_eq!(ids(&doc), [2, 3, 4, 1]);
        assert_eq!((top(&doc), hit(&doc)), ([255, 0, 0, 255], Some(1)));
        let one = doc.marks.clone();
        doc.undo();
        assert_eq!(doc.marks, base);
        doc.redo();
        assert_eq!(doc.marks, one);

        // The group is already at the back: no change, no history.
        assert!(!doc.reorder_marks(&[3], false));
        assert_eq!(doc.marks, one);
        // One member moves the whole group, keeping 2 below 3.
        assert!(doc.reorder_marks(&[2], true));
        assert_eq!(ids(&doc), [4, 1, 2, 3]);
        assert_eq!(top(&doc), [0, 0, 255, 255]);
        assert!(!doc.reorder_marks(&[4], false));
        assert!(!doc.reorder_marks(&[99], true));
        // A mixed selection keeps its own relative order (1, 2, 3) and leaves 4 where it is relative
        // to the unselected.
        assert!(doc.reorder_marks(&[3, 1], false));
        assert_eq!(ids(&doc), [1, 2, 3, 4]);
        assert_eq!(doc.marks, base);

        doc.undo();
        assert_eq!(ids(&doc), [4, 1, 2, 3]);
        doc.undo();
        assert_eq!(ids(&doc), [2, 3, 4, 1]);
        doc.undo();
        assert_eq!(doc.marks, base);
        for _ in 0..3 {
            doc.redo();
        }
        assert_eq!(ids(&doc), [1, 2, 3, 4]);
        Ok(())
    }

    #[test]
    fn privacy_masks_stay_above_annotations_and_reorder_only_among_themselves() -> Result<()> {
        let mut doc = big(60, 50)?;
        doc.add(Mark::redaction(1, point(10., 10.), point(30., 30.)));
        doc.add(Mark::path(
            2,
            vec![point(0., 25.), point(60., 25.)],
            false,
            RED,
            6.,
        ));
        doc.add(Mark::privacy_blur(3, point(20., 20.), point(40., 40.)));
        let px = |d: &Document, x, y| d.render().unwrap().get_pixel(x, y).0;
        let before = doc.render()?;
        assert_eq!(px(&doc, 5, 25), [255, 0, 0, 255]);
        assert_eq!(px(&doc, 15, 25), [0, 0, 0, 255], "line never shows through");

        // Neither layer can change the other's stacking: these change nothing visible, so no edit.
        assert!(!doc.reorder_marks(&[2], true));
        assert!(!doc.reorder_marks(&[2], false));
        assert!(!doc.reorder_marks(&[1], false));
        assert_eq!(ids(&doc), [1, 2, 3]);

        // Among masks the order is real, and the annotation between them keeps its slot.
        assert!(doc.reorder_marks(&[1], true));
        assert_eq!(ids(&doc), [3, 2, 1]);
        assert_eq!(px(&doc, 25, 25), [0, 0, 0, 255]);
        assert_eq!(doc.hit_test(point(25., 25.), 0.), Some(1));
        assert_eq!(px(&doc, 5, 25), [255, 0, 0, 255]);
        doc.undo();
        assert_eq!(ids(&doc), [1, 2, 3]);
        assert_eq!(doc.render()?, before);
        Ok(())
    }

    #[test]
    fn set_privacy_blur_switches_masks_only_as_one_undo() -> Result<()> {
        let mut doc = big(60, 40)?;
        doc.add(Mark::redaction(1, point(5., 5.), point(20., 20.)));
        doc.add(mark(2));
        doc.add(Mark::redaction(3, point(30., 5.), point(50., 20.)));
        doc.add(mark(4));
        assert!(doc.group_marks(&[1, 2]));
        let before = doc.marks.clone();
        assert!(!doc.set_privacy_blur(&[1], false), "already black");
        assert!(!doc.set_privacy_blur(&[4], true), "a path is not a mask");
        assert!(!doc.set_privacy_blur(&[99], true));

        // Selecting the path reaches the mask through the group; the path itself is untouched.
        assert!(doc.set_privacy_blur(&[2], true));
        assert_eq!(doc.marks[0].kind, MarkKind::PrivacyBlur);
        assert_eq!(doc.marks[1], before[1]);
        assert_eq!(doc.marks[2], before[2]);
        assert!(!doc.set_privacy_blur(&[1], true));
        doc.undo();
        assert_eq!(doc.marks, before);
        doc.redo();
        assert!(doc.set_privacy_blur(&[1, 3], true));
        assert!(doc.marks[2].is_redaction() && doc.marks[2].kind == MarkKind::PrivacyBlur);
        doc.undo();
        assert_eq!(doc.marks[2], before[2], "one undo for the whole batch");
        doc.undo();
        assert_eq!(doc.marks, before);
        Ok(())
    }

    #[test]
    fn privacy_masks_are_independent_of_sensitive_pixels_even_through_a_lens() -> Result<()> {
        let base = tagged(100);
        let lens = |id, cx: f32, cy: f32, size: f32, a, b| {
            let source = Bounds {
                x: cx - size / 2.,
                y: cy - size / 2.,
                width: size,
                height: size,
            };
            Mark::magnifier(id, source, 2., a, b, [0., 1., 0., 1.], 2.)
        };
        for (rect, lo, hi) in [
            ((20., 20., 60., 60.), 20, 59),
            ((19.5, 19.5, 60.5, 60.5), 19, 60),
        ] {
            // Differs from `base` exactly where the mask touches pixels.
            let secret = RgbaImage::from_fn(100, 100, |x, y| {
                if (lo..=hi).contains(&x) && (lo..=hi).contains(&y) {
                    image::Rgba([255 - x as u8, 17, 99, 255])
                } else {
                    *base.get_pixel(x, y)
                }
            });
            for blur in [false, true] {
                let build = |img: &RgbaImage, mask: bool| -> Result<Document> {
                    let mut d = Document::new(img.clone())?;
                    if mask {
                        let (a, b) = (point(rect.0, rect.1), point(rect.2, rect.3));
                        d.add(if blur {
                            Mark::privacy_blur(1, a, b)
                        } else {
                            Mark::redaction(1, a, b)
                        });
                    }
                    // One lens entirely over the mask, one straddling its edge.
                    d.add(lens(2, 40., 40., 30., point(62., 2.), point(98., 38.)));
                    d.add(lens(3, 62., 62., 20., point(5., 70.), point(45., 98.)));
                    Ok(d)
                };
                assert_ne!(
                    build(&base, false)?.render()?,
                    build(&secret, false)?.render()?,
                    "without a mask the secret pixels do show"
                );
                let (a, b) = (
                    build(&base, true)?.render()?,
                    build(&secret, true)?.render()?,
                );
                assert_eq!(
                    a, b,
                    "blur={blur} rect={rect:?}: output depends on hidden pixels"
                );
                assert_eq!(
                    a.get_pixel(80, 20).0,
                    [0, 0, 0, 255],
                    "lens over the mask is black"
                );
                let p = a.get_pixel(40, 40).0;
                if blur {
                    assert!(
                        p[2] > p[0] && p != [0, 0, 0, 255] && p[3] == 255,
                        "grey-blue {p:?}"
                    );
                    let shades: HashSet<[u8; 4]> = (lo..=hi)
                        .flat_map(|y| (lo..=hi).map(move |x| (x, y)))
                        .map(|(x, y)| a.get_pixel(x, y).0)
                        .collect();
                    assert!(shades.len() >= 2, "the mosaic is not a flat fill");
                    assert_eq!(a.get_pixel(24, 24), a.get_pixel(31, 31), "crisp 8px blocks");
                } else {
                    assert_eq!(p, [0, 0, 0, 255]);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn blur_covers_exactly_the_pixels_black_does() -> Result<()> {
        let tf = Affine::rotation_about(0.6, point(50., 50.)).multiply(Affine::scale_about(
            1.3,
            0.9,
            point(50., 50.),
        ));
        let render = |blur: bool| -> Result<RgbaImage> {
            let mut d = big(100, 100)?;
            let (a, b) = (point(30.3, 40.7), point(70.1, 60.2));
            d.add(if blur {
                Mark::privacy_blur(1, a, b)
            } else {
                Mark::redaction(1, a, b)
            });
            assert!(d.transform_marks(&[1], tf));
            d.render()
        };
        let (black, blur) = (render(false)?, render(true)?);
        let bg = [20, 40, 60, 255];
        let mut covered = 0;
        for (x, y, p) in black.enumerate_pixels() {
            let masked = p.0 != bg;
            assert_eq!(masked, blur.get_pixel(x, y).0 != bg, "({x},{y})");
            covered += usize::from(masked);
        }
        assert!(covered > 500);
        Ok(())
    }

    #[test]
    fn unfilled_paths_pass_clicks_through_to_what_is_beneath() -> Result<()> {
        let mut doc = big(120, 80)?;
        doc.add(text_mark(1, 4.));
        let b = doc.mark_bounds(1).unwrap();
        let centre = point(b.x + b.width / 2., b.y + b.height / 2.);
        doc.add(shape(
            2,
            &[(0., 0.), (100., 0.), (100., 60.), (0., 60.)],
            true,
        ));
        assert_eq!(
            doc.hit_test(centre, 0.),
            Some(1),
            "a closed path has no filled interior"
        );
        assert_eq!(doc.hit_test(point(0., 30.), 0.), Some(2), "closing edge");
        assert_eq!(doc.hit_test(point(50., 0.), 0.), Some(2));

        // Privacy masks are solid and win over everything.
        doc.add(Mark::privacy_blur(3, point(0., 0.), point(120., 80.)));
        assert_eq!(doc.hit_test(centre, 0.), Some(3));
        assert_eq!(doc.hit_test(point(50., 0.), 0.), Some(3));
        Ok(())
    }

    #[test]
    fn privacy_blur_keeps_the_redaction_validation() {
        let image = || RgbaImage::new(20, 20);
        let good = Mark::privacy_blur(1, point(1., 1.), point(5., 5.));
        assert!(Document::from_parts(image(), vec![good.clone()], None).is_ok());
        let singular = Affine {
            a: 0.,
            d: 0.,
            ..Affine::identity()
        };
        for bad in [
            Mark {
                points: vec![point(1., 1.)],
                ..good.clone()
            },
            Mark {
                width: 0.,
                ..good.clone()
            },
            Mark {
                color: [2., 0., 0., 1.],
                ..good.clone()
            },
            Mark {
                transform: singular,
                ..good.clone()
            },
        ] {
            assert!(Document::from_parts(image(), vec![bad], None).is_err());
        }
    }

    #[test]
    fn magnifier_zoom_keeps_lens_and_source_centre() -> Result<()> {
        let mut doc = big(100, 100)?;
        doc.add(Mark::magnifier(
            1,
            Bounds {
                x: 20.,
                y: 20.,
                width: 20.,
                height: 10.,
            },
            2.,
            point(50., 50.),
            point(90., 70.),
            [0., 1., 0., 1.],
            2.,
        ));
        let before = doc.marks.clone();
        assert!(doc.set_magnifier_zoom(1, 4.));
        let m = doc.mark(1).unwrap();
        let MarkKind::Magnifier { source, zoom } = &m.kind else {
            panic!("lens");
        };
        assert_eq!(*zoom, 4.);
        assert!((source.width - 10.).abs() < 1e-4 && (source.height - 5.).abs() < 1e-4);
        assert!((source.x + source.width / 2. - 30.).abs() < 1e-4);
        assert_eq!(m.points, before[0].points);
        assert!(!doc.set_magnifier_zoom(1, 0.5) && !doc.set_magnifier_zoom(1, f32::NAN));
        doc.undo();
        assert_eq!(doc.marks, before);
        Ok(())
    }

    #[test]
    fn steps_renumber_on_delete_reorder_and_duplicate() -> Result<()> {
        let mut doc = big(100, 60)?;
        for i in 0..3 {
            let (id, n) = (doc.id(), doc.next_step_number());
            doc.add(Mark::step(
                id,
                point(20. + 25. * i as f32, 30.),
                n,
                [0., 0.6, 0., 1.],
                2.,
            ));
        }
        let number = |d: &Document, id| match d.mark(id).unwrap().kind {
            MarkKind::Step { number } => number,
            _ => unreachable!(),
        };
        assert_eq!([1, 2, 3].map(|id| number(&doc, id)), [1, 2, 3]);
        assert_eq!(doc.next_step_number(), 4);
        let b = doc.mark(1).unwrap().bounds().unwrap();
        assert!(b.width >= 22. && b.height >= 22., "readable radius: {b:?}");
        let image = doc.render()?;
        assert_eq!(
            image.get_pixel(12, 30).0,
            [0, 153, 0, 255],
            "disc fill beside the digit"
        );

        let all = doc.marks.clone();
        assert!(doc.remove_many(&[2]));
        assert_eq!((number(&doc, 1), number(&doc, 3)), (1, 2));
        doc.undo();
        assert_eq!(doc.marks, all, "numbers restored exactly");

        assert!(doc.reorder_step(3, -1));
        assert_eq!([1, 2, 3].map(|id| number(&doc, id)), [1, 3, 2]);
        assert!(!doc.reorder_step(1, -1) && !doc.reorder_step(2, 1) && !doc.reorder_step(3, 0));
        assert!(!doc.reorder_step(9, 1));
        assert!(doc.reorder_step(3, -5), "clamped to the first position");
        assert_eq!([1, 2, 3].map(|id| number(&doc, id)), [2, 3, 1]);
        doc.undo();
        doc.undo();
        assert_eq!(doc.marks, all);

        let copy = doc.duplicate_marks(&[1, 3]);
        assert_eq!((number(&doc, copy[0]), number(&doc, copy[1])), (4, 5));
        Ok(())
    }

    #[test]
    fn text_wraps_in_its_box_and_background_is_opaque() -> Result<()> {
        let mut doc = big(200, 120)?;
        let long = "alpha beta gamma delta epsilon";
        doc.add(Mark::text(1, point(10., 10.), long, RED, 3.));
        let free = doc.mark_bounds(1).unwrap();
        let marks = doc.marks.clone();
        assert!(doc.set_text(1, long, 60., false, 18.));
        let wrapped = doc.mark_bounds(1).unwrap();
        assert!(
            wrapped.width <= 61. && wrapped.height > free.height * 1.5,
            "{free:?} {wrapped:?}"
        );
        assert!(doc.set_text(1, long, 60., true, 18.));
        let boxed = doc.mark_bounds(1).unwrap();
        assert!((boxed.width - wrapped.width - 8.).abs() < 0.01);
        let image = doc.render()?;
        let px = image.get_pixel(boxed.x as u32 + 1, boxed.y as u32 + 1).0;
        assert!(
            px[3] == 255 && px[0] > 240 && px[1] > 240 && px[2] > 240,
            "{px:?}"
        );
        doc.undo();
        doc.undo();
        assert_eq!(doc.marks, marks);

        for (value, w) in [
            ("", 60.),
            ("  ", 60.),
            (long, f32::NAN),
            (long, 5.),
            (long, -1.),
            (long, f32::INFINITY),
        ] {
            assert!(!doc.set_text(1, value, w, false, 18.), "{value:?} {w}");
        }
        assert!(
            !doc.set_text(1, long, 0., false, 18.),
            "no change is no edit"
        );
        doc.add(mark(2));
        assert!(!doc.set_text(2, "x", 0., false, 18.) && !doc.set_text(9, "x", 0., false, 18.));
        Ok(())
    }

    #[test]
    fn from_parts_validates_untrusted_input() -> Result<()> {
        let img = || RgbaImage::from_fn(20, 10, |x, y| image::Rgba([x as u8, y as u8, 7, 255]));
        let rect = shape(1, &[(2., 2.), (8., 2.), (8., 8.)], true);
        let conn = |id, anchor| {
            Mark::connector(
                id,
                Endpoint {
                    point: point(1., 1.),
                    anchor,
                },
                Endpoint {
                    point: point(5., 5.),
                    anchor: None,
                },
                RED,
                2.,
            )
        };
        let anchor = |mark, u| Some(Anchor { mark, u, v: 0.5 });

        let good = vec![
            rect.clone(),
            conn(2, anchor(1, 0.5)),
            Mark::step(3, point(5., 5.), 1, RED, 2.),
            Mark::redaction(4, point(0., 0.), point(3., 3.)),
        ];
        let crop = Some(Crop {
            x: 2,
            y: 1,
            width: 10,
            height: 8,
        });
        let mut doc = Document::from_parts(img(), good.clone(), crop)?;
        assert_eq!(doc.marks, good);
        assert_eq!(doc.crop, crop);
        assert!(!doc.can_undo());
        assert_eq!(doc.id(), 5, "fresh ids never collide");
        assert_eq!(doc.background_image()?, img(), "original pixels come back");

        let with = |m: Mark| vec![rect.clone(), m];
        let mut grouped = good.clone();
        grouped[0].group = Some(7);
        let mut cases: Vec<(&str, Vec<Mark>)> = vec![
            (
                "duplicate id",
                vec![rect.clone(), shape(1, &[(0., 0.)], false)],
            ),
            ("zero id", vec![shape(0, &[(0., 0.)], false)]),
            ("NaN point", vec![shape(1, &[(f32::NAN, 0.)], false)]),
            ("empty path", vec![shape(1, &[], false)]),
            ("lone group member", grouped),
            ("dangling anchor", with(conn(2, anchor(9, 0.5)))),
            ("self anchor", vec![conn(2, anchor(2, 0.5))]),
            ("anchor outside 0..1", with(conn(2, anchor(1, 1.5)))),
            (
                "anchor to connector",
                vec![rect.clone(), conn(2, None), conn(3, anchor(2, 0.5))],
            ),
            (
                "anchor to redaction",
                with_redaction(conn(3, anchor(2, 0.5))),
            ),
            (
                "text with two points",
                with(Mark {
                    points: vec![point(0., 0.), point(1., 1.)],
                    ..text_mark(2, 3.)
                }),
            ),
            (
                "empty text",
                with(Mark::text(2, point(1., 1.), " ", RED, 3.)),
            ),
            (
                "huge text",
                with(Mark::text(
                    2,
                    point(1., 1.),
                    "x".repeat(MAX_TEXT + 1),
                    RED,
                    3.,
                )),
            ),
            ("step zero", with(Mark::step(2, point(1., 1.), 0, RED, 2.))),
            (
                "bad colour",
                with(Mark {
                    color: [2., 0., 0., 1.],
                    ..mark(2)
                }),
            ),
            (
                "bad width",
                with(Mark {
                    width: 0.,
                    ..mark(2)
                }),
            ),
            ("far coordinate", with(shape(2, &[(1e9, 0.)], false))),
            (
                "redaction one point",
                with(Mark {
                    points: vec![point(0., 0.)],
                    ..Mark::redaction(2, point(0., 0.), point(1., 1.))
                }),
            ),
            (
                "zero-size source",
                with(lens(
                    Bounds {
                        x: 0.,
                        y: 0.,
                        width: 0.,
                        height: 5.,
                    },
                    2.,
                )),
            ),
            (
                "zoom zero",
                with(lens(
                    Bounds {
                        x: 0.,
                        y: 0.,
                        width: 5.,
                        height: 5.,
                    },
                    0.,
                )),
            ),
            (
                "zoom NaN",
                with(lens(
                    Bounds {
                        x: 0.,
                        y: 0.,
                        width: 5.,
                        height: 5.,
                    },
                    f32::NAN,
                )),
            ),
            (
                "singular transform",
                with(Mark {
                    transform: Affine {
                        a: 0.,
                        b: 0.,
                        ..Affine::identity()
                    },
                    ..mark(2)
                }),
            ),
            (
                "NaN transform",
                with(Mark {
                    transform: Affine {
                        tx: f32::NAN,
                        ..Affine::identity()
                    },
                    ..mark(2)
                }),
            ),
            (
                "too many marks",
                (1..=MAX_MARKS as u64 + 1).map(mark).collect(),
            ),
            (
                "too many points",
                vec![Mark::path(
                    1,
                    vec![point(0., 0.); MAX_POINTS + 1],
                    false,
                    RED,
                    2.,
                )],
            ),
        ];
        cases.push((
            "box width",
            with(Mark {
                kind: MarkKind::Text {
                    value: "a".into(),
                    box_width: 2.,
                    background: false,
                },
                ..text_mark(2, 3.)
            }),
        ));
        for (name, marks) in cases {
            assert!(
                Document::from_parts(img(), marks, None).is_err(),
                "{name} accepted"
            );
        }
        for crop in [
            Crop {
                x: 0,
                y: 0,
                width: 0,
                height: 5,
            },
            Crop {
                x: 15,
                y: 0,
                width: 10,
                height: 5,
            },
            Crop {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 5,
            },
            Crop {
                x: 0,
                y: 6,
                width: 5,
                height: 5,
            },
        ] {
            assert!(
                Document::from_parts(img(), vec![], Some(crop)).is_err(),
                "{crop:?}"
            );
        }
        assert!(Document::from_parts(RgbaImage::new(MAX_DIM + 1, 1), vec![], None).is_err());
        assert!(Document::from_parts(RgbaImage::new(0, 4), vec![], None).is_err());
        Ok(())
    }

    fn lens(source: Bounds, zoom: f32) -> Mark {
        Mark::magnifier(2, source, zoom, point(0., 0.), point(5., 5.), RED, 2.)
    }

    fn with_redaction(connector: Mark) -> Vec<Mark> {
        vec![
            shape(1, &[(2., 2.), (8., 2.)], false),
            Mark::redaction(2, point(0., 0.), point(4., 4.)),
            Mark { id: 3, ..connector },
        ]
    }

    fn px(doc: &Document, x: u32, y: u32) -> [u8; 4] {
        doc.render().unwrap().get_pixel(x, y).0
    }

    const BG: [u8; 4] = [20, 40, 60, 255];

    #[test]
    fn arrowhead_is_independent_of_width_and_zero_means_no_head() -> Result<()> {
        let free = |x| Endpoint {
            point: point(x, 20.),
            anchor: None,
        };
        let mut doc = big(80, 40)?;
        doc.add(Mark::connector(
            1,
            free(10.),
            free(60.),
            [0., 1., 0., 1.],
            2.,
        ));
        let (_, barbs) = arrow_head(point(10., 20.), point(60., 20.), head_px(2.0)).unwrap();
        assert!(
            (barbs[0].y - barbs[1].y).abs() > 19.,
            "head barbs span 0.8 x 24px"
        );
        assert!(doc.set_head_size(&[1], 2.0));
        assert_eq!(
            px(&doc, 45, 15),
            [0, 255, 0, 255],
            "a 24px head covers (45,15)"
        );
        assert!(doc.set_style_many(&[1], [0., 1., 0., 1.], 40.));
        assert_eq!(
            doc.mark(1).unwrap().head_size,
            2.0,
            "width never changes the head"
        );
        assert!(doc.set_style_many(&[1], [0., 1., 0., 1.], 2.));
        assert!(doc.set_head_size(&[1], 0.0));
        assert_eq!(px(&doc, 45, 15), BG, "no head at 0");
        assert_eq!(
            px(&doc, 45, 20),
            [0, 255, 0, 255],
            "the shaft still reaches the tip"
        );
        assert_eq!(px(&doc, 59, 20), [0, 255, 0, 255]);
        doc.undo();
        doc.undo();
        doc.undo();
        doc.undo();
        assert_eq!(
            doc.mark(1).unwrap().head_size,
            1.0,
            "each change was one undo"
        );
        assert!(!doc.set_head_size(&[1], 17.0) && !doc.set_head_size(&[1], f64::NAN));
        assert!(!doc.set_head_size(&[1], 1.0), "unchanged");
        Ok(())
    }

    #[test]
    fn short_arrow_with_the_largest_head_stays_between_its_endpoints() -> Result<()> {
        let (a, b) = (point(10., 10.), point(13., 10.));
        let (base, barbs) = arrow_head(a, b, head_px(MAX_HEAD_SIZE)).unwrap();
        for p in [base, barbs[0], barbs[1]] {
            assert!(p.x > a.x && p.x < b.x, "{p:?}");
        }
        let mut doc = big(40, 20)?;
        let end = |x| Endpoint {
            point: point(x, 10.),
            anchor: None,
        };
        let mut m = Mark::connector(1, end(10.), end(13.), [0., 1., 0., 1.], 2.);
        m.head_size = MAX_HEAD_SIZE;
        doc.add(m);
        assert_eq!(px(&doc, 20, 10), BG, "nothing past the round cap");
        assert_eq!(px(&doc, 5, 10), BG, "nothing behind the tail");
        Ok(())
    }

    #[test]
    fn text_scale_follows_transform_and_font_size_is_independent_of_width() -> Result<()> {
        let mut doc = big(200, 120)?;
        let mut t = text_mark(1, 3.);
        t.font_size = 20.;
        doc.add(t);
        let b0 = doc.mark_bounds(1).unwrap();
        assert!(doc.set_style_many(&[1], RED, 40.));
        assert_eq!(
            doc.mark_bounds(1).unwrap(),
            b0,
            "line width never resizes text"
        );
        assert_eq!(doc.text_size(1), Some(20.));
        let c = point(10., 10.);
        assert!(doc.transform_marks(&[1], Affine::scale_about(2., 2., c)));
        assert_eq!(doc.text_size(1), Some(40.));
        let b = doc.mark_bounds(1).unwrap();
        assert!((b.width / b0.width - 2.).abs() < 1e-3 && (b.height / b0.height - 2.).abs() < 1e-3);
        // 60 px shown = 30 local under a 2x transform, one undo.
        assert!(doc.set_font_size(&[1], 60.));
        assert_eq!(doc.mark(1).unwrap().font_size, 30.);
        assert_eq!(doc.text_size(1), Some(60.));
        assert!(!doc.set_font_size(&[1], 60.) && !doc.set_font_size(&[1], f64::NAN));
        assert!(
            doc.set_text(1, "Hello", 0., false, 80.),
            "text and size in one edit"
        );
        assert_eq!(doc.mark(1).unwrap().font_size, 40.);
        assert!(
            !doc.set_text(1, "Hello", 0., false, 80.),
            "unchanged is no edit"
        );
        doc.undo();
        assert_eq!(doc.mark(1).unwrap().font_size, 30.);
        doc.undo();
        assert_eq!(doc.mark(1).unwrap().font_size, 20.);
        assert!(!doc.set_font_size(&[1], 5000.), "too large for the model");
        Ok(())
    }

    #[test]
    fn anisotropic_and_sheared_transforms_never_distort_text() -> Result<()> {
        let mut doc = big(300, 200)?;
        doc.add(text_mark(1, 4.));
        let b0 = doc.mark_bounds(1).unwrap();
        let centre = point(b0.x + b0.width / 2., b0.y + b0.height / 2.);
        assert!(doc.transform_marks(&[1], Affine::scale_about(3., 1., centre)));
        let b = doc.mark_bounds(1).unwrap();
        let k = 3f32.sqrt();
        assert!((b.width / b0.width - k).abs() < 1e-3, "{b:?} {b0:?}");
        assert!((b.height / b0.height - k).abs() < 1e-3, "no stretch");
        assert!(doc.mark(1).unwrap().hit_test(centre, 0.));
        doc.undo();
        let shear = Affine {
            a: 1.,
            b: 0.,
            c: 0.8,
            d: 1.,
            tx: 0.,
            ty: 0.,
        };
        let m = doc.mark(1).unwrap();
        let d = m.drawn_transform(shear);
        assert!(
            (d.a - d.d).abs() < 1e-5 && (d.b + d.c).abs() < 1e-5,
            "{d:?}"
        );
        assert!(
            (d.det() - shear.det()).abs() < 1e-4,
            "area-preserving scale"
        );
        let mirror = Affine::scale_about(-1., 1., centre);
        assert!(m.drawn_transform(mirror).det() > 0., "never mirrored");
        assert_eq!(
            m.drawn_transform(Affine::rotation_about(0.3, centre)),
            Affine::rotation_about(0.3, centre),
            "conformal transforms pass through"
        );
        // The drawn centre follows the transform of the centre.
        let moved = m.drawn_transform(shear).apply(centre);
        let want = shear.apply(centre);
        assert!(near(moved, want), "{moved:?} {want:?}");
        Ok(())
    }

    #[test]
    fn group_with_text_previews_and_commits_the_same_undistorted_text() -> Result<()> {
        let mut doc = big(200, 120)?;
        doc.add(text_mark(1, 4.));
        doc.add(shape(2, &[(100., 30.), (160., 90.)], false));
        assert!(doc.group_marks(&[1, 2]));
        let m = Affine::scale_about(1.7, 0.6, point(20., 20.));
        let preview = render_preview(&doc, &[1], m)?;
        let before = doc.render()?;
        assert_ne!(preview, before);
        let b0 = doc.mark_bounds(1).unwrap();
        assert!(doc.transform_marks(&[2], m), "the group moves together");
        assert_eq!(doc.render()?, preview, "preview == commit");
        let b = doc.mark_bounds(1).unwrap();
        assert!(
            ((b.width / b.height) - (b0.width / b0.height)).abs() < 1e-3,
            "aspect ratio kept"
        );
        let p = point(b.x + b.width / 2., b.y + b.height / 2.);
        assert_eq!(doc.hit_test(p, 0.), Some(1), "hits follow the drawn text");
        doc.undo();
        assert_eq!(doc.render()?, before);
        Ok(())
    }

    #[test]
    fn old_marks_without_head_or_font_size_migrate_explicitly() -> Result<()> {
        let strip = |m: &Mark| -> Mark {
            let mut v = serde_json::to_value(m).unwrap();
            let o = v.as_object_mut().unwrap();
            o.remove("head_size");
            o.remove("font_size");
            serde_json::from_value(v).unwrap()
        };
        let free = |x| Endpoint {
            point: point(x, 5.),
            anchor: None,
        };
        let mut old = vec![
            strip(&text_mark(1, 3.)),
            strip(&Mark::connector(2, free(1.), free(9.), RED, 8.)),
            strip(&mark(3)),
        ];
        assert_eq!((old[0].font_size, old[1].head_size), (0., 1.));
        assert!(
            Document::from_parts(RgbaImage::new(20, 20), old.clone(), None).is_err(),
            "unmigrated text has no size"
        );
        migrate_v1(&mut old);
        assert_eq!(old[0].font_size, 18., "text was drawn at 6 x width");
        assert!(
            (old[1].head_size - 32. / HEAD_BASE).abs() < 1e-9,
            "4 x width head"
        );
        assert_eq!(old[2].head_size, 1.);
        let doc = Document::from_parts(RgbaImage::new(20, 20), old, None)?;
        assert_eq!(doc.text_size(1), Some(18.));
        Ok(())
    }

    #[test]
    fn line_widths_up_to_128_are_valid_everywhere() -> Result<()> {
        let mut doc = big(300, 100)?;
        doc.add(mark(1));
        assert!(doc.set_style_many(&[1], RED, 128.));
        assert!(!doc.set_style_many(&[1], RED, 128.5));
        let mut wide = mark(2);
        wide.width = 128.;
        assert!(Document::from_parts(RgbaImage::new(20, 20), vec![wide], None).is_ok());
        Ok(())
    }

    #[test]
    fn pixelated_privacy_is_source_independent_crisp_mosaic() -> Result<()> {
        let base = tagged(100);
        let secret = RgbaImage::from_fn(100, 100, |x, y| {
            if (20..60).contains(&x) && (20..60).contains(&y) {
                image::Rgba([255 - x as u8, 3, 77, 255])
            } else {
                *base.get_pixel(x, y)
            }
        });
        let build = |img: &RgbaImage| -> Result<Document> {
            let mut d = Document::new(img.clone())?;
            d.add(Mark::privacy_blur(1, point(20., 20.), point(60., 60.)));
            // A spotlight over the mask and an annotation under it never reveal the source.
            d.add(Mark::spotlight(2, point(10., 10.), point(70., 70.)));
            d.add(shape(3, &[(30., 30.), (50., 50.)], false));
            Ok(d)
        };
        let (a, b) = (build(&base)?.render()?, build(&secret)?.render()?);
        assert_eq!(a, b);
        let blocks: HashSet<[u8; 4]> = (20..60)
            .flat_map(|y| (20..60).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                (x, y) != (30, 30) && !(28..52).contains(&x) && !(28..52).contains(&y)
            })
            .map(|(x, y)| a.get_pixel(x, y).0)
            .collect();
        assert!(blocks.len() >= 2);
        assert_eq!(
            a.get_pixel(24, 24),
            a.get_pixel(31, 31),
            "8px blocks are crisp"
        );
        Ok(())
    }

    #[test]
    fn spotlights_dim_outside_their_union_with_privacy_masks_on_top() -> Result<()> {
        let mut doc = big(100, 100)?;
        doc.add(Mark::spotlight(1, point(10., 10.), point(30., 30.)));
        doc.add(Mark::spotlight(2, point(20., 20.), point(40., 40.)));
        for p in [(25, 25), (15, 15), (35, 35)] {
            assert_eq!(px(&doc, p.0, p.1), BG, "inside the union: {p:?}");
        }
        let dimmed = px(&doc, 80, 80);
        assert!(
            (i32::from(dimmed[0]) - 9).abs() <= 1 && dimmed[3] == 255,
            "{dimmed:?}"
        );
        assert_eq!(
            px(&doc, 5, 5),
            dimmed,
            "one dim layer, not one per spotlight"
        );
        assert_eq!(px(&doc, 45, 45), dimmed);
        let white = (0..100).any(|x| px(&doc, x, 20) == [255, 255, 255, 255]);
        assert!(white, "a white outline is drawn");

        doc.add(Mark::redaction(3, point(22., 22.), point(28., 28.)));
        assert_eq!(
            px(&doc, 25, 25),
            [0, 0, 0, 255],
            "a mask covers the spotlight"
        );
        // A mask added first is still on top of a later spotlight.
        let mut other = big(100, 100)?;
        other.add(Mark::redaction(1, point(22., 22.), point(28., 28.)));
        other.add(Mark::spotlight(2, point(10., 10.), point(30., 30.)));
        assert_eq!(px(&other, 25, 25), [0, 0, 0, 255]);

        // Hits: filled, but ordinary marks beneath it keep winning.
        let mut hits = big(100, 100)?;
        hits.add(shape(1, &[(10., 50.), (50., 50.)], false));
        hits.add(Mark::spotlight(2, point(5., 40.), point(60., 60.)));
        assert_eq!(hits.hit_test(point(30., 50.), 1.), Some(1));
        assert_eq!(hits.hit_test(point(30., 55.), 0.), Some(2));
        let before = hits.render()?;
        assert!(hits.transform_marks(&[2], Affine::translation(point(20., 0.))));
        assert_ne!(hits.render()?, before);
        hits.undo();
        assert_eq!(hits.render()?, before);
        Ok(())
    }

    #[test]
    fn spotlight_is_validated() {
        let ok = Mark::spotlight(1, point(1., 1.), point(9., 9.));
        let img = || RgbaImage::new(20, 20);
        assert!(Document::from_parts(img(), vec![ok.clone()], None).is_ok());
        for kind in [
            MarkKind::Spotlight {
                radius: -1.,
                dim: 0.5,
            },
            MarkKind::Spotlight {
                radius: f64::NAN,
                dim: 0.5,
            },
            MarkKind::Spotlight {
                radius: 4.,
                dim: 1.5,
            },
        ] {
            let bad = Mark { kind, ..ok.clone() };
            assert!(Document::from_parts(img(), vec![bad], None).is_err());
        }
        let one_point = Mark {
            points: vec![point(1., 1.)],
            ..ok
        };
        assert!(Document::from_parts(img(), vec![one_point], None).is_err());
    }

    #[test]
    fn is_private_is_conservative_and_follows_transforms() -> Result<()> {
        let mut doc = big(200, 100)?;
        let r = |x: f32| Bounds {
            x,
            y: 20.,
            width: 10.,
            height: 10.,
        };
        assert!(!doc.privacy_snapshot().is_private(r(25.)));
        doc.add(Mark::redaction(1, point(20., 20.), point(40., 40.)));
        let privacy = doc.privacy_snapshot();
        assert!(
            privacy.is_private(r(25.)) && privacy.is_private(r(39.)) && privacy.is_private(r(12.))
        );
        assert!(!privacy.is_private(r(100.)));
        assert!(doc.move_marks(&[1], point(100., 0.)));
        let privacy = doc.privacy_snapshot();
        assert!(!privacy.is_private(r(25.)) && privacy.is_private(r(125.)));
        assert!(doc.transform_marks(&[1], Affine::rotation_about(0.7, point(130., 30.))));
        let privacy = doc.privacy_snapshot();
        assert!(privacy.is_private(r(125.)));
        assert!(privacy.is_private(Bounds {
            x: f32::NAN,
            y: 0.,
            width: 1.,
            height: 1.
        }));
        Ok(())
    }

    #[test]
    fn add_many_is_one_undo_and_all_or_nothing() -> Result<()> {
        let mut doc = big(60, 60)?;
        doc.add(mark(1));
        let before = doc.marks.clone();
        assert!(!doc.add_many(vec![]));
        assert!(!doc.add_many(vec![mark(2), mark(2)]), "duplicate ids");
        assert!(!doc.add_many(vec![mark(3), mark(1)]), "id already used");
        assert_eq!(doc.marks, before);
        assert!(doc.add_many(vec![mark(5), mark(6), mark(7)]));
        assert_eq!(ids(&doc), [1, 5, 6, 7]);
        assert!(doc.id() > 7);
        doc.undo();
        assert_eq!(doc.marks, before);
        doc.redo();
        assert_eq!(ids(&doc), [1, 5, 6, 7]);
        Ok(())
    }

    #[test]
    fn presentation_is_undoable_validated_and_renders_after_the_scene() -> Result<()> {
        let mut doc = big(40, 30)?;
        doc.add(mark(1));
        let plain = doc.render()?;
        assert!(doc.set_presentation(Presentation::default()).is_ok());
        doc.undo();
        assert!(
            doc.marks.is_empty(),
            "a default presentation recorded no step"
        );
        doc.redo();
        let framed = Presentation {
            padding: 12,
            ..Presentation::default()
        };
        doc.set_presentation(framed.clone())?;
        let out = doc.render()?;
        assert!(out.width() > plain.width() && out.height() > plain.height());
        assert_eq!((doc.width, doc.height), (40, 30), "the scene is untouched");
        doc.undo();
        assert_eq!(doc.render()?, plain);
        assert_eq!(doc.presentation, Presentation::default());
        doc.redo();
        assert_eq!(doc.presentation, framed);

        let invalid = Presentation {
            shadow: 7.,
            ..Presentation::default()
        };
        assert!(doc.set_presentation(invalid).is_err());
        assert_eq!(doc.presentation, framed, "rejected changes leave no trace");

        // Presentation and crop change together as one undo.
        let crop = Some(Crop {
            x: 2,
            y: 2,
            width: 20,
            height: 10,
        });
        doc.set_presentation_and_crop(Presentation::default(), crop)?;
        assert_eq!((doc.render()?.width(), doc.render()?.height()), (20, 10));
        doc.undo();
        assert_eq!((doc.crop, &doc.presentation), (None, &framed));
        let outside = Some(Crop {
            x: 30,
            y: 0,
            width: 20,
            height: 10,
        });
        assert!(doc.set_presentation_and_crop(framed, outside).is_err());
        Ok(())
    }

    #[test]
    fn highlight_is_a_translucent_fill_under_privacy_masks() -> Result<()> {
        let mut doc = big(60, 40)?;
        doc.add(Mark::highlight(
            1,
            point(10., 10.),
            point(40., 20.),
            [1., 1., 0., 0.5],
        ));
        let p = px(&doc, 20, 15);
        assert!(
            p[0] > 100 && p[1] > p[0] - 20 && p[2] < BG[2] && p[3] == 255,
            "{p:?}"
        );
        assert_eq!(px(&doc, 50, 30), BG);
        assert_eq!(
            doc.hit_test(point(20., 15.), 0.),
            Some(1),
            "filled hit target"
        );
        doc.add(Mark::redaction(2, point(15., 10.), point(25., 20.)));
        assert_eq!(px(&doc, 20, 15), [0, 0, 0, 255]);
        let bad = Mark {
            points: vec![point(1., 1.)],
            ..mark(3)
        };
        let bad = Mark {
            kind: MarkKind::Highlight,
            ..bad
        };
        assert!(Document::from_parts(RgbaImage::new(9, 9), vec![bad], None).is_err());
        Ok(())
    }

    // --- caches: text metrics, id slots, privacy snapshot, Cairo-first framing -----------------

    /// A measurement without the cache, as every call was made before it existed.
    fn measured(
        text: &str,
        family: &str,
        font_style: &str,
        size: f64,
        wrap: f64,
        style: TextStyle,
    ) -> Bounds {
        MEASURE.with(|cr| {
            logical(&text_layout(
                cr.as_ref().unwrap(),
                text,
                family,
                font_style,
                size,
                wrap,
                style,
            ))
        })
    }

    #[test]
    fn cached_text_extents_equal_a_fresh_measurement_for_every_input() {
        let bold = TextStyle {
            bold: true,
            ..TextStyle::default()
        };
        let styles = [
            TextStyle::default(),
            bold,
            TextStyle {
                italic: true,
                align: TextAlign::Right,
                ..TextStyle::default()
            },
            TextStyle {
                align: TextAlign::Center,
                ..bold
            },
        ];
        let texts = [
            "Hello",
            "Hello world, this wraps",
            "日本語 😀 العربية",
            "",
            "a\nb",
        ];
        for pass in 0..2 {
            for text in texts {
                for (size, wrap) in [(12., 0.), (24.5, 40.), (24.5, 160.)] {
                    for style in styles {
                        for family in ["Sans", "Serif", "Monospace, Sans"] {
                            for face in ["Regular", "Heavy", "Light Italic"] {
                                assert_eq!(
                                    text_extents(text, family, face, size, wrap, style),
                                    Some(measured(text, family, face, size, wrap, style)),
                                    "pass {pass}: {text:?} {family} {face} {size} {wrap} {style:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn text_bounds_follow_direct_edits_of_every_input() {
        let text = |value: &str, box_width: f32, background: bool| MarkKind::Text {
            value: value.into(),
            box_width,
            background,
        };
        let mut m = text_mark(1, 4.);
        let base = m.local_bounds().unwrap();
        assert_eq!(m.local_bounds(), Some(base), "a cache hit answers the same");

        m.kind = text("Hello, a much wider world", 0., false);
        let wide = m.local_bounds().unwrap();
        assert!(wide.width > base.width);
        m.kind = text("Hello, a much wider world", 60., false);
        let wrapped = m.local_bounds().unwrap();
        assert!(wrapped.height > wide.height && wrapped.width < wide.width);
        m.kind = text("Hello, a much wider world", 0., true);
        let padded = m.local_bounds().unwrap();
        assert_eq!(
            (padded.width, padded.height),
            (wide.width + 2. * TEXT_PAD, wide.height + 2. * TEXT_PAD)
        );
        m.kind = text("Hello, a much wider world", 0., false);
        m.font_size *= 2.;
        assert!(m.local_bounds().unwrap().width > wide.width * 1.5);
        m.font_size /= 2.;
        m.text_style.bold = true;
        let b = m.local_bounds().unwrap();
        let fresh = measured(
            "Hello, a much wider world",
            &m.font_family,
            &m.font_style,
            m.font_size,
            0.,
            m.text_style,
        );
        assert_eq!((b.width, b.height), (fresh.width, fresh.height));
        m.text_style.bold = false;
        m.points[0].x += 50.;
        assert!(
            (m.local_bounds().unwrap().x - (wide.x + 50.)).abs() < 1e-3,
            "origin is never cached"
        );
        m.points[0].x -= 50.;
        m.kind = text("Hello", 0., false);
        assert_eq!(
            m.local_bounds(),
            Some(base),
            "back to the first inputs, the first answer"
        );
    }

    #[test]
    fn metrics_cache_rejects_collisions_and_stays_bounded() {
        let style = TextStyle::default();
        let key =
            |text, size| MetricsKey::new(text, DEFAULT_FAMILY, DEFAULT_FONT_STYLE, size, 0., style);
        let wide = Bounds {
            x: 0.,
            y: 0.,
            width: 9.,
            height: 3.,
        };
        let narrow = Bounds { width: 1., ..wide };
        let mut cache = MetricsCache::default();
        cache.put(7, &key("a", 12.), wide);
        assert_eq!(cache.get(7, &key("a", 12.), 0), Some(wide));
        assert_eq!(
            cache.get(7, &key("b", 12.), 0),
            None,
            "same hash, other text"
        );
        assert_eq!(
            cache.get(7, &key("a", 13.), 0),
            None,
            "same hash, other size"
        );
        let wrapped = MetricsKey::new("a", DEFAULT_FAMILY, DEFAULT_FONT_STYLE, 12., 50., style);
        assert_eq!(cache.get(7, &wrapped, 0), None, "same hash, other wrap");
        let bold = TextStyle {
            bold: true,
            ..style
        };
        assert_eq!(
            cache.get(
                7,
                &MetricsKey::new("a", DEFAULT_FAMILY, DEFAULT_FONT_STYLE, 12., 0., bold),
                0
            ),
            None
        );
        cache.put(7, &key("b", 12.), narrow);
        assert_eq!(
            cache.get(7, &key("a", 12.), 0),
            None,
            "a collision replaces"
        );
        assert_eq!(cache.get(7, &key("b", 12.), 0), Some(narrow));
        assert_eq!(cache.text_bytes, 1);
        assert_eq!(
            cache.get(7, &key("b", 12.), 1),
            None,
            "a new font map drops all"
        );
        assert!(cache.entries.is_empty() && cache.text_bytes == 0);

        for i in 0..2 * MetricsCache::MAX_ENTRIES as u64 {
            cache.put(i, &key("a", 12.), wide);
            assert!(cache.entries.len() <= MetricsCache::MAX_ENTRIES);
        }
        let long = "x".repeat(1 << 20);
        for i in 0..8 {
            cache.put(i, &key(&long, 12.), wide);
            assert!(cache.text_bytes <= MetricsCache::MAX_TEXT_BYTES);
        }
        assert_eq!(cache.get(7, &key(&long, 12.), 1), Some(wide));
    }

    #[test]
    fn mark_lookup_survives_reorder_removal_and_direct_edits() -> Result<()> {
        let mut doc = background()?;
        for id in 1..=4 {
            doc.add(mark(id));
        }
        assert_eq!(doc.mark(3).map(|m| m.id), Some(3));
        doc.marks.swap(0, 3);
        assert!(std::ptr::eq(doc.mark(1).unwrap(), &doc.marks[3]));
        assert!(std::ptr::eq(doc.mark(4).unwrap(), &doc.marks[0]));
        let gone = doc.marks.remove(1);
        assert!(doc.mark(gone.id).is_none(), "a removed mark is not found");
        assert_eq!(doc.mark(3).map(|m| m.id), Some(3));
        doc.marks[0].id = 77;
        assert!(doc.mark(4).is_none() && doc.mark(77).is_some());

        let mut doc = background()?;
        for id in 1..=3 {
            doc.add(mark(id));
        }
        assert_eq!(
            doc.expand_groups(&[3, 99, 1, 3]),
            [1, 3],
            "z-order, unknown dropped"
        );
        assert!(doc.reorder_marks(&[1], true));
        assert_eq!(doc.expand_groups(&[1, 3]), [3, 1]);
        assert_eq!(doc.mark(1).map(|m| m.id), Some(1));
        doc.undo();
        assert_eq!(doc.expand_groups(&[3, 1]), [1, 3]);
        assert!(std::ptr::eq(doc.mark(3).unwrap(), &doc.marks[2]));
        Ok(())
    }

    #[test]
    fn selection_geometry_tracks_edits_groups_and_attachments_through_undo() -> Result<()> {
        let mut doc = big(400, 200)?;
        doc.add(text_mark(1, 4.));
        let b0 = doc.selection_bounds(&[1]).unwrap();
        let edge = point(b0.x + b0.width, b0.y + b0.height / 2.);
        let start = doc.endpoint(edge, 3.);
        assert_eq!(start.anchor.map(|a| a.mark), Some(1));
        let end = Endpoint {
            point: point(300., 150.),
            anchor: None,
        };
        doc.add(Mark::connector(2, start, end, RED, 2.));
        doc.add(mark(3));
        let tail_x = |d: &Document| d.connector_ends(2).unwrap()[0].x;
        let x0 = tail_x(&doc);
        assert!((x0 - edge.x).abs() < 1e-3);

        // A bigger font widens the text; the attached end, the connector's box and the
        // selection all follow, and undo/redo bring each back exactly.
        let grow = MarkEdit {
            font_px: Some(48.),
            ..MarkEdit::default()
        };
        assert!(doc.edit_marks(&[1], &grow));
        let (x1, b1) = (tail_x(&doc), doc.selection_bounds(&[1]).unwrap());
        assert!(x1 > x0 + 10. && b1.width > b0.width);
        assert_eq!(
            doc.selection_bounds(&[2]).unwrap().x,
            doc.mark_bounds(2).unwrap().x
        );
        doc.undo();
        assert_eq!((tail_x(&doc), doc.selection_bounds(&[1])), (x0, Some(b0)));
        doc.redo();
        assert_eq!((tail_x(&doc), doc.selection_bounds(&[1])), (x1, Some(b1)));
        assert!(doc.move_marks(&[1], point(30., 0.)));
        assert!((tail_x(&doc) - (x1 + 30.)).abs() < 1e-3);

        // Group membership read straight from the marks, also when edited behind the API.
        assert!(doc.group_marks(&[1, 3]));
        assert_eq!(doc.expand_groups(&[3]), [1, 3]);
        assert_eq!(
            doc.selection_bounds(&[3]),
            Some(
                doc.mark_bounds(1)
                    .unwrap()
                    .union(doc.mark_bounds(3).unwrap())
            )
        );
        doc.marks[2].group = None;
        assert_eq!(doc.expand_groups(&[3]), [3]);
        assert_eq!(doc.expand_groups(&[1]), [1]);
        Ok(())
    }

    #[test]
    fn privacy_snapshot_is_stable_until_a_mask_changes_and_old_ones_stay_true() -> Result<()> {
        let area = |x, y, width, height| Bounds {
            x,
            y,
            width,
            height,
        };
        let mut doc = big(200, 100)?;
        doc.add(mark(1));
        let empty = doc.privacy_snapshot();
        assert!(Rc::ptr_eq(&empty, &doc.privacy_snapshot()));
        assert!(!empty.is_private(area(0., 0., 200., 100.)));
        assert!(
            empty.is_private(area(f32::NAN, 0., 1., 1.)),
            "non-finite blocks"
        );

        doc.add(Mark::redaction(2, point(10., 10.), point(30., 30.)));
        let one = doc.privacy_snapshot();
        assert!(!Rc::ptr_eq(&empty, &one));
        doc.add(mark(3));
        assert!(
            Rc::ptr_eq(&one, &doc.privacy_snapshot()),
            "annotations leave it alone"
        );

        // Disjoint masks: the gap between them stays clear, as a union hull would not allow.
        doc.add(Mark::privacy_blur(4, point(150., 60.), point(180., 90.)));
        let two = doc.privacy_snapshot();
        let gap = area(80., 40., 20., 10.);
        assert!(!two.is_private(gap) && !doc.privacy_snapshot().is_private(gap));
        assert!(two.is_private(area(25., 25., 2., 2.)));
        assert!(two.is_private(area(160., 70., 2., 2.)));
        assert!(two.is_private(area(30.5, 15., 1., 1.)), "a pixel of slack");
        assert!(!two.is_private(area(32., 15., 1., 1.)), "and no more");

        assert!(doc.move_marks(&[2], point(70., 30.)));
        let moved = doc.privacy_snapshot();
        assert!(!Rc::ptr_eq(&two, &moved));
        assert!(moved.is_private(gap) && doc.privacy_snapshot().is_private(gap));
        assert!(!two.is_private(gap), "an old snapshot never changes");
        doc.undo();
        assert!(!doc.privacy_snapshot().is_private(gap));

        // Edits straight through the public marks are seen, and unusable geometry blocks all.
        doc.marks[1].points.iter_mut().for_each(|p| {
            p.x += 70.;
            p.y += 30.;
        });
        let edited = doc.privacy_snapshot();
        assert!(!Rc::ptr_eq(&two, &edited) && edited.is_private(gap));
        let far = area(1000., 1000., 1., 1.);
        assert!(!edited.is_private(far));
        doc.marks[1].points[0].x = f32::INFINITY;
        assert!(doc.privacy_snapshot().is_private(far));
        doc.marks[1].points.clear();
        assert!(doc.privacy_snapshot().is_private(far));
        Ok(())
    }

    #[test]
    fn render_frames_the_cairo_scene_exactly_like_the_rgba_round_trip_did() -> Result<()> {
        use crate::presentation::Background;
        // See-through screenshot: any drift in a premultiplied round trip would show here.
        let image = RgbaImage::from_fn(40, 30, |x, y| {
            image::Rgba([
                (x * 6) as u8,
                (y * 8) as u8,
                100,
                ((x + y) * 4).min(255) as u8,
            ])
        });
        let mut doc = Document::new(image)?;
        doc.add(text_mark(1, 3.));
        doc.add(Mark::highlight(
            2,
            point(2., 4.),
            point(20., 14.),
            [1., 1., 0., 0.4],
        ));
        doc.add(Mark::redaction(3, point(25., 5.), point(35., 12.)));
        doc.set_crop(Some(Crop {
            x: 3,
            y: 2,
            width: 30,
            height: 22,
        }));
        let before_frame = |doc: &Document| {
            let pixels = rgba_from(&doc.scene_surface()?)?;
            rgba_from(&doc.presentation.apply_surface(surface_from(&pixels)?)?)
        };
        let looks = [
            Presentation::default(),
            Presentation {
                padding: 4,
                border: 2,
                radius: 5.,
                shadow: 0.5,
                background: Background::Gradient([1., 0., 0., 1.], [0., 0., 1., 0.5]),
                ..Presentation::default()
            },
            Presentation {
                canvas_size: Some([100, 40]),
                output_size: Some([77, 31]),
                background: Background::Solid([1., 1., 1., 0.5]),
                ..Presentation::default()
            },
        ];
        for look in looks {
            doc.set_presentation(look)?;
            assert_eq!(doc.render()?, before_frame(&doc)?);
        }
        doc.set_presentation(Presentation {
            padding: 4,
            ..Presentation::default()
        })?;
        let out = doc.render()?;
        assert_eq!(out.dimensions(), (38, 30));
        assert_eq!(
            *out.get_pixel(0, 0),
            image::Rgba([0; 4]),
            "the frame is transparent"
        );
        let scene = rgba_from(&doc.scene_surface()?)?;
        assert_eq!(scene.dimensions(), (30, 22), "the crop, before framing");
        Ok(())
    }

    // --- inspector model: revision, text style, mixed editing, layout --------------------------

    fn boxy(id: u64, x: f32, y: f32, w: f32, h: f32) -> Mark {
        Mark::highlight(id, point(x, y), point(x + w, y + h), RED)
    }

    fn bounds_of(doc: &Document, id: u64) -> Bounds {
        doc.mark_bounds(id).unwrap()
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn paint_text_to_image(
        doc: &Document,
        original: Option<u64>,
        preview: &Mark,
    ) -> Result<RgbaImage> {
        let surface = ImageSurface::create(Format::ARgb32, doc.width as i32, doc.height as i32)?;
        {
            let cr = Cairo::new(&surface)?;
            doc.paint_text_preview(&cr, original, preview)?;
        }
        rgba_from(&surface)
    }

    #[test]
    fn revision_is_monotonic_across_edits_undo_redo_and_snap_but_not_reads_or_noops() -> Result<()>
    {
        let mut doc = big(80, 60)?;
        assert_eq!(doc.revision(), 0);
        let mut last = doc.revision();
        let mut bumped = |doc: &Document| {
            assert!(doc.revision() > last, "revision must grow");
            last = doc.revision();
        };
        doc.add(mark(1));
        bumped(&doc);
        assert!(doc.move_marks(&[1], point(3., 0.)));
        bumped(&doc);
        doc.set_crop(Some(Crop {
            x: 1,
            y: 1,
            width: 20,
            height: 20,
        }));
        bumped(&doc);
        assert!(doc.snap(1, vec![point(1., 1.), point(9., 9.)], false));
        bumped(&doc);
        doc.undo();
        bumped(&doc);
        doc.redo();
        bumped(&doc);
        let now = doc.revision();
        let _ = (doc.render()?, doc.mark_bounds(1), doc.can_undo());
        assert!(!doc.move_marks(&[1], point(f32::NAN, 0.)));
        assert!(!doc.set_style_many(&[1], [2., 0., 0., 1.], 2.));
        assert!(!doc.snap(99, vec![point(0., 0.)], false));
        doc.set_crop(doc.crop);
        assert_eq!(
            doc.revision(),
            now,
            "reads and rejected or empty edits change nothing"
        );
        Ok(())
    }

    #[test]
    fn text_style_changes_rendering_alignment_and_survives_edit_and_undo() -> Result<()> {
        let ink = |doc: &Document| -> Vec<u32> {
            let img = doc.render().unwrap();
            img.enumerate_pixels()
                .filter(|(_, _, p)| p.0[0] > 150 && p.0[1] < 90)
                .map(|(x, _, _)| x)
                .collect()
        };
        let mut doc = big(200, 60)?;
        doc.add(Mark::text(1, point(10., 10.), "Hi", RED, 4.));
        assert!(doc.set_text(1, "Hi", 120., false, 20.));
        let plain_ink = ink(&doc);
        assert!(!plain_ink.is_empty());
        assert!(
            plain_ink.iter().all(|&x| x < 60),
            "left aligned: {plain_ink:?}"
        );
        let plain = doc.render()?;

        let right = TextStyle {
            align: TextAlign::Right,
            ..TextStyle::default()
        };
        assert!(doc.set_text_styled(
            1,
            "Hi",
            120.,
            false,
            20.,
            right,
            "Sans",
            "Regular",
            TextEffect::None,
            None
        ));
        assert_eq!(doc.mark(1).unwrap().text_style, right);
        let right_ink = ink(&doc);
        assert!(
            right_ink.iter().all(|&x| x > 90),
            "right aligned: {right_ink:?}"
        );
        let b = doc.mark(1).unwrap().bounds().unwrap();
        assert!(b.x + b.width <= 131. && b.x + b.width >= 100., "{b:?}");

        let centre = TextStyle {
            align: TextAlign::Center,
            ..TextStyle::default()
        };
        assert!(doc.set_text_styled(
            1,
            "Hi",
            120.,
            false,
            20.,
            centre,
            "Sans",
            "Regular",
            TextEffect::None,
            None
        ));
        assert!(ink(&doc).iter().all(|&x| (40..100).contains(&x)));

        for style in [
            TextStyle {
                bold: true,
                ..TextStyle::default()
            },
            TextStyle {
                italic: true,
                ..TextStyle::default()
            },
        ] {
            assert!(doc.set_text_styled(
                1,
                "Hi",
                120.,
                false,
                20.,
                style,
                "Sans",
                "Regular",
                TextEffect::None,
                None
            ));
            assert_ne!(doc.render()?, plain, "{style:?} must change the glyphs");
        }
        // Plain `set_text` keeps whatever style the mark has; a no-op restyle is no undo step.
        let italic = doc.mark(1).unwrap().text_style;
        assert!(doc.set_text(1, "Hi!", 120., false, 20.));
        assert_eq!(doc.mark(1).unwrap().text_style, italic);
        assert!(!doc.set_text_styled(
            1,
            "Hi!",
            120.,
            false,
            20.,
            italic,
            "Sans",
            "Regular",
            TextEffect::None,
            None
        ));
        doc.undo();
        assert!(
            matches!(&doc.mark(1).unwrap().kind, MarkKind::Text { value, .. } if value == "Hi")
        );
        assert_eq!(
            doc.mark(1).unwrap().text_style,
            italic,
            "one undo restored the text only"
        );
        Ok(())
    }

    #[test]
    fn edits_keep_each_marks_other_fields_so_mixed_values_survive() -> Result<()> {
        let mut doc = big(200, 100)?;
        let mut a = shape(1, &[(10., 10.), (60., 10.)], false);
        a.color = [1., 0., 0., 1.];
        let mut b = shape(2, &[(10., 30.), (60., 30.)], false);
        b.color = [0., 0., 1., 0.5];
        b.width = 6.;
        doc.add(a.clone());
        doc.add(b.clone());
        doc.add(Mark::redaction(3, point(100., 10.), point(120., 30.)));
        let all = [1, 2, 3];
        assert!(doc.edit_marks(
            &all,
            &MarkEdit {
                width: Some(10.),
                ..MarkEdit::default()
            }
        ));
        assert_eq!(
            (doc.mark(1).unwrap().color, doc.mark(2).unwrap().color),
            (a.color, b.color),
            "width alone never touches colours"
        );
        assert_eq!(
            doc.mark(3).unwrap().width,
            1.0,
            "privacy masks keep their fixed look"
        );
        assert!(doc.edit_marks(
            &all,
            &MarkEdit {
                rgb: Some([0., 1., 0.]),
                ..MarkEdit::default()
            }
        ));
        assert_eq!(doc.mark(1).unwrap().color, [0., 1., 0., 1.]);
        assert_eq!(
            doc.mark(2).unwrap().color,
            [0., 1., 0., 0.5],
            "colour alone keeps opacity"
        );
        assert_eq!(
            doc.mark(3).unwrap().color,
            [0., 0., 0., 1.],
            "masks stay opaque black"
        );
        assert!(doc.edit_marks(
            &all,
            &MarkEdit {
                alpha: Some(0.25),
                ..MarkEdit::default()
            }
        ));
        assert_eq!(doc.mark(2).unwrap().color, [0., 1., 0., 0.25]);
        assert_eq!(doc.mark(3).unwrap().color, [0., 0., 0., 1.]);
        doc.undo();
        assert_eq!(
            doc.mark(2).unwrap().color,
            [0., 1., 0., 0.5],
            "one undo per edit"
        );

        let before = doc.revision();
        for bad in [
            MarkEdit {
                alpha: Some(1.5),
                ..MarkEdit::default()
            },
            MarkEdit {
                width: Some(0.),
                ..MarkEdit::default()
            },
            MarkEdit {
                rgb: Some([f64::NAN, 0., 0.]),
                ..MarkEdit::default()
            },
            MarkEdit {
                box_width: Some(3.),
                ..MarkEdit::default()
            },
            MarkEdit {
                dim: Some(2.),
                ..MarkEdit::default()
            },
            MarkEdit {
                head_size: Some(99.),
                ..MarkEdit::default()
            },
        ] {
            assert!(!doc.edit_marks(&all, &bad), "{bad:?}");
        }
        assert_eq!(doc.revision(), before, "rejected edits leave no history");
        assert!(
            !doc.edit_marks(&all, &MarkEdit::default()),
            "empty edit changes nothing"
        );
        Ok(())
    }

    #[test]
    fn kind_specific_fields_reach_only_the_marks_that_have_them() -> Result<()> {
        let mut doc = big(300, 200)?;
        doc.add(boxy(1, 10., 10., 30., 30.));
        let c = doc.id();
        doc.add(Mark::connector(
            c,
            Endpoint {
                point: point(10., 100.),
                anchor: None,
            },
            Endpoint {
                point: point(90., 100.),
                anchor: None,
            },
            RED,
            2.,
        ));
        doc.add(Mark::text(3, point(100., 10.), "Cap", RED, 4.));
        doc.add(Mark::spotlight(4, point(150., 100.), point(250., 180.)));
        let before = doc.marks.clone();
        let ids = [1, c, 3, 4];
        let e = MarkEdit {
            head_size: Some(3.),
            font_px: Some(40.),
            background: Some(true),
            box_width: Some(80.),
            bold: Some(true),
            italic: Some(true),
            align: Some(TextAlign::Center),
            radius: Some(30.),
            dim: Some(0.5),
            ..MarkEdit::default()
        };
        assert!(doc.edit_marks(&ids, &e));
        assert_eq!(
            doc.mark(1).unwrap(),
            &before[0],
            "a plain box has none of those fields"
        );
        assert_eq!(doc.mark(c).unwrap().head_size, 3.);
        assert_eq!(doc.mark(c).unwrap().text_style, TextStyle::default());
        let t = doc.mark(3).unwrap();
        assert_eq!(doc.text_size(3), Some(40.));
        assert_eq!(
            t.text_style,
            TextStyle {
                bold: true,
                italic: true,
                align: TextAlign::Center
            }
        );
        assert!(matches!(
            t.kind,
            MarkKind::Text { box_width, background: true, .. } if box_width == 80.
        ));
        assert_eq!(t.head_size, before[2].head_size);
        assert!(matches!(
            doc.mark(4).unwrap().kind,
            MarkKind::Spotlight { radius, dim } if radius == 30. && dim == 0.5
        ));
        doc.undo();
        assert_eq!(doc.marks, before, "the whole mixed edit is one undo");
        Ok(())
    }

    #[test]
    fn align_and_distribute_are_one_undo_and_treat_groups_as_one_object() -> Result<()> {
        let mut doc = big(200, 100)?;
        doc.add(boxy(1, 10., 10., 20., 10.));
        doc.add(boxy(2, 50., 30., 10., 10.));
        doc.add(boxy(3, 100., 5., 30., 30.));
        let start = doc.marks.clone();
        let ids = [1, 2, 3];
        let pos = |doc: &Document| ids.map(|id| bounds_of(doc, id));
        let cases: [(Align, fn(&Bounds) -> f32, f32); 6] = [
            (Align::Left, |b| b.x, 10.),
            (Align::Center, |b| b.x + b.width / 2., 70.),
            (Align::Right, |b| b.x + b.width, 130.),
            (Align::Top, |b| b.y, 5.),
            (Align::Middle, |b| b.y + b.height / 2., 22.5),
            (Align::Bottom, |b| b.y + b.height, 40.),
        ];
        for (how, get, want) in cases {
            assert!(doc.align_marks(&ids, how), "{how:?}");
            assert!(pos(&doc).iter().all(|b| approx(get(b), want)), "{how:?}");
            doc.undo();
            assert_eq!(doc.marks, start, "{how:?} is exactly one undo");
        }
        assert!(
            !doc.align_marks(&[1], Align::Left),
            "one object has nothing to align to"
        );

        // 10..30, 50..60, 100..130: equal gaps of 30 keep the outer two where they are.
        assert!(!doc.distribute_marks(&[1, 2], Axis::Horizontal));
        assert!(doc.distribute_marks(&[3, 1, 2], Axis::Horizontal));
        let [a, b, c] = pos(&doc);
        assert!(
            approx(a.x, 10.) && approx(b.x, 60.) && approx(c.x, 100.),
            "{a:?} {b:?} {c:?}"
        );
        assert!(approx(b.x - (a.x + a.width), c.x - (b.x + b.width)));
        doc.undo();
        assert_eq!(doc.marks, start);
        assert!(doc.distribute_marks(&ids, Axis::Vertical));
        let [a, b, c] = pos(&doc);
        // Sorted by top edge: c (5..35), a (10..20), b (30..40); outer edges stay, gaps equal.
        assert!(
            approx(c.y, 5.) && approx(b.y + b.height, 40.),
            "{a:?} {b:?} {c:?}"
        );
        assert!(
            approx(a.y - (c.y + c.height), b.y - (a.y + a.height)),
            "{a:?} {b:?} {c:?}"
        );
        doc.undo();

        // A group is a single object: aligning moves it whole, relative layout intact.
        assert!(doc.group_marks(&[2, 3]));
        assert_eq!(doc.units(&ids).len(), 2);
        assert!(
            !doc.distribute_marks(&ids, Axis::Horizontal),
            "two objects, not three"
        );
        assert!(doc.align_marks(&ids, Align::Left));
        let [a, b, c] = pos(&doc);
        assert!(
            approx(a.x, 10.) && approx(b.x, 10.) && approx(c.x, 60.),
            "{a:?} {b:?} {c:?}"
        );
        Ok(())
    }

    #[test]
    fn numeric_geometry_sets_each_objects_box_and_rejects_nonsense() -> Result<()> {
        let mut doc = big(300, 200)?;
        doc.add(boxy(1, 10., 10., 40., 20.));
        doc.add(Mark::text(2, point(100., 100.), "Text", RED, 4.));
        let before = doc.marks.clone();
        assert!(doc.set_geometry(
            &[1],
            Geometry {
                x: Some(5.),
                y: Some(6.),
                width: Some(80.),
                height: Some(10.)
            }
        ));
        let b = bounds_of(&doc, 1);
        assert!(
            approx(b.x, 5.) && approx(b.y, 6.) && approx(b.width, 80.) && approx(b.height, 10.),
            "{b:?}"
        );
        assert_eq!(
            doc.mark(1).unwrap().points,
            before[0].points,
            "geometry is transform only"
        );
        doc.undo();
        assert_eq!(doc.marks, before);

        // Text only scales uniformly; the width wins when both are given.
        let t0 = bounds_of(&doc, 2);
        assert!(doc.set_geometry(
            &[2],
            Geometry {
                width: Some(f64::from(t0.width) * 2.),
                height: Some(1.),
                ..Geometry::default()
            }
        ));
        let t1 = bounds_of(&doc, 2);
        assert!(
            approx(t1.width, t0.width * 2.) && approx(t1.height, t0.height * 2.),
            "{t0:?} {t1:?}"
        );
        assert!(
            approx(t1.x, t0.x) && approx(t1.y, t0.y),
            "scaled about its corner"
        );
        doc.undo();

        // Several objects: the value is applied to each one's own box.
        assert!(doc.set_geometry(
            &[1, 2],
            Geometry {
                x: Some(0.),
                ..Geometry::default()
            }
        ));
        assert!(approx(bounds_of(&doc, 1).x, 0.) && approx(bounds_of(&doc, 2).x, 0.));
        doc.undo();

        // A group is scaled as one object about the corner of its union box.
        doc.add(boxy(3, 60., 40., 20., 20.));
        assert!(doc.group_marks(&[1, 3]));
        let u = doc.selection_bounds(&[1]).unwrap();
        assert!(doc.set_geometry(
            &[1],
            Geometry {
                width: Some(f64::from(u.width) * 2.),
                ..Geometry::default()
            }
        ));
        let v = doc.selection_bounds(&[1]).unwrap();
        assert!(
            approx(v.x, u.x) && approx(v.width, u.width * 2.) && approx(v.height, u.height),
            "{u:?} {v:?}"
        );
        doc.undo();

        let rev = doc.revision();
        for bad in [
            Geometry {
                width: Some(0.5),
                ..Geometry::default()
            },
            Geometry {
                height: Some(f64::NAN),
                ..Geometry::default()
            },
            Geometry {
                x: Some(f64::INFINITY),
                ..Geometry::default()
            },
            Geometry {
                x: Some(1.0e9),
                ..Geometry::default()
            },
            Geometry {
                width: Some(1.0e9),
                ..Geometry::default()
            },
        ] {
            assert!(!doc.set_geometry(&[1], bad), "{bad:?}");
        }
        assert!(
            !doc.set_geometry(&[1], Geometry::default()),
            "nothing to change"
        );
        assert_eq!(doc.revision(), rev);
        Ok(())
    }

    #[test]
    fn rotation_is_absolute_about_the_centre_and_reversible() -> Result<()> {
        let mut doc = big(100, 100)?;
        doc.add(boxy(1, 10., 10., 20., 10.));
        let before = doc.marks.clone();
        assert!(doc.set_rotation(&[1], 90.));
        assert!((doc.rotation_degrees(1).unwrap() - 90.).abs() < 1e-3);
        let b = bounds_of(&doc, 1);
        assert!(
            approx(b.x, 15.) && approx(b.y, 5.) && approx(b.width, 10.) && approx(b.height, 20.),
            "{b:?}"
        );
        assert!(!doc.set_rotation(&[1], 90.), "already there");
        assert!(!doc.set_rotation(&[1], f64::NAN));
        assert!(doc.set_rotation(&[1], 0.));
        let b = bounds_of(&doc, 1);
        assert!(approx(b.x, 10.) && approx(b.width, 20.), "{b:?}");
        doc.undo();
        doc.undo();
        assert_eq!(doc.marks, before);
        Ok(())
    }

    #[test]
    fn text_preview_replaces_the_original_in_place_and_privacy_stays_on_top() -> Result<()> {
        let mut doc = big(120, 40)?;
        doc.add(Mark::text(1, point(5., 5.), "Old", RED, 4.));
        let pixels =
            |img: &RgbaImage, f: fn(&[u8; 4]) -> bool| img.pixels().filter(|p| f(&p.0)).count();
        let red = |p: &[u8; 4]| p[0] > 200 && p[1] < 60;
        let green = |p: &[u8; 4]| p[1] > 200 && p[0] < 60;
        let original = doc.mark(1).unwrap().clone();
        assert!(pixels(&paint_text_to_image(&doc, Some(1), &original)?, red) > 0);

        let mut edited = doc
            .retexted(
                1,
                "New",
                0.,
                false,
                24.,
                TextStyle::default(),
                "Sans",
                "Regular",
                TextEffect::None,
                None,
            )
            .unwrap();
        edited.color = [0., 1., 0., 1.];
        let marks = doc.marks.clone();
        let rev = doc.revision();
        let img = paint_text_to_image(&doc, Some(1), &edited)?;
        assert_eq!(
            pixels(&img, red),
            0,
            "the old caption must not ghost through"
        );
        assert!(pixels(&img, green) > 0);
        assert_eq!(
            (doc.marks.clone(), doc.revision()),
            (marks, rev),
            "a preview mutates nothing"
        );

        // A new caption is drawn above the annotations, below privacy masks.
        let mut fresh = Mark::text(0, point(60., 5.), "New", [0., 1., 0., 1.], 4.);
        fresh.font_size = 24.;
        let img = paint_text_to_image(&doc, None, &fresh)?;
        assert!(
            pixels(&img, red) > 0 && pixels(&img, green) > 0,
            "original stays, new added"
        );
        doc.add(Mark::redaction(2, point(0., 0.), point(120., 40.)));
        let img = paint_text_to_image(&doc, None, &fresh)?;
        assert!(
            img.pixels().all(|p| p.0 == [0, 0, 0, 255]),
            "mask covers the preview"
        );
        Ok(())
    }

    // --- filled shapes, rounded rectangles, caps, highlighters and fonts ------------------------

    fn rect_mark(id: u64, color: [f64; 4], width: f64) -> Mark {
        Mark::path(
            id,
            vec![
                point(10., 10.),
                point(50., 10.),
                point(50., 40.),
                point(10., 40.),
            ],
            true,
            color,
            width,
        )
    }

    #[test]
    fn filled_shapes_hit_their_interior_and_transparent_ones_click_through() -> Result<()> {
        let mut doc = big(80, 60)?;
        let centre = point(30., 25.);
        doc.add(rect_mark(1, RED, 2.));
        assert_eq!(
            doc.hit_test(centre, 0.),
            None,
            "an outline is not hit inside"
        );
        assert!(doc.edit_marks(
            &[1],
            &MarkEdit {
                fill: Some(true),
                ..MarkEdit::default()
            }
        ));
        assert_eq!(doc.hit_test(centre, 0.), Some(1));
        assert_eq!(doc.hit_test(point(70., 25.), 0.), None);
        assert!(doc.edit_marks(
            &[1],
            &MarkEdit {
                alpha: Some(0.),
                ..MarkEdit::default()
            }
        ));
        assert_eq!(
            doc.hit_test(centre, 0.),
            None,
            "fully transparent: click through"
        );
        assert!(doc.edit_marks(
            &[1],
            &MarkEdit {
                alpha: Some(0.3),
                ..MarkEdit::default()
            }
        ));
        assert_eq!(
            doc.hit_test(centre, 0.),
            Some(1),
            "translucent fills are solid targets"
        );

        // An ellipse is hit in its curve, not in the corners of its box.
        let mut ellipse = Mark::path(
            2,
            (0..96)
                .map(|i| {
                    let t = i as f32 / 96. * std::f32::consts::TAU;
                    point(40. + 20. * t.cos(), 30. + 15. * t.sin())
                })
                .collect(),
            true,
            RED,
            2.,
        );
        ellipse.fill = true;
        // On its own: the rectangle above would otherwise be the thing hit at (22, 17).
        let mut alone = big(80, 60)?;
        alone.add(ellipse);
        assert_eq!(alone.hit_test(point(40., 30.), 0.), Some(2));
        assert_eq!(
            alone.hit_test(point(22., 17.), 0.),
            None,
            "inside the box, outside the curve"
        );

        // An open path has no interior whatever its flag says.
        let mut open = shape(3, &[(0., 50.), (30., 50.), (30., 59.)], false);
        open.fill = true;
        assert_eq!(open.hit_test(point(20., 55.), 0.), false);
        Ok(())
    }

    #[test]
    fn rounded_corners_apply_to_rectangles_only_and_shape_the_hit_and_the_pixels() -> Result<()> {
        let mut square = rect_mark(1, RED, 2.);
        assert!(square.is_rectangle());
        // The same rectangle turned 30 degrees is still one.
        let turned = Mark {
            points: square
                .points
                .iter()
                .map(|p| Affine::rotation_about(0.5, point(30., 25.)).apply(*p))
                .collect(),
            ..square.clone()
        };
        assert!(turned.is_rectangle());
        let kite = shape(2, &[(10., 10.), (50., 20.), (30., 40.), (12., 30.)], true);
        let triangle = shape(3, &[(10., 10.), (50., 10.), (30., 40.)], true);
        let slanted = shape(4, &[(10., 10.), (50., 10.), (60., 40.), (20., 40.)], true);
        let ellipse = Mark::path(
            5,
            (0..96)
                .map(|i| {
                    let t = i as f32 / 96. * std::f32::consts::TAU;
                    point(40. + 20. * t.cos(), 30. + 15. * t.sin())
                })
                .collect(),
            true,
            RED,
            2.,
        );
        for other in [&kite, &triangle, &slanted, &ellipse] {
            assert!(!other.is_rectangle(), "{:?}", other.points);
        }

        let round = |radius: f64, shape: &Mark| -> Result<RgbaImage> {
            let mut doc = big(80, 60)?;
            let mut m = shape.clone();
            m.fill = true;
            m.corner_radius = radius;
            doc.add(m);
            doc.render()
        };
        let sharp = round(0., &square)?;
        let rounded = round(10., &square)?;
        // Pixel (10, 10) is the square corner, 13.4 px from the arc's centre: beyond the arc and
        // its 1 px half-stroke, so a rounded corner leaves the background there.
        assert_eq!(sharp.get_pixel(10, 10).0, [255, 0, 0, 255]);
        assert_eq!(
            rounded.get_pixel(10, 10).0,
            [20, 40, 60, 255],
            "the corner is cut"
        );
        assert_eq!(rounded.get_pixel(30, 25), sharp.get_pixel(30, 25));
        for other in [&triangle, &kite, &slanted, &ellipse] {
            assert_eq!(round(10., other)?, round(0., other)?, "{:?}", other.points);
        }

        let mut doc = big(80, 60)?;
        square.fill = true;
        square.corner_radius = 10.;
        doc.add(square);
        assert_eq!(doc.hit_test(point(11.5, 11.5), 0.), None, "outside the arc");
        assert_eq!(doc.hit_test(point(20., 20.), 0.), Some(1));

        // Without a fill only the arc is a target, not the removed square corner.
        assert!(doc.edit_marks(
            &[1],
            &MarkEdit {
                fill: Some(false),
                ..MarkEdit::default()
            }
        ));
        assert_eq!(doc.hit_test(point(10., 10.), 0.), None, "removed corner");
        assert_eq!(doc.hit_test(point(12.93, 12.93), 0.), Some(1), "on the arc");
        assert_eq!(doc.hit_test(point(30., 10.), 0.), Some(1), "straight edge");
        Ok(())
    }

    #[test]
    fn translucent_fill_and_outline_form_one_flat_layer() -> Result<()> {
        let mut doc = big(80, 60)?;
        let mut m = rect_mark(1, [1., 0., 0., 0.5], 6.);
        m.fill = true;
        doc.add(m);
        let img = doc.render()?;
        let inner = img.get_pixel(30, 25);
        assert_ne!(*inner, image::Rgba([20, 40, 60, 255]), "the fill shows");
        // Inner half of the outline, outer half and interior share one colour: no darker rim.
        assert_eq!(img.get_pixel(11, 25), inner);
        assert_eq!(img.get_pixel(8, 25), inner);
        Ok(())
    }

    #[test]
    fn highlighter_crossing_itself_does_not_darken_but_two_marks_do() -> Result<()> {
        let yellow = [1., 1., 0., 0.4];
        let cross = vec![
            point(10., 30.),
            point(50., 30.),
            point(30., 10.),
            point(30., 50.),
        ];
        let mut doc = big(60, 60)?;
        doc.add(Mark::highlighter(1, cross, yellow, 8.));
        let img = doc.render()?;
        let single = *img.get_pixel(15, 30);
        assert_ne!(single, image::Rgba([20, 40, 60, 255]));
        assert_eq!(
            *img.get_pixel(30, 30),
            single,
            "the crossing keeps the same tint"
        );
        assert_eq!(*img.get_pixel(30, 45), single);

        let mut two = big(60, 60)?;
        two.add(Mark::highlighter(
            1,
            vec![point(10., 30.), point(50., 30.)],
            yellow,
            8.,
        ));
        two.add(Mark::highlighter(
            2,
            vec![point(30., 10.), point(30., 50.)],
            yellow,
            8.,
        ));
        assert_ne!(
            *two.render()?.get_pixel(30, 30),
            single,
            "separate highlights still stack"
        );
        Ok(())
    }

    #[test]
    fn butt_caps_end_flat_and_the_bounds_contain_the_mitres() -> Result<()> {
        let line = |round: bool| {
            let mut m = Mark::path(1, vec![point(20., 20.), point(40., 20.)], false, RED, 10.);
            m.round_caps = round;
            m
        };
        let render = |m: Mark| -> Result<RgbaImage> {
            let mut doc = big(60, 40)?;
            doc.add(m);
            doc.render()
        };
        let (round, flat) = (render(line(true))?, render(line(false))?);
        assert_eq!(round.get_pixel(17, 20).0, [255, 0, 0, 255]);
        assert_eq!(
            flat.get_pixel(17, 20).0,
            [20, 40, 60, 255],
            "no cap past the end"
        );
        assert_eq!(flat.get_pixel(30, 20), round.get_pixel(30, 20));

        // A sharp corner's tip is painted; every painted pixel lies inside the reported bounds.
        let mut v = shape(1, &[(10., 30.), (30., 10.), (50., 30.)], false);
        v.width = 8.;
        v.round_caps = false;
        let b = v.bounds().unwrap();
        let img = render(v)?;
        for (x, y, p) in img.enumerate_pixels() {
            if p.0 != [20, 40, 60, 255] {
                let (x, y) = (x as f32 + 0.5, y as f32 + 0.5);
                assert!(
                    x >= b.x && x <= b.x + b.width && y >= b.y && y <= b.y + b.height,
                    "{x},{y} outside {b:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn style_edits_reach_only_the_kinds_that_use_them_and_undo_as_one_step() -> Result<()> {
        let mut doc = big(80, 60)?;
        doc.add(rect_mark(1, RED, 2.));
        doc.add(shape(2, &[(0., 50.), (30., 50.)], false));
        doc.add(text_mark(3, 4.));
        doc.add(Mark::step(4, point(60., 40.), 1, RED, 6.));
        doc.add(Mark::redaction(5, point(0., 0.), point(5., 5.)));
        let before = doc.marks.clone();
        let all = [1, 2, 3, 4, 5];
        let e = MarkEdit {
            fill: Some(true),
            round_caps: Some(false),
            corner_radius: Some(7.),
            font_family: Some("Serif, Sans".into()),
            ..MarkEdit::default()
        };
        assert!(doc.edit_marks(&all, &e));
        let m = |id| doc.mark(id).unwrap();
        assert!(m(1).fill && !m(1).round_caps && m(1).corner_radius == 7.);
        assert!(
            !m(2).fill && !m(2).round_caps && m(2).corner_radius == 0.,
            "open ink: caps only"
        );
        assert_eq!(
            (m(3).font_family.as_str(), m(4).font_family.as_str()),
            ("Serif, Sans", "Serif, Sans")
        );
        assert_eq!(m(1).font_family, DEFAULT_FAMILY, "a rectangle has no font");
        assert_eq!(*m(5), before[4], "a privacy mask never takes a style");
        doc.undo();
        assert_eq!(doc.marks, before);
        assert!(!doc.can_undo() || doc.revision() > 0);

        for bad in ["", "   ", "Se\nrif", &"x".repeat(MAX_FAMILY + 1)] {
            let e = MarkEdit {
                font_family: Some(bad.into()),
                ..MarkEdit::default()
            };
            assert!(!doc.edit_marks(&all, &e), "{bad:?}");
        }
        let e = MarkEdit {
            corner_radius: Some(-1.),
            ..MarkEdit::default()
        };
        assert!(!doc.edit_marks(&all, &e));
        assert_eq!(doc.marks, before);

        // Copies keep the whole style.
        assert!(doc.edit_marks(
            &[1],
            &MarkEdit {
                fill: Some(true),
                ..MarkEdit::default()
            }
        ));
        let copy = doc.duplicate_marks(&[1])[0];
        assert_eq!(doc.mark(copy).unwrap().fill, true);
        Ok(())
    }

    #[test]
    fn font_family_is_in_the_layout_the_metrics_and_validation() -> Result<()> {
        let layout = MEASURE.with(|cr| {
            text_layout(
                cr.as_ref().unwrap(),
                "Hi",
                "Monospace, Serif",
                DEFAULT_FONT_STYLE,
                20.,
                0.,
                TextStyle::default(),
            )
        });
        let font = layout.font_description().unwrap();
        assert_eq!(font.family().as_deref(), Some("Monospace, Serif"));
        let blank = font_description("  ", DEFAULT_FONT_STYLE, 20., TextStyle::default());
        assert_eq!(blank.family().as_deref(), Some(DEFAULT_FAMILY));

        // Every surface asks `font_description`, so a live caption and the saved mark agree.
        let mut m = text_mark(1, 4.);
        for family in ["Sans", "Monospace", "Serif, Sans"] {
            m.font_family = family.into();
            let b = m.local_bounds().unwrap();
            let fresh = measured(
                "Hello",
                family,
                &m.font_style,
                m.font_size,
                0.,
                m.text_style,
            );
            assert_eq!((b.width, b.height), (fresh.width, fresh.height), "{family}");
        }
        assert!(valid_family("Inter, Noto Sans"));
        for bad in ["", " ", "a\u{0}b", &"y".repeat(MAX_FAMILY + 1)] {
            assert!(!valid_family(bad), "{bad:?}");
        }
        let mut bad = text_mark(2, 4.);
        bad.font_family = "x\ny".into();
        assert!(Document::from_parts(RgbaImage::new(30, 30), vec![bad], None).is_err());
        let mut neg = rect_mark(3, RED, 2.);
        neg.corner_radius = f64::NAN;
        assert!(Document::from_parts(RgbaImage::new(60, 60), vec![neg], None).is_err());
        Ok(())
    }

    #[test]
    fn privacy_masks_stay_opaque_above_fills_highlights_and_fonts() -> Result<()> {
        let mut doc = big(60, 50)?;
        let mut filled = rect_mark(1, [1., 1., 0., 0.5], 6.);
        filled.fill = true;
        doc.add(filled);
        doc.add(Mark::highlighter(
            2,
            vec![point(5., 25.), point(55., 25.)],
            [1., 0., 1., 0.4],
            14.,
        ));
        doc.add(Mark::highlight(
            3,
            point(5., 5.),
            point(55., 45.),
            [0., 1., 0., 0.4],
        ));
        let mut text = Mark::text(4, point(12., 18.), "SECRET", [1., 1., 1., 1.], 4.);
        text.font_family = "Monospace".into();
        doc.add(text);
        doc.add(Mark::redaction(5, point(8., 8.), point(52., 42.)));
        doc.add(Mark::privacy_blur(6, point(0., 44.), point(10., 50.)));
        let img = doc.render()?;
        for (x, y) in [(9, 9), (30, 25), (51, 41), (12, 20)] {
            assert_eq!(img.get_pixel(x, y).0, [0, 0, 0, 255], "{x},{y}");
        }
        Ok(())
    }

    #[test]
    fn text_effects_paint_behind_the_glyphs_and_grow_the_bounds() -> Result<()> {
        assert_eq!(
            TextEffect::ALL.map(|e| e.next()),
            [
                TextEffect::InvertedOutline,
                TextEffect::ContrastOutline,
                TextEffect::Shadow,
                TextEffect::None
            ]
        );
        let text = |effect: TextEffect| {
            let mut m = Mark::text(1, point(15., 10.), "Hi", RED, 4.);
            m.font_size = 40.;
            m.text_effect = effect;
            m
        };
        let render = |m: Mark| -> Result<RgbaImage> {
            let mut doc = big(100, 70)?;
            doc.add(m);
            doc.render()
        };
        let painted =
            |img: &RgbaImage, f: fn(&[u8; 4]) -> bool| img.pixels().filter(|p| f(&p.0)).count();
        let cyan = |p: &[u8; 4]| p[1] > 200 && p[2] > 200 && p[0] < 60;
        let black = |p: &[u8; 4]| p[0] < 15;
        let plain = render(text(TextEffect::None))?;
        assert_eq!(painted(&plain, cyan) + painted(&plain, black), 0);
        assert!(painted(&render(text(TextEffect::InvertedOutline))?, cyan) > 0);
        // Red text is dark enough for a white contrast outline, so there is no black.
        let contrast = render(text(TextEffect::ContrastOutline))?;
        assert!(contrast.pixels().any(|p| p.0 == [255, 255, 255, 255]));
        assert!(painted(&render(text(TextEffect::Shadow))?, black) > 0);

        let base = text(TextEffect::None).local_bounds().unwrap();
        for effect in [
            TextEffect::InvertedOutline,
            TextEffect::ContrastOutline,
            TextEffect::Shadow,
        ] {
            let b = text(effect).local_bounds().unwrap();
            assert!(b.width > base.width && b.height > base.height, "{effect:?}");
        }
        // Every painted pixel stays inside the reported bounds, so selection and export see it all.
        for effect in TextEffect::ALL {
            let m = text(effect);
            let b = m.bounds().unwrap();
            for (x, y, p) in render(m)?.enumerate_pixels() {
                if p.0 != [20, 40, 60, 255] {
                    let (x, y) = (x as f32 + 0.5, y as f32 + 0.5);
                    assert!(
                        x >= b.x - 1.
                            && x <= b.x + b.width + 1.
                            && y >= b.y - 1.
                            && y <= b.y + b.height + 1.,
                        "{effect:?}: {x},{y} outside {b:?}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn step_ring_is_drawn_hit_and_edited_only_on_steps() -> Result<()> {
        let mut doc = big(80, 60)?;
        doc.add(Mark::step(1, point(40., 30.), 1, RED, 4.));
        doc.add(rect_mark(2, RED, 2.));
        let plain = doc.mark(1).unwrap().clone();
        let r = step_radius(4.) as f32;
        assert_eq!(doc.hit_test(point(40. + r * 1.3, 30.), 0.), None);
        assert!(doc.edit_marks(
            &[1, 2],
            &MarkEdit {
                step_ring: Some(true),
                ..MarkEdit::default()
            }
        ));
        assert!(doc.mark(1).unwrap().step_ring && !doc.mark(2).unwrap().step_ring);
        assert_eq!(doc.hit_test(point(40. + r * 1.3, 30.), 0.), Some(1));
        let (a, b) = (
            plain.bounds().unwrap(),
            doc.mark(1).unwrap().bounds().unwrap(),
        );
        assert!(b.width > a.width);
        let img = doc.render()?;
        assert!(img.get_pixel(40 + (r * 1.3) as u32, 30).0[0] > 200);
        let gap = img.get_pixel(40 + (r * 1.15) as u32, 30);
        assert_eq!(
            gap.0,
            [20, 40, 60, 255],
            "a gap separates the ring from the disc"
        );
        doc.undo();
        assert_eq!(*doc.mark(1).unwrap(), plain);
        Ok(())
    }

    #[test]
    fn decimal_widths_down_to_a_tenth_are_valid_and_old_values_are_unchanged() -> Result<()> {
        let thin = |w: f64| Mark::path(1, vec![point(2., 2.), point(9., 2.)], false, RED, w);
        assert!(Document::from_parts(RgbaImage::new(12, 12), vec![thin(MIN_WIDTH)], None).is_ok());
        assert!(Document::from_parts(RgbaImage::new(12, 12), vec![thin(0.05)], None).is_err());
        assert!(
            Document::from_parts(RgbaImage::new(12, 12), vec![thin(MAX_WIDTH + 1.)], None).is_err()
        );
        let mut doc = big(20, 12)?;
        doc.add(thin(3.));
        assert!(doc.edit_marks(
            &[1],
            &MarkEdit {
                width: Some(0.1),
                ..MarkEdit::default()
            }
        ));
        assert_eq!(doc.mark(1).unwrap().width, 0.1);
        assert!(!doc.edit_marks(
            &[1],
            &MarkEdit {
                width: Some(0.09),
                ..MarkEdit::default()
            }
        ));
        assert!(doc.set_style_many(&[1], RED, 0.5));
        assert!(!doc.set_style_many(&[1], RED, 0.));
        Ok(())
    }

    #[test]
    fn marks_saved_before_the_new_styles_deserialize_with_the_old_look() {
        let mut m = Mark::text(1, point(3., 4.), "Old", RED, 3.);
        let mut v = serde_json::to_value(&m).unwrap();
        let o = v.as_object_mut().unwrap();
        for key in [
            "fill",
            "round_caps",
            "corner_radius",
            "font_family",
            "text_effect",
            "step_ring",
        ] {
            assert!(o.remove(key).is_some(), "{key}");
        }
        m.text_effect = TextEffect::None;
        assert_eq!(serde_json::from_value::<Mark>(v).unwrap(), m);
        assert_eq!(
            serde_json::to_value(TextEffect::ContrastOutline).unwrap(),
            "contrast-outline"
        );
    }

    #[test]
    fn font_style_strings_set_the_pango_face_survive_storage_and_default_to_regular() {
        let plain = TextStyle::default();
        let face = |style: &str, rich: TextStyle| font_description("Serif", style, 20., rich);
        let regular = face("Regular", plain);
        assert_eq!(
            (regular.weight(), regular.style()),
            (Weight::Normal, Style::Normal)
        );
        // Toggles start from what the style string names, so the face is exactly that one.
        let named = |s: &str| face(s, font_style_defaults(s));
        assert_eq!(named("Heavy").weight(), Weight::Heavy);
        assert_eq!(named("Light").weight(), Weight::Light);
        let light_italic = named("Light Italic");
        assert_eq!(
            (light_italic.weight(), light_italic.style()),
            (Weight::Light, Style::Italic)
        );
        // The family and the absolute size are the mark's, whatever the style string says.
        let sized = face("Heavy 99", font_style_defaults("Heavy"));
        assert_eq!(sized.family().as_deref(), Some("Serif"));
        assert_eq!(sized.size(), (20.0 * f64::from(SCALE)) as i32);
        // Explicit rich toggles win over the style string.
        let bold = TextStyle {
            bold: true,
            ..plain
        };
        assert_eq!(face("Light", bold).weight(), Weight::Bold);
        assert_eq!(face("Regular", bold).weight(), Weight::Bold);
        assert_eq!(face("Bogus Words", plain).weight(), Weight::Normal);

        // The mark stores it, the cache keys on it, and layouts carry it.
        let mut m = text_mark(1, 4.);
        assert_eq!(m.font_style, DEFAULT_FONT_STYLE);
        m.font_style = "Heavy".into();
        let layout = MEASURE.with(|cr| {
            text_layout(
                cr.as_ref().unwrap(),
                "Hi",
                "Sans",
                &m.font_style,
                20.,
                0.,
                font_style_defaults("Heavy"),
            )
        });
        assert_eq!(layout.font_description().unwrap().weight(), Weight::Heavy);
        m.text_style = font_style_defaults("Heavy");
        let heavy = m.local_bounds().unwrap();
        m.font_style = "Regular".into();
        m.text_style = plain;
        let fresh = measured("Hello", "Sans", "Regular", m.font_size, 0., plain);
        let regular = m.local_bounds().unwrap();
        assert_eq!((regular.width, regular.height), (fresh.width, fresh.height));
        m.font_style = "Heavy".into();
        m.text_style = font_style_defaults("Heavy");
        assert_eq!(
            m.local_bounds(),
            Some(heavy),
            "a cache hit answers the same"
        );
        assert!(
            heavy.width >= regular.width,
            "heavy glyphs are never narrower"
        );

        // Persistence: it roundtrips, an old mark without the key reads Regular, bad ones are refused.
        let back: Mark = serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        let mut v = serde_json::to_value(&m).unwrap();
        assert!(v.as_object_mut().unwrap().remove("font_style").is_some());
        let old: Mark = serde_json::from_value(v).unwrap();
        assert_eq!(old.font_style, "Regular");
        for bad in ["", "  ", "Hea\nvy", &"H".repeat(MAX_FONT_STYLE + 1)] {
            assert!(!valid_font_style(bad), "{bad:?}");
            let mut broken = text_mark(2, 4.);
            broken.font_style = bad.into();
            assert!(Document::from_parts(RgbaImage::new(30, 30), vec![broken], None).is_err());
        }

        // One undo step changes the style of captions and steps, and only those.
        let mut doc = big(80, 60).unwrap();
        doc.add(text_mark(1, 4.));
        doc.add(Mark::step(2, point(40., 40.), 1, RED, 4.));
        doc.add(rect_mark(3, RED, 2.));
        let before = doc.marks.clone();
        let edit = MarkEdit {
            font_style: Some("Heavy".into()),
            ..MarkEdit::default()
        };
        assert!(doc.edit_marks(&[1, 2, 3], &edit));
        assert_eq!(
            doc.marks
                .iter()
                .map(|m| m.font_style.as_str())
                .collect::<Vec<_>>(),
            ["Heavy", "Heavy", "Regular"]
        );
        let bad = MarkEdit {
            font_style: Some(" ".into()),
            ..MarkEdit::default()
        };
        assert!(!doc.edit_marks(&[1], &bad));
        assert!(
            doc.marks[0].text_style.bold && !doc.marks[2].text_style.bold,
            "the caption takes the bold of Heavy; a step has no text style"
        );
        doc.undo();
        assert_eq!(doc.marks, before);
    }

    #[test]
    fn bold_and_italic_toggles_turn_a_configured_face_on_and_off() {
        use gtk::glib::translate::IntoGlib;
        let weight = |d: &FontDescription| d.weight().into_glib();
        let face = |style: &str, bold: bool, italic: bool| {
            font_description(
                "Serif",
                style,
                20.,
                TextStyle {
                    bold,
                    italic,
                    ..TextStyle::default()
                },
            )
        };
        // What a style string stands for.
        let flags = |s: &str| {
            let d = font_style_defaults(s);
            (d.bold, d.italic)
        };
        assert_eq!(flags("Regular"), (false, false));
        assert_eq!(flags("Bold"), (true, false));
        assert_eq!(flags("Bold Italic"), (true, true));
        assert_eq!(flags("Heavy"), (true, false));
        assert_eq!(flags("Light Italic"), (false, true));
        assert_eq!(flags("Medium"), (false, false));
        assert_eq!(flags("Oblique"), (false, true));
        assert_eq!(flags("Nonsense"), (false, false));

        // Starting from the defaults shows the configured face exactly...
        let d = font_style_defaults("Bold Italic");
        let shown = font_description("Serif", "Bold Italic", 20., d);
        assert_eq!(
            (shown.weight(), shown.style()),
            (Weight::Bold, Style::Italic)
        );
        assert_eq!(
            font_description("Serif", "Heavy", 20., font_style_defaults("Heavy")).weight(),
            Weight::Heavy
        );
        // ...and switching the toggles off really removes bold and slant.
        let off = face("Bold Italic", false, false);
        assert_eq!((off.weight(), off.style()), (Weight::Normal, Style::Normal));
        assert_eq!(face("Heavy", false, false).weight(), Weight::Normal);
        // Off then on again uses the plain Bold and Italic, or keeps a configured heavier face.
        let on = face("Regular", true, true);
        assert_eq!((on.weight(), on.style()), (Weight::Bold, Style::Italic));
        assert_eq!(face("Heavy", true, false).weight(), Weight::Heavy);
        assert_eq!(face("Oblique", false, true).style(), Style::Oblique);
        // A lighter weight is not bold, so turning bold off leaves it, and on gives Bold.
        assert_eq!(face("Light", false, false).weight(), Weight::Light);
        assert_eq!(face("Medium", false, false).weight(), Weight::Medium);
        assert_eq!(
            weight(&face("Light", true, false)),
            Weight::Bold.into_glib()
        );
        // Width and variant are kept whatever the toggles do.
        assert_eq!(
            face("Condensed Bold", false, false).stretch(),
            gtk::pango::Stretch::Condensed
        );

        // Pixels: a Bold-configured caption looks bold, unbolds when B is off, and then equals
        // the Regular one; B on again is bold again.
        let render = |style: &str, bold: bool, italic: bool| -> RgbaImage {
            let mut m = Mark::text(1, point(4., 4.), "Wide", [1., 1., 1., 1.], 4.);
            m.font_size = 28.;
            m.font_style = style.into();
            m.text_style = TextStyle {
                bold,
                italic,
                ..TextStyle::default()
            };
            let mut doc = big(140, 50).unwrap();
            doc.add(m);
            doc.render().unwrap()
        };
        let bold = render("Bold", true, false);
        let unbolded = render("Bold", false, false);
        let regular = render("Regular", false, false);
        assert_ne!(bold, regular, "bold glyphs differ from regular ones");
        assert_eq!(unbolded, regular, "B off removes the configured bold");
        assert_eq!(
            render("Regular", true, false),
            bold,
            "B on is the same bold"
        );
        let slanted = render("Italic", true, true);
        assert_ne!(render("Italic", false, false), slanted);
        assert_eq!(
            render("Italic", false, false),
            regular,
            "I off removes the slant"
        );

        // Changing the style of a caption takes the toggles it names, so they can undo it;
        // explicit flags in the same edit still win, and steps never get a text style.
        let mut doc = big(80, 60).unwrap();
        doc.add(text_mark(1, 4.));
        doc.add(text_mark(2, 4.));
        doc.add(Mark::step(3, point(40., 40.), 1, RED, 4.));
        let styles = |d: &Document| {
            d.marks
                .iter()
                .map(|m| (m.text_style.bold, m.text_style.italic))
                .collect::<Vec<_>>()
        };
        let before = doc.marks.clone();
        let set = |s: &str, bold: Option<bool>| MarkEdit {
            font_style: Some(s.into()),
            bold,
            ..MarkEdit::default()
        };
        assert!(doc.edit_marks(&[1, 3], &set("Bold Italic", None)));
        assert_eq!(styles(&doc), [(true, true), (false, false), (false, false)]);
        assert!(doc.edit_marks(&[2], &set("Bold Italic", Some(false))));
        assert_eq!(styles(&doc)[1], (false, true), "explicit bold wins");
        assert!(doc.edit_marks(&[1], &set("Regular", None)));
        assert_eq!(styles(&doc)[0], (false, false));
        doc.undo();
        doc.undo();
        doc.undo();
        assert_eq!(doc.marks, before);
    }
}
