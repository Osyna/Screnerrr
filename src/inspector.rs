//! Object inspector: numeric geometry, style, text and layout of the selected objects.
//!
//! `Inspection` is the read model (what each field shows, including "mixed" when the selected
//! objects disagree); `Inspector` is the side panel. Every control applies one undo step through
//! `Document`, touching only its own field, so editing one value never flattens another that
//! differs across the selection.

use crate::{
    document::{
        Align, Axis, Bounds, Document, Geometry, MAX_FONT, MAX_HEAD_SIZE, MAX_WIDTH, Mark,
        MarkEdit, MarkKind, TextAlign,
    },
    interaction::State,
};
use gtk::{
    cairo::{Context, LineCap, LineJoin},
    gdk,
    prelude::*,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// What one field shows for a selection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value<T> {
    /// No selected object has this field.
    Absent,
    Same(T),
    /// The objects differ; carries the first one's value.
    Mixed(T),
}

impl<T: Copy> Value<T> {
    fn of(items: impl IntoIterator<Item = T>, same: impl Fn(T, T) -> bool) -> Self {
        let mut items = items.into_iter();
        let Some(first) = items.next() else {
            return Self::Absent;
        };
        if items.all(|v| same(first, v)) {
            Self::Same(first)
        } else {
            Self::Mixed(first)
        }
    }
    pub fn first(self) -> Option<T> {
        match self {
            Self::Absent => None,
            Self::Same(v) | Self::Mixed(v) => Some(v),
        }
    }
    pub fn is_mixed(self) -> bool {
        matches!(self, Self::Mixed(_))
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.05
}

/// Read model of the inspector for one selection. Geometry is per object (a group is one
/// object); the style fields are per mark and skip marks that have no such field.
#[derive(Clone, Debug, PartialEq)]
pub struct Inspection {
    pub objects: usize,
    pub x: Value<f64>,
    pub y: Value<f64>,
    pub width: Value<f64>,
    pub height: Value<f64>,
    pub rotation: Value<f64>,
    pub rgb: Value<[f64; 3]>,
    pub alpha: Value<f64>,
    pub stroke: Value<f64>,
    pub tip: Value<f64>,
    pub radius: Value<f64>,
    pub dim: Value<f64>,
    /// Only for a single magnifier.
    pub zoom: Value<f64>,
    pub font: Value<f64>,
    pub box_width: Value<f64>,
    pub background: Value<bool>,
    pub bold: Value<bool>,
    pub italic: Value<bool>,
    pub align: Value<TextAlign>,
}

impl Inspection {
    pub fn of(doc: &Document, ids: &[u64]) -> Self {
        let units = doc.units(ids);
        let boxes: Vec<Bounds> = units
            .iter()
            .filter_map(|u| doc.selection_bounds(u))
            .collect();
        let marks: Vec<&Mark> = doc
            .expand_groups(ids)
            .into_iter()
            .filter_map(|id| doc.mark(id))
            .collect();
        let edge =
            |get: fn(&Bounds) -> f32| Value::of(boxes.iter().map(|b| f64::from(get(b))), near);
        let styled = || marks.iter().copied().filter(|m| !m.is_redaction());
        let texts = || {
            marks
                .iter()
                .copied()
                .filter(|m| matches!(m.kind, MarkKind::Text { .. }))
        };
        let text_kind = |m: &Mark| match m.kind {
            MarkKind::Text {
                box_width,
                background,
                ..
            } => Some((f64::from(box_width), background)),
            _ => None,
        };
        let spot = |m: &Mark| match m.kind {
            MarkKind::Spotlight { radius, dim } => Some((radius, dim)),
            _ => None,
        };
        let flag = |get: fn(&Mark) -> bool| Value::of(texts().map(get), |a, b| a == b);
        Self {
            objects: units.len(),
            x: edge(|b| b.x),
            y: edge(|b| b.y),
            width: edge(|b| b.width),
            height: edge(|b| b.height),
            rotation: Value::of(
                units
                    .iter()
                    .filter_map(|u| doc.rotation_degrees(*u.first()?)),
                near,
            ),
            rgb: Value::of(
                styled().map(|m| [m.color[0], m.color[1], m.color[2]]),
                |a, b| a.iter().zip(b).all(|(p, q)| (p - q).abs() < 2e-3),
            ),
            alpha: Value::of(styled().map(|m| m.color[3]), |a, b| (a - b).abs() < 2e-3),
            stroke: Value::of(
                styled()
                    .filter(|m| {
                        matches!(
                            m.kind,
                            MarkKind::Path
                                | MarkKind::Connector { .. }
                                | MarkKind::Step { .. }
                                | MarkKind::Magnifier { .. }
                                | MarkKind::Spotlight { .. }
                        )
                    })
                    .map(|m| m.width),
                near,
            ),
            tip: Value::of(
                marks
                    .iter()
                    .filter(|m| matches!(m.kind, MarkKind::Connector { .. }))
                    .map(|m| m.head_size),
                |a, b| (a - b).abs() < 1e-3,
            ),
            radius: Value::of(marks.iter().filter_map(|m| spot(m)).map(|s| s.0), near),
            dim: Value::of(marks.iter().filter_map(|m| spot(m)).map(|s| s.1), |a, b| {
                (a - b).abs() < 1e-3
            }),
            zoom: if units.len() == 1 {
                Value::of(
                    marks.iter().filter_map(|m| match m.kind {
                        MarkKind::Magnifier { zoom, .. } => Some(f64::from(zoom)),
                        _ => None,
                    }),
                    |a, b| (a - b).abs() < 1e-3,
                )
            } else {
                Value::Absent
            },
            font: Value::of(texts().filter_map(|m| doc.text_size(m.id)), near),
            box_width: Value::of(texts().filter_map(|m| text_kind(m)).map(|t| t.0), near),
            background: Value::of(texts().filter_map(|m| text_kind(m)).map(|t| t.1), |a, b| {
                a == b
            }),
            bold: flag(|m| m.text_style.bold),
            italic: flag(|m| m.text_style.italic),
            align: Value::of(texts().map(|m| m.text_style.align), |a, b| a == b),
        }
    }
}

/// A labelled row; hidden when the selection has no such field, tagged "mixed" when it differs.
struct Field {
    label: gtk::Label,
    name: &'static str,
    control: gtk::Widget,
}

impl Field {
    fn new(
        grid: &gtk::Grid,
        row: i32,
        name: &'static str,
        control: &impl IsA<gtk::Widget>,
    ) -> Self {
        let label = gtk::Label::new(Some(name));
        label.add_css_class("field-label");
        label.set_halign(gtk::Align::Start);
        control.set_hexpand(true);
        grid.attach(&label, 0, row, 1, 1);
        grid.attach(control, 1, row, 1, 1);
        Self {
            label,
            name,
            control: control.clone().upcast(),
        }
    }
    /// Returns whether the row is shown.
    fn show<T>(&self, v: &Value<T>) -> bool {
        let on = !matches!(v, Value::Absent);
        self.label.set_visible(on);
        self.control.set_visible(on);
        self.label.set_text(&if matches!(v, Value::Mixed(_)) {
            format!("{} · mixed", self.name)
        } else {
            self.name.to_owned()
        });
        on
    }
}

fn spin(min: f64, max: f64, step: f64, digits: u32, label: &str, tip: &str) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(min, max, step);
    s.set_digits(digits);
    s.set_numeric(true);
    s.set_tooltip_text(Some(tip));
    s.update_property(&[gtk::accessible::Property::Label(label)]);
    s
}

