//! Editable `.sharp` projects: the original screenshot plus every annotation, in one file.
//!
//! Layout (little endian): `SHARPPRJ` magic, `u32` version, `u32` metadata length, `u64` PNG
//! length, JSON metadata, PNG of the *original* background. The two lengths must account for
//! the whole file, so truncation and trailing bytes are both rejected. Projects keep the pixels
//! that privacy masks (black or pixelated) cover; they are editing files, not artifacts to share.
//!
//! Version 2 adds the output presentation and the per-mark head and font sizes. Version 1 files
//! load through `migrate_v1`, which gives their arrowheads and text the sizes they were drawn at.
//! Version 3 adds the rich text style (bold, italic, alignment) of text marks; versions 1 and 2
//! load with plain text. Version 4 adds the presentation's fitted `canvas_size`; older files load
//! without one, and a canvas inside a file that predates it is refused. Version 5 adds the shape
//! fill, round-caps and corner-radius styles, the per-mark font family, caption effect and step
//! ring, the freehand highlighter kind and decimal line widths below 1 px; older files load with
//! the look they always had, and a file that predates them but uses one is refused. This writer
//! always writes version 5.
use crate::document::{
    Crop, DEFAULT_FAMILY, DEFAULT_FONT_STYLE, Document, Mark, MarkKind, TextEffect, TextStyle,
    migrate_v1,
};
use crate::presentation::Presentation;
use anyhow::{Context, Result, bail, ensure};
use image::{ImageEncoder, ImageFormat, ImageReader, Limits, RgbaImage};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufReader, BufWriter, Cursor, Read, Write},
    path::Path,
};

const MAGIC: &[u8; 8] = b"SHARPPRJ";
const VERSION: u32 = 5;
const HEADER: u64 = 24;
const MAX_META: u64 = 16 << 20;
const MAX_PNG: u64 = 1 << 30;
const MAX_SIDE: u32 = 32_768;
const MAX_PIXELS: u64 = 100_000_000;

#[derive(Serialize)]
struct MetaRef<'a> {
    width: u32,
    height: u32,
    crop: Option<Crop>,
    marks: &'a [Mark],
    presentation: &'a Presentation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    width: u32,
    height: u32,
    crop: Option<Crop>,
    marks: Vec<Mark>,
    /// Absent in version 1 files and in v2 files from a default presentation.
    #[serde(default)]
    presentation: Option<Presentation>,
}

/// Everything a project stores. Undo history is intentionally not part of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub image: RgbaImage,
    pub marks: Vec<Mark>,
    pub crop: Option<Crop>,
    pub presentation: Presentation,
}

fn check_dims(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0
            && height > 0
            && width <= MAX_SIDE
            && height <= MAX_SIDE
            && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "Project image size {width} × {height} is outside the supported range"
    );
    Ok(())
}

fn write_frame(w: &mut impl Write, meta: &[u8], png: &[u8]) -> Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&u32::try_from(meta.len())?.to_le_bytes())?;
    w.write_all(&(png.len() as u64).to_le_bytes())?;
    w.write_all(meta)?;
    w.write_all(png)?;
    Ok(())
}

impl Snapshot {
    pub fn capture(document: &Document) -> Result<Self> {
        Ok(Self {
            image: document.background_image()?,
            marks: document.marks.clone(),
            crop: document.crop,
            presentation: document.presentation.clone(),
        })
    }

    /// Validates and builds a live document; the snapshot is consumed, nothing else is touched.
    pub fn into_document(self) -> Result<Document> {
        Document::from_parts(self.image, self.marks, self.crop)?
            .with_presentation(self.presentation)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let (file, meta) = crate::open_regular_file(path)
            .with_context(|| format!("Cannot open {}", path.display()))?;
        decode(BufReader::new(file), meta.len())
            .with_context(|| format!("Cannot load project {}", path.display()))
    }

