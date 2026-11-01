//! Designer dialog: a live preview beside one-click looks, canvas formats and manual frame
//! controls (background, padding, border, corners, shadow, output size, crop).
//! Nothing touches the document until Apply, which commits crop and presentation as one undo step.

mod looks;

use crate::{
    chrome::{self, Icon},
    document::Crop,
    interaction::State,
    presentation::{Background, MAX_BORDER, MAX_DIM, MAX_PADDING, MAX_RADIUS, Presentation},
    presets::{self, Preset, PresetFile},
};
use anyhow::Result;
use gtk::{
    cairo::{Context, Filter, Format, ImageSurface, SurfacePattern},
    gdk, glib,
    pango::EllipsizeMode,
    prelude::*,
};
use looks::LOOKS;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// Largest preview image, in pixels; the cached scene is rendered for this and then shrunk.
const PREVIEW_W: f64 = 1200.0;
const PREVIEW_H: f64 = 800.0;
/// Long side of the small screenshot copy every look thumbnail is framed from.
const THUMB_SIDE: f64 = 240.0;

struct CanvasFormat {
    name: &'static str,
    /// `None` follows the screenshot's own proportions.
    size: Option<[u32; 2]>,
    note: &'static str,
}

static FORMATS: [CanvasFormat; 7] = [
    CanvasFormat {
        name: "Original",
        size: None,
        note: "Follows the screenshot's own proportions",
    },
    CanvasFormat {
        name: "Square",
        size: Some([1080, 1080]),
        note: "1:1 feed post",
    },
    CanvasFormat {
        name: "Portrait",
        size: Some([1080, 1350]),
        note: "4:5 portrait feed post",
    },
    CanvasFormat {
        name: "Story",
        size: Some([1080, 1920]),
        note: "9:16 stories and short video",
    },
    CanvasFormat {
        name: "Landscape",
        size: Some([1920, 1080]),
        note: "16:9 slides and video thumbnails",
    },
    CanvasFormat {
        name: "Link post",
        size: Some([1200, 630]),
        note: "Link preview and social share card",
    },
    CanvasFormat {
        name: "Product",
        size: Some([1600, 1200]),
        note: "4:3 store and product page image",
    },
];

struct Ui {
    window: glib::WeakRef<gtk::Window>,
    state: Rc<RefCell<State>>,
    refresh: Rc<dyn Fn()>,
    size: (u32, u32),
    pres: RefCell<Presentation>,
    crop: Cell<Option<Crop>>,
    /// The look the frame still equals; `None` once it was edited by hand ("Custom").
    look: Cell<Option<usize>>,
    /// Set while code writes widgets, so their handlers stay quiet.
    guard: Cell<bool>,
    natural: Cell<(u32, u32)>,
    scene: RefCell<Option<(Option<Crop>, f64, ImageSurface)>>,
    /// Every look framed around a small copy of the scene, valid for one crop.
    thumbs: RefCell<Option<(Option<Crop>, Vec<Option<ImageSurface>>)>>,
    preview: RefCell<Option<ImageSurface>>,
    look_cards: Vec<(gtk::ToggleButton, gtk::DrawingArea)>,
    look_state: gtk::Label,
    format_cards: Vec<gtk::ToggleButton>,
    canvas_note: gtk::Label,
    /// Transparent, solid, gradient.
    bg_kind: [gtk::ToggleButton; 3],
    padding: gtk::SpinButton,
    border: gtk::SpinButton,
    border_color: gtk::ColorButton,
    color1: gtk::ColorButton,
    color2: gtk::ColorButton,
    radius: gtk::SpinButton,
    shadow: gtk::Scale,
    lock: gtk::CheckButton,
    out_w: gtk::SpinButton,
    out_h: gtk::SpinButton,
    crop_spins: [gtk::SpinButton; 4],
    status: gtk::Label,
    error: gtk::Label,
    apply: gtk::Button,
    area: gtk::DrawingArea,
    /// Saved presets, in the order the drop-down lists them.
    saved: RefCell<PresetFile>,
    /// The preset file cannot be read; saving would replace it, so it is off.
    saved_broken: Cell<bool>,
    saved_names: gtk::StringList,
    saved_pick: gtk::DropDown,
    saved_name: gtk::Entry,
    saved_apply: gtk::Button,
    saved_delete: gtk::Button,
    saved_save: gtk::Button,
    saved_note: gtk::Label,
}

fn clamp_crop(v: [u32; 4], (w, h): (u32, u32)) -> [u32; 4] {
    let (x, y) = (v[0].min(w - 1), v[1].min(h - 1));
    [x, y, v[2].clamp(1, w - x), v[3].clamp(1, h - y)]
}

fn crop_of(v: [u32; 4], (w, h): (u32, u32)) -> Option<Crop> {
    (v != [0, 0, w, h]).then_some(Crop {
        x: v[0],
        y: v[1],
        width: v[2],
        height: v[3],
    })
}

/// `value` scaled by `num / den`, rounded and kept inside the supported side length.
fn scale_side(value: u32, num: u32, den: u32) -> u32 {
    ((u64::from(value) * u64::from(num) + u64::from(den) / 2) / u64::from(den.max(1)))
        .clamp(1, u64::from(MAX_DIM)) as u32
}

/// Scene-cache scale in quarter-octave steps, so small frame edits reuse the same source raster.
fn preview_scale((w, h): (u32, u32)) -> f64 {
    let fit = (PREVIEW_W / f64::from(w))
        .min(PREVIEW_H / f64::from(h))
        .min(1.0);
    2f64.powf((fit.log2() * 4.0).floor() / 4.0)
}

/// Bound the decorated source and final preview independently of canvas and export sizes.
fn preview_frame(
    pres: &Presentation,
    source: (u32, u32),
    (w, h): (u32, u32),
) -> Result<(f64, Presentation)> {
    let mut decoration = pres.clone();
    decoration.canvas_size = None;
    decoration.output_size = None;
    let scene_scale = preview_scale(decoration.dimensions(source.0, source.1)?);
    let fit = (PREVIEW_W / f64::from(w))
        .min(PREVIEW_H / f64::from(h))
        .min(1.0);
    let side = |v: u32| ((f64::from(v) * fit).round() as u32).max(1);
    let mut frame = pres.scaled(scene_scale);
    frame.output_size = Some([side(w), side(h)]);
    Ok((scene_scale, frame))
}

