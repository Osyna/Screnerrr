//! Output frame around the rendered scene: background, padding, border, rounded corners, shadow
//! and a final resize. It only ever sees the finished image, so it can never reveal or alter what
//! the scene (privacy masks included) already decided to show.

use anyhow::{Result, ensure};
use gtk::cairo::{
    Context, Extend, FillRule, Filter, Format, ImageSurface, LinearGradient, SurfacePattern,
};
use image::RgbaImage;
use serde::{Deserialize, Serialize};

pub const MAX_DIM: u32 = 32_767;
pub const MAX_PIXELS: u64 = 1 << 27;
pub const MAX_PADDING: u32 = 4096;
pub const MAX_BORDER: u32 = 256;
pub const MAX_RADIUS: f64 = 4096.0;
/// Blur radius in pixels at `shadow == 1`; the shadow reaches `4 * radius` past the frame.
const SHADOW_BLUR: f64 = 12.0;
const SHADOW_ALPHA: f64 = 0.45;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Background {
    #[default]
    Transparent,
    Solid([f64; 4]),
    Gradient([f64; 4], [f64; 4]),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Presentation {
    /// Space beyond the frame and shadow, before canvas fitting or final resizing.
    pub padding: u32,
    /// Screenshot corner radius; `0` keeps square corners.
    pub radius: f64,
    /// Strength `0..=1`; `0` draws none. The decorated frame grows so its shadow is never clipped.
    pub shadow: f64,
    pub background: Background,
    /// Frame drawn around the screenshot, outside it.
    pub border: u32,
    pub border_color: [f64; 4],
    /// Exact canvas the whole decorated screenshot is fitted into: scaled uniformly to fit and
    /// centred, never cropped or stretched. The background fills the whole canvas.
    pub canvas_size: Option<[u32; 2]>,
    /// Final size of the complete canvas (the fitted canvas, or the decorated screenshot when
    /// there is none); the canvas is resampled to fit.
    pub output_size: Option<[u32; 2]>,
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            padding: 0,
            radius: 0.,
            shadow: 0.,
            background: Background::Transparent,
            border: 0,
            border_color: [1.; 4],
            canvas_size: None,
            output_size: None,
        }
    }
}

/// Geometry of one composition; see `Presentation::layout`.
struct Layout {
    decorated: (u32, u32),
    /// Background canvas; the decorated screenshot is fitted into it.
    base: (u32, u32),
    fit: f64,
    offset: (f64, f64),
    out: (u32, u32),
}

fn color_ok(c: &[f64; 4]) -> bool {
    c.iter().all(|v| (0.0..=1.0).contains(v))
}