fn section(title: &str) -> (gtk::Box, gtk::Grid) {
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("title");
    heading.set_halign(gtk::Align::Start);
    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(8);
    frame.append(&heading);
    frame.append(&grid);
    frame.set_margin_top(10);
    (frame, grid)
}

#[derive(Clone, Copy)]
enum Glyph {
    Align(Align),
    Spread(Axis),
}

/// Lines then outlined boxes of a 20 × 20 layout icon.
fn glyph_shapes(g: Glyph) -> (Vec<[f64; 4]>, Vec<[f64; 4]>) {
    match g {
        Glyph::Align(Align::Left) => (
            vec![[3., 2., 3., 18.]],
            vec![[5., 5., 10., 4.], [5., 11., 6., 4.]],
        ),
        Glyph::Align(Align::Center) => (
            vec![[10., 2., 10., 18.]],
            vec![[4., 5., 12., 4.], [6., 11., 8., 4.]],
        ),
        Glyph::Align(Align::Right) => (
            vec![[17., 2., 17., 18.]],
            vec![[7., 5., 10., 4.], [11., 11., 6., 4.]],
        ),
        Glyph::Align(Align::Top) => (
            vec![[2., 3., 18., 3.]],
            vec![[5., 5., 4., 10.], [11., 5., 4., 6.]],
        ),
        Glyph::Align(Align::Middle) => (
            vec![[2., 10., 18., 10.]],
            vec![[5., 4., 4., 12.], [11., 6., 4., 8.]],
        ),
        Glyph::Align(Align::Bottom) => (
            vec![[2., 17., 18., 17.]],
            vec![[5., 7., 4., 10.], [11., 11., 4., 6.]],
        ),
        Glyph::Spread(Axis::Horizontal) => (
            vec![[3., 2., 3., 18.], [17., 2., 17., 18.]],
            vec![[8., 5., 4., 10.]],
        ),
        Glyph::Spread(Axis::Vertical) => (
            vec![[2., 3., 18., 3.], [2., 17., 18., 17.]],
            vec![[5., 8., 10., 4.]],
        ),
    }
}