fn rgba(c: [f64; 4]) -> gdk::RGBA {
    gdk::RGBA::new(c[0] as f32, c[1] as f32, c[2] as f32, c[3] as f32)
}

fn color(button: &gtk::ColorButton) -> [f64; 4] {
    let c = button.rgba();
    [
        f64::from(c.red()),
        f64::from(c.green()),
        f64::from(c.blue()),
        f64::from(c.alpha()),
    ]
}

fn color_button(name: &str, tip: &str, value: [f64; 4]) -> gtk::ColorButton {
    let button = gtk::ColorButton::new();
    button.set_title(name);
    button.set_use_alpha(true);
    button.set_rgba(&rgba(value));
    named(&button, name, tip);
    button
}

fn spin(min: f64, max: f64, name: &str, tip: &str) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(min, max, 1.0);
    spin.set_numeric(true);
    named(&spin, name, tip);
    spin
}

pub(crate) fn named<W: IsA<gtk::Widget> + IsA<gtk::Accessible>>(widget: &W, name: &str, tip: &str) {
    widget.set_tooltip_text(Some(tip));
    widget.update_property(&[gtk::accessible::Property::Label(name)]);
}

pub(crate) fn caption(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class(class);
    label
}

fn row(text: &str, widgets: &[&gtk::Widget]) -> gtk::Box {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let label = caption(text, "field-label");
    label.set_hexpand(true);
    line.append(&label);
    for widget in widgets {
        line.append(*widget);
    }
    line
}

/// A titled group of controls: tight inside, separated from its neighbours by the panel.
fn section(title: &str) -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 8);
    group.append(&caption(title, "title"));
    group
}

fn panel() -> gtk::Box {
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 20);
    panel.set_margin_start(14);
    panel.set_margin_end(14);
    panel.set_margin_top(14);
    panel.set_margin_bottom(14);
    panel
}

fn page(panel: &gtk::Box) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(120)
        .vexpand(true)
        .child(panel)
        .build()
}

fn hook(ui: &Rc<Ui>, f: fn(&Ui)) -> impl Fn() + 'static {
    let weak = Rc::downgrade(ui);
    move || {
        if let Some(ui) = weak.upgrade() {
            f(&ui);
        }
    }
}

fn checker(cr: &Context, w: f64, h: f64, tile: f64) -> Result<()> {
    cr.set_source_rgb(0.16, 0.19, 0.21);
    cr.rectangle(0.0, 0.0, w, h);
    cr.fill()?;
    cr.set_source_rgb(0.21, 0.25, 0.28);
    for r in 0..(h / tile).ceil() as i32 {
        for c in 0..(w / tile).ceil() as i32 {
            if (r + c) % 2 == 0 {
                cr.rectangle(f64::from(c) * tile, f64::from(r) * tile, tile, tile);
            }
        }
    }
    cr.fill()?;
    Ok(())
}

/// A selectable card; `body` is its picture and caption.
fn card(body: &impl IsA<gtk::Widget>, name: &str, tip: &str) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.set_child(Some(body));
    button.add_css_class("look-card");
    named(&button, name, tip);
    button
}

fn look_card(look: &looks::Look) -> (gtk::ToggleButton, gtk::DrawingArea) {
    let area = gtk::DrawingArea::builder()
        .content_width(88)
        .content_height(90)
        .hexpand(true)
        .can_target(false)
        .build();
    let label = caption(look.name, "card-name");
    label.set_xalign(0.5);
    label.set_ellipsize(EllipsizeMode::End);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.append(&area);
    body.append(&label);
    let button = card(
        &body,
        &format!("{} look", look.name),
        &format!("{}: {}", look.name, look.note),
    );
    (button, area)
}

fn format_card(format: &CanvasFormat, image: (u32, u32)) -> gtk::ToggleButton {
    let (w, h) = format.size.map_or(image, |[w, h]| (w, h));
    let ratio = f64::from(w) / f64::from(h);
    let glyph = gtk::DrawingArea::builder()
        .content_width(28)
        .content_height(28)
        .valign(gtk::Align::Center)
        .can_target(false)
        .build();
    glyph.set_draw_func(move |area, cr, w, h| {
        let color = area.style_context().color();
        let (w, h) = (f64::from(w), f64::from(h));
        // The largest box of the format's ratio that fits the glyph, drawn like the chrome icons.
        let (bw, bh) = if ratio >= (w - 4.0) / (h - 4.0) {
            (w - 4.0, (w - 4.0) / ratio)
        } else {
            ((h - 4.0) * ratio, h - 4.0)
        };
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            color.alpha().into(),
        );
        cr.set_line_width(1.5);
        cr.rectangle((w - bw) / 2.0, (h - bh) / 2.0, bw, bh);
        if let Err(error) = cr.stroke() {
            eprintln!("Cannot draw format icon: {error}");
        }
    });
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.append(&caption(format.name, "card-name"));
    text.append(&caption(
        &format
            .size
            .map_or_else(|| "Natural size".to_owned(), |[w, h]| format!("{w} × {h}")),
        "dimension-label",
    ));
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    body.append(&glyph);
    body.append(&text);
    card(
        &body,
        &format!("{} format", format.name),
        &match format.size {
            Some([w, h]) => format!("{}: {w} × {h} px canvas. {}", format.name, format.note),
            None => format!("{}: {}", format.name, format.note),
        },
    )
}

impl Ui {
    fn close(&self) {
        if let Some(window) = self.window.upgrade() {
            window.close();
        }
    }

    /// The cropped screenshot the frame goes around.
    fn scene_size(&self) -> (u32, u32) {
        self.crop.get().map_or(self.size, |c| (c.width, c.height))
    }

    fn matching_look(&self) -> Option<usize> {
        let (scene, pres) = (self.scene_size(), self.pres.borrow());
        LOOKS.iter().position(|l| l.matches(scene, &pres))
    }