impl Presentation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.padding <= MAX_PADDING,
            "padding must be at most {MAX_PADDING}"
        );
        ensure!(
            self.border <= MAX_BORDER,
            "border must be at most {MAX_BORDER}"
        );
        ensure!(
            (0.0..=MAX_RADIUS).contains(&self.radius),
            "corner radius is out of range"
        );
        ensure!(
            (0.0..=1.0).contains(&self.shadow),
            "shadow must be between 0 and 1"
        );
        ensure!(
            color_ok(&self.border_color),
            "border colour is out of range"
        );
        match &self.background {
            Background::Transparent => {}
            Background::Solid(c) => ensure!(color_ok(c), "background colour is out of range"),
            Background::Gradient(a, b) => {
                ensure!(
                    color_ok(a) && color_ok(b),
                    "background colour is out of range"
                );
            }
        }
        for (size, what) in [
            (self.canvas_size, "canvas size"),
            (self.output_size, "output size"),
        ] {
            if let Some([w, h]) = size {
                check_size(w, h, what)?;
            }
        }
        Ok(())
    }

    /// No decoration; canvas fitting and final resizing can still change the image.
    fn is_plain(&self) -> bool {
        self.padding == 0
            && self.border == 0
            && self.shadow == 0.0
            && self.radius == 0.0
            && self.background == Background::Transparent
    }

    fn shadow_radius(&self) -> u32 {
        (self.shadow * SHADOW_BLUR).ceil() as u32
    }

    /// Distance from the decorated frame's edge to the screenshot.
    fn inset(&self) -> u32 {
        self.padding + 4 * self.shadow_radius() + self.border
    }

    /// The decorated screenshot (padding, border, shadow) before it is fitted or resized.
    fn decorated(&self, width: u32, height: u32) -> Result<(u32, u32)> {
        self.validate()?;
        check_size(width, height, "image size")?;
        let grow = 2 * u64::from(self.inset());
        let (w, h) = (u64::from(width) + grow, u64::from(height) + grow);
        ensure!(
            w <= u64::from(MAX_DIM) && h <= u64::from(MAX_DIM) && w * h <= MAX_PIXELS,
            "the framed image would be {w}x{h} pixels, which is too large"
        );
        Ok((w as u32, h as u32))
    }

    /// Where the decorated screenshot lands: the background canvas, the uniform fit into it and
    /// the final size. Without `canvas_size` the fit is exactly the identity.
    fn layout(&self, width: u32, height: u32) -> Result<Layout> {
        let (dw, dh) = self.decorated(width, height)?;
        let base = self.canvas_size.map_or((dw, dh), |[w, h]| (w, h));
        let (bw, bh) = (f64::from(base.0), f64::from(base.1));
        let fit = (bw / f64::from(dw)).min(bh / f64::from(dh));
        Ok(Layout {
            decorated: (dw, dh),
            base,
            fit,
            offset: (
                (bw - f64::from(dw) * fit) / 2.0,
                (bh - f64::from(dh) * fit) / 2.0,
            ),
            out: self.output_size.map_or(base, |[w, h]| (w, h)),
        })
    }

    /// Final pixel size for a `width` x `height` scene.
    pub fn dimensions(&self, width: u32, height: u32) -> Result<(u32, u32)> {
        Ok(self.layout(width, height)?.out)
    }

    /// The same look for a scene scaled by `k`, for a cheap live preview.
    pub fn scaled(&self, k: f64) -> Presentation {
        let px = |v: u32| {
            if v == 0 {
                0
            } else {
                ((f64::from(v) * k).round() as u32).max(1)
            }
        };
        Presentation {
            padding: px(self.padding),
            border: px(self.border),
            radius: self.radius * k,
            shadow: self.shadow * k,
            canvas_size: self.canvas_size.map(|[w, h]| [px(w), px(h)]),
            output_size: self.output_size.map(|[w, h]| [px(w), px(h)]),
            ..self.clone()
        }
    }

    /// True when framing leaves the surface unchanged and its size is valid.
    fn is_identity(&self, width: u32, height: u32) -> Result<bool> {
        let out = self.dimensions(width, height)?;
        // The fitted canvas equals the image only when it is the image's own size.
        let unfitted = self.canvas_size.is_none_or(|c| c == [width, height]);
        Ok(self.is_plain() && unfitted && out == (width, height))
    }

    /// Keeps the rendered scene in Cairo until the final pixel conversion.
    pub fn apply_surface(&self, source: ImageSurface) -> Result<ImageSurface> {
        if self.is_identity(source.width() as u32, source.height() as u32)? {
            return Ok(source);
        }
        self.compose(&source)
    }

    /// Frames `source`; the result is `dimensions(source size)` pixels.
    pub fn compose(&self, source: &ImageSurface) -> Result<ImageSurface> {
        let (width, height) = (source.width() as u32, source.height() as u32);
        let lay = self.layout(width, height)?;
        let (out_w, out_h) = lay.out;
        let (canvas_w, canvas_h) = lay.decorated;
        let inset = f64::from(self.inset());
        let (w, h) = (f64::from(width), f64::from(height));
        let out = ImageSurface::create(Format::ARgb32, out_w.try_into()?, out_h.try_into()?)?;
        {
            let cr = Context::new(&out)?;
            let (bw, bh) = (f64::from(lay.base.0), f64::from(lay.base.1));
            cr.scale(f64::from(out_w) / bw, f64::from(out_h) / bh);
            match self.background {
                Background::Transparent => {}
                Background::Solid([r, g, b, a]) => {
                    cr.set_source_rgba(r, g, b, a);
                    cr.rectangle(0.0, 0.0, bw, bh);
                    cr.fill()?;
                }
                Background::Gradient(from, to) => {
                    let gradient = LinearGradient::new(0.0, 0.0, bw, bh);
                    gradient.add_color_stop_rgba(0.0, from[0], from[1], from[2], from[3]);
                    gradient.add_color_stop_rgba(1.0, to[0], to[1], to[2], to[3]);
                    cr.set_source(&gradient)?;
                    cr.rectangle(0.0, 0.0, bw, bh);
                    cr.fill()?;
                }
            }
            cr.translate(lay.offset.0, lay.offset.1);
            cr.scale(lay.fit, lay.fit);
            let blur = self.shadow_radius();
            if blur > 0 {
                // The shadow follows the frame's real alpha, so a transparent screenshot casts none.
                let mut plane =
                    ImageSurface::create(Format::A8, canvas_w.try_into()?, canvas_h.try_into()?)?;
                {
                    let plane_cr = Context::new(&plane)?;
                    self.draw_frame(&plane_cr, source, inset, w, h)?;
                }
                let stride = plane.stride() as usize;
                box_blur3(
                    &mut plane.data()?,
                    stride,
                    canvas_w as usize,
                    canvas_h as usize,
                    blur as usize,
                );
                plane.mark_dirty();
                cr.save()?;
                cr.translate(0.0, f64::from(blur));
                cr.set_source_rgba(0.0, 0.0, 0.0, SHADOW_ALPHA);
                cr.mask_surface(&plane, 0.0, 0.0)?;
                cr.restore()?;
            }
            self.draw_frame(&cr, source, inset, w, h)?;
        }
        Ok(out)
    }

    /// Maps scene pixels onto the `dimensions(width, height)` output exactly where `compose` puts
    /// the screenshot (padding, border, shadow, resize) and clips to its rounded corners, so vector
    /// marks drawn afterwards land on the pixels `compose` produced. Pair with `save`/`restore`.
    pub fn place_scene(&self, cr: &Context, width: u32, height: u32) -> Result<()> {
        let lay = self.layout(width, height)?;
        cr.scale(
            f64::from(lay.out.0) / f64::from(lay.base.0),
            f64::from(lay.out.1) / f64::from(lay.base.1),
        );
        cr.translate(lay.offset.0, lay.offset.1);
        cr.scale(lay.fit, lay.fit);
        let inset = f64::from(self.inset());
        cr.translate(inset, inset);
        let (w, h) = (f64::from(width), f64::from(height));
        let radius = self.radius.min(w.min(h) / 2.0);
        if radius > 0.0 {
            round_rect(cr, 0.0, 0.0, w, h, radius);
            cr.clip();
        }
        Ok(())
    }

    /// Border ring plus the (optionally rounded) screenshot, `inset` px from the canvas edge.
    fn draw_frame(
        &self,
        cr: &Context,
        source: &ImageSurface,
        inset: f64,
        w: f64,
        h: f64,
    ) -> Result<()> {
        let border = f64::from(self.border);
        let (x, y) = (inset, inset);
        let radius = self.radius.min(w.min(h) / 2.0);
        if self.border > 0 {
            let [r, g, b, a] = self.border_color;
            cr.save()?;
            cr.set_fill_rule(FillRule::EvenOdd);
            let outer = if radius > 0.0 { radius + border } else { 0.0 };
            round_rect(
                cr,
                x - border,
                y - border,
                w + 2.0 * border,
                h + 2.0 * border,
                outer,
            );
            round_rect(cr, x, y, w, h, radius);
            cr.set_source_rgba(r, g, b, a);
            cr.fill()?;
            cr.restore()?;
        }
        cr.save()?;
        round_rect(cr, x, y, w, h, radius);
        cr.clip();
        cr.translate(x, y);
        let pattern = SurfacePattern::create(source);
        pattern.set_extend(Extend::Pad);
        pattern.set_filter(Filter::Best);
        cr.set_source(&pattern)?;
        cr.paint()?;
        cr.restore()?;
        Ok(())
    }
}

