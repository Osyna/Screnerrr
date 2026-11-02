use crate::{chrome, document::Crop, presentation::surface_from};
use anyhow::{Result, ensure};
use gtk::{
    cairo::{Context, Filter, ImageSurface},
    gdk, glib,
    prelude::*,
};
use image::RgbaImage;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Clone, Copy)]
struct Viewport {
    scale: f64,
    x: f64,
    y: f64,
}
impl Viewport {
    fn new(w: u32, h: u32, aw: i32, ah: i32) -> Self {
        let scale = (f64::from(aw.max(1)) / f64::from(w)).min(f64::from(ah.max(1)) / f64::from(h));
        Self {
            scale,
            x: (f64::from(aw) - f64::from(w) * scale) / 2.,
            y: (f64::from(ah) - f64::from(h) * scale) / 2.,
        }
    }
    fn point(self, x: f64, y: f64, w: u32, h: u32) -> (u32, u32) {
        (
            ((x - self.x) / self.scale)
                .floor()
                .clamp(0., f64::from(w - 1)) as u32,
            ((y - self.y) / self.scale)
                .floor()
                .clamp(0., f64::from(h - 1)) as u32,
        )
    }
}

struct Selection {
    width: u32,
    height: u32,
    min_side: u32,
    max_pixels: u64,
    pointer: (u32, u32),
    anchor: Option<(u32, u32)>,
    crop: Option<Crop>,
}
impl Selection {
    fn accepted(&self) -> Option<Crop> {
        self.crop.filter(|c| {
            c.width >= self.min_side
                && c.height >= self.min_side
                && u64::from(c.width) * u64::from(c.height) <= self.max_pixels
        })
    }
    fn drag(&mut self, p: (u32, u32)) {
        self.pointer = p;
        if let Some(a) = self.anchor {
            self.crop = Some(Crop {
                x: a.0.min(p.0),
                y: a.1.min(p.1),
                width: a.0.abs_diff(p.0) + 1,
                height: a.1.abs_diff(p.1) + 1,
            });
        }
    }
    fn release(&mut self, point: (u32, u32)) -> Option<Crop> {
        self.drag(point);
        self.anchor.take()?;
        self.accepted()
    }
    fn adjust(&mut self, dx: i32, dy: i32, near: bool, far: bool) {
        let mut c = self.crop.unwrap_or(Crop {
            x: self.pointer.0,
            y: self.pointer.1,
            width: 1,
            height: 1,
        });
        if near {
            let right = c.x + c.width;
            let bottom = c.y + c.height;
            c.x = (i64::from(c.x) + i64::from(dx)).clamp(0, i64::from(right - 1)) as u32;
            c.y = (i64::from(c.y) + i64::from(dy)).clamp(0, i64::from(bottom - 1)) as u32;
            c.width = right - c.x;
            c.height = bottom - c.y;
        } else if far {
            c.width =
                (i64::from(c.width) + i64::from(dx)).clamp(1, i64::from(self.width - c.x)) as u32;
            c.height =
                (i64::from(c.height) + i64::from(dy)).clamp(1, i64::from(self.height - c.y)) as u32;
        } else {
            c.x = (i64::from(c.x) + i64::from(dx)).clamp(0, i64::from(self.width - c.width)) as u32;
            c.y =
                (i64::from(c.y) + i64::from(dy)).clamp(0, i64::from(self.height - c.height)) as u32;
        }
        self.pointer = if near {
            (c.x, c.y)
        } else {
            (c.x + c.width - 1, c.y + c.height - 1)
        };
        self.crop = Some(c);
    }
}