    fn bg_index(&self) -> usize {
        self.bg_kind.iter().position(|b| b.is_active()).unwrap_or(0)
    }

    /// Writes every control from the model.
    fn load(&self) {
        self.guard.set(true);
        let p = self.pres.borrow().clone();
        self.padding.set_value(f64::from(p.padding));
        self.border.set_value(f64::from(p.border));
        self.border_color.set_rgba(&rgba(p.border_color));
        self.radius.set_value(p.radius);
        self.shadow.set_value(p.shadow * 100.0);
        let kind = match p.background {
            Background::Transparent => 0,
            Background::Solid(c) => {
                self.color1.set_rgba(&rgba(c));
                1
            }
            Background::Gradient(a, b) => {
                self.color1.set_rgba(&rgba(a));
                self.color2.set_rgba(&rgba(b));
                2
            }
        };
        self.bg_kind[kind].set_active(true);
        self.color1.set_sensitive(kind > 0);
        self.color2.set_sensitive(kind == 2);
        self.guard.set(false);
        self.set_crop_spins(self.crop_values());
        self.look.set(self.matching_look());
    }

    fn crop_values(&self) -> [u32; 4] {
        let (w, h) = self.size;
        self.crop
            .get()
            .map_or([0, 0, w, h], |c| [c.x, c.y, c.width, c.height])
    }

    fn set_crop_spins(&self, v: [u32; 4]) {
        let (w, h) = self.size;
        self.guard.set(true);
        for (spin, upper) in self
            .crop_spins
            .iter()
            .zip([w - 1, h - 1, w - v[0], h - v[1]])
        {
            spin.adjustment().set_upper(f64::from(upper));
        }
        for (spin, value) in self.crop_spins.iter().zip(v) {
            spin.set_value(f64::from(value));
        }
        self.guard.set(false);
    }

    /// A manual edit of the frame: it no longer equals any look.
    fn on_style(&self) {
        if self.guard.get() {
            return;
        }
        let kind = self.bg_index();
        self.color1.set_sensitive(kind > 0);
        self.color2.set_sensitive(kind == 2);
        {
            let mut p = self.pres.borrow_mut();
            p.padding = self.padding.value_as_int() as u32;
            p.border = self.border.value_as_int() as u32;
            p.border_color = color(&self.border_color);
            p.radius = self.radius.value();
            p.shadow = self.shadow.value() / 100.0;
            p.background = match kind {
                0 => Background::Transparent,
                1 => Background::Solid(color(&self.color1)),
                _ => Background::Gradient(color(&self.color1), color(&self.color2)),
            };
        }
        self.look.set(None);
        self.update();
    }

    /// Frames the screenshot with look `i`; canvas format and output size stay as chosen.
    fn use_look(&self, i: usize) {
        let frame = LOOKS[i].frame(self.scene_size());
        {
            let mut p = self.pres.borrow_mut();
            let (canvas_size, output_size) = (p.canvas_size, p.output_size);
            *p = Presentation {
                canvas_size,
                output_size,
                ..frame
            };
        }
        self.load();
        self.look.set(Some(i));
        self.update();
    }

    fn use_format(&self, i: usize) {
        {
            let mut p = self.pres.borrow_mut();
            p.canvas_size = FORMATS[i].size;
            p.output_size = None;
        }
        // A fresh format is a clean resize: locked aspect, never a stretch.
        if self.lock.is_active() {
            self.update();
        } else {
            self.lock.set_active(true);
        }
    }

    fn on_output(&self, from_width: bool) {
        if self.guard.get() {
            return;
        }
        let (nw, nh) = self.natural.get();
        let mut w = self.out_w.value_as_int() as u32;
        let mut h = self.out_h.value_as_int() as u32;
        if self.lock.is_active() {
            if from_width {
                h = scale_side(w, nh, nw);
            } else {
                w = scale_side(h, nw, nh);
            }
        }
        self.pres.borrow_mut().output_size = ((w, h) != (nw, nh)).then_some([w, h]);
        self.update();
    }

    fn natural_size(&self) {
        self.pres.borrow_mut().output_size = None;
        self.update();
    }

    /// A look is sized from the screenshot, so it follows a crop; a hand-made frame stays as is.
    fn refit(&self) {
        match self.look.get() {
            Some(i) => self.use_look(i),
            None => self.update(),
        }
    }

    fn on_crop(&self) {
        if self.guard.get() {
            return;
        }
        let v = clamp_crop(
            std::array::from_fn(|i| self.crop_spins[i].value_as_int().max(0) as u32),
            self.size,
        );
        self.crop.set(crop_of(v, self.size));
        self.set_crop_spins(v);
        self.refit();
    }

    fn clear_crop(&self) {
        self.crop.set(None);
        self.set_crop_spins(self.crop_values());
        self.refit();
    }

    fn reset(&self) {
        *self.pres.borrow_mut() = Presentation::default();
        self.crop.set(None);
        self.load();
        self.update();
    }

    fn fail(&self, message: &str) {
        *self.preview.borrow_mut() = None;
        self.status.set_text(message);
        self.status.set_tooltip_text(Some(message));
        self.error.set_text(message);
        self.error.set_visible(true);
        self.apply.set_sensitive(false);
        self.area.queue_draw();
    }

    /// Shows which look, canvas format and background kind the model currently equals.
    fn sync_choices(&self) {
        let look = self.look.get();
        for (i, (button, _)) in self.look_cards.iter().enumerate() {
            button.set_active(look == Some(i));
        }
        self.look_state
            .set_text(look.map_or("Custom", |i| LOOKS[i].name));
        let canvas = self.pres.borrow().canvas_size;
        for (button, format) in self.format_cards.iter().zip(&FORMATS) {
            button.set_active(format.size == canvas);
        }
        self.canvas_note.set_text(&match canvas {
            None => "The canvas follows the screenshot.".to_owned(),
            Some([w, h]) => format!(
                "{w} × {h} px canvas. The framed screenshot is centred in it, never cropped or stretched."
            ),
        });
    }