fn paint_glyph(cr: &Context, g: Glyph) -> Result<(), gtk::cairo::Error> {
    let (lines, boxes) = glyph_shapes(g);
    for [x1, y1, x2, y2] in lines {
        cr.move_to(x1, y1);
        cr.line_to(x2, y2);
    }
    for [x, y, w, h] in boxes {
        cr.rectangle(x, y, w, h);
    }
    cr.stroke()
}

/// Layout buttons look like the toolbar's: borderless, drawn in the current text colour.
fn glyph_button(g: Glyph, name: &str, tip: &str) -> gtk::Button {
    let area = gtk::DrawingArea::builder()
        .content_width(20)
        .content_height(20)
        .can_target(false)
        .build();
    area.set_draw_func(move |area, cr, w, h| {
        let c = area.style_context().color();
        cr.set_source_rgba(
            c.red().into(),
            c.green().into(),
            c.blue().into(),
            c.alpha().into(),
        );
        cr.translate((f64::from(w) - 20.) / 2., (f64::from(h) - 20.) / 2.);
        cr.set_line_width(1.5);
        cr.set_line_cap(LineCap::Round);
        cr.set_line_join(LineJoin::Round);
        if let Err(error) = paint_glyph(cr, g) {
            eprintln!("Cannot draw inspector icon: {error}");
        }
    });
    let button = gtk::Button::builder()
        .child(&area)
        .tooltip_text(tip)
        .build();
    button.add_css_class("tool-button");
    button.update_property(&[gtk::accessible::Property::Label(name)]);
    button
}

const ALIGNS: [(Align, &str, &str); 6] = [
    (
        Align::Left,
        "Align left",
        "Align left edges (two or more objects)",
    ),
    (
        Align::Center,
        "Align horizontal centres",
        "Align horizontal centres (two or more objects)",
    ),
    (
        Align::Right,
        "Align right",
        "Align right edges (two or more objects)",
    ),
    (
        Align::Top,
        "Align top",
        "Align top edges (two or more objects)",
    ),
    (
        Align::Middle,
        "Align vertical centres",
        "Align vertical centres (two or more objects)",
    ),
    (
        Align::Bottom,
        "Align bottom",
        "Align bottom edges (two or more objects)",
    ),
];

pub struct Inspector {
    pub widget: gtk::Box,
    state: Rc<RefCell<State>>,
    changed: Rc<dyn Fn()>,
    /// Set while code writes the controls, so their handlers stay quiet.
    syncing: Cell<bool>,
    hint: gtk::Label,
    geometry: gtk::Box,
    arrange: gtk::Box,
    style: gtk::Box,
    text: gtk::Box,
    x: (Field, gtk::SpinButton),
    y: (Field, gtk::SpinButton),
    width: (Field, gtk::SpinButton),
    height: (Field, gtk::SpinButton),
    rotation: (Field, gtk::SpinButton),
    color: (Field, gtk::ColorButton),
    opacity: (Field, gtk::SpinButton),
    stroke: (Field, gtk::SpinButton),
    tip: (Field, gtk::SpinButton),
    radius: (Field, gtk::SpinButton),
    dim: (Field, gtk::SpinButton),
    zoom: (Field, gtk::SpinButton),
    font: (Field, gtk::SpinButton),
    box_width: (Field, gtk::SpinButton),
    bold: (Field, gtk::CheckButton),
    italic: (Field, gtk::CheckButton),
    background: (Field, gtk::CheckButton),
    align: (Field, gtk::DropDown),
    align_buttons: Vec<gtk::Button>,
    spread_buttons: Vec<gtk::Button>,
}

