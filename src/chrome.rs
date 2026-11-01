use gtk::{
    cairo::{Context, LineCap, LineJoin},
    prelude::*,
};

#[derive(Clone, Copy)]
pub enum Icon {
    Select,
    Smart,
    Pen,
    Arrow,
    Rectangle,
    Ellipse,
    Text,
    Crop,
    Redact,
    Step,
    Magnify,
    Project,
    Undo,
    Redo,
    Delete,
    Copy,
    Save,
    Fit,
    Minus,
    Plus,
    ResetCrop,
    Help,
    Line,
    Spotlight,
    Designer,
    Resize,
    Front,
    Back,
    Group,
    Ungroup,
    Duplicate,
    Earlier,
    Later,
    Check,
    Close,
    Open,
    Ocr,
    Highlight,
    Underline,
    Strike,
    Bold,
    Italic,
    Inspector,
    Pin,
    Search,
    Recovery,
    Scroll,
}

thread_local! {
    static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Pins GTK's built-in dark theme and SharpShot's own CSS above any external theme.
/// GTK_THEME outranks `gtk-theme-name`, so the built-in theme is also loaded at USER priority;
/// the app CSS sits one step higher, above the user's gtk.css (also USER).
pub fn install() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    if INSTALLED.replace(true) {
        return;
    }
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_theme_name(Some("Adwaita"));
        settings.set_gtk_icon_theme_name(Some("Adwaita"));
        settings.set_gtk_application_prefer_dark_theme(true);
    }
    let theme = gtk::CssProvider::new();
    theme.load_from_resource("/org/gtk/libgtk/theme/Default/gtk-dark.css");
    gtk::style_context_add_provider_for_display(
        &display,
        &theme,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("editor.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
    );
    // Dialogs GTK creates itself (colour chooser, file-chooser fallback) get no class from us.
    let toplevels = gtk::Window::toplevels();
    let adopt = |list: &gtk::gio::ListModel, from: u32, count: u32| {
        for i in from..from + count {
            if let Some(window) = list.item(i).and_downcast::<gtk::Window>() {
                window.add_css_class("sharpshot");
            }
        }
    };
    adopt(&toplevels, 0, toplevels.n_items());
    toplevels.connect_items_changed(move |list, position, _, added| adopt(list, position, added));
    // The list model keeps its own reference; dropping the handle here is intended.
}

pub fn icon(kind: Icon) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(20)
        .content_height(20)
        .can_target(false)
        .build();
    area.set_draw_func(move |area, cr, w, h| {
        let color = area.style_context().color();
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            color.alpha().into(),
        );
        cr.translate((f64::from(w) - 20.) / 2., (f64::from(h) - 20.) / 2.);
        cr.set_line_width(1.5);
        cr.set_line_cap(LineCap::Round);
        cr.set_line_join(LineJoin::Round);
        if let Err(error) = paint(kind, cr) {
            eprintln!("Cannot draw toolbar icon: {error}");
        }
    });
    area
}

pub fn button(kind: Icon, name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .child(&icon(kind))
        .tooltip_text(tooltip)
        .build();
    button.add_css_class("tool-button");
    button.update_property(&[gtk::accessible::Property::Label(name)]);
    button
}

pub fn menu_button(kind: Icon, name: &str, tooltip: &str) -> gtk::Button {
    let button = button(kind, name, tooltip);
    let icon = button.child().expect("action icon");
    button.set_child(None::<&gtk::Widget>);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.append(&icon);
    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.);
    label.set_hexpand(true);
    row.append(&label);
    button.set_child(Some(&row));
    button
}

fn path(cr: &Context, points: &[(f64, f64)]) {
    if let Some(&(x, y)) = points.first() {
        cr.move_to(x, y);
    }
    for &(x, y) in &points[1..] {
        cr.line_to(x, y);
    }
}