/// The returned rectangle addresses the frozen source pixels, not GTK logical coordinates.
/// A nested loop also permits use from the recorder without starting a second application.
pub fn select(image: &RgbaImage, min_side: u32, max_pixels: u64) -> Result<Option<Crop>> {
    ensure!(
        image.width() > 0 && image.height() > 0,
        "Cannot select an empty screenshot"
    );
    ensure!(
        min_side > 0 && image.width() >= min_side && image.height() >= min_side,
        "The captured source must be at least {min_side} × {min_side} pixels"
    );
    gtk::init()?;
    chrome::install();
    let surface = surface_from(image)?;
    let state = Rc::new(RefCell::new(Selection {
        width: image.width(),
        height: image.height(),
        min_side,
        max_pixels,
        pointer: (image.width() / 2, image.height() / 2),
        anchor: None,
        crop: None,
    }));
    let window = gtk::Window::builder()
        .title("SharpShot — Precision capture")
        .default_width(1280)
        .default_height(800)
        .decorated(false)
        .build();
    window.add_css_class("sharpshot");
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let area = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .focusable(true)
        .accessible_role(gtk::AccessibleRole::Img)
        .build();
    area.set_cursor_from_name(Some("crosshair"));
    area.update_property(&[gtk::accessible::Property::Label("Precision capture canvas")]);
    root.append(&area);
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    bar.add_css_class("style-bar");
    let info = gtk::Label::new(Some("Drag a region or use the arrow keys"));
    info.set_hexpand(true);
    info.set_xalign(0.);
    info.set_wrap(true);
    info.update_property(&[gtk::accessible::Property::Label("Capture dimensions")]);
    let loupe = gtk::CheckButton::with_label("Pixel loupe");
    loupe.set_active(true);
    loupe.set_tooltip_text(Some("Show source pixels at 8× magnification · L"));
    let cancel = chrome::button(
        chrome::Icon::Close,
        "Cancel capture",
        "Cancel without capturing · Escape",
    );
    let accept = chrome::button(
        chrome::Icon::Check,
        "Capture selection",
        "Capture the selected pixels · Enter",
    );
    accept.set_sensitive(false);
    for w in [
        info.clone().upcast::<gtk::Widget>(),
        loupe.clone().upcast(),
        cancel.clone().upcast(),
        accept.clone().upcast(),
    ] {
        bar.append(&w);
    }
    root.append(&bar);
    let help = gtk::Label::new(Some(
        "Drag and release to capture · Arrows move · Ctrl+arrows resize right/bottom · Alt+arrows resize left/top · Shift = 10 px · Enter captures keyboard selection · Esc cancels",
    ));
    help.set_wrap(true);
    help.add_css_class("hint");
    root.append(&help);
    window.set_child(Some(&root));
    let show_loupe = Rc::new(Cell::new(true));
    area.set_draw_func({
        let state = state.clone();
        let show_loupe = show_loupe.clone();
        let info = info.downgrade();
        move |_, cr, w, h| {
            let s = state.borrow();
            let v = Viewport::new(s.width, s.height, w, h);
            if let Err(error) = paint(cr, &surface, &s, v, w, h, show_loupe.get()) {
                if let Some(info) = info.upgrade() {
                    info.set_text(&format!("Cannot draw capture: {error}. Escape cancels."));
                }
            }
        }
    });
    let refresh: Rc<dyn Fn()> = Rc::new({
        let state = state.clone();
        let area = area.downgrade();
        let info = info.downgrade();
        let accept = accept.downgrade();
        move || {
            if let Some(area) = area.upgrade() {
                area.queue_draw();
            }
            if let Some(accept) = accept.upgrade() {
                accept.set_sensitive(state.borrow().accepted().is_some());
            }
            if let Some(info) = info.upgrade() {
                let s = state.borrow();
                info.set_text(&match s.crop {
                    Some(c) if s.accepted().is_none() => format!(
                        "{} × {} px · select at least {} × {} px and at most {} pixels",
                        c.width, c.height, s.min_side, s.min_side, s.max_pixels
                    ),
                    Some(c) => format!(
                        "{} × {} px · x {}, y {} · pointer {}, {}",
                        c.width, c.height, c.x, c.y, s.pointer.0, s.pointer.1
                    ),
                    None => format!(
                        "Source {} × {} px · pointer {}, {}",
                        s.width, s.height, s.pointer.0, s.pointer.1
                    ),
                });
            }
        }
    });
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let state = state.clone();
        let refresh = refresh.clone();
        let area = area.downgrade();
        move |_, x, y| {
            if let Some(a) = area.upgrade() {
                let mut s = state.borrow_mut();
                let v = Viewport::new(s.width, s.height, a.width(), a.height());
                s.pointer = v.point(x, y, s.width, s.height);
                drop(s);
                refresh();
            }
        }
    });
    area.add_controller(motion);
    let done = glib::MainLoop::new(None, false);
    let outcome = Rc::new(Cell::new(None));
    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    let start = Rc::new(Cell::new((0., 0.)));
    drag.connect_drag_begin({
        let state = state.clone();
        let start = start.clone();
        let refresh = refresh.clone();
        let area = area.downgrade();
        move |_, x, y| {
            if let Some(a) = area.upgrade() {
                a.grab_focus();
                start.set((x, y));
                let mut s = state.borrow_mut();
                let p = Viewport::new(s.width, s.height, a.width(), a.height())
                    .point(x, y, s.width, s.height);
                s.anchor = Some(p);
                s.drag(p);
                drop(s);
                refresh();
            }
        }
    });
    drag.connect_drag_update({
        let state = state.clone();
        let start = start.clone();
        let refresh = refresh.clone();
        let area = area.downgrade();
        move |_, dx, dy| {
            if let Some(a) = area.upgrade() {
                let mut s = state.borrow_mut();
                let (x, y) = start.get();
                let p = Viewport::new(s.width, s.height, a.width(), a.height()).point(
                    x + dx,
                    y + dy,
                    s.width,
                    s.height,
                );
                s.drag(p);
                drop(s);
                refresh();
            }
        }
    });
    drag.connect_cancel({
        let state = state.clone();
        move |_, _| state.borrow_mut().anchor = None
    });
    drag.connect_drag_end({
        let state = state.clone();
        let start = start.clone();
        let area = area.downgrade();
        let done = done.clone();
        let outcome = outcome.clone();
        let refresh = refresh.clone();
        move |_, dx, dy| {
            if !done.is_running() {
                return;
            }
            if let Some(area) = area.upgrade() {
                let mut s = state.borrow_mut();
                let (x, y) = start.get();
                let point = Viewport::new(s.width, s.height, area.width(), area.height()).point(
                    x + dx,
                    y + dy,
                    s.width,
                    s.height,
                );
                let crop = s.release(point);
                drop(s);
                if let Some(crop) = crop {
                    outcome.set(Some(crop));
                    done.quit();
                } else {
                    refresh();
                }
            }
        }
    });
    area.add_controller(drag);
    loupe.connect_toggled({
        let show = show_loupe.clone();
        let refresh = refresh.clone();
        move |b| {
            show.set(b.is_active());
            refresh();
        }
    });
    accept.connect_clicked({
        let state = state.clone();
        let done = done.clone();
        let outcome = outcome.clone();
        move |_| {
            if let Some(c) = state.borrow().accepted() {
                outcome.set(Some(c));
                done.quit();
            }
        }
    });
    cancel.connect_clicked({
        let done = done.clone();
        move |_| done.quit()
    });
    window.connect_close_request({
        let done = done.clone();
        move |_| {
            done.quit();
            glib::Propagation::Proceed
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let state = state.clone();
        let refresh = refresh.clone();
        let outcome = outcome.clone();
        let done = done.clone();
        let loupe = loupe.downgrade();
        move |_, key, _, mods| {
            if key == gdk::Key::Escape {
                done.quit();
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::Return || key == gdk::Key::KP_Enter {
                if let Some(c) = state.borrow().accepted() {
                    outcome.set(Some(c));
                    done.quit();
                }
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::l || key == gdk::Key::L {
                if let Some(b) = loupe.upgrade() {
                    b.set_active(!b.is_active());
                }
                return glib::Propagation::Stop;
            }
            let step = if mods.contains(gdk::ModifierType::SHIFT_MASK) {
                10
            } else {
                1
            };
            let (dx, dy) = match key {
                gdk::Key::Left => (-step, 0),
                gdk::Key::Right => (step, 0),
                gdk::Key::Up => (0, -step),
                gdk::Key::Down => (0, step),
                _ => return glib::Propagation::Proceed,
            };
            state.borrow_mut().adjust(
                dx,
                dy,
                mods.contains(gdk::ModifierType::ALT_MASK),
                mods.contains(gdk::ModifierType::CONTROL_MASK),
            );
            refresh();
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);
    window.fullscreen();
    window.present();
    area.grab_focus();
    refresh();
    done.run();
    window.destroy();
    Ok(outcome.get())
}

fn paint(
    cr: &Context,
    image: &ImageSurface,
    s: &Selection,
    v: Viewport,
    aw: i32,
    ah: i32,
    loupe: bool,
) -> Result<()> {
    cr.set_source_rgb(0.08, 0.09, 0.11);
    cr.paint()?;
    cr.save()?;
    cr.translate(v.x, v.y);
    cr.scale(v.scale, v.scale);
    cr.set_source_surface(image, 0., 0.)?;
    cr.source().set_filter(Filter::Nearest);
    cr.paint()?;
    cr.restore()?;
    cr.set_source_rgba(0., 0., 0., 0.45);
    cr.rectangle(
        v.x,
        v.y,
        f64::from(s.width) * v.scale,
        f64::from(s.height) * v.scale,
    );
    cr.fill()?;
    if let Some(c) = s.crop {
        let x = v.x + f64::from(c.x) * v.scale;
        let y = v.y + f64::from(c.y) * v.scale;
        let w = f64::from(c.width) * v.scale;
        let h = f64::from(c.height) * v.scale;
        cr.save()?;
        cr.rectangle(x, y, w, h);
        cr.clip();
        cr.translate(v.x, v.y);
        cr.scale(v.scale, v.scale);
        cr.set_source_surface(image, 0., 0.)?;
        cr.source().set_filter(Filter::Nearest);
        cr.paint()?;
        cr.restore()?;
        cr.set_source_rgb(0.21, 0.78, 0.79);
        cr.set_line_width(2.);
        cr.rectangle(x, y, w, h);
        cr.stroke()?;
    }
    if loupe {
        let px = v.x + (f64::from(s.pointer.0) + 0.5) * v.scale;
        let py = v.y + (f64::from(s.pointer.1) + 0.5) * v.scale;
        let size = 104.;
        let x = (px + 24.).min(f64::from(aw) - size - 8.).max(0.);
        let y = (py + 24.).min(f64::from(ah) - size - 8.).max(0.);
        cr.save()?;
        cr.rectangle(x, y, size, size);
        cr.clip();
        cr.set_source_rgb(0.08, 0.09, 0.11);
        cr.paint()?;
        cr.translate(x + size / 2., y + size / 2.);
        cr.scale(8., 8.);
        cr.set_source_surface(
            image,
            -f64::from(s.pointer.0) - 0.5,
            -f64::from(s.pointer.1) - 0.5,
        )?;
        cr.source().set_filter(Filter::Nearest);
        cr.paint()?;
        cr.restore()?;
        cr.set_line_width(1.);
        cr.set_source_rgb(1., 1., 1.);
        cr.rectangle(x + 0.5, y + 0.5, size - 1., size - 1.);
        cr.stroke()?;
        cr.set_source_rgb(0.21, 0.78, 0.79);
        cr.rectangle(x + size / 2. - 4., y + size / 2. - 4., 8., 8.);
        cr.stroke()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_accepts_the_final_pointer_only_for_a_live_valid_drag() {
        let mut s = Selection {
            width: 100,
            height: 100,
            min_side: 16,
            max_pixels: 400,
            pointer: (0, 0),
            anchor: Some((0, 0)),
            crop: None,
        };
        assert_eq!(
            s.release((15, 15)),
            Some(Crop {
                x: 0,
                y: 0,
                width: 16,
                height: 16
            })
        );
        assert_eq!(
            s.release((19, 19)),
            None,
            "a completed drag cannot capture twice"
        );
        s.anchor = Some((0, 0));
        assert_eq!(s.release((14, 15)), None);
        assert_eq!(
            s.crop.unwrap().width,
            15,
            "an invalid region stays available to adjust"
        );
        s.anchor = Some((0, 0));
        assert_eq!(
            s.release((20, 19)),
            None,
            "the pixel budget also applies on release"
        );
        s.anchor = Some((0, 0));
        s.drag((19, 19));
        s.anchor = None;
        assert_eq!(
            s.release((19, 19)),
            None,
            "cancelled gestures never capture"
        );
    }

    #[test]
    fn recorder_limits_reject_small_or_oversized_regions_without_losing_the_selection() {
        let mut s = Selection {
            width: 100,
            height: 100,
            min_side: 16,
            max_pixels: 400,
            pointer: (0, 0),
            anchor: None,
            crop: None,
        };
        for (width, height, accepted) in [
            (15, 16, false),
            (16, 16, true),
            (20, 20, true),
            (21, 20, false),
        ] {
            let crop = Crop {
                x: 0,
                y: 0,
                width,
                height,
            };
            s.crop = Some(crop);
            assert_eq!(s.accepted(), accepted.then_some(crop));
            assert_eq!(s.crop, Some(crop));
        }
    }
    #[test]
    fn precision_edges_preserve_opposite_edges_and_stay_inside_pixels() {
        let mut s = Selection {
            width: 100,
            height: 80,
            min_side: 1,
            max_pixels: 8000,
            pointer: (10, 20),
            anchor: Some((10, 20)),
            crop: None,
        };
        s.drag((29, 39));
        assert_eq!(
            s.crop,
            Some(Crop {
                x: 10,
                y: 20,
                width: 20,
                height: 20
            })
        );
        s.adjust(-5, 0, true, false);
        assert_eq!(
            s.crop,
            Some(Crop {
                x: 5,
                y: 20,
                width: 25,
                height: 20
            })
        );
        s.adjust(500, -500, false, true);
        assert_eq!(
            s.crop,
            Some(Crop {
                x: 5,
                y: 20,
                width: 95,
                height: 1
            })
        );
        s.adjust(500, 500, false, false);
        assert_eq!(
            s.crop,
            Some(Crop {
                x: 5,
                y: 79,
                width: 95,
                height: 1
            })
        );
        s.adjust(500, 500, true, false);
        assert_eq!(
            s.crop,
            Some(Crop {
                x: 99,
                y: 79,
                width: 1,
                height: 1
            })
        );
    }
    #[test]
    fn scaled_letterboxed_capture_maps_to_native_pixels() {
        let v = Viewport::new(2000, 1000, 1000, 800);
        assert_eq!(v.point(250., 275., 2000, 1000), (500, 250));
        assert_eq!(v.point(-100., -100., 2000, 1000), (0, 0));
        assert_eq!(v.point(10000., 10000., 2000, 1000), (1999, 999));
    }
}