impl Inspector {
    /// `changed` runs after every edit attempt (successful or not) so the owner can redraw and
    /// call `refresh`; the state is no longer borrowed then.
    pub fn new(state: Rc<RefCell<State>>, changed: Rc<dyn Fn()>) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("inspector");
        widget.set_width_request(280);
        widget.update_property(&[gtk::accessible::Property::Label("Object inspector")]);
        let title = gtk::Label::new(Some("Inspector"));
        title.add_css_class("title");
        title.set_halign(gtk::Align::Start);
        title.set_margin_top(8);
        title.set_margin_start(12);
        let hint = gtk::Label::new(Some(
            "Select one or more objects to edit their position, size and style.",
        ));
        hint.add_css_class("field-label");
        hint.set_wrap(true);
        hint.set_xalign(0.);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.set_margin_start(12);
        body.set_margin_end(12);
        body.set_margin_bottom(12);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&body)
            .build();
        widget.append(&title);
        widget.append(&scroller);
        body.append(&hint);

        const COORD: f64 = 1.0e6;
        let (geometry, grid) = section("Position & size");
        let x = spin(
            -COORD,
            COORD,
            1.,
            1,
            "X position",
            "Left edge in image pixels",
        );
        let y = spin(
            -COORD,
            COORD,
            1.,
            1,
            "Y position",
            "Top edge in image pixels",
        );
        let width = spin(
            1.,
            COORD,
            1.,
            1,
            "Width",
            "Width in image pixels (text keeps its proportions)",
        );
        let height = spin(
            1.,
            COORD,
            1.,
            1,
            "Height",
            "Height in image pixels (text keeps its proportions)",
        );
        let rotation = spin(
            -180.,
            180.,
            1.,
            1,
            "Rotation",
            "Rotation in degrees about the centre",
        );
        let x = (Field::new(&grid, 0, "X", &x), x);
        let y = (Field::new(&grid, 1, "Y", &y), y);
        let width = (Field::new(&grid, 2, "Width", &width), width);
        let height = (Field::new(&grid, 3, "Height", &height), height);
        let rotation = (Field::new(&grid, 4, "Rotation °", &rotation), rotation);
        body.append(&geometry);

        let (arrange, grid) = section("Arrange");
        let row = |buttons: &[&gtk::Button]| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            buttons.iter().for_each(|b| row.append(*b));
            row
        };
        let align_buttons: Vec<gtk::Button> = ALIGNS
            .iter()
            .map(|(how, name, tip)| glyph_button(Glyph::Align(*how), name, tip))
            .collect();
        let spread_buttons = vec![
            glyph_button(
                Glyph::Spread(Axis::Horizontal),
                "Distribute horizontally",
                "Equal horizontal gaps (three or more objects)",
            ),
            glyph_button(
                Glyph::Spread(Axis::Vertical),
                "Distribute vertically",
                "Equal vertical gaps (three or more objects)",
            ),
        ];
        grid.attach(&row(&align_buttons.iter().collect::<Vec<_>>()), 0, 0, 1, 1);
        grid.attach(&row(&spread_buttons.iter().collect::<Vec<_>>()), 0, 1, 1, 1);
        body.append(&arrange);

        let (style, grid) = section("Style");
        let color = gtk::ColorButton::new();
        color.set_title("Object colour");
        color.set_use_alpha(false);
        color.set_tooltip_text(Some("Colour; each object keeps its own opacity"));
        color.update_property(&[gtk::accessible::Property::Label("Object colour")]);
        let opacity = spin(
            0.,
            100.,
            1.,
            0,
            "Opacity",
            "Opacity in percent; each object keeps its own colour",
        );
        let stroke = spin(
            1.,
            MAX_WIDTH,
            1.,
            1,
            "Line width",
            "Line width or marker size in pixels",
        );
        let tip = spin(
            0.,
            MAX_HEAD_SIZE,
            0.25,
            2,
            "Arrow tip",
            "Arrowhead size (0 = plain line)",
        );
        let radius = spin(
            0.,
            10_000.,
            1.,
            0,
            "Corner radius",
            "Spotlight corner radius in pixels",
        );
        let dim = spin(
            0.,
            100.,
            1.,
            0,
            "Dimming",
            "How strongly everything outside the spotlights is dimmed, in percent",
        );
        let zoom = spin(
            1.,
            8.,
            0.25,
            2,
            "Lens zoom",
            "Magnification of the selected lens",
        );
        let color = (Field::new(&grid, 0, "Colour", &color), color);
        let opacity = (Field::new(&grid, 1, "Opacity %", &opacity), opacity);
        let stroke = (Field::new(&grid, 2, "Line width", &stroke), stroke);
        let tip = (Field::new(&grid, 3, "Arrow tip", &tip), tip);
        let radius = (Field::new(&grid, 4, "Corner radius", &radius), radius);
        let dim = (Field::new(&grid, 5, "Dimming %", &dim), dim);
        let zoom = (Field::new(&grid, 6, "Lens zoom", &zoom), zoom);
        body.append(&style);

        let (text, grid) = section("Text");
        let font = spin(
            1.,
            MAX_FONT,
            1.,
            1,
            "Text size",
            "Text size in image pixels",
        );
        let box_width = spin(
            0.,
            COORD,
            1.,
            0,
            "Wrap width",
            "Wrapping width in pixels; 0 = no wrapping, otherwise at least 16",
        );
        let check = |label: &str, tip: &str| {
            let c = gtk::CheckButton::with_label(label);
            c.set_tooltip_text(Some(tip));
            c
        };
        let bold = check("Bold", "Bold text");
        let italic = check("Italic", "Italic text");
        let background = check("Background", "Readable label background behind the text");
        let align = gtk::DropDown::from_strings(&["Left", "Centre", "Right"]);
        align.set_tooltip_text(Some("Alignment of the lines inside the text box"));
        align.update_property(&[gtk::accessible::Property::Label("Text alignment")]);
        let font = (Field::new(&grid, 0, "Size", &font), font);
        let box_width = (Field::new(&grid, 1, "Wrap width", &box_width), box_width);
        let bold = (Field::new(&grid, 2, "Weight", &bold), bold);
        let italic = (Field::new(&grid, 3, "Slant", &italic), italic);
        let align = (Field::new(&grid, 4, "Align", &align), align);
        let background = (Field::new(&grid, 5, "Fill", &background), background);
        body.append(&text);

        let this = Rc::new(Self {
            widget,
            state,
            changed,
            syncing: Cell::new(false),
            hint,
            geometry,
            arrange,
            style,
            text,
            x,
            y,
            width,
            height,
            rotation,
            color,
            opacity,
            stroke,
            tip,
            radius,
            dim,
            zoom,
            font,
            box_width,
            bold,
            italic,
            background,
            align,
            align_buttons,
            spread_buttons,
        });
        this.wire();
        for part in [&this.geometry, &this.arrange, &this.style, &this.text] {
            part.set_visible(false);
        }
        this
    }

    /// Runs `edit` on the state as one gesture of the user, then lets the owner redraw. Ignored
    /// while the panel itself writes controls, while a caption is open, or if the state is busy.
    fn apply(&self, edit: impl FnOnce(&mut State)) {
        if self.syncing.get() {
            return;
        }
        {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            if s.text_draft.is_some() {
                return;
            }
            edit(&mut s);
        }
        (self.changed)();
    }

    fn spin(self: &Rc<Self>, spin: &gtk::SpinButton, edit: impl Fn(&mut State, f64) + 'static) {
        let (weak, edit) = (Rc::downgrade(self), Rc::new(edit));
        // Enter also applies, so retyping the first value of a mixed field unifies the rest.
        let run = move |spin: &gtk::SpinButton| {
            if let Some(this) = weak.upgrade() {
                let (edit, value) = (edit.clone(), spin.value());
                this.apply(move |s| edit(s, value));
            }
        };
        spin.connect_value_changed(run.clone());
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak_spin = spin.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter) {
                if let Some(spin) = weak_spin.upgrade() {
                    spin.update();
                    run(&spin);
                }
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
        spin.add_controller(keys);
    }

    fn check(self: &Rc<Self>, check: &gtk::CheckButton, edit: impl Fn(&mut State, bool) + 'static) {
        let weak = Rc::downgrade(self);
        check.connect_toggled(move |c| {
            if let Some(this) = weak.upgrade() {
                let active = c.is_active();
                this.apply(|s| edit(s, active));
            }
        });
    }

    fn wire(self: &Rc<Self>) {
        fn geometry(s: &mut State, g: Geometry) {
            s.doc.set_geometry(&s.selected, g);
        }
        fn edit(s: &mut State, e: MarkEdit) {
            s.doc.edit_marks(&s.selected, &e);
        }
        self.spin(&self.x.1, |s, v| {
            geometry(
                s,
                Geometry {
                    x: Some(v),
                    ..Geometry::default()
                },
            )
        });
        self.spin(&self.y.1, |s, v| {
            geometry(
                s,
                Geometry {
                    y: Some(v),
                    ..Geometry::default()
                },
            )
        });
        self.spin(&self.width.1, |s, v| {
            geometry(
                s,
                Geometry {
                    width: Some(v),
                    ..Geometry::default()
                },
            )
        });
        self.spin(&self.height.1, |s, v| {
            geometry(
                s,
                Geometry {
                    height: Some(v),
                    ..Geometry::default()
                },
            )
        });
        self.spin(&self.rotation.1, |s, v| {
            s.doc.set_rotation(&s.selected, v);
        });
        self.spin(&self.opacity.1, |s, v| s.set_opacity(v / 100.));
        self.spin(&self.stroke.1, |s, v| s.set_style(None, Some(v)));
        self.spin(&self.tip.1, |s, v| s.set_tip(v));
        self.spin(&self.radius.1, |s, v| {
            edit(
                s,
                MarkEdit {
                    radius: Some(v),
                    ..MarkEdit::default()
                },
            )
        });
        self.spin(&self.dim.1, |s, v| {
            edit(
                s,
                MarkEdit {
                    dim: Some(v / 100.),
                    ..MarkEdit::default()
                },
            )
        });
        self.spin(&self.zoom.1, |s, v| {
            let ids = s.doc.expand_groups(&s.selected);
            if let [id] = ids[..] {
                s.doc.set_magnifier_zoom(id, v as f32);
            }
        });
        self.spin(&self.font.1, |s, v| s.set_font(v));
        self.spin(&self.box_width.1, |s, v| {
            edit(
                s,
                MarkEdit {
                    box_width: Some(v as f32),
                    ..MarkEdit::default()
                },
            )
        });
        self.check(&self.bold.1, |s, on| {
            s.text_style.bold = on;
            edit(
                s,
                MarkEdit {
                    bold: Some(on),
                    ..MarkEdit::default()
                },
            );
        });
        self.check(&self.italic.1, |s, on| {
            s.text_style.italic = on;
            edit(
                s,
                MarkEdit {
                    italic: Some(on),
                    ..MarkEdit::default()
                },
            );
        });
        self.check(&self.background.1, |s, on| {
            edit(
                s,
                MarkEdit {
                    background: Some(on),
                    ..MarkEdit::default()
                },
            )
        });

        let weak = Rc::downgrade(self);
        self.color.1.connect_color_set(move |b| {
            if let Some(this) = weak.upgrade() {
                let c: gdk::RGBA = b.rgba();
                let rgb = [c.red(), c.green(), c.blue()].map(f64::from);
                this.apply(|s| s.set_rgb(rgb));
            }
        });
        let weak = Rc::downgrade(self);
        self.align.1.connect_selected_notify(move |d| {
            let align = match d.selected() {
                0 => TextAlign::Left,
                1 => TextAlign::Center,
                2 => TextAlign::Right,
                _ => return,
            };
            if let Some(this) = weak.upgrade() {
                this.apply(|s| {
                    s.text_style.align = align;
                    edit(
                        s,
                        MarkEdit {
                            align: Some(align),
                            ..MarkEdit::default()
                        },
                    );
                });
            }
        });
        for ((how, ..), button) in ALIGNS.iter().zip(&self.align_buttons) {
            let (weak, how) = (Rc::downgrade(self), *how);
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.apply(|s| {
                        s.doc.align_marks(&s.selected, how);
                    });
                }
            });
        }
        for (axis, button) in [Axis::Horizontal, Axis::Vertical]
            .into_iter()
            .zip(&self.spread_buttons)
        {
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.apply(|s| {
                        s.doc.distribute_marks(&s.selected, axis);
                    });
                }
            });
        }
    }

    /// Rewrites every control from the state. Cheap: reads marks only, never copies the image.
    pub fn refresh(&self, state: &State) {
        let i = Inspection::of(&state.doc, &state.selected);
        self.syncing.set(true);
        self.widget.set_sensitive(state.text_draft.is_none());
        self.hint.set_visible(i.objects == 0);
        self.geometry.set_visible(i.objects > 0);
        self.arrange.set_visible(i.objects > 0);
        self.align_buttons
            .iter()
            .for_each(|b| b.set_sensitive(i.objects >= 2));
        self.spread_buttons
            .iter()
            .for_each(|b| b.set_sensitive(i.objects >= 3));

        let put = |(field, spin): &(Field, gtk::SpinButton), v: Value<f64>, scale: f64| {
            if field.show(&v) {
                spin.set_value(v.first().unwrap_or(0.) * scale);
            }
            !matches!(v, Value::Absent)
        };
        put(&self.x, i.x, 1.);
        put(&self.y, i.y, 1.);
        put(&self.width, i.width, 1.);
        put(&self.height, i.height, 1.);
        put(&self.rotation, i.rotation, 1.);

        let rgb = i.rgb.first();
        let shown = [
            self.color.0.show(&i.rgb),
            put(&self.opacity, i.alpha, 100.),
            put(&self.stroke, i.stroke, 1.),
            put(&self.tip, i.tip, 1.),
            put(&self.radius, i.radius, 1.),
            put(&self.dim, i.dim, 100.),
            put(&self.zoom, i.zoom, 1.),
        ];
        if let Some([r, g, b]) = rgb {
            self.color
                .1
                .set_rgba(&gdk::RGBA::new(r as f32, g as f32, b as f32, 1.));
        }
        self.style.set_visible(shown.contains(&true));

        let flag = |(field, check): &(Field, gtk::CheckButton), v: Value<bool>| {
            if field.show(&v) {
                check.set_inconsistent(v.is_mixed());
                check.set_active(v.first().unwrap_or(false));
            }
            !matches!(v, Value::Absent)
        };
        let shown = [
            put(&self.font, i.font, 1.),
            put(&self.box_width, i.box_width, 1.),
            flag(&self.bold, i.bold),
            flag(&self.italic, i.italic),
            flag(&self.background, i.background),
            self.align.0.show(&i.align),
        ];
        self.align.1.set_selected(match i.align {
            Value::Same(TextAlign::Left) => 0,
            Value::Same(TextAlign::Center) => 1,
            Value::Same(TextAlign::Right) => 2,
            _ => gtk::INVALID_LIST_POSITION,
        });
        self.text.set_visible(shown.contains(&true));
        self.syncing.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{document::Endpoint, shapes::Point};
    use image::RgbaImage;

    fn pt(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    fn doc() -> Document {
        Document::new(RgbaImage::new(300, 200)).unwrap()
    }

    fn boxy(id: u64, x: f32, color: [f64; 4]) -> Mark {
        Mark::highlight(id, pt(x, 10.), pt(x + 20., 30.), color)
    }

    #[test]
    fn selection_reports_same_mixed_and_absent_per_field() {
        let mut d = doc();
        d.add(boxy(1, 10., [1., 0., 0., 1.]));
        d.add(boxy(2, 50., [1., 0., 0., 0.5]));
        d.add(Mark::text(3, pt(100., 100.), "Cap", [0., 0., 1., 1.], 4.));
        let none = Inspection::of(&d, &[]);
        assert_eq!(none.objects, 0);
        assert_eq!(none.x, Value::Absent);

        let one = Inspection::of(&d, &[1]);
        assert_eq!(
            (one.objects, one.x, one.width),
            (1, Value::Same(10.), Value::Same(20.))
        );
        assert_eq!(one.rgb, Value::Same([1., 0., 0.]));
        assert_eq!(
            (one.font, one.tip, one.radius, one.zoom),
            (Value::Absent, Value::Absent, Value::Absent, Value::Absent)
        );

        let two = Inspection::of(&d, &[1, 2]);
        assert_eq!(two.objects, 2);
        assert_eq!(two.x, Value::Mixed(10.));
        assert_eq!(
            (two.y, two.width, two.height),
            (Value::Same(10.), Value::Same(20.), Value::Same(20.))
        );
        assert_eq!(two.rgb, Value::Same([1., 0., 0.]));
        assert_eq!(two.alpha, Value::Mixed(1.));

        let mixed = Inspection::of(&d, &[1, 3]);
        assert_eq!(mixed.rgb, Value::Mixed([1., 0., 0.]));
        assert!(
            mixed.font.first().is_some() && !mixed.font.is_mixed(),
            "only the text has a size"
        );
        assert_eq!(mixed.bold, Value::Same(false));
        assert_eq!(
            mixed.stroke,
            Value::Absent,
            "boxes and captions have no line width"
        );
    }

    #[test]
    fn privacy_masks_never_report_a_colour_and_groups_count_once() {
        let mut d = doc();
        d.add(Mark::redaction(1, pt(10., 10.), pt(50., 40.)));
        let only_mask = Inspection::of(&d, &[1]);
        assert_eq!(
            (only_mask.rgb, only_mask.alpha, only_mask.stroke),
            (Value::Absent, Value::Absent, Value::Absent)
        );
        assert_eq!(only_mask.width, Value::Same(40.));

        d.add(boxy(2, 100., [0., 1., 0., 1.]));
        d.add(boxy(3, 140., [0., 1., 0., 1.]));
        assert!(d.group_marks(&[2, 3]));
        let all = Inspection::of(&d, &[1, 2, 3]);
        assert_eq!(all.objects, 2, "the group is one object");
        let group = Inspection::of(&d, &[2]);
        assert_eq!(
            (group.objects, group.x, group.width),
            (1, Value::Same(100.), Value::Same(60.))
        );
    }

    #[test]
    fn kind_specific_fields_follow_the_selected_kinds() {
        let mut d = doc();
        d.add(Mark::connector(
            1,
            Endpoint {
                point: pt(10., 100.),
                anchor: None,
            },
            Endpoint {
                point: pt(90., 100.),
                anchor: None,
            },
            [1., 1., 1., 1.],
            3.,
        ));
        d.add(Mark::spotlight(2, pt(150., 100.), pt(250., 180.)));
        d.add(Mark::magnifier(
            3,
            Bounds {
                x: 10.,
                y: 10.,
                width: 20.,
                height: 20.,
            },
            2.,
            pt(120., 10.),
            pt(180., 60.),
            [1., 1., 1., 1.],
            2.,
        ));
        let c = Inspection::of(&d, &[1]);
        assert_eq!(c.tip, Value::Same(1.));
        assert_eq!(c.stroke, Value::Same(3.));
        let s = Inspection::of(&d, &[2]);
        assert!(matches!(s.radius, Value::Same(r) if r > 0.));
        assert!(matches!(s.dim, Value::Same(v) if v > 0. && v <= 1.));
        assert_eq!(Inspection::of(&d, &[3]).zoom, Value::Same(2.));
        assert_eq!(
            Inspection::of(&d, &[3, 1]).zoom,
            Value::Absent,
            "lens zoom is for a single lens"
        );
        assert!(Inspection::of(&d, &[1, 2]).tip.first().is_some());
    }

    #[test]
    fn rotation_and_text_style_are_read_back_after_edits() {
        let mut d = doc();
        d.add(Mark::text(1, pt(20., 20.), "One", [1., 0., 0., 1.], 4.));
        d.add(Mark::text(2, pt(20., 80.), "Two", [1., 0., 0., 1.], 4.));
        assert!(d.edit_marks(
            &[1],
            &MarkEdit {
                bold: Some(true),
                align: Some(TextAlign::Right),
                ..MarkEdit::default()
            }
        ));
        let i = Inspection::of(&d, &[1, 2]);
        assert_eq!(
            (i.bold, i.italic, i.align),
            (
                Value::Mixed(true),
                Value::Same(false),
                Value::Mixed(TextAlign::Right)
            )
        );
        assert!(d.set_rotation(&[2], 30.));
        let i = Inspection::of(&d, &[1, 2]);
        assert!(i.rotation.is_mixed());
        assert!(d.set_rotation(&[1, 2], 30.));
        assert!(
            matches!(Inspection::of(&d, &[1, 2]).rotation, Value::Same(r) if (r - 30.).abs() < 0.05)
        );
    }
}