    fn update(&self) {
        self.sync_choices();
        let crop = self.crop.get();
        let scene = self.scene_size();
        let mut pres = self.pres.borrow().clone();
        let mut plain = pres.clone();
        plain.output_size = None;
        let sizes = plain.dimensions(scene.0, scene.1).and_then(|n| {
            if self.lock.is_active()
                && let Some([w, _]) = pres.output_size
            {
                let h = scale_side(w, n.1, n.0);
                pres.output_size = ((w, h) != n).then_some([w, h]);
                self.pres.borrow_mut().output_size = pres.output_size;
            }
            pres.dimensions(scene.0, scene.1).map(|o| (n, o))
        });
        let (natural, out) = match sizes {
            Ok(sizes) => sizes,
            Err(error) => return self.fail(&format!("{error:#}")),
        };
        self.natural.set(natural);
        self.guard.set(true);
        self.out_w.set_value(f64::from(out.0));
        self.out_h.set_value(f64::from(out.1));
        self.guard.set(false);
        match self.render_preview(&pres, crop, scene, out) {
            Ok(image) => {
                *self.preview.borrow_mut() = Some(image);
                self.status
                    .set_text(&format!("Output {} × {} px", out.0, out.1));
                self.status
                    .set_tooltip_text(Some("Size of the exported image"));
                self.error.set_visible(false);
                self.apply.set_sensitive(true);
                self.area.queue_draw();
            }
            Err(error) => self.fail(&format!("{error:#}")),
        }
    }

    /// Cache the cropped scene at a bounded scale, then compose only the final preview pixels.
    fn render_preview(
        &self,
        pres: &Presentation,
        crop: Option<Crop>,
        (sw, sh): (u32, u32),
        output: (u32, u32),
    ) -> Result<ImageSurface> {
        let (k, frame) = preview_frame(pres, (sw, sh), output)?;
        let mut cache = self.scene.borrow_mut();
        if cache
            .as_ref()
            .is_none_or(|(c, cached_k, _)| *c != crop || *cached_k != k)
        {
            let side = |v: u32| ((f64::from(v) * k).round() as i32).max(1);
            let (pw, ph) = (side(sw), side(sh));
            let surface = ImageSurface::create(Format::ARgb32, pw, ph)?;
            {
                let cr = Context::new(&surface)?;
                cr.scale(f64::from(pw) / f64::from(sw), f64::from(ph) / f64::from(sh));
                let (x, y) = crop.map_or((0, 0), |c| (c.x, c.y));
                cr.translate(-f64::from(x), -f64::from(y));
                // Masks are applied here, so previews and thumbnails never show what export hides.
                self.state.try_borrow()?.doc.paint(&cr)?;
            }
            *cache = Some((crop, k, surface));
        }
        let (_, _, scene) = cache.as_ref().expect("scene cached above");
        self.refresh_thumbs(crop, (sw, sh), scene);
        frame.compose(scene)
    }

    /// Frames a small copy of the cached scene with every look. Runs once per crop, not per
    /// repaint or per tweak, and never renders the document again.
    fn refresh_thumbs(&self, crop: Option<Crop>, full: (u32, u32), scene: &ImageSurface) {
        if self
            .thumbs
            .borrow()
            .as_ref()
            .is_some_and(|(cached, _)| *cached == crop)
        {
            return;
        }
        let built = (|| -> Result<Vec<Option<ImageSurface>>> {
            let (sw, sh) = (scene.width(), scene.height());
            let t = (THUMB_SIDE / f64::from(sw.max(sh))).min(1.0);
            let side = |v: i32| ((f64::from(v) * t).round() as i32).max(1);
            let (tw, th) = (side(sw), side(sh));
            let small = ImageSurface::create(Format::ARgb32, tw, th)?;
            {
                let cr = Context::new(&small)?;
                cr.scale(f64::from(tw) / f64::from(sw), f64::from(th) / f64::from(sh));
                let pattern = SurfacePattern::create(scene);
                pattern.set_filter(Filter::Best);
                cr.set_source(&pattern)?;
                cr.paint()?;
            }
            let k = f64::from(tw) / f64::from(full.0);
            Ok(LOOKS
                .iter()
                .map(|look| {
                    look.frame(full)
                        .scaled(k)
                        .compose(&small)
                        .map_err(|e| eprintln!("Cannot draw {} thumbnail: {e}", look.name))
                        .ok()
                })
                .collect())
        })();
        *self.thumbs.borrow_mut() = Some((
            crop,
            built.unwrap_or_else(|error| {
                eprintln!("Cannot draw look thumbnails: {error}");
                Vec::new()
            }),
        ));
        for (_, area) in &self.look_cards {
            area.queue_draw();
        }
    }

    fn draw_thumb(&self, i: usize, cr: &Context, w: i32, h: i32) {
        let thumbs = self.thumbs.borrow();
        let Some(Some(image)) = thumbs.as_ref().and_then(|(_, all)| all.get(i)) else {
            return;
        };
        let (iw, ih) = (f64::from(image.width()), f64::from(image.height()));
        let s = (f64::from(w) / iw).min(f64::from(h) / ih);
        cr.translate((f64::from(w) - iw * s) / 2.0, (f64::from(h) - ih * s) / 2.0);
        cr.scale(s, s);
        let drawn = (|| -> Result<()> {
            cr.rectangle(0.0, 0.0, iw, ih);
            cr.clip();
            checker(cr, iw, ih, 5.0 / s)?;
            cr.set_source_surface(image, 0.0, 0.0)?;
            cr.paint()?;
            Ok(())
        })();
        if let Err(error) = drawn {
            eprintln!("Cannot draw look thumbnail: {error}");
        }
    }