fn check_size(w: u32, h: u32, what: &str) -> Result<()> {
    ensure!(
        (1..=MAX_DIM).contains(&w)
            && (1..=MAX_DIM).contains(&h)
            && u64::from(w) * u64::from(h) <= MAX_PIXELS,
        "{what} {w}x{h} is outside the supported range"
    );
    Ok(())
}

fn round_rect(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    if r <= 0.0 {
        cr.rectangle(x, y, w, h);
        return;
    }
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, PI + FRAC_PI_2);
    cr.close_path();
}

/// Three box passes of radius `r` per axis approximate a Gaussian; outside the plane is empty.
fn box_blur3(plane: &mut [u8], stride: usize, w: usize, h: usize, r: usize) {
    let mut tmp = vec![0u8; plane.len()];
    let window = (2 * r + 1) as u32;
    for _ in 0..3 {
        for y in 0..h {
            let (src, dst) = (&plane[y * stride..][..w], &mut tmp[y * stride..][..w]);
            let mut sum: u32 = src[..r.min(w)].iter().map(|&v| u32::from(v)).sum();
            for x in 0..w {
                if x + r < w {
                    sum += u32::from(src[x + r]);
                }
                if x > r {
                    sum -= u32::from(src[x - r - 1]);
                }
                dst[x] = ((sum + window / 2) / window) as u8;
            }
        }
        let mut column = vec![0u32; w];
        for y in 0..r.min(h) {
            for (c, &v) in column.iter_mut().zip(&tmp[y * stride..][..w]) {
                *c += u32::from(v);
            }
        }
        for y in 0..h {
            if y + r < h {
                for (c, &v) in column.iter_mut().zip(&tmp[(y + r) * stride..][..w]) {
                    *c += u32::from(v);
                }
            }
            if y > r {
                for (c, &v) in column.iter_mut().zip(&tmp[(y - r - 1) * stride..][..w]) {
                    *c -= u32::from(v);
                }
            }
            for (d, &c) in plane[y * stride..][..w].iter_mut().zip(&column) {
                *d = ((c + window / 2) / window) as u8;
            }
        }
    }
}