fn paint(kind: Icon, cr: &Context) -> Result<(), gtk::cairo::Error> {
    match kind {
        Icon::Select => {
            path(
                cr,
                &[
                    (4., 2.),
                    (5., 16.),
                    (9., 12.),
                    (12., 18.),
                    (15., 16.),
                    (11., 10.),
                    (17., 9.),
                    (4., 2.),
                ],
            );
        }
        Icon::Smart => {
            path(
                cr,
                &[
                    (9., 2.),
                    (11., 7.),
                    (16., 9.),
                    (11., 11.),
                    (9., 16.),
                    (7., 11.),
                    (2., 9.),
                    (7., 7.),
                    (9., 2.),
                ],
            );
            path(cr, &[(16., 2.), (16., 5.)]);
            path(cr, &[(14.5, 3.5), (17.5, 3.5)]);
        }
        Icon::Pen => {
            path(
                cr,
                &[
                    (3., 17.),
                    (4., 12.),
                    (14., 2.),
                    (18., 6.),
                    (8., 16.),
                    (3., 17.),
                ],
            );
            path(cr, &[(12., 4.), (16., 8.)]);
        }
        Icon::Arrow => {
            path(cr, &[(3., 17.), (17., 3.), (9., 3.)]);
            path(cr, &[(17., 3.), (17., 11.)]);
        }
        Icon::Line => path(cr, &[(3., 17.), (17., 3.)]),
        Icon::Spotlight => {
            cr.rectangle(2., 2., 16., 16.);
            cr.rectangle(6., 6., 8., 8.);
            path(cr, &[(2., 2.), (6., 6.)]);
            path(cr, &[(14., 14.), (18., 18.)]);
        }
        Icon::Designer => {
            cr.rectangle(2., 2., 16., 16.);
            cr.rectangle(5., 5., 10., 10.);
            path(cr, &[(2., 13.), (7., 18.)]);
        }
        Icon::Resize => {
            path(cr, &[(2., 8.), (2., 2.), (8., 2.)]);
            path(cr, &[(12., 18.), (18., 18.), (18., 12.)]);
            path(cr, &[(3., 3.), (8., 8.)]);
            path(cr, &[(12., 12.), (17., 17.)]);
        }
        Icon::Front | Icon::Back => {
            if matches!(kind, Icon::Back) {
                cr.translate(20., 20.);
                cr.rotate(std::f64::consts::PI);
            }
            cr.rectangle(2., 8., 10., 10.);
            cr.stroke()?;
            cr.rectangle(8., 2., 10., 10.);
            cr.fill()?;
        }
        Icon::Group | Icon::Ungroup => {
            cr.rectangle(3., 5., 8., 8.);
            cr.rectangle(9., 9., 8., 8.);
            if matches!(kind, Icon::Group) {
                cr.rectangle(1., 2., 18., 17.);
            } else {
                path(cr, &[(2., 18.), (18., 2.)]);
            }
        }
        Icon::Duplicate => {
            cr.rectangle(2., 2., 10., 10.);
            cr.rectangle(7., 7., 11., 11.);
            path(cr, &[(10., 12.5), (15., 12.5)]);
            path(cr, &[(12.5, 10.), (12.5, 15.)]);
        }
        Icon::Earlier | Icon::Later => {
            if matches!(kind, Icon::Later) {
                cr.translate(20., 20.);
                cr.rotate(std::f64::consts::PI);
            }
            path(cr, &[(10., 17.), (10., 3.)]);
            path(cr, &[(4., 9.), (10., 3.), (16., 9.)]);
        }
        Icon::Check => path(cr, &[(3., 10.), (8., 15.), (17., 4.)]),
        Icon::Close => {
            path(cr, &[(4., 4.), (16., 16.)]);
            path(cr, &[(16., 4.), (4., 16.)]);
        }
        Icon::Open => {
            path(cr, &[(2., 16.), (2., 4.), (8., 4.), (10., 7.), (17., 7.)]);
            path(
                cr,
                &[(2., 16.), (5., 10.), (19., 10.), (16., 17.), (2., 17.)],
            );
        }
        Icon::Ocr => {
            path(cr, &[(6., 2.), (2., 2.), (2., 6.)]);
            path(cr, &[(14., 2.), (18., 2.), (18., 6.)]);
            path(cr, &[(2., 14.), (2., 18.), (6., 18.)]);
            path(cr, &[(14., 18.), (18., 18.), (18., 14.)]);
            path(cr, &[(6., 7.), (14., 7.)]);
            path(cr, &[(10., 7.), (10., 14.)]);
        }
        Icon::Highlight => {
            path(
                cr,
                &[(4., 12.), (11., 3.), (17., 8.), (10., 16.), (4., 12.)],
            );
            path(cr, &[(3., 18.), (17., 18.)]);
        }
        Icon::Underline | Icon::Strike => {
            path(cr, &[(4., 4.), (16., 4.)]);
            path(cr, &[(10., 4.), (10., 16.)]);
            let y = if matches!(kind, Icon::Underline) {
                18.
            } else {
                10.
            };
            path(cr, &[(3., y), (17., y)]);
        }
        Icon::Rectangle => cr.rectangle(3., 3., 14., 14.),
        Icon::Ellipse => cr.arc(10., 10., 7., 0., std::f64::consts::TAU),
        Icon::Text => {
            path(cr, &[(3., 4.), (17., 4.)]);
            path(cr, &[(10., 4.), (10., 17.)]);
            path(cr, &[(7., 17.), (13., 17.)]);
        }
        Icon::Crop => {
            path(cr, &[(5., 2.), (5., 15.), (18., 15.)]);
            path(cr, &[(2., 5.), (15., 5.), (15., 18.)]);
        }
        Icon::Redact => {
            cr.rectangle(2., 4., 16., 12.);
            cr.stroke()?;
            cr.rectangle(4., 8., 12., 4.);
            cr.fill()?;
        }
        Icon::Step => {
            cr.arc(10., 10., 8., 0., std::f64::consts::TAU);
            path(cr, &[(8., 7.), (10., 5.), (10., 15.)]);
            path(cr, &[(7., 15.), (13., 15.)]);
        }
        Icon::Magnify => {
            cr.arc(8., 8., 5., 0., std::f64::consts::TAU);
            path(cr, &[(12., 12.), (18., 18.)]);
            path(cr, &[(5., 8.), (11., 8.)]);
            path(cr, &[(8., 5.), (8., 11.)]);
        }
        Icon::Project => {
            path(
                cr,
                &[
                    (2., 4.),
                    (8., 4.),
                    (10., 7.),
                    (18., 7.),
                    (18., 17.),
                    (2., 17.),
                    (2., 4.),
                ],
            );
            path(cr, &[(5., 10.), (15., 10.)]);
        }
        Icon::Undo | Icon::Redo => {
            if matches!(kind, Icon::Redo) {
                cr.translate(20., 0.);
                cr.scale(-1., 1.);
            }
            path(cr, &[(7., 3.), (3., 7.), (7., 11.)]);
            cr.move_to(3., 7.);
            cr.line_to(12., 7.);
            cr.curve_to(19., 7., 19., 17., 11., 17.);
        }
        Icon::Delete => {
            path(cr, &[(3., 5.), (17., 5.)]);
            path(cr, &[(7., 5.), (7., 2.), (13., 2.), (13., 5.)]);
            path(cr, &[(5., 5.), (6., 18.), (14., 18.), (15., 5.)]);
            path(cr, &[(8., 8.), (8., 14.)]);
            path(cr, &[(12., 8.), (12., 14.)]);
        }
        Icon::Copy => {
            cr.rectangle(7., 7., 10., 10.);
            path(cr, &[(13., 4.), (13., 2.), (2., 2.), (2., 13.), (4., 13.)]);
        }
        Icon::Save => {
            path(
                cr,
                &[
                    (3., 2.),
                    (15., 2.),
                    (18., 5.),
                    (18., 18.),
                    (2., 18.),
                    (2., 2.),
                    (3., 2.),
                ],
            );
            cr.rectangle(6., 2., 7., 5.);
            cr.rectangle(6., 11., 8., 7.);
        }
        Icon::Fit => {
            path(cr, &[(7., 3.), (3., 3.), (3., 7.)]);
            path(cr, &[(13., 3.), (17., 3.), (17., 7.)]);
            path(cr, &[(3., 13.), (3., 17.), (7., 17.)]);
            path(cr, &[(17., 13.), (17., 17.), (13., 17.)]);
            cr.rectangle(7., 7., 6., 6.);
        }
        Icon::Minus | Icon::Plus => {
            path(cr, &[(4., 10.), (16., 10.)]);
            if matches!(kind, Icon::Plus) {
                path(cr, &[(10., 4.), (10., 16.)]);
            }
        }
        Icon::ResetCrop => {
            path(cr, &[(5., 2.), (5., 15.), (18., 15.)]);
            path(cr, &[(2., 5.), (15., 5.), (15., 18.)]);
            path(cr, &[(2., 18.), (18., 2.)]);
        }
        Icon::Bold => {
            path(cr, &[(6., 3.), (6., 17.), (11., 17.)]);
            cr.move_to(6., 3.);
            cr.line_to(11., 3.);
            cr.curve_to(17., 3., 17., 10., 11., 10.);
            cr.line_to(6., 10.);
            cr.move_to(11., 10.);
            cr.curve_to(18., 10., 18., 17., 11., 17.);
        }
        Icon::Italic => {
            path(cr, &[(8., 3.), (16., 3.)]);
            path(cr, &[(4., 17.), (12., 17.)]);
            path(cr, &[(13., 3.), (7., 17.)]);
        }
        Icon::Inspector => {
            for (y, x) in [(5., 7.), (10., 13.), (15., 9.)] {
                path(cr, &[(3., y), (x - 2., y)]);
                path(cr, &[(x + 2., y), (17., y)]);
                cr.rectangle(x - 2., y - 2., 4., 4.);
            }
        }
        Icon::Pin => {
            path(
                cr,
                &[
                    (7., 2.),
                    (15., 2.),
                    (13., 5.),
                    (13., 10.),
                    (17., 13.),
                    (3., 13.),
                    (7., 10.),
                    (7., 5.),
                    (7., 2.),
                ],
            );
            path(cr, &[(10., 13.), (10., 19.)]);
        }
        Icon::Search => {
            cr.arc(8., 8., 5., 0., std::f64::consts::TAU);
            path(cr, &[(12., 12.), (18., 18.)]);
        }
        Icon::Recovery => {
            cr.arc(10., 10., 7., -1.2, 4.7);
            path(cr, &[(3., 3.), (3., 8.), (8., 8.)]);
            path(cr, &[(10., 5.), (10., 10.), (13., 12.)]);
        }
        Icon::Scroll => {
            cr.rectangle(3., 2., 10., 16.);
            path(cr, &[(6., 6.), (10., 6.)]);
            path(cr, &[(6., 10.), (10., 10.)]);
            path(cr, &[(17., 3.), (17., 16.), (14., 13.)]);
            path(cr, &[(17., 16.), (19., 13.)]);
        }
        Icon::Help => {
            cr.arc(10., 10., 8., 0., std::f64::consts::TAU);
            cr.stroke()?;
            cr.move_to(7., 7.);
            cr.curve_to(7., 3., 15., 4., 13., 8.);
            cr.curve_to(12., 10., 10., 9., 10., 12.);
            cr.stroke()?;
            cr.arc(10., 15., 0.6, 0., std::f64::consts::TAU);
            cr.fill()?;
        }
    }
    cr.stroke()
}