    fn draw(&self, cr: &Context, w: i32, h: i32) {
        let preview = self.preview.borrow();
        let Some(image) = preview.as_ref() else {
            return;
        };
        let (pw, ph) = (f64::from(image.width()), f64::from(image.height()));
        // Small outputs are enlarged a little so the preview stays the focus of the dialog.
        let s = ((f64::from(w) - 32.0) / pw)
            .min((f64::from(h) - 32.0) / ph)
            .clamp(0.01, 2.0);
        cr.translate((f64::from(w) - pw * s) / 2.0, (f64::from(h) - ph * s) / 2.0);
        cr.scale(s, s);
        let drawn = (|| -> Result<()> {
            cr.rectangle(0.0, 0.0, pw, ph);
            cr.clip();
            checker(cr, pw, ph, 8.0 / s)?;
            cr.set_source_surface(image, 0.0, 0.0)?;
            cr.paint()?;
            Ok(())
        })();
        if let Err(error) = drawn {
            eprintln!("Cannot draw designer preview: {error}");
        }
    }

    /// Rebuilds the drop-down from `saved`, selecting `select` or else the first entry.
    fn fill_saved(&self, select: Option<&str>) {
        let file = self.saved.borrow();
        let names: Vec<&str> = file.presets.iter().map(|p| p.name.as_str()).collect();
        self.saved_names
            .splice(0, self.saved_names.n_items(), &names);
        let at = select
            .and_then(|s| file.find(s))
            .and_then(|p| file.presets.iter().position(|q| q.name == p.name))
            .unwrap_or(0);
        self.saved_pick.set_selected(if names.is_empty() {
            gtk::INVALID_LIST_POSITION
        } else {
            at as u32
        });
        drop(file);
        self.saved_sensitivity();
    }

    fn load_saved(&self) {
        match presets::load() {
            Ok(file) => {
                self.saved_broken.set(false);
                *self.saved.borrow_mut() = file;
                self.saved_note.set_text("");
            }
            Err(error) => {
                self.saved_broken.set(true);
                *self.saved.borrow_mut() = PresetFile::default();
                self.saved_note.set_text(&format!("{error:#}"));
            }
        }
        self.fill_saved(None);
    }

    fn picked_saved(&self) -> Option<Preset> {
        let at = self.saved_pick.selected();
        (at != gtk::INVALID_LIST_POSITION)
            .then(|| self.saved.borrow().presets.get(at as usize).cloned())
            .flatten()
    }

    fn saved_sensitivity(&self) {
        let picked = self.picked_saved().is_some();
        self.saved_apply.set_sensitive(picked);
        self.saved_delete.set_sensitive(picked);
        self.saved_save.set_sensitive(!self.saved_broken.get());
        self.saved_name.set_sensitive(!self.saved_broken.get());
    }

    /// Only the frame; the chosen canvas format, output size and crop stay as they are.
    fn apply_saved(&self) {
        let Some(preset) = self.picked_saved() else {
            return;
        };
        {
            let mut p = self.pres.borrow_mut();
            let (canvas_size, output_size) = (p.canvas_size, p.output_size);
            *p = Presentation {
                canvas_size,
                output_size,
                ..preset.presentation
            };
        }
        self.load();
        self.update();
        self.saved_note
            .set_text(&format!("Loaded “{}”", preset.name));
    }

    fn save_saved(&self) {
        let pres = self.pres.borrow().clone();
        let preset = match self.state.try_borrow() {
            Ok(state) => Preset::from_parts(self.saved_name.text().as_str(), &state, pres),
            Err(error) => Err(error.into()),
        };
        let name = preset.as_ref().map(|p| p.name.clone()).unwrap_or_default();
        match preset.and_then(|p| presets::update(|f| f.insert(p))) {
            Ok((replaced, file)) => {
                *self.saved.borrow_mut() = file;
                self.fill_saved(Some(&name));
                self.saved_note.set_text(&format!(
                    "{} “{name}”",
                    if replaced { "Replaced" } else { "Saved" }
                ));
            }
            Err(error) => self.saved_note.set_text(&format!("{error:#}")),
        }
    }

    fn delete_saved(self: &Rc<Self>) {
        let (Some(preset), Some(window)) = (self.picked_saved(), self.window.upgrade()) else {
            return;
        };
        let weak = Rc::downgrade(self);
        let name = preset.name.clone();
        presets::confirm_delete(&window, &name, move || {
            let Some(ui) = weak.upgrade() else { return };
            match presets::update(|f| Ok(f.remove(&preset.name))) {
                Ok((_, file)) => {
                    *ui.saved.borrow_mut() = file;
                    ui.fill_saved(None);
                    ui.saved_note
                        .set_text(&format!("Deleted “{}”", preset.name));
                }
                Err(error) => ui.saved_note.set_text(&format!("{error:#}")),
            }
        });
    }

    fn apply(&self) {
        let pres = self.pres.borrow().clone();
        let result = match self.state.try_borrow_mut() {
            Ok(mut state) => state.doc.set_presentation_and_crop(pres, self.crop.get()),
            Err(error) => Err(error.into()),
        };
        match result {
            Ok(()) => {
                (self.refresh)();
                self.close();
            }
            Err(error) => self.status.set_text(&format!("{error:#}")),
        }
    }
}

/// Window size for the monitor the parent is on: roomy on desktops, still fully on a laptop.
fn window_size(parent: &gtk::ApplicationWindow) -> (i32, i32) {
    parent
        .surface()
        .and_then(|surface| gtk::prelude::WidgetExt::display(parent).monitor_at_surface(&surface))
        .map_or((1120, 720), |monitor| {
            let g = monitor.geometry();
            (
                (g.width() - 80).clamp(640, 1200),
                (g.height() - 120).clamp(480, 780),
            )
        })
}