/// Premultiplied native-endian ARGB32 surface of `image`. Opaque pixels are copied exactly and
/// transparent ones are all-zero, both without arithmetic; partial alpha rounds to nearest.
pub(crate) fn surface_from(image: &RgbaImage) -> Result<ImageSurface> {
    let (width, height) = image.dimensions();
    let mut surface = ImageSurface::create(Format::ARgb32, width.try_into()?, height.try_into()?)?;
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data()?;
        for (y, row) in image.rows().enumerate() {
            let out = data[y * stride..][..width as usize * 4].chunks_exact_mut(4);
            for (pixel, out) in row.zip(out) {
                let [r, g, b, a] = pixel.0;
                let argb = match a {
                    255 => 0xFF00_0000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
                    0 => 0,
                    _ => {
                        let premul = |c: u8| (u32::from(c) * u32::from(a) + 127) / 255;
                        (u32::from(a) << 24) | (premul(r) << 16) | (premul(g) << 8) | premul(b)
                    }
                };
                out.copy_from_slice(&argb.to_ne_bytes());
            }
        }
    }
    surface.mark_dirty();
    Ok(surface)
}

/// Un-premultiplies a surface read in place. Opaque pixels are copied exactly, fully transparent
/// ones become `[0, 0, 0, 0]`.
pub(crate) fn rgba_from(surface: &ImageSurface) -> Result<RgbaImage> {
    let (width, height) = (surface.width() as u32, surface.height() as u32);
    let stride = surface.stride() as usize;
    let mut rgba = RgbaImage::new(width, height);
    surface.with_data(|data| {
        for (y, row) in rgba.rows_mut().enumerate() {
            let src = data[y * stride..][..width as usize * 4].chunks_exact(4);
            for (pixel, src) in row.zip(src) {
                let argb = u32::from_ne_bytes([src[0], src[1], src[2], src[3]]);
                let a = (argb >> 24) as u8;
                *pixel = match a {
                    255 => image::Rgba([(argb >> 16) as u8, (argb >> 8) as u8, argb as u8, 255]),
                    0 => image::Rgba([0; 4]),
                    _ => {
                        let unpremul = |c: u8| {
                            ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
                        };
                        image::Rgba([
                            unpremul((argb >> 16) as u8),
                            unpremul((argb >> 8) as u8),
                            unpremul(argb as u8),
                            a,
                        ])
                    }
                };
            }
        }
    })?;
    Ok(rgba)
}