    /// Never overwrites: an existing path is an error, and a failure leaves no partial file.
    pub fn save_new(&self, path: &Path) -> Result<()> {
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
            self.encode(&mut writer)?;
            writer.flush()?;
        }
        temp.as_file().sync_all()?;
        temp.persist_noclobber(path)
            .map_err(|e| e.error)
            .with_context(|| format!("Cannot save {}", path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    fn encode(&self, w: &mut impl Write) -> Result<()> {
        let (width, height) = self.image.dimensions();
        check_dims(width, height)?;
        let meta = serde_json::to_vec(&MetaRef {
            width,
            height,
            crop: self.crop,
            marks: &self.marks,
            presentation: &self.presentation,
        })
        .context("Cannot encode project")?;
        ensure!(
            meta.len() as u64 <= MAX_META,
            "Project has too many annotations"
        );
        // JSON turns NaN/infinity into null; reading it back refuses them, so a file that
        // could never be opened is not written.
        let back: Meta =
            serde_json::from_slice(&meta).context("Project contains invalid numbers")?;
        ensure!(
            back.marks == self.marks && back.presentation.as_ref() == Some(&self.presentation),
            "Project does not survive saving unchanged"
        );
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(
            self.image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )?;
        ensure!(png.len() as u64 <= MAX_PNG, "Project image is too large");
        write_frame(w, &meta, &png)
    }
}

/// `len` is the true size of `r`; every length in the header is checked against it before
/// anything is allocated.
fn decode(mut r: impl Read, len: u64) -> Result<Snapshot> {
    ensure!(len >= HEADER, "Not a SharpShot project (file too short)");
    let mut h = [0u8; HEADER as usize];
    r.read_exact(&mut h).context("Truncated project header")?;
    ensure!(&h[..8] == MAGIC, "Not a SharpShot project");
    let version = u32::from_le_bytes(h[8..12].try_into()?);
    ensure!(
        (1..=VERSION).contains(&version),
        "Unsupported project version {version}; this SharpShot reads versions 1 to {VERSION}"
    );
    let meta_len = u64::from(u32::from_le_bytes(h[12..16].try_into()?));
    let png_len = u64::from_le_bytes(h[16..24].try_into()?);
    ensure!(
        (1..=MAX_META).contains(&meta_len) && (1..=MAX_PNG).contains(&png_len),
        "Project section sizes are out of range"
    );
    ensure!(
        HEADER + meta_len + png_len == len,
        "Project file is truncated or has trailing data"
    );
    let mut meta = vec![0u8; meta_len as usize];
    r.read_exact(&mut meta)
        .context("Truncated project metadata")?;
    let mut png = vec![0u8; png_len as usize];
    r.read_exact(&mut png).context("Truncated project image")?;

    let mut meta: Meta = serde_json::from_slice(&meta).context("Invalid project metadata")?;
    ensure!(
        version >= 2 || meta.presentation.is_none(),
        "Version 1 project has a presentation"
    );
    if let Some(p) = &meta.presentation {
        p.validate().context("Invalid project presentation")?;
    }
    if version < 3 {
        ensure!(
            meta.marks
                .iter()
                .all(|m| m.text_style == TextStyle::default()),
            "Version {version} project has text styles"
        );
    }
    ensure!(
        version >= 5
            || meta.marks.iter().all(|m| {
                !m.fill
                    && m.round_caps
                    && m.corner_radius == 0.0
                    && m.font_family == DEFAULT_FAMILY
                    && m.font_style == DEFAULT_FONT_STYLE
                    && m.text_effect == TextEffect::None
                    && !m.step_ring
                    && m.width >= 1.0
                    && !matches!(m.kind, MarkKind::Highlighter)
            }),
        "Version {version} project has shape, cap, font, effect, thin-line or highlighter styles"
    );
    ensure!(
        version >= 4
            || meta
                .presentation
                .as_ref()
                .is_none_or(|p| p.canvas_size.is_none()),
        "Version {version} project has a canvas size"
    );
    if version < 2 {
        migrate_v1(&mut meta.marks);
    }
    check_dims(meta.width, meta.height)?;
    let reader = || ImageReader::with_format(Cursor::new(&png), ImageFormat::Png);
    let dims = reader()
        .into_dimensions()
        .context("Invalid project image")?;
    ensure!(
        dims == (meta.width, meta.height),
        "Project image does not match its metadata"
    );
    let mut reader = reader();
    let mut limits = Limits::default();
    limits.max_image_width = Some(meta.width);
    limits.max_image_height = Some(meta.height);
    reader.limits(limits);
    let image = reader
        .decode()
        .context("Invalid project image")?
        .into_rgba8();
    Ok(Snapshot {
        image,
        marks: meta.marks,
        crop: meta.crop,
        presentation: meta.presentation.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Affine, Anchor, Bounds, Endpoint, MarkKind, TextAlign};
    use crate::presentation::Background;
    use crate::shapes::Point;
    use image::Rgba;

    fn p(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    fn gradient() -> RgbaImage {
        RgbaImage::from_fn(120, 90, |x, y| {
            Rgba([(x * 2) as u8, (y * 2) as u8, ((x + y) % 251) as u8, 255])
        })
    }

    /// One mark of every kind, a group, a transform and a crop.
    fn rich_document() -> Document {
        let mut d = Document::new(gradient()).unwrap();
        let red = [1., 0., 0., 1.];
        let (a, b, t, c, r, s, m) = (d.id(), d.id(), d.id(), d.id(), d.id(), d.id(), d.id());
        d.add(Mark::path(
            a,
            vec![p(10., 10.), p(50., 12.), p(48., 40.)],
            false,
            red,
            3.,
        ));
        d.add(Mark::path(
            b,
            vec![p(60., 50.), p(100., 50.), p(100., 80.), p(60., 80.)],
            true,
            [0., 0.5, 1., 1.],
            2.,
        ));
        d.add(Mark::text(t, p(8., 60.), "Héllo ünï", [1., 1., 0., 1.], 3.));
        assert!(d.set_text(t, "Héllo ünï", 70., true, 30.));
        let mut conn = Mark::path(c, vec![p(50., 12.), p(60., 50.)], false, red, 2.);
        conn.kind = MarkKind::Connector {
            start: Endpoint {
                point: p(50., 12.),
                anchor: Some(Anchor {
                    mark: a,
                    u: 0.9,
                    v: 0.1,
                }),
            },
            end: Endpoint {
                point: p(60., 50.),
                anchor: Some(Anchor {
                    mark: b,
                    u: 0.,
                    v: 0.,
                }),
            },
        };
        d.add(conn);
        let mut red_box = Mark::path(r, vec![p(20., 20.), p(45., 35.)], false, red, 2.);
        red_box.kind = MarkKind::Redaction;
        d.add(red_box);
        let mut step = Mark::path(s, vec![p(90., 20.)], false, red, 8.);
        step.kind = MarkKind::Step { number: 3 };
        d.add(step);
        let mut mag = Mark::path(m, vec![p(70., 5.), p(110., 40.)], false, red, 2.);
        mag.kind = MarkKind::Magnifier {
            source: Bounds {
                x: 15.,
                y: 15.,
                width: 20.,
                height: 20.,
            },
            zoom: 2.,
        };
        d.add(mag);
        assert!(d.set_head_size(&[c], 2.5));
        let (sp, hl) = (d.id(), d.id());
        d.add(Mark::spotlight(sp, p(5., 5.), p(100., 70.)));
        d.add(Mark::highlight(
            hl,
            p(10., 50.),
            p(40., 62.),
            [1., 1., 0., 0.3],
        ));
        assert!(d.group_marks(&[a, b]));
        assert!(d.transform_marks(&[t], Affine::rotation_about(0.25, p(8., 60.))));
        d.set_crop(Some(Crop {
            x: 2,
            y: 3,
            width: 110,
            height: 80,
        }));
        d
    }

    fn bytes(snapshot: &Snapshot) -> Vec<u8> {
        let mut out = Vec::new();
        snapshot.encode(&mut out).unwrap();
        out
    }

    fn open(bytes: &[u8]) -> Result<Snapshot> {
        decode(Cursor::new(bytes), bytes.len() as u64)
    }

    fn split(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let meta_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let png_at = HEADER as usize + meta_len;
        (
            bytes[HEADER as usize..png_at].to_vec(),
            bytes[png_at..].to_vec(),
        )
    }

    #[test]
    fn roundtrip_keeps_metadata_and_rendering() -> Result<()> {
        let original = rich_document();
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.sharp");
        Snapshot::capture(&original)?.save_new(&path)?;

        let loaded = Snapshot::load(&path)?;
        assert_eq!(loaded.marks, original.marks);
        assert_eq!(loaded.crop, original.crop);
        assert_eq!(loaded.image, gradient());

        let mut reopened = loaded.into_document()?;
        assert_eq!(reopened.render()?, original.render()?);
        assert_eq!(reopened.marks, original.marks);
        assert!(!reopened.can_undo());
        let newest = original.marks.iter().map(|m| m.id).max().unwrap();
        assert!(reopened.id() > newest);
        Ok(())
    }

    #[test]
    fn redacted_pixels_stay_black_after_reopen_but_original_is_retained() -> Result<()> {
        let reopened = Snapshot::capture(&rich_document())?;
        let bytes = bytes(&reopened);
        let loaded = open(&bytes)?;
        let rendered = loaded.clone().into_document()?.render()?;
        // (30, 28) sits inside the redaction; the render is cropped by (2, 3).
        assert_eq!(rendered.get_pixel(28, 25).0, [0, 0, 0, 255]);
        assert_eq!(loaded.image.get_pixel(30, 28), gradient().get_pixel(30, 28));
        Ok(())
    }

    #[test]
    fn privacy_blur_roundtrips_and_old_redaction_loads_unchanged() -> Result<()> {
        let mut d = Document::new(gradient())?;
        let (r, b) = (d.id(), d.id());
        d.add(Mark::redaction(r, p(5., 5.), p(20., 20.)));
        d.add(Mark::privacy_blur(b, p(30., 30.), p(50., 50.)));
        let bytes = bytes(&Snapshot::capture(&d)?);
        let (meta, png) = split(&bytes);
        let meta = String::from_utf8(meta)?;
        assert!(meta.contains("\"Redaction\"") && meta.contains("\"PrivacyBlur\""));

        let loaded = open(&bytes)?;
        assert_eq!(loaded.marks, d.marks);
        assert!(matches!(loaded.marks[0].kind, MarkKind::Redaction));
        assert!(matches!(loaded.marks[1].kind, MarkKind::PrivacyBlur));
        assert_eq!(loaded.into_document()?.render()?, d.render()?);

        // A file from before the blur variant only knows `Redaction`; it loads as plain black.
        let mut old = Vec::new();
        let legacy = meta.replace("\"PrivacyBlur\"", "\"Redaction\"");
        write_frame(&mut old, legacy.as_bytes(), &png)?;
        let old = open(&old)?;
        assert!(
            old.marks
                .iter()
                .all(|m| matches!(m.kind, MarkKind::Redaction))
        );
        Ok(())
    }

    #[test]
    fn shape_cap_and_font_styles_roundtrip_and_older_files_load_unchanged() -> Result<()> {
        let mut d = Document::new(gradient())?;
        let (r, h, t, s) = (d.id(), d.id(), d.id(), d.id());
        let mut rect = Mark::path(
            r,
            vec![p(10., 10.), p(60., 10.), p(60., 40.), p(10., 40.)],
            true,
            [0., 0.5, 1., 0.6],
            3.,
        );
        (rect.fill, rect.corner_radius, rect.round_caps) = (true, 6.5, false);
        d.add(rect);
        d.add(Mark::highlighter(
            h,
            vec![p(5., 70.), p(40., 72.), p(80., 68.)],
            [1., 1., 0., 0.4],
            12.,
        ));
        let mut text = Mark::text(t, p(8., 50.), "Fonty", [1., 0., 0., 1.], 3.);
        text.font_family = "DejaVu Serif, Noto Sans".into();
        text.text_effect = TextEffect::Shadow;
        d.add(text);
        let mut step = Mark::step(s, p(90., 20.), 4, [1., 0., 0., 1.], 5.);
        step.font_family = "Monospace".into();
        step.font_style = "Heavy".into();
        step.step_ring = true;
        step.width = 0.5;
        d.add(step);

        let good = bytes(&Snapshot::capture(&d)?);
        let loaded = open(&good)?;
        assert_eq!(loaded.marks, d.marks);
        assert_eq!(loaded.into_document()?.render()?, d.render()?);
        // Files that predate the styles cannot name them.
        for version in [2u32, 3, 4] {
            let mut old = good.clone();
            old[8..12].copy_from_slice(&version.to_le_bytes());
            let err = open(&old).unwrap_err().to_string();
            assert!(err.contains("highlighter styles"), "v{version}: {err}");
        }

        // A version 4 file has none of the new keys: every mark loads with the look it always had.
        let mut plain = Document::new(gradient())?;
        let (a, b) = (plain.id(), plain.id());
        plain.add(Mark::path(
            a,
            vec![p(10., 10.), p(50., 12.)],
            false,
            [1., 0., 0., 1.],
            3.,
        ));
        plain.add(Mark::text(b, p(8., 60.), "Plain", [1., 1., 0., 1.], 3.));
        let (meta, png) = split(&bytes(&Snapshot::capture(&plain)?));
        let mut v: serde_json::Value = serde_json::from_slice(&meta)?;
        for m in v["marks"].as_array_mut().unwrap() {
            let m = m.as_object_mut().unwrap();
            for key in [
                "fill",
                "round_caps",
                "corner_radius",
                "font_family",
                "font_style",
            ] {
                assert!(m.remove(key).is_some());
            }
        }
        let mut old = Vec::new();
        write_frame(&mut old, &serde_json::to_vec(&v)?, &png)?;
        old[8..12].copy_from_slice(&4u32.to_le_bytes());
        let old = open(&old)?;
        assert_eq!(old.marks, plain.marks);
        assert!(old.marks.iter().all(|m| m.round_caps && !m.fill));
        assert!(old.marks.iter().all(|m| m.font_family == DEFAULT_FAMILY));
        Ok(())
    }

    #[test]
    fn rejects_bad_magic_and_unknown_version() {
        let good = bytes(&Snapshot::capture(&rich_document()).unwrap());
        let mut bad = good.clone();
        bad[0] ^= 1;
        assert!(open(&bad).is_err());
        let mut future = good.clone();
        future[8..12].copy_from_slice(&6u32.to_le_bytes());
        let err = open(&future).unwrap_err().to_string();
        assert!(err.contains("version 6"), "{err}");
        assert!(open(&[]).is_err());
    }

    #[test]
    fn rejects_truncation_trailing_data_and_lying_lengths() {
        let good = bytes(&Snapshot::capture(&rich_document()).unwrap());
        for cut in [1, 10, 23, 24, 30, good.len() / 2, good.len() - 1] {
            assert!(open(&good[..cut]).is_err(), "cut at {cut}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(open(&trailing).is_err());
        let mut huge = good.clone();
        huge[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(open(&huge).is_err());
        let mut huge_png = good.clone();
        huge_png[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(open(&huge_png).is_err());
    }

    #[test]
    fn rejects_corrupt_image_and_metadata() {
        let good = bytes(&Snapshot::capture(&rich_document()).unwrap());
        let (meta, png) = split(&good);
        let frame = |meta: &[u8], png: &[u8]| {
            let mut out = Vec::new();
            write_frame(&mut out, meta, png).unwrap();
            out
        };
        assert!(open(&frame(&meta, &png[..png.len() / 2])).is_err());
        assert!(open(&frame(&meta, b"not a png")).is_err());
        assert!(open(&frame(b"{", &png)).is_err());
        assert!(
            open(&frame(
                br#"{"width":120,"height":90,"crop":null,"marks":[],"extra":1}"#,
                &png
            ))
            .is_err()
        );
        // image and declared size must agree, and sizes beyond the caps never decode.
        assert!(
            open(&frame(
                br#"{"width":121,"height":90,"crop":null,"marks":[]}"#,
                &png
            ))
            .is_err()
        );
        assert!(
            open(&frame(
                br#"{"width":40000,"height":90,"crop":null,"marks":[]}"#,
                &png
            ))
            .is_err()
        );
        assert!(check_dims(0, 5).is_err());
        assert!(check_dims(MAX_SIDE, MAX_SIDE).is_err());
        assert!(check_dims(10_000, 10_000).is_ok());
    }

    #[test]
    fn rejects_out_of_range_numbers_in_file_and_in_memory() -> Result<()> {
        let mut d = Document::new(gradient())?;
        let id = d.id();
        d.add(Mark::path(
            id,
            vec![p(12345.5, 1.), p(2., 3.)],
            false,
            [1., 0., 0., 1.],
            2.,
        ));
        let good = bytes(&Snapshot::capture(&d)?);
        let (meta, png) = split(&good);
        let meta = String::from_utf8(meta)?.replace("12345.5", "1e999");
        let mut forged = Vec::new();
        write_frame(&mut forged, meta.as_bytes(), &png)?;
        assert!(open(&forged).is_err());

        let mut snapshot = Snapshot::capture(&d)?;
        snapshot.marks[0].points[0].x = f32::NAN;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nan.sharp");
        assert!(snapshot.save_new(&path).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn semantic_errors_surface_before_a_document_exists() -> Result<()> {
        let good = Snapshot::capture(&rich_document())?;

        let mut duplicate = good.clone();
        duplicate.marks[1].id = duplicate.marks[0].id;
        assert!(open(&bytes(&duplicate))?.into_document().is_err());

        let mut zero = good.clone();
        zero.marks[0].id = 0;
        assert!(open(&bytes(&zero))?.into_document().is_err());

        let mut dangling = good.clone();
        let connector = dangling
            .marks
            .iter_mut()
            .find(|m| matches!(m.kind, MarkKind::Connector { .. }))
            .unwrap();
        if let MarkKind::Connector { start, .. } = &mut connector.kind {
            start.anchor.as_mut().unwrap().mark = 9999;
        }
        assert!(open(&bytes(&dangling))?.into_document().is_err());

        let mut lone_group = good.clone();
        lone_group.marks.truncate(1);
        lone_group.marks[0].group = Some(77);
        assert!(open(&bytes(&lone_group))?.into_document().is_err());

        let mut bad_crop = good.clone();
        bad_crop.crop = Some(Crop {
            x: 100,
            y: 0,
            width: 100,
            height: 10,
        });
        assert!(open(&bytes(&bad_crop))?.into_document().is_err());

        let mut singular = good.clone();
        singular.marks[0].transform = Affine {
            a: 0.,
            b: 0.,
            c: 0.,
            d: 0.,
            tx: 0.,
            ty: 0.,
        };
        assert!(open(&bytes(&singular))?.into_document().is_err());

        assert!(good.into_document().is_ok());
        Ok(())
    }

    /// Without serde_json's `float_roundtrip`, 52/255 comes back one ulp off and saving fails.
    #[test]
    fn float_values_survive_save_and_load_bit_exactly() -> Result<()> {
        let mut d = Document::new(gradient())?;
        let id = d.id();
        let color = [245. / 255., 155. / 255., 52. / 255., 1.];
        d.add(Mark::path(
            id,
            vec![p(1. / 3., 2. / 3.), p(50. / 7., 80. / 3.)],
            false,
            color,
            7. / 3.,
        ));
        assert!(d.transform_marks(&[id], Affine::rotation_about(0.3, p(1. / 3., 1. / 7.))));
        let original = Snapshot::capture(&d)?;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("exact.sharp");
        original.save_new(&path)?;
        let loaded = Snapshot::load(&path)?;
        assert_eq!(loaded.marks[0].color, color);
        assert_eq!(loaded.marks[0].width, 7. / 3.);
        assert_eq!(loaded.marks, original.marks);
        assert_eq!(loaded.into_document()?.marks, d.marks);
        Ok(())
    }

    #[test]
    fn save_never_overwrites_and_leaves_no_temp_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("p.sharp");
        let snapshot = Snapshot::capture(&rich_document())?;
        snapshot.save_new(&path)?;
        let first = std::fs::read(&path)?;

        let mut other = snapshot.clone();
        other.marks.clear();
        assert!(other.save_new(&path).is_err());
        assert_eq!(std::fs::read(&path)?, first);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn load_refuses_non_files_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Snapshot::load(dir.path()).is_err());
        assert!(Snapshot::load(&dir.path().join("missing.sharp")).is_err());
    }

    fn v1_bytes(with_presentation: bool) -> Result<Vec<u8>> {
        let good = bytes(&Snapshot::capture(&rich_document())?);
        let (meta, png) = split(&good);
        let mut v: serde_json::Value = serde_json::from_slice(&meta)?;
        let root = v.as_object_mut().unwrap();
        root.remove("presentation");
        if with_presentation {
            root.insert(
                "presentation".into(),
                serde_json::to_value(Presentation::default())?,
            );
        }
        for m in root.get_mut("marks").unwrap().as_array_mut().unwrap() {
            let o = m.as_object_mut().unwrap();
            o.remove("head_size");
            o.remove("font_size");
            o.remove("text_style");
        }
        let mut old = Vec::new();
        write_frame(&mut old, &serde_json::to_vec(&v)?, &png)?;
        old[8..12].copy_from_slice(&1u32.to_le_bytes());
        Ok(old)
    }

    #[test]
    fn version_1_projects_migrate_while_version_2_stays_strict() -> Result<()> {
        let old = v1_bytes(false)?;
        let loaded = open(&old)?;
        let text = loaded
            .marks
            .iter()
            .find(|m| matches!(m.kind, MarkKind::Text { .. }))
            .unwrap();
        assert_eq!(
            text.font_size,
            text.width * 6.,
            "text keeps the size it was drawn at"
        );
        let arrow = loaded
            .marks
            .iter()
            .find(|m| matches!(m.kind, MarkKind::Connector { .. }))
            .unwrap();
        assert!((arrow.head_size - 10. / crate::document::HEAD_BASE).abs() < 1e-9);
        assert_eq!(loaded.presentation, Presentation::default());
        assert!(loaded.into_document().is_ok());

        // The same body claimed as version 2 is not migrated: unsized text is refused.
        let mut v2 = old.clone();
        v2[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(open(&v2)?.into_document().is_err());
        // A version 1 file cannot carry a presentation.
        assert!(open(&v1_bytes(true)?).is_err());
        Ok(())
    }

    fn styled_text_document() -> Document {
        let mut d = Document::new(gradient()).unwrap();
        let id = d.id();
        d.add(Mark::text(id, p(8., 20.), "Styled", [1., 1., 0., 1.], 3.));
        let style = TextStyle {
            bold: true,
            italic: true,
            align: TextAlign::Right,
        };
        assert!(d.set_text_styled(
            id,
            "Styled\nbox",
            90.,
            true,
            28.,
            style,
            "Sans",
            "Regular",
            TextEffect::None,
            None
        ));
        d
    }

    #[test]
    fn text_styles_persist_and_older_files_stay_plain() -> Result<()> {
        let d = styled_text_document();
        let good = bytes(&Snapshot::capture(&d)?);
        let loaded = open(&good)?;
        assert_eq!(loaded.marks[0].text_style, d.marks[0].text_style);
        assert_eq!(loaded.into_document()?.render()?, d.render()?);

        // A version 2 file has no `text_style` key: text loads plain.
        let (meta, png) = split(&good);
        let mut v: serde_json::Value = serde_json::from_slice(&meta)?;
        v["marks"][0].as_object_mut().unwrap().remove("text_style");
        let frame = |v: &serde_json::Value, version: u32| -> Result<Vec<u8>> {
            let mut out = Vec::new();
            write_frame(&mut out, &serde_json::to_vec(v)?, &png)?;
            out[8..12].copy_from_slice(&version.to_le_bytes());
            Ok(out)
        };
        let marks = open(&frame(&v, 2)?)?.marks;
        assert_eq!(marks[0].text_style, TextStyle::default(), "v2");
        // Version 1 files also predate the presentation.
        v.as_object_mut().unwrap().remove("presentation");
        let marks = open(&frame(&v, 1)?)?.marks;
        assert_eq!(marks[0].text_style, TextStyle::default(), "v1");

        // ...and a file claiming version 2 yet carrying a style is inconsistent.
        let mut lie = good.clone();
        lie[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(open(&lie).is_err());

        // A style on anything but text is refused when the document is built.
        let mut bad = Snapshot::capture(&d)?;
        bad.marks.push(Mark {
            id: 99,
            text_style: bad.marks[0].text_style,
            ..Mark::path(99, vec![p(1., 1.), p(5., 5.)], false, [1., 0., 0., 1.], 2.)
        });
        assert!(bad.into_document().is_err());
        Ok(())
    }

    #[test]
    fn presentation_persists_and_is_validated_on_load() -> Result<()> {
        let mut d = Document::new(gradient())?;
        let id = d.id();
        d.add(Mark::path(
            id,
            vec![p(5., 5.), p(40., 40.)],
            false,
            [1., 0., 0., 1.],
            3.,
        ));
        let look = Presentation {
            padding: 16,
            radius: 8.,
            shadow: 0.4,
            background: Background::Gradient([0.1, 0.2, 0.9, 1.], [0.9, 0.2, 0.1, 1.]),
            border: 2,
            border_color: [1., 1., 1., 1.],
            canvas_size: None,
            output_size: None,
        };
        d.set_presentation(look.clone())?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("p.sharp");
        Snapshot::capture(&d)?.save_new(&path)?;
        let loaded = Snapshot::load(&path)?;
        assert_eq!(loaded.presentation, look);
        let reopened = loaded.into_document()?;
        assert_eq!(reopened.presentation, look);
        assert_eq!(reopened.render()?, d.render()?);
        assert!(!reopened.can_undo());
        assert_eq!(
            reopened.background_image()?,
            gradient(),
            "original pixels stay unframed"
        );

        let mut bad = Snapshot::capture(&d)?;
        bad.presentation.shadow = 3.;
        assert!(open(&bytes(&bad)).is_err());
        assert!(bad.into_document().is_err());
        Ok(())
    }

    fn fitted_document() -> Document {
        let mut d = Document::new(RgbaImage::from_pixel(40, 20, Rgba([0, 0, 255, 255]))).unwrap();
        let id = d.id();
        d.add(Mark::path(
            id,
            vec![Point { x: 5., y: 5. }, Point { x: 30., y: 12. }],
            false,
            [1., 0., 0., 1.],
            2.,
        ));
        d.set_presentation(Presentation {
            padding: 4,
            background: Background::Solid([0.1, 0.9, 0.1, 1.]),
            canvas_size: Some([120, 120]),
            ..Presentation::default()
        })
        .unwrap();
        d
    }

    #[test]
    fn fitted_canvas_survives_save_and_reopen_and_renders_the_same() -> Result<()> {
        let d = fitted_document();
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("fit.sharp");
        Snapshot::capture(&d)?.save_new(&path)?;
        let reopened = Snapshot::load(&path)?.into_document()?;
        let (want, got) = (d.render()?, reopened.render()?);
        assert_eq!(got.dimensions(), (120, 120));
        assert_eq!(got, want);
        Ok(())
    }

    #[test]
    fn files_from_before_the_canvas_load_without_one_and_cannot_smuggle_one_in() -> Result<()> {
        let good = bytes(&Snapshot::capture(&fitted_document())?);
        assert!(open(&good)?.presentation.canvas_size.is_some());
        let frame = |canvas: serde_json::Value, version: u32| -> Result<Vec<u8>> {
            let (meta, png) = split(&good);
            let mut v: serde_json::Value = serde_json::from_slice(&meta)?;
            v["presentation"]["canvas_size"] = canvas;
            let mut out = Vec::new();
            write_frame(&mut out, &serde_json::to_vec(&v)?, &png)?;
            out[8..12].copy_from_slice(&version.to_le_bytes());
            Ok(out)
        };
        for version in [2, 3] {
            let old = open(&frame(serde_json::Value::Null, version)?)?;
            assert_eq!(old.presentation.canvas_size, None, "v{version}");
            assert_eq!(old.presentation.padding, 4, "v{version}");
            assert!(
                open(&frame(serde_json::json!([120, 120]), version)?).is_err(),
                "v{version} must not carry a canvas"
            );
        }
        assert_eq!(
            open(&frame(serde_json::json!([120, 120]), 4)?)?
                .presentation
                .canvas_size,
            Some([120, 120])
        );
        assert!(open(&frame(serde_json::json!([0, 120]), 4)?).is_err());
        Ok(())
    }
}