pub fn open(parent: &gtk::ApplicationWindow, state: Rc<RefCell<State>>, refresh: Rc<dyn Fn()>) {
    let (size, pres, crop) = {
        let s = state.borrow();
        (
            (s.doc.width, s.doc.height),
            s.doc.presentation.clone(),
            s.doc.crop,
        )
    };
    let (width, height) = window_size(parent);
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Designer")
        .default_width(width)
        .default_height(height)
        .build();
    window.add_css_class("sharpshot");
    window.set_titlebar(Some(&gtk::HeaderBar::new()));

    let area = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .content_width(260)
        .content_height(260)
        .accessible_role(gtk::AccessibleRole::Img)
        .build();
    area.add_css_class("canvas");
    named(
        &area,
        "Designer preview",
        "Live preview of the exported image",
    );
    let error = caption("", "dimension-label");
    error.set_wrap(true);
    error.set_xalign(0.5);
    error.set_justify(gtk::Justification::Center);
    error.set_max_width_chars(48);
    error.set_halign(gtk::Align::Center);
    error.set_valign(gtk::Align::Center);
    error.set_can_target(false);
    error.set_visible(false);
    let status = caption("", "dimension-label");
    status.set_hexpand(true);
    status.set_ellipsize(EllipsizeMode::End);
    let look_state = caption("Custom", "dimension-label");
    look_state.set_ellipsize(EllipsizeMode::End);
    look_state.set_max_width_chars(14);
    look_state.set_margin_end(10);
    look_state.set_tooltip_text(Some(
        "The look in use; Custom once the frame is edited by hand",
    ));
    let canvas_note = caption("", "dimension-label");
    canvas_note.set_wrap(true);
    canvas_note.set_max_width_chars(32);
    let output = |name: &str| spin(1.0, f64::from(MAX_DIM), name, &format!("{name} in pixels"));
    let kinds = [
        (
            "Transparent",
            "No background fill; the canvas stays transparent",
        ),
        ("Solid", "One background colour"),
        ("Gradient", "Diagonal blend between two colours"),
    ]
    .map(|(name, tip)| {
        let button = gtk::ToggleButton::with_label(name);
        button.add_css_class("tool-button");
        named(&button, &format!("{name} background"), tip);
        button
    });
    kinds[1].set_group(Some(&kinds[0]));
    kinds[2].set_group(Some(&kinds[0]));
    let names = gtk::StringList::new(&[""; 0]);
    let ui = Rc::new(Ui {
        window: window.downgrade(),
        state,
        refresh,
        size,
        pres: RefCell::new(pres),
        crop: Cell::new(crop),
        look: Cell::new(None),
        guard: Cell::new(false),
        natural: Cell::new(size),
        scene: RefCell::new(None),
        thumbs: RefCell::new(None),
        preview: RefCell::new(None),
        look_cards: LOOKS.iter().map(look_card).collect(),
        look_state,
        format_cards: FORMATS.iter().map(|f| format_card(f, size)).collect(),
        canvas_note,
        bg_kind: kinds,
        padding: spin(
            0.0,
            f64::from(MAX_PADDING),
            "Padding",
            "Space around the screenshot in source pixels; shrinks with the frame when a canvas format fits it",
        ),
        border: spin(
            0.0,
            f64::from(MAX_BORDER),
            "Border",
            "Border width in source pixels; 0 for none",
        ),
        border_color: color_button("Border color", "Border color", [1.0, 1.0, 1.0, 1.0]),
        color1: color_button(
            "Background color",
            "Background color",
            [0.13, 0.55, 0.95, 1.0],
        ),
        color2: color_button(
            "Second gradient color",
            "Second gradient color",
            [0.40, 0.85, 0.90, 1.0],
        ),
        radius: spin(
            0.0,
            MAX_RADIUS,
            "Corner radius",
            "Corner radius in source pixels; 0 for square corners",
        ),
        shadow: gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0),
        lock: gtk::CheckButton::with_label("Lock aspect ratio"),
        out_w: output("Output width"),
        out_h: output("Output height"),
        crop_spins: [
            spin(0.0, 1.0, "Crop X", "Left edge of the crop in pixels"),
            spin(0.0, 1.0, "Crop Y", "Top edge of the crop in pixels"),
            spin(1.0, 1.0, "Crop width", "Crop width in pixels"),
            spin(1.0, 1.0, "Crop height", "Crop height in pixels"),
        ],
        status,
        error,
        apply: chrome::menu_button(Icon::Check, "Apply", "Apply as one undo step"),
        area: area.clone(),
        saved: RefCell::new(PresetFile::default()),
        saved_broken: Cell::new(false),
        saved_names: names.clone(),
        saved_pick: gtk::DropDown::new(Some(names), gtk::Expression::NONE),
        saved_name: gtk::Entry::builder()
            .placeholder_text("Name this frame")
            .max_length(presets::MAX_NAME as i32)
            .hexpand(true)
            .build(),
        saved_apply: chrome::button(
            Icon::Check,
            "Use saved frame",
            "Load the selected frame into this designer; Apply still commits it",
        ),
        saved_delete: chrome::button(
            Icon::Delete,
            "Delete saved frame",
            "Delete the selected saved frame",
        ),
        saved_save: chrome::button(
            Icon::Save,
            "Save frame",
            "Save this frame and the current pen defaults under this name; the same name replaces",
        ),
        saved_note: caption("", "dimension-label"),
    });
    let weak = Rc::downgrade(&ui);
    ui.shadow.set_draw_value(false);
    ui.shadow.set_hexpand(true);
    ui.shadow.set_width_request(150);
    named(
        &ui.shadow,
        "Shadow",
        "Shadow strength; scales with the frame when a canvas format fits it",
    );
    // A saved output that is not the canvas ratio opens unlocked, so the preview shows it as saved.
    let lock = {
        let p = ui.pres.borrow();
        let scene = ui.scene_size();
        let mut plain = p.clone();
        plain.output_size = None;
        match (p.output_size, plain.dimensions(scene.0, scene.1)) {
            (Some([w, h]), Ok((nw, nh))) => scale_side(w, nh, nw) == h,
            _ => true,
        }
    };
    ui.lock.set_active(lock);
    ui.lock.set_tooltip_text(Some(
        "On: changing one output side adjusts the other. Off: any width and height, which stretches the image",
    ));
    ui.apply.add_css_class("export-button");
    ui.saved_note.set_wrap(true);
    ui.saved_note.set_max_width_chars(32);
    ui.saved_pick.set_enable_search(false);
    named(&ui.saved_name, "Frame name", "Name for this reusable frame");
    named(&ui.saved_pick, "Saved frames", "Choose a saved frame");
    ui.saved_apply
        .connect_clicked(hook_click(&ui, Ui::apply_saved));
    ui.saved_delete.connect_clicked({
        let weak = weak.clone();
        move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.delete_saved();
            }
        }
    });
    ui.saved_save
        .connect_clicked(hook_click(&ui, Ui::save_saved));
    {
        let h = hook(&ui, Ui::save_saved);
        ui.saved_name.connect_activate(move |_| h());
        let h = hook(&ui, Ui::saved_sensitivity);
        ui.saved_pick.connect_selected_notify(move |_| h());
    }

    for (i, (button, thumb)) in ui.look_cards.iter().enumerate() {
        button.connect_clicked({
            let weak = weak.clone();
            move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.use_look(i);
                }
            }
        });
        thumb.set_draw_func({
            let weak = weak.clone();
            move |_, cr, w, h| {
                if let Some(ui) = weak.upgrade() {
                    ui.draw_thumb(i, cr, w, h);
                }
            }
        });
    }
    for (i, button) in ui.format_cards.iter().enumerate() {
        let weak = weak.clone();
        button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.use_format(i);
            }
        });
    }

    let style = || hook(&ui, Ui::on_style);
    for spin in [&ui.padding, &ui.border, &ui.radius] {
        let h = style();
        spin.connect_value_changed(move |_| h());
    }
    let h = style();
    ui.shadow.connect_value_changed(move |_| h());
    for button in [&ui.border_color, &ui.color1, &ui.color2] {
        let h = style();
        button.connect_color_set(move |_| h());
    }
    for button in &ui.bg_kind {
        let h = style();
        button.connect_toggled(move |b| {
            if b.is_active() {
                h();
            }
        });
    }
    let h = hook(&ui, Ui::update);
    ui.lock.connect_toggled(move |_| h());
    ui.out_w.connect_value_changed({
        let weak = weak.clone();
        move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.on_output(true);
            }
        }
    });
    ui.out_h.connect_value_changed({
        let weak = weak.clone();
        move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.on_output(false);
            }
        }
    });
    for spin in &ui.crop_spins {
        let h = hook(&ui, Ui::on_crop);
        spin.connect_value_changed(move |_| h());
    }
    area.set_draw_func({
        let weak = weak.clone();
        move |_, cr, w, h| {
            if let Some(ui) = weak.upgrade() {
                ui.draw(cr, w, h);
            }
        }
    });

    // Looks: curated complete frames, then the user's own saved frames.
    let looks_panel = panel();
    let mut at = 0;
    for group in LOOKS.chunk_by(|a, b| a.group == b.group) {
        let block = section(group[0].group);
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(6)
            .column_homogeneous(true)
            .build();
        for (n, (button, _)) in ui.look_cards[at..at + group.len()].iter().enumerate() {
            grid.attach(button, n as i32 % 2, n as i32 / 2, 1, 1);
        }
        at += group.len();
        block.append(&grid);
        looks_panel.append(&block);
    }
    let saved = section("Saved frames");
    let pick = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    ui.saved_pick.set_hexpand(true);
    pick.append(&ui.saved_pick);
    pick.append(&ui.saved_apply);
    pick.append(&ui.saved_delete);
    saved.append(&pick);
    let store = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    store.append(&ui.saved_name);
    store.append(&ui.saved_save);
    saved.append(&store);
    saved.append(&ui.saved_note);
    looks_panel.append(&saved);

    // Canvas: where the framed screenshot sits, how large the file is, what part is shown.
    let canvas_panel = panel();
    let formats = section("Canvas format");
    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(6)
        .column_homogeneous(true)
        .build();
    for (n, button) in ui.format_cards.iter().enumerate() {
        grid.attach(button, n as i32 % 2, n as i32 / 2, 1, 1);
    }
    formats.append(&grid);
    formats.append(&ui.canvas_note);
    canvas_panel.append(&formats);
    let output_size = section("Output size");
    let natural = chrome::button(
        Icon::Fit,
        "Natural size",
        "Use the natural size of the canvas",
    );
    natural.connect_clicked(hook_click(&ui, Ui::natural_size));
    output_size.append(&row("Width", &[ui.out_w.upcast_ref()]));
    output_size.append(&row("Height", &[ui.out_h.upcast_ref()]));
    output_size.append(&row("", &[ui.lock.upcast_ref(), natural.upcast_ref()]));
    canvas_panel.append(&output_size);
    let crop_section = section("Crop");
    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(8)
        .build();
    for (i, (name, spin)) in ["X", "Y", "Width", "Height"]
        .iter()
        .zip(&ui.crop_spins)
        .enumerate()
    {
        let (col, line) = ((i as i32 % 2) * 2, i as i32 / 2);
        grid.attach(&caption(name, "field-label"), col, line, 1, 1);
        grid.attach(spin, col + 1, line, 1, 1);
    }
    crop_section.append(&grid);
    let clear = chrome::menu_button(Icon::ResetCrop, "Clear crop", "Remove the crop");
    clear.connect_clicked(hook_click(&ui, Ui::clear_crop));
    clear.set_halign(gtk::Align::Start);
    crop_section.append(&clear);
    canvas_panel.append(&crop_section);

    // Adjust: every manual control; any change turns the look into Custom.
    let adjust_panel = panel();
    let background = section("Background");
    let kinds = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    kinds.set_homogeneous(true);
    for button in &ui.bg_kind {
        kinds.append(button);
    }
    background.append(&kinds);
    background.append(&row("Color", &[ui.color1.upcast_ref()]));
    background.append(&row("Gradient end", &[ui.color2.upcast_ref()]));
    adjust_panel.append(&background);
    let frame = section("Frame");
    frame.append(&row("Padding", &[ui.padding.upcast_ref()]));
    frame.append(&row(
        "Border",
        &[ui.border.upcast_ref(), ui.border_color.upcast_ref()],
    ));
    frame.append(&row("Corner radius", &[ui.radius.upcast_ref()]));
    frame.append(&row("Shadow", &[ui.shadow.upcast_ref()]));
    adjust_panel.append(&frame);

    let tabs = gtk::Notebook::new();
    tabs.add_css_class("designer-tabs");
    tabs.set_show_border(false);
    tabs.set_vexpand(true);
    for (title, content) in [
        ("Looks", &looks_panel),
        ("Canvas", &canvas_panel),
        ("Adjust", &adjust_panel),
    ] {
        let content = page(content);
        tabs.append_page(&content, Some(&gtk::Label::new(Some(title))));
        tabs.page(&content).set_tab_expand(true);
    }
    tabs.set_action_widget(&ui.look_state, gtk::PackType::End);
    let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
    side.set_width_request(360);
    side.set_hexpand(false);
    side.append(&tabs);

    // Actions
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.add_css_class("context-bar");
    let reset = chrome::menu_button(
        Icon::Undo,
        "Reset",
        "Reset background, frame, canvas, size and crop",
    );
    reset.connect_clicked(hook_click(&ui, Ui::reset));
    let cancel = chrome::menu_button(
        Icon::Close,
        "Cancel",
        "Close without changing the screenshot",
    );
    cancel.connect_clicked(hook_click(&ui, Ui::close));
    reset.set_hexpand(false);
    cancel.set_hexpand(false);
    ui.apply.set_hexpand(false);
    ui.apply.connect_clicked(hook_click(&ui, Ui::apply));
    actions.append(&reset);
    actions.append(&ui.status);
    actions.append(&cancel);
    actions.append(&ui.apply);

    let preview = gtk::Overlay::new();
    preview.set_hexpand(true);
    preview.set_vexpand(true);
    preview.set_child(Some(&area));
    preview.add_overlay(&ui.error);
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.set_vexpand(true);
    body.append(&preview);
    body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    body.append(&side);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&body);
    root.append(&actions);
    window.set_child(Some(&root));

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let weak = weak.clone();
        move |_, key, _, _| {
            if key != gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);
    // The window owns the dialog state; dropping the window drops it.
    let keep = ui.clone();
    window.connect_destroy(move |_| {
        let _ = &keep;
    });

    ui.load_saved();
    ui.load();
    ui.update();
    window.present();
}

