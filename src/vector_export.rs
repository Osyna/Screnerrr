//! Native SVG and PDF export through Cairo's vector surfaces.
//!
//! The page is the finished PNG scene: the screenshot with every privacy mask burned in by the
//! live renderer, framed by the presentation, then, only where that is provably safe, the
//! annotations on top as real vector paths and text.
//!
//! * The embedded raster is `Document::scene_surface` of a scene without the vector marks, so it
//!   never holds a pixel a mask covers or the crop removes, and cairo has nothing else to embed.
//! * A mark is emitted as vector only when its padded bounds lie wholly inside the crop and miss
//!   every privacy mask. A mark that is hidden, clipped or masked never becomes a path or a glyph
//!   run in the file; it stays in the raster, where the mask paints over it.
//! * Z-order: the vector marks are the longest suffix of the stack made only of such safe marks,
//!   so everything below an unsafe mark (magnifier, masked, cropped) stays raster and nothing is
//!   drawn twice. A safe mark never overlaps a mask, so masks on top of it in the raster are moot.
//! * Limit: spotlights dim the whole scene, which a vector page cannot express beneath later
//!   marks, so any spotlight rasterises the entire scene. Magnifiers always stay raster.
//! * Wrapped text: Pango lays text out in the vector surface itself. `document::text_layout` pins
//!   hint metrics off, so advances (and so line breaks) do not depend on the surface or scale.
//!
//! No `.sharp` bytes or metadata are written: only the page itself.

use crate::document::{
    Bounds, Crop, DEFAULT_FAMILY, DEFAULT_FONT_STYLE, Document, Mark, MarkKind, PrivacySnapshot,
};
use crate::presentation::Presentation;
use crate::project::Snapshot;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use gtk::cairo::{Context, Filter, ImageSurface, PdfSurface, Surface, SurfacePattern, SvgSurface};
use image::RgbaImage;
use std::{collections::HashMap, fs::File, io::Write, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Svg,
    Pdf,
}

/// PDF points per output pixel at 96 dpi; SVG uses CSS pixels directly.
const PT: f64 = 0.75;
/// Largest page side the PDF specification (1.7, annex C) lets conforming readers handle.
const PDF_MAX_PT: f64 = 14_400.0;

/// Writes the export to a new file, consuming the snapshot so its screenshot is moved, not copied.
/// Never overwrites: an existing path is an error, nothing is visible at `path` until the file is
/// complete, and a failure leaves no partial file.
pub fn save_new(snapshot: Snapshot, path: &Path, format: Format) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; choose a new filename", path.display());
    }
    write_new(path, &render(snapshot, format)?)
}