#[cfg(test)]
fn framed(p: &Presentation, src: RgbaImage) -> RgbaImage {
    rgba_from(&p.apply_surface(surface_from(&src).unwrap()).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    const RED: [f64; 4] = [1.0, 0.0, 0.0, 1.0];
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);

    #[test]
    fn dimensions_include_padding_border_shadow_and_output_size() {
        let mut p = Presentation {
            padding: 10,
            border: 2,
            ..Default::default()
        };
        assert_eq!(p.dimensions(100, 50).unwrap(), (124, 74));
        p.shadow = 0.5; // blur 6 => 24 px reach each side
        assert_eq!(p.dimensions(100, 50).unwrap(), (172, 122));
        p.output_size = Some([60, 40]);
        assert_eq!(p.dimensions(100, 50).unwrap(), (60, 40));
        let out = framed(&p, RgbaImage::from_pixel(100, 50, BLUE));
        assert_eq!(out.dimensions(), (60, 40));
    }

    #[test]
    fn solid_edges_stay_exact_and_padding_shows_the_background() {
        let p = Presentation {
            padding: 2,
            background: Background::Solid(RED),
            ..Default::default()
        };
        let out = framed(&p, RgbaImage::from_pixel(4, 4, BLUE));
        assert_eq!(out.dimensions(), (8, 8));
        assert_eq!(*out.get_pixel(2, 2), BLUE);
        assert_eq!(*out.get_pixel(5, 5), BLUE);
        for (x, y) in [(0, 0), (1, 1), (6, 6), (7, 3)] {
            assert_eq!(*out.get_pixel(x, y), Rgba([255, 0, 0, 255]), "{x},{y}");
        }
    }

    #[test]
    fn transparent_source_keeps_alpha_over_transparent_and_solid_backgrounds() {
        let half = Rgba([10, 20, 30, 128]);
        let p = Presentation {
            padding: 1,
            ..Default::default()
        };
        let out = framed(&p, RgbaImage::from_pixel(2, 2, half));
        assert_eq!(out.get_pixel(0, 0).0[3], 0, "padding stays transparent");
        let kept = out.get_pixel(1, 1).0;
        assert_eq!(kept[3], 128);
        assert!(
            kept[..3]
                .iter()
                .zip([10u8, 20, 30])
                .all(|(a, b)| a.abs_diff(b) <= 2)
        );

        let p = Presentation {
            padding: 1,
            background: Background::Solid(RED),
            ..Default::default()
        };
        let clear = framed(&p, RgbaImage::new(2, 2));
        assert!(
            clear.pixels().all(|px| *px == Rgba([255, 0, 0, 255])),
            "clear source shows background"
        );
        let mixed = framed(&p, RgbaImage::from_pixel(2, 2, half))
            .get_pixel(1, 1)
            .0;
        assert_eq!(mixed[3], 255);
        assert!(
            mixed[0] > 100 && mixed[0] < 200,
            "background blends under half alpha"
        );
    }

    #[test]
    fn rounded_corners_cut_the_screenshot_only() {
        let p = Presentation {
            radius: 8.0,
            ..Default::default()
        };
        let out = framed(&p, RgbaImage::from_pixel(20, 20, BLUE));
        assert_eq!(out.dimensions(), (20, 20));
        assert_eq!(out.get_pixel(0, 0).0[3], 0);
        assert_eq!(*out.get_pixel(10, 10), BLUE);
        assert_eq!(
            *out.get_pixel(10, 0),
            BLUE,
            "straight edge keeps full coverage"
        );
    }

    #[test]
    fn border_surrounds_the_frame() {
        let p = Presentation {
            border: 3,
            border_color: RED,
            ..Default::default()
        };
        let out = framed(&p, RgbaImage::from_pixel(4, 4, BLUE));
        assert_eq!(out.dimensions(), (10, 10));
        assert_eq!(*out.get_pixel(0, 0), Rgba([255, 0, 0, 255]));
        assert_eq!(*out.get_pixel(3, 3), BLUE);
    }

    #[test]
    fn shadow_follows_source_alpha_and_stays_inside_the_canvas() {
        let p = Presentation {
            shadow: 1.0,
            ..Default::default()
        };
        let solid = framed(&p, RgbaImage::from_pixel(10, 10, BLUE));
        assert_eq!(solid.dimensions(), (106, 106));
        assert_eq!(solid.get_pixel(0, 0).0[3], 0, "far corner is clear");
        let below = solid.get_pixel(53, 48 + 10 + 3).0;
        assert!(
            below[3] > 0 && below[3] < 255,
            "shadow below the frame: {below:?}"
        );
        assert_eq!(*solid.get_pixel(53, 53), BLUE);

        let clear = framed(&p, RgbaImage::new(10, 10));
        assert!(
            clear.pixels().all(|px| px.0[3] == 0),
            "transparent source casts no shadow"
        );
    }

    #[test]
    fn gradient_blends_between_its_corners() {
        let p = Presentation {
            padding: 4,
            background: Background::Gradient([0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0]),
            ..Default::default()
        };
        let out = framed(&p, RgbaImage::new(2, 2));
        assert!(out.get_pixel(0, 0).0[0] < out.get_pixel(9, 9).0[0]);
    }

    #[test]
    fn invalid_settings_are_rejected_before_any_allocation() {
        let bad = |p: Presentation| assert!(p.validate().is_err() && p.dimensions(10, 10).is_err());
        bad(Presentation {
            radius: f64::NAN,
            ..Default::default()
        });
        bad(Presentation {
            shadow: 1.5,
            ..Default::default()
        });
        bad(Presentation {
            padding: MAX_PADDING + 1,
            ..Default::default()
        });
        bad(Presentation {
            border: MAX_BORDER + 1,
            ..Default::default()
        });
        bad(Presentation {
            border_color: [0.0, 0.0, 2.0, 1.0],
            ..Default::default()
        });
        bad(Presentation {
            background: Background::Solid([f64::NAN; 4]),
            ..Default::default()
        });
        bad(Presentation {
            output_size: Some([0, 5]),
            ..Default::default()
        });
        bad(Presentation {
            output_size: Some([MAX_DIM, MAX_DIM]),
            ..Default::default()
        });
        bad(Presentation {
            canvas_size: Some([5, 0]),
            ..Default::default()
        });
        bad(Presentation {
            canvas_size: Some([MAX_DIM, MAX_DIM]),
            ..Default::default()
        });
        let pad = Presentation {
            padding: MAX_PADDING,
            ..Default::default()
        };
        assert!(
            pad.dimensions(MAX_DIM - 100, 10).is_err(),
            "canvas wider than the limit"
        );
        assert!(pad.dimensions(0, 10).is_err());
        assert!(pad.dimensions(1, 1).is_ok());
    }

    #[test]
    fn serde_defaults_fill_missing_fields() {
        let p: Presentation = serde_json::from_str(r#"{"padding":5}"#).unwrap();
        assert_eq!(
            p,
            Presentation {
                padding: 5,
                ..Default::default()
            }
        );
        let q = Presentation {
            background: Background::Gradient(RED, [0.0, 0.0, 1.0, 1.0]),
            output_size: Some([3, 4]),
            ..p
        };
        assert_eq!(
            serde_json::from_str::<Presentation>(&serde_json::to_string(&q).unwrap()).unwrap(),
            q
        );
    }
}

#[cfg(test)]
mod canvas_tests {
    use super::*;
    use image::Rgba;

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);

    fn on_red(canvas: [u32; 2], padding: u32) -> Presentation {
        Presentation {
            padding,
            background: Background::Solid([1.0, 0.0, 0.0, 1.0]),
            canvas_size: Some(canvas),
            ..Default::default()
        }
    }

    #[test]
    fn canvas_fits_uniformly_centres_and_fills_the_background() {
        // Wide screenshot in a square canvas: scaled 2.5x to 100x50, bars above and below.
        let wide = framed(&on_red([100, 100], 0), RgbaImage::from_pixel(40, 20, BLUE));
        assert_eq!(wide.dimensions(), (100, 100));
        assert_eq!(*wide.get_pixel(50, 50), BLUE);
        assert_eq!(*wide.get_pixel(0, 50), BLUE, "full width, nothing cropped");
        assert_eq!(*wide.get_pixel(99, 26), BLUE);
        assert_eq!(*wide.get_pixel(50, 24), RED);
        assert_eq!(*wide.get_pixel(50, 75), RED);
        assert_eq!(*wide.get_pixel(0, 0), RED);

        // Tall screenshot in a wide canvas, with padding: the frame (padding included) is 40x40,
        // fitted at 2.5x to 100x100 and centred; the screenshot sits inside it.
        let tall = framed(&on_red([200, 100], 10), RgbaImage::from_pixel(20, 20, BLUE));
        assert_eq!(tall.dimensions(), (200, 100));
        assert_eq!(*tall.get_pixel(100, 50), BLUE);
        assert_eq!(*tall.get_pixel(76, 26), BLUE);
        assert_eq!(*tall.get_pixel(73, 50), RED, "padding of the fitted frame");
        assert_eq!(*tall.get_pixel(127, 50), RED);
        assert_eq!(
            *tall.get_pixel(5, 50),
            RED,
            "background fills the whole canvas"
        );
        assert_eq!(*tall.get_pixel(195, 95), RED);
    }

    #[test]
    fn smaller_canvas_shrinks_instead_of_cropping_and_keeps_transparent_bars() {
        let p = Presentation {
            canvas_size: Some([20, 40]),
            ..Default::default()
        };
        let out = framed(&p, RgbaImage::from_pixel(40, 20, BLUE));
        assert_eq!(out.dimensions(), (20, 40));
        assert_eq!(*out.get_pixel(0, 20), BLUE);
        assert_eq!(*out.get_pixel(19, 20), BLUE);
        assert_eq!(out.get_pixel(10, 4).0[3], 0, "bars stay transparent");
        assert_eq!(out.get_pixel(10, 36).0[3], 0);
    }

    #[test]
    fn output_size_resizes_the_whole_canvas_and_scaled_previews_keep_the_fit() {
        let mut p = on_red([100, 100], 0);
        p.output_size = Some([50, 50]);
        let out = framed(&p, RgbaImage::from_pixel(40, 20, BLUE));
        assert_eq!(out.dimensions(), (50, 50));
        assert_eq!(*out.get_pixel(25, 25), BLUE);
        assert_eq!(*out.get_pixel(25, 5), RED);
        assert_eq!(*out.get_pixel(25, 45), RED);

        let preview = p.scaled(0.5);
        assert_eq!(preview.dimensions(20, 10).unwrap(), (25, 25));
        let small = framed(&preview, RgbaImage::from_pixel(20, 10, BLUE));
        assert_eq!(*small.get_pixel(12, 12), BLUE);
        assert_eq!(*small.get_pixel(12, 2), RED);
    }

    #[test]
    fn a_canvas_the_size_of_the_image_is_a_no_op_but_a_different_one_is_not() {
        let img = RgbaImage::from_pixel(8, 4, BLUE);
        let same = Presentation {
            canvas_size: Some([8, 4]),
            ..Default::default()
        };
        assert_eq!(framed(&same, img.clone()), img);
        // Same output size as the image, yet the fitted canvas squeezes it: not a no-op.
        let squeezed = Presentation {
            canvas_size: Some([8, 8]),
            output_size: Some([8, 4]),
            ..Default::default()
        };
        let out = framed(&squeezed, RgbaImage::from_pixel(8, 4, BLUE));
        assert_eq!(out.dimensions(), (8, 4));
        assert_eq!(*out.get_pixel(4, 2), BLUE);
        assert_eq!(out.get_pixel(4, 0).0[3], 0, "letterbox rows are clear");
    }

    #[test]
    fn shadow_and_border_stay_whole_inside_a_tight_canvas() {
        let p = Presentation {
            shadow: 1.0,
            border: 2,
            border_color: [1.0, 0.0, 0.0, 1.0],
            canvas_size: Some([53, 300]),
            ..Default::default()
        };
        // Decorated frame is 110x110; the width limits the fit to 53/110.
        let out = framed(&p, RgbaImage::from_pixel(10, 10, BLUE));
        assert_eq!(out.dimensions(), (53, 300));
        assert_eq!(*out.get_pixel(26, 150), BLUE);
        assert_eq!(out.get_pixel(0, 5).0[3], 0, "far letterbox is clear");
        assert!(
            out.get_pixel(26, 150 + 12).0[3] > 0,
            "shadow below the frame survives the fit"
        );
    }

    fn premul(c: u8, a: u8) -> u8 {
        ((u32::from(c) * u32::from(a) + 127) / 255) as u8
    }

    fn unpremul(c: u8, a: u8) -> u8 {
        if a == 0 {
            0
        } else {
            ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
        }
    }

    #[test]
    fn converters_keep_the_reference_rounding_for_every_alpha() {
        // Row `y` is alpha `y`, so 0 and 255 (the fast paths) sit beside every partial alpha.
        let img = RgbaImage::from_fn(256, 256, |x, y| {
            Rgba([x as u8, 255 - x as u8, (x * 7) as u8, y as u8])
        });
        let surface = surface_from(&img).unwrap();
        let stride = surface.stride() as usize;
        surface
            .with_data(|data| {
                for (x, y, p) in img.enumerate_pixels() {
                    let [r, g, b, a] = p.0;
                    let at = y as usize * stride + x as usize * 4;
                    let got = u32::from_ne_bytes(data[at..at + 4].try_into().unwrap());
                    let want = (u32::from(a) << 24)
                        | (u32::from(premul(r, a)) << 16)
                        | (u32::from(premul(g, a)) << 8)
                        | u32::from(premul(b, a));
                    assert_eq!(got, want, "premultiplied ({x}, {y})");
                }
            })
            .unwrap();
        let back = rgba_from(&surface).unwrap();
        for (x, y, p) in img.enumerate_pixels() {
            let [r, g, b, a] = p.0;
            let want = Rgba([
                unpremul(premul(r, a), a),
                unpremul(premul(g, a), a),
                unpremul(premul(b, a), a),
                a,
            ]);
            assert_eq!(*back.get_pixel(x, y), want, "un-premultiplied ({x}, {y})");
        }
        assert!(
            (0..256).all(|x| *back.get_pixel(x, 255) == *img.get_pixel(x, 255)),
            "opaque pixels survive exactly"
        );
        assert!(
            (0..256).all(|x| *back.get_pixel(x, 0) == Rgba([0; 4])),
            "transparent pixels are all zero"
        );
    }
}