fn hook_click(ui: &Rc<Ui>, f: fn(&Ui)) -> impl Fn(&gtk::Button) + 'static {
    let h = hook(ui, f);
    move |_| h()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_values_stay_inside_the_image() {
        let size = (100, 50);
        assert_eq!(clamp_crop([0, 0, 100, 50], size), [0, 0, 100, 50]);
        assert_eq!(clamp_crop([500, 500, 9, 9], size), [99, 49, 1, 1]);
        assert_eq!(clamp_crop([90, 10, 40, 60], size), [90, 10, 10, 40]);
        assert_eq!(clamp_crop([0, 0, 0, 0], size), [0, 0, 1, 1]);
    }

    #[test]
    fn full_image_crop_means_no_crop() {
        assert_eq!(crop_of([0, 0, 100, 50], (100, 50)), None);
        assert_eq!(
            crop_of([1, 2, 30, 40], (100, 50)),
            Some(Crop {
                x: 1,
                y: 2,
                width: 30,
                height: 40
            })
        );
    }

    #[test]
    fn locked_aspect_follows_the_natural_ratio() {
        assert_eq!(scale_side(960, 540, 1920), 270);
        assert_eq!(scale_side(1, 1, 3), 1, "never collapses to zero");
        assert_eq!(scale_side(u32::MAX, u32::MAX, 1), MAX_DIM);
    }

    #[test]
    fn preview_fits_the_pane_and_keeps_its_scale_across_small_edits() {
        for canvas in [(300, 200), (1920, 1080), (1080, 1920), (32000, 100)] {
            let k = preview_scale(canvas);
            assert!(k <= 1.0, "{canvas:?}");
            assert!(f64::from(canvas.0) * k <= PREVIEW_W + 0.5, "{canvas:?}");
            assert!(f64::from(canvas.1) * k <= PREVIEW_H + 0.5, "{canvas:?}");
        }
        assert_eq!(preview_scale((1920, 1080)), preview_scale((1921, 1080)));
    }

    #[test]
    fn final_preview_rasters_are_bounded_without_undersampling_small_exports() {
        let scene = ImageSurface::create(Format::ARgb32, 1000, 650).unwrap();
        for output in [(4096, 4096), (8192, 4096), (4096, 8192), (1, 32000)] {
            let pres = Presentation {
                output_size: Some([output.0, output.1]),
                ..Default::default()
            };
            let (_, frame) = preview_frame(&pres, (1000, 650), output).unwrap();
            let size = frame.dimensions(1000, 650).unwrap();
            assert!(size.0 > 0 && f64::from(size.0) <= PREVIEW_W);
            assert!(size.1 > 0 && f64::from(size.1) <= PREVIEW_H);
            let raster = frame.compose(&scene).unwrap();
            assert_eq!((raster.width() as u32, raster.height() as u32), size);
        }
        let pres = Presentation {
            output_size: Some([320, 180]),
            ..Default::default()
        };
        let (_, frame) = preview_frame(&pres, (4000, 2600), (320, 180)).unwrap();
        let raster = frame.compose(&scene).unwrap();
        assert_eq!((raster.width(), raster.height()), (320, 180));
    }

    #[test]
    fn fixed_canvases_do_not_force_large_scene_caches_or_shrink_tiny_sources() {
        for source in [(3840, 2160), (8000, 8000), (100, 65)] {
            let pres = Presentation {
                canvas_size: Some([1080, 1920]),
                padding: 64,
                shadow: 0.9,
                ..Default::default()
            };
            let (k, frame) = preview_frame(&pres, source, (1080, 1920)).unwrap();
            let (w, h) = (
                (f64::from(source.0) * k).round().max(1.) as i32,
                (f64::from(source.1) * k).round().max(1.) as i32,
            );
            assert!(f64::from(w) <= PREVIEW_W && f64::from(h) <= PREVIEW_H);
            if source == (100, 65) {
                assert_eq!((w, h), (100, 65));
            }
            let source = ImageSurface::create(Format::ARgb32, w, h).unwrap();
            let raster = frame.compose(&source).unwrap();
            assert!(f64::from(raster.width()) <= PREVIEW_W);
            assert!(f64::from(raster.height()) <= PREVIEW_H);
        }
    }
}