fn render(snapshot: Snapshot, format: Format) -> Result<Vec<u8>> {
    let page = Page::build(snapshot)?;
    let scale = if format == Format::Pdf { PT } else { 1. };
    let (w, h) = (
        f64::from(page.framed.width()) * scale,
        f64::from(page.framed.height()) * scale,
    );
    if format == Format::Pdf {
        ensure!(
            w <= PDF_MAX_PT && h <= PDF_MAX_PT,
            "{w:.0} × {h:.0} pt exceeds the PDF page limit of {PDF_MAX_PT} pt; use a smaller output size"
        );
    }
    let surface: Surface = match format {
        Format::Pdf => (*PdfSurface::for_stream(w, h, Vec::<u8>::new())?).clone(),
        Format::Svg => {
            let mut svg = SvgSurface::for_stream(w, h, Vec::<u8>::new())?;
            svg.set_document_unit(gtk::cairo::SvgUnit::Px);
            (*svg).clone()
        }
    };
    {
        let cr = Context::new(&surface)?;
        cr.scale(scale, scale);
        page.draw(&cr)?;
    }
    let stream = surface
        .finish_output_stream()
        .map_err(std::io::Error::from)?;
    surface.status()?;
    let bytes = stream
        .downcast::<Vec<u8>>()
        .map_err(|_| anyhow!("unexpected export stream type"))?;
    Ok(*bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .context("Cannot create file in destination folder")?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|e| e.error)
        .with_context(|| format!("Cannot save {}", path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(PartialEq)]
enum Class {
    /// Wholly visible, unmasked annotation that can leave the raster.
    Vector,
    /// Magnifier, mask, spotlight, or a mark clipped or covered: only ever pixels.
    Raster,
    /// Beyond the crop: not visible at all, so neither raster nor vector shows it.
    Hidden,
}

fn classify(scene: &Document, privacy: &PrivacySnapshot, m: &Mark, crop: Crop) -> Class {
    if !matches!(
        m.kind,
        MarkKind::Path
            | MarkKind::Text { .. }
            | MarkKind::Connector { .. }
            | MarkKind::Step { .. }
            | MarkKind::Highlight
            | MarkKind::Highlighter
    ) {
        return Class::Raster;
    }
    let Some(b) = scene.mark_bounds(m.id) else {
        return Class::Raster;
    };
    // Generous slack for stroke caps, glyph overhang and Pango differing between surfaces; being
    // wrong here only turns a vector mark into raster, never the other way. A mitred corner
    // reaches a whole line width, and an installed font may overhang its logical box by more than
    // the default sans does, so both get their own room.
    let scale = m.transform.det().abs().sqrt().max(1.0);
    let stroke = if m.round_caps { m.width } else { 2.0 * m.width };
    let glyphs = match m.kind {
        MarkKind::Text { .. }
            if m.font_family == DEFAULT_FAMILY && m.font_style == DEFAULT_FONT_STYLE =>
        {
            m.font_size / 2.0
        }
        MarkKind::Text { .. } => m.font_size,
        _ => 0.0,
    };
    let pad = (2.0 + (stroke + glyphs) * scale) as f32;
    let e = Bounds {
        x: b.x - pad,
        y: b.y - pad,
        width: b.width + 2.0 * pad,
        height: b.height + 2.0 * pad,
    };
    let (x0, y0) = (crop.x as f32, crop.y as f32);
    let (x1, y1) = (x0 + crop.width as f32, y0 + crop.height as f32);
    let (right, bottom) = (e.x + e.width, e.y + e.height);
    if e.x >= x1 || right <= x0 || e.y >= y1 || bottom <= y0 {
        Class::Hidden
    } else if e.x >= x0 && right <= x1 && e.y >= y0 && bottom <= y1 && !privacy.is_private(e) {
        Class::Vector
    } else {
        Class::Raster
    }
}

fn anchors(m: &Mark) -> impl Iterator<Item = u64> {
    match &m.kind {
        MarkKind::Connector { start, end } => [start.anchor, end.anchor],
        _ => [None, None],
    }
    .into_iter()
    .flatten()
    .map(|a| a.mark)
}

/// The marks (copied, groups dropped since only part of a group may stay) as a document that
/// knows geometry and masks but holds no screenshot, plus the ascending indices of the marks to
/// draw as vectors.
fn plan(marks: &[Mark], crop: Crop) -> Result<(Document, Vec<usize>)> {
    let marks = marks
        .iter()
        .cloned()
        .map(|mut m| {
            m.group = None;
            m
        })
        .collect();
    let scene = Document::from_parts(RgbaImage::new(1, 1), marks, None)?;
    let n = scene.marks.len();
    let privacy = scene.privacy_snapshot();
    let class: Vec<Class> = scene
        .marks
        .iter()
        .map(|m| classify(&scene, &privacy, m, crop))
        .collect();
    let spotlight = scene
        .marks
        .iter()
        .any(|m| matches!(m.kind, MarkKind::Spotlight { .. }));
    // Masks float above everything whatever their index, so only other raster marks fix the order.
    let mut start = if spotlight {
        n
    } else {
        scene
            .marks
            .iter()
            .zip(&class)
            .rposition(|(m, c)| *c == Class::Raster && !m.is_redaction())
            .map_or(0, |i| i + 1)
    };
    let index: HashMap<u64, usize> = scene
        .marks
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id, i))
        .collect();
    // A raster connector attached to a vector mark would lose its target: the target goes raster,
    // and with it everything below it.
    loop {
        let in_vector = |i: usize| i >= start && class[i] == Class::Vector;
        let moved = scene
            .marks
            .iter()
            .enumerate()
            .filter(|&(i, _)| !in_vector(i))
            .flat_map(|(_, m)| anchors(m))
            .filter_map(|id| index.get(&id).copied())
            .filter(|&t| in_vector(t))
            .max();
        match moved {
            Some(t) => start = t + 1,
            None => break,
        }
    }
    let vector = (start..n).filter(|&i| class[i] == Class::Vector).collect();
    Ok((scene, vector))
}

struct Page {
    scene: Document,
    vector: Vec<usize>,
    /// Raster of everything that is not vector, framed by the presentation at output size.
    framed: ImageSurface,
    presentation: Presentation,
    crop: Crop,
}

impl Page {
    /// Consumes the snapshot: its screenshot moves into the raster document, and only the small
    /// mark metadata is copied for the geometry-only scene.
    fn build(snapshot: Snapshot) -> Result<Self> {
        let Snapshot {
            image,
            marks,
            crop: snapshot_crop,
            presentation,
        } = snapshot;
        let (width, height) = image.dimensions();
        let crop = snapshot_crop.unwrap_or(Crop {
            x: 0,
            y: 0,
            width,
            height,
        });
        let (scene, vector) = plan(&marks, crop)?;
        let rest: Vec<Mark> = marks
            .into_iter()
            .enumerate()
            .filter(|(i, _)| vector.binary_search(i).is_err())
            .map(|(_, mut m)| {
                m.group = None;
                m
            })
            .collect();
        // The live renderer decides every pixel: masks, lenses and spotlights included.
        let base = Document::from_parts(image, rest, snapshot_crop)?.scene_surface()?;
        let framed = presentation.compose(&base)?;
        Ok(Self {
            scene,
            vector,
            framed,
            presentation,
            crop,
        })
    }

    /// Draws in output pixels: the raster page, then the vector marks over it.
    fn draw(&self, cr: &Context) -> Result<()> {
        let raster = SurfacePattern::create(&self.framed);
        raster.set_filter(Filter::Nearest);
        cr.set_source(&raster)?;
        cr.paint()?;
        if !self.vector.is_empty() {
            cr.save()?;
            self.presentation
                .place_scene(cr, self.crop.width, self.crop.height)?;
            cr.translate(-f64::from(self.crop.x), -f64::from(self.crop.y));
            for &i in &self.vector {
                self.scene.paint_mark(cr, &self.scene.marks[i])?;
            }
            cr.restore()?;
        }
        cr.status()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Anchor, Endpoint};
    use crate::presentation::{Background, rgba_from};
    use crate::shapes::Point;
    use std::io::Read;

    const RED: [f64; 4] = [1., 0., 0., 1.];

    fn pt(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    /// Every pixel a distinct colour (`b` fixed), so a decoded colour names its source pixel.
    fn tagged() -> RgbaImage {
        RgbaImage::from_fn(64, 48, |x, y| image::Rgba([x as u8, y as u8, 200, 255]))
    }

    fn snap(marks: Vec<Mark>, crop: Option<Crop>, presentation: Presentation) -> Snapshot {
        Snapshot {
            image: tagged(),
            marks,
            crop,
            presentation,
        }
    }

    fn plain(marks: Vec<Mark>) -> Snapshot {
        snap(marks, None, Presentation::default())
    }

    #[test]
    fn svg_intrinsic_dimensions_match_the_resized_png_in_css_pixels() {
        let mut snapshot = plain(vec![]);
        snapshot.presentation.output_size = Some([123, 91]);
        let bytes = render(snapshot, Format::Svg).unwrap();
        let svg = String::from_utf8(bytes).unwrap();
        let root = svg
            .split("<svg ")
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        let attribute = |name: &str| {
            root.split(&format!("{name}=\""))
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
        };
        assert_eq!(
            attribute("width")
                .trim_end_matches("px")
                .parse::<f64>()
                .unwrap(),
            123.
        );
        assert_eq!(
            attribute("height")
                .trim_end_matches("px")
                .parse::<f64>()
                .unwrap(),
            91.
        );
        let viewbox: Vec<f64> = attribute("viewBox")
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(viewbox, vec![0., 0., 123., 91.]);
    }

    fn line(id: u64, y: f32) -> Mark {
        Mark::path(id, vec![pt(10., y), pt(30., y)], false, RED, 3.0)
    }

    fn small_text(id: u64, at: Point, text: &str) -> Mark {
        let mut m = Mark::text(id, at, text, RED, 1.0);
        m.font_size = 8.0;
        m
    }

    fn vector_ids(s: &Snapshot) -> Vec<u64> {
        let crop = Crop {
            x: 0,
            y: 0,
            width: 64,
            height: 48,
        };
        let (scene, vector) = plan(&s.marks, s.crop.unwrap_or(crop)).unwrap();
        vector.iter().map(|&i| scene.marks[i].id).collect()
    }

    fn lens(id: u64) -> Mark {
        let source = Bounds {
            x: 40.,
            y: 30.,
            width: 8.,
            height: 8.,
        };
        Mark::magnifier(id, source, 2.0, pt(30., 20.), pt(50., 40.), RED, 2.0)
    }

    // --- planning: z-order and what may leave the raster -------------------------------------

    #[test]
    fn vector_marks_are_the_safe_suffix_above_every_unsafe_mark() {
        let mut marks = vec![
            line(1, 10.),
            lens(2),
            line(3, 20.),
            Mark::redaction(4, pt(44., 8.), pt(60., 40.)),
            // Reaches the mask and the crop edge: raster, so the line below it stays raster too.
            small_text(5, pt(40., 6.), "gone"),
            line(6, 40.),
        ];
        assert_eq!(vector_ids(&plain(marks.clone())), [6]);
        marks.remove(4);
        assert_eq!(vector_ids(&plain(marks.clone())), [3, 6]);
        marks.remove(1);
        assert_eq!(vector_ids(&plain(marks)), [1, 3, 6]);
    }

    #[test]
    fn a_spotlight_rasterises_the_whole_scene() {
        let marks = vec![line(1, 10.), Mark::spotlight(2, pt(5., 5.), pt(40., 40.))];
        assert!(vector_ids(&plain(marks)).is_empty());
    }

    #[test]
    fn marks_beyond_the_crop_neither_force_raster_nor_become_vector() {
        let crop = Crop {
            x: 20,
            y: 10,
            width: 40,
            height: 30,
        };
        let marks = vec![small_text(1, pt(1., 1.), "o"), line(2, 25.)];
        // Line spans x 10..30, so it is clipped by the crop: raster. Moved right it is whole.
        assert!(vector_ids(&snap(marks.clone(), Some(crop), Presentation::default())).is_empty());
        let mut marks = marks;
        marks[1].points = vec![pt(30., 25.), pt(50., 25.)];
        assert_eq!(
            vector_ids(&snap(marks, Some(crop), Presentation::default())),
            [2]
        );
    }

    #[test]
    fn a_raster_connector_pulls_its_target_out_of_the_vector_suffix() {
        let anchored = Endpoint {
            point: pt(20., 30.),
            anchor: Some(Anchor {
                mark: 2,
                u: 0.5,
                v: 0.5,
            }),
        };
        let free = |x: f32| Endpoint {
            point: pt(x, 24.),
            anchor: None,
        };
        let connector = |x: f32| Mark::connector(1, free(x), anchored, RED, 2.0);
        // Whole and visible: connector and target are both vector.
        assert_eq!(
            vector_ids(&plain(vec![connector(24.), line(2, 30.)])),
            [1, 2]
        );
        // One end beyond the crop makes the connector raster, which cannot attach to a vector mark.
        assert!(vector_ids(&plain(vec![connector(-30.), line(2, 30.)])).is_empty());
    }

    // --- output files: assets, privacy, structure --------------------------------------------

    struct Raster {
        w: u32,
        h: u32,
        rgb: Vec<[u8; 3]>,
    }

    impl Raster {
        fn at(&self, x: u32, y: u32) -> [u8; 3] {
            self.rgb[(y * self.w + x) as usize]
        }
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    fn int_after(dict: &str, key: &str) -> Option<u32> {
        let rest = &dict[dict.find(key)? + key.len()..];
        rest.split_whitespace().next()?.parse().ok()
    }

    struct Obj {
        num: u32,
        dict: String,
        data: Option<Vec<u8>>,
    }

    /// Indirect objects of a cairo PDF with stream data inflated, including the objects packed
    /// into `/Type /ObjStm` streams (Cairo 1.18 writes page, font and resource dictionaries
    /// there). Cairo writes the stream keyword on its own line and may give `/Length` as an
    /// indirect reference, so streams end at `endstream`; stream bytes are never scanned for
    /// objects or references.
    fn pdf_objects(pdf: &[u8]) -> Vec<Obj> {
        let mut out = vec![];
        let mut at = 0;
        while let Some(p) = find(&pdf[at..], b" 0 obj") {
            let marker = at + p;
            let first = pdf[..marker]
                .iter()
                .rposition(|b| !b.is_ascii_digit())
                .map_or(0, |i| i + 1);
            let num = std::str::from_utf8(&pdf[first..marker])
                .unwrap()
                .parse()
                .unwrap();
            let body = &pdf[marker + 6..];
            let end = find(body, b"endobj").expect("object without endobj");
            match find(body, b"stream\n").filter(|&s| s < end) {
                Some(s) => {
                    let data = &body[s + 7..];
                    let len = find(data, b"\nendstream").expect("stream without endstream");
                    let dict = String::from_utf8_lossy(&body[..s]).into_owned();
                    let raw = &data[..len];
                    let data = if dict.contains("/FlateDecode") {
                        let mut v = vec![];
                        flate2::read::ZlibDecoder::new(raw)
                            .read_to_end(&mut v)
                            .expect("inflate");
                        v
                    } else {
                        raw.to_vec()
                    };
                    if dict.contains("/ObjStm") {
                        let n = int_after(&dict, "/N ").expect("/N") as usize;
                        let first = int_after(&dict, "/First ").expect("/First") as usize;
                        let text = String::from_utf8_lossy(&data[..first.min(data.len())]);
                        let head: Vec<usize> = text
                            .split_whitespace()
                            .map(|t| t.parse().expect("object stream header"))
                            .collect();
                        assert_eq!(head.len(), 2 * n, "object stream header");
                        for i in 0..n {
                            let from = first + head[2 * i + 1];
                            let to = if i + 1 < n {
                                first + head[2 * i + 3]
                            } else {
                                data.len()
                            };
                            out.push(Obj {
                                num: head[2 * i] as u32,
                                dict: String::from_utf8_lossy(&data[from..to]).into_owned(),
                                data: None,
                            });
                        }
                    }
                    out.push(Obj {
                        num,
                        dict,
                        data: Some(data),
                    });
                    at = marker + 6 + s + 7 + len;
                }
                None => {
                    out.push(Obj {
                        num,
                        dict: String::from_utf8_lossy(&body[..end]).into_owned(),
                        data: None,
                    });
                    at = marker + 6 + end;
                }
            }
        }
        out
    }

    fn pdf_images(objs: &[Obj]) -> Vec<Raster> {
        // Soft masks are alpha, not screenshot colour.
        let masks: Vec<u32> = objs
            .iter()
            .filter_map(|o| int_after(&o.dict, "/SMask"))
            .collect();
        objs.iter()
            .filter(|o| o.dict.contains("/Subtype /Image") && !masks.contains(&o.num))
            .filter_map(|o| {
                let (w, h) = (
                    int_after(&o.dict, "/Width")?,
                    int_after(&o.dict, "/Height")?,
                );
                let data = o.data.as_ref()?;
                let rgb = if o.dict.contains("/DeviceRGB") {
                    data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect()
                } else if o.dict.contains("/DeviceGray") {
                    data.iter().map(|&v| [v, v, v]).collect::<Vec<_>>()
                } else {
                    return None;
                };
                assert_eq!(
                    rgb.len(),
                    (w * h) as usize,
                    "image {} has wrong size",
                    o.num
                );
                Some(Raster { w, h, rgb })
            })
            .collect()
    }

    fn b64(s: &str) -> Vec<u8> {
        let (mut out, mut acc, mut bits) = (vec![], 0u32, 0);
        for c in s.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => break,
                _ => continue,
            };
            acc = (acc << 6) | u32::from(v);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        out
    }

    fn svg_images(svg: &str) -> Vec<Raster> {
        let mut out = vec![];
        let mut rest = svg;
        while let Some(i) = rest.find("base64,") {
            let tail = &rest[i + 7..];
            let end = tail.find(['"', '\'']).expect("unterminated data URI");
            let png = b64(&tail[..end]);
            let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                .expect("embedded asset is a PNG")
                .to_rgba8();
            out.push(Raster {
                w: img.width(),
                h: img.height(),
                rgb: img.pixels().map(|p| [p[0], p[1], p[2]]).collect(),
            });
            rest = &tail[end..];
        }
        out
    }

    /// What a viewer could extract: decoded embedded images, vector paths, glyph runs.
    struct Inspected {
        images: Vec<Raster>,
        paths: usize,
        glyphs: usize,
    }

    fn export(s: &Snapshot, format: Format) -> (Vec<u8>, Inspected) {
        let bytes = render(s.clone(), format).unwrap();
        let inspected = match format {
            Format::Pdf => {
                let objs = pdf_objects(&bytes);
                // Every reference in any object dictionary resolves: the file is closed. Stream
                // bytes are binary and are not searched.
                for o in &objs {
                    let mut seen = o.dict.as_str();
                    while let Some(i) = seen.find(" 0 R") {
                        let digits: String = seen[..i]
                            .chars()
                            .rev()
                            .take_while(char::is_ascii_digit)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect();
                        if let Ok(n) = digits.parse::<u32>() {
                            assert!(
                                objs.iter().any(|t| t.num == n),
                                "object {}: dangling reference {n} 0 R in {:?}",
                                o.num,
                                o.dict
                            );
                        }
                        seen = &seen[i + 4..];
                    }
                }
                let ops = |names: &[&str]| -> usize {
                    objs.iter()
                        .filter(|o| {
                            !o.dict.contains("/Subtype")
                                && !o.dict.contains("/Length1")
                                && !o.dict.contains("/ObjStm")
                                && !o.dict.contains("/XRef")
                        })
                        .filter_map(|o| o.data.as_ref())
                        .flat_map(|d| {
                            String::from_utf8_lossy(d)
                                .split_whitespace()
                                .filter(|t| names.contains(t))
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .count()
                };
                Inspected {
                    images: pdf_images(&objs),
                    paths: ops(&["m", "l", "c", "S", "f", "f*", "re"]),
                    glyphs: objs
                        .iter()
                        .filter(|o| o.dict.contains("/Type /Font"))
                        .count()
                        + ops(&["BT", "Tj", "TJ"]),
                }
            }
            Format::Svg => {
                let svg = std::str::from_utf8(&bytes).expect("SVG is UTF-8");
                assert!(svg.trim_end().ends_with("</svg>"));
                Inspected {
                    images: svg_images(svg),
                    paths: svg.matches("<path").count(),
                    glyphs: svg.matches("glyph").count(),
                }
            }
        };
        (bytes, inspected)
    }

    const BOTH: [Format; 2] = [Format::Pdf, Format::Svg];

    fn covers(x: u32, y: u32) -> bool {
        (4..40).contains(&x) && (4..16).contains(&y)
    }

    #[test]
    fn masked_annotations_leave_no_pixels_paths_or_text_in_the_file() {
        let mask = || Mark::redaction(1, pt(4., 4.), pt(40., 16.));
        let hidden = vec![
            mask(),
            small_text(2, pt(5., 5.), "SECRET"),
            Mark::path(3, vec![pt(10., 8.), pt(30., 14.)], false, RED, 2.0),
        ];
        let baseline = plain(vec![mask()]);
        let control = plain(vec![
            mask(),
            small_text(2, pt(10., 26.), "VISIBLE"),
            Mark::path(3, vec![pt(10., 34.), pt(30., 42.)], false, RED, 2.0),
        ]);
        for format in BOTH {
            let (_, base) = export(&baseline, format);
            let (_, hid) = export(&plain(hidden.clone()), format);
            let (_, vis) = export(&control, format);
            assert_eq!(
                (hid.paths, hid.glyphs),
                (base.paths, base.glyphs),
                "{format:?}"
            );
            assert!(
                vis.paths > base.paths && vis.glyphs > base.glyphs,
                "{format:?}: the control must show vector marks, or the test proves nothing"
            );
            let shot = |i: &Inspected| {
                let r = i.images.iter().find(|r| (r.w, r.h) == (64, 48));
                r.map(|r| r.rgb.clone())
                    .unwrap_or_else(|| panic!("{format:?}: no page raster"))
            };
            // Hidden marks cannot change a single embedded pixel.
            assert_eq!(shot(&hid), shot(&base), "{format:?}");
            for image in hid.images.iter().filter(|r| (r.w, r.h) == (64, 48)) {
                for y in 0..48 {
                    for x in 0..64 {
                        let want = if covers(x, y) {
                            [0, 0, 0]
                        } else {
                            [x as u8, y as u8, 200]
                        };
                        assert_eq!(image.at(x, y), want, "{format:?} pixel {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn privacy_holds_when_the_screenshot_is_fitted_into_a_canvas() {
        let presentation = Presentation {
            canvas_size: Some([200, 100]),
            padding: 4,
            background: Background::Solid([0.2, 0.4, 0.8, 1.]),
            ..Presentation::default()
        };
        let mask = || Mark::redaction(1, pt(4., 4.), pt(40., 16.));
        let with = |marks: Vec<Mark>| snap(marks, None, presentation.clone());
        let baseline = with(vec![mask()]);
        let hidden = with(vec![
            mask(),
            small_text(2, pt(5., 5.), "SECRET"),
            Mark::path(3, vec![pt(10., 8.), pt(30., 14.)], false, RED, 2.0),
        ]);
        let control = with(vec![mask(), small_text(2, pt(10., 26.), "VISIBLE")]);
        for format in BOTH {
            let (_, base) = export(&baseline, format);
            let (_, hid) = export(&hidden, format);
            let (_, vis) = export(&control, format);
            assert_eq!(
                (hid.paths, hid.glyphs),
                (base.paths, base.glyphs),
                "{format:?}"
            );
            assert!(vis.glyphs > base.glyphs, "{format:?}: control shows text");
            let shot = |i: &Inspected| {
                let r = i.images.iter().find(|r| (r.w, r.h) == (200, 100));
                r.map(|r| r.rgb.clone())
                    .unwrap_or_else(|| panic!("{format:?}: no canvas-sized raster"))
            };
            assert_eq!(shot(&hid), shot(&base), "{format:?}");
        }
    }

    /// Every new drawing style shifted by `(dx, dy)`: filled rounded flat-ended rectangle,
    /// freehand highlighter, block highlight and a shadowed caption in a custom font family.
    fn styled(dx: f32, dy: f32) -> Vec<Mark> {
        let mut filled = Mark::path(
            2,
            vec![
                pt(6. + dx, 6. + dy),
                pt(26. + dx, 6. + dy),
                pt(26. + dx, 14. + dy),
                pt(6. + dx, 14. + dy),
            ],
            true,
            RED,
            2.0,
        );
        (filled.fill, filled.corner_radius, filled.round_caps) = (true, 3.0, false);
        let mut text = small_text(5, pt(5. + dx, 5. + dy), "SEC");
        text.font_family = "Monospace, Serif".into();
        text.text_effect = crate::document::TextEffect::Shadow;
        vec![
            filled,
            Mark::highlighter(
                3,
                vec![pt(8. + dx, 10. + dy), pt(24. + dx, 10. + dy)],
                [1., 1., 0., 0.4],
                6.0,
            ),
            Mark::highlight(
                4,
                pt(5. + dx, 5. + dy),
                pt(24. + dx, 15. + dy),
                [1., 1., 0., 0.3],
            ),
            text,
        ]
    }

    #[test]
    fn new_styles_under_a_mask_never_reach_the_vector_file_but_clear_of_it_they_do() {
        // Small enough that the shifted marks' padded bounds (the text's reach is 11 px plus a
        // 1 px shadow) stay clear of the mask's one pixel of privacy slack, yet every unshifted
        // mark still overlaps it.
        let mask = || Mark::redaction(1, pt(4., 4.), pt(14., 10.));
        let with = |dx: f32, dy: f32| {
            let mut marks = vec![mask()];
            marks.extend(styled(dx, dy));
            plain(marks)
        };
        let baseline = plain(vec![mask()]);
        assert!(
            vector_ids(&with(0., 0.)).is_empty(),
            "masked styles stay raster"
        );
        assert_eq!(
            vector_ids(&with(28., 20.)),
            [2, 3, 4, 5],
            "clear styles are vector"
        );
        for format in BOTH {
            let (_, base) = export(&baseline, format);
            let (_, hid) = export(&with(0., 0.), format);
            let (_, vis) = export(&with(28., 20.), format);
            assert_eq!(
                (hid.paths, hid.glyphs),
                (base.paths, base.glyphs),
                "{format:?}: nothing under the mask is emitted"
            );
            assert!(
                vis.paths > base.paths && vis.glyphs > base.glyphs,
                "{format:?}: the control must show vector marks, or the test proves nothing"
            );
            for image in hid.images.iter().filter(|r| (r.w, r.h) == (64, 48)) {
                for y in 4..10 {
                    for x in 4..14 {
                        assert_eq!(image.at(x, y), [0, 0, 0], "{format:?} pixel {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn pixels_and_marks_outside_the_crop_are_not_in_the_file() {
        let crop = Crop {
            x: 20,
            y: 10,
            width: 16,
            height: 12,
        };
        let marks = vec![
            small_text(1, pt(2., 2.), "I"),
            Mark::path(2, vec![pt(24., 16.), pt(32., 16.)], false, RED, 1.0),
        ];
        let s = snap(marks, Some(crop), Presentation::default());
        assert_eq!(vector_ids(&s), [2]);
        for format in BOTH {
            let (_, out) = export(&s, format);
            assert_eq!(
                out.glyphs, 0,
                "{format:?}: off-crop text must not be emitted"
            );
            assert!(out.paths > 0, "{format:?}: the visible line stays vector");
            let pages: Vec<_> = out.images.iter().filter(|r| r.w * r.h > 1).collect();
            assert!(!pages.is_empty());
            for image in pages {
                assert_eq!(
                    (image.w, image.h),
                    (16, 12),
                    "{format:?}: page raster is the crop"
                );
                for y in 0..12 {
                    for x in 0..16 {
                        // Exactly the cropped pixel, and no vector line baked into the raster.
                        assert_eq!(image.at(x, y), [20 + x as u8, 10 + y as u8, 200]);
                    }
                }
            }
        }
    }

    #[test]
    fn magnifier_over_a_masked_region_shows_only_masked_pixels() {
        let source = Bounds {
            x: 8.,
            y: 8.,
            width: 8.,
            height: 8.,
        };
        let marks = vec![
            Mark::redaction(1, pt(4., 4.), pt(40., 24.)),
            Mark::magnifier(2, source, 3.0, pt(42., 26.), pt(62., 46.), RED, 2.0),
        ];
        for format in BOTH {
            let (_, out) = export(&plain(marks.clone()), format);
            let page = out.images.iter().find(|r| (r.w, r.h) == (64, 48)).unwrap();
            for y in 26..46 {
                for x in 42..62 {
                    let p = page.at(x, y);
                    // Lens interior: sampled from masked source pixels, so black, never tagged.
                    if (44..60).contains(&x) && (28..44).contains(&y) {
                        assert_eq!(p, [0, 0, 0], "{format:?} lens pixel {x},{y}");
                    }
                }
            }
        }
    }

    /// Rows of an image that hold any ink, as a height.
    fn ink_height(img: &ImageSurface) -> f64 {
        let rgba = rgba_from(img).unwrap();
        let rows: Vec<u32> = (0..rgba.height())
            .filter(|&y| (0..rgba.width()).any(|x| rgba.get_pixel(x, y)[3] > 0))
            .collect();
        match (rows.first(), rows.last()) {
            (Some(a), Some(b)) => f64::from(b - a + 1),
            _ => 0.0,
        }
    }

    #[test]
    fn wrapped_text_breaks_lines_on_the_vector_surface_as_on_the_png() {
        use gtk::cairo::{Content, RecordingSurface};
        let words = "The quick brown fox jumps over the lazy dog while counting wiggly iiii mmmm";
        for width in (40..=220).step_by(3) {
            let mut m = small_text(1, pt(4., 4.), words);
            m.font_size = 16.0;
            if let MarkKind::Text { box_width, .. } = &mut m.kind {
                *box_width = width as f32;
            }
            let scene = Document::from_parts(RgbaImage::new(1, 1), vec![m.clone()], None).unwrap();
            // The PNG path: an image surface at scale 1.
            let png = ImageSurface::create(gtk::cairo::Format::ARgb32, 300, 1200).unwrap();
            {
                let cr = Context::new(&png).unwrap();
                scene.paint_mark(&cr, &m).unwrap();
            }
            let reference = ink_height(&png);
            // The export path: a vector-style surface under the page's point scale.
            for scale in [1.0, PT] {
                let rec = RecordingSurface::create(Content::ColorAlpha, None).unwrap();
                {
                    let cr = Context::new(&rec).unwrap();
                    cr.scale(scale, scale);
                    scene.paint_mark(&cr, &m).unwrap();
                }
                let ink = rec.ink_extents().3 / scale;
                // A different break adds or drops a whole 19 px line; antialiasing is under 3 px.
                assert!(
                    (ink - reference).abs() <= 3.0,
                    "width {width}, scale {scale}: vector ink {ink:.1}px high, PNG {reference:.1}px"
                );
            }
        }
    }

    fn pixels(s: &Snapshot) -> RgbaImage {
        let page = Page::build(s.clone()).unwrap();
        let out = ImageSurface::create(
            gtk::cairo::Format::ARgb32,
            page.framed.width(),
            page.framed.height(),
        )
        .unwrap();
        {
            let cr = Context::new(&out).unwrap();
            page.draw(&cr).unwrap();
        }
        rgba_from(&out).unwrap()
    }

    fn difference(a: &RgbaImage, b: &RgbaImage) -> (u8, f64) {
        assert_eq!(a.dimensions(), b.dimensions());
        let (mut max, mut sum) = (0u8, 0u64);
        for (p, q) in a.pixels().zip(b.pixels()) {
            for c in 0..4 {
                let d = p[c].abs_diff(q[c]);
                max = max.max(d);
                sum += u64::from(d);
            }
        }
        (max, sum as f64 / (a.as_raw().len() as f64))
    }

    fn framed() -> Presentation {
        Presentation {
            padding: 6,
            radius: 5.0,
            border: 2,
            border_color: [0., 0., 1., 1.],
            background: Background::Solid([0.9, 0.8, 0.2, 1.]),
            ..Presentation::default()
        }
    }

    #[test]
    fn vector_overlay_lands_on_the_png_pixels_through_crop_frame_and_resize() {
        let crop = Crop {
            x: 8,
            y: 4,
            width: 48,
            height: 40,
        };
        let marks = vec![Mark::path(
            1,
            vec![pt(16., 14.), pt(44., 14.), pt(30., 34.)],
            true,
            RED,
            3.0,
        )];
        for (presentation, crop, tolerance) in [
            (Presentation::default(), None, (1, 0.05)),
            (framed(), Some(crop), (3, 0.05)),
            (
                Presentation {
                    output_size: Some([150, 120]),
                    ..framed()
                },
                Some(crop),
                // Resampling differs along the stroke edge only.
                (255, 1.5),
            ),
            (
                // Wide canvas around a cropped, framed screenshot: pillarboxed, never stretched.
                Presentation {
                    canvas_size: Some([200, 90]),
                    ..framed()
                },
                Some(crop),
                (255, 1.5),
            ),
            (
                Presentation {
                    canvas_size: Some([90, 200]),
                    output_size: Some([120, 160]),
                    ..framed()
                },
                None,
                (255, 1.5),
            ),
        ] {
            let s = snap(marks.clone(), crop, presentation);
            assert_eq!(
                vector_ids(&s),
                [1],
                "the overlay must be vector for this test"
            );
            let live = s.clone().into_document().unwrap().render().unwrap();
            let (max, mean) = difference(&pixels(&s), &live);
            assert!(
                max <= tolerance.0 && mean <= tolerance.1,
                "max {max}, mean {mean}"
            );
        }
    }

    #[test]
    fn save_new_refuses_to_overwrite_and_writes_only_the_page() {
        let dir = tempfile::tempdir().unwrap();
        let s = plain(vec![line(1, 10.)]);
        for (name, format, magic) in [
            ("a.pdf", Format::Pdf, &b"%PDF-"[..]),
            ("a.svg", Format::Svg, &b"<?xml"[..]),
        ] {
            let path = dir.path().join(name);
            save_new(s.clone(), &path, format).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.starts_with(magic));
            for forbidden in [&b"SHARPPRJ"[..], b"marks", b"presentation"] {
                assert!(
                    find(&bytes, forbidden).is_none(),
                    "{name} leaks project data"
                );
            }
            let before = bytes.clone();
            assert!(save_new(s.clone(), &path, format).is_err());
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "existing file replaced"
            );
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            2,
            "temp file left behind"
        );
    }

    #[test]
    fn an_unwritable_destination_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("a.pdf");
        assert!(save_new(plain(vec![]), &path, Format::Pdf).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
