//! A reusable right-click menu: a popover over the canvas whose rows the caller rebuilds on every opening.

use std::cell::Cell;

use gtk::{gdk, glib, prelude::*};

use crate::chrome::{self, Icon};

const MAX_HEIGHT: i32 = 520;
const VIEWPORT_MARGIN: i32 = 24;
const MIN_HEIGHT: i32 = 120;

pub struct ContextMenu {
    popover: gtk::Popover,
    scroller: gtk::ScrolledWindow,
    rows: gtk::Box,
    area: gtk::DrawingArea,
    /// Rows added since the last `clear`, so separators are never leading.
    content: Cell<bool>,
    /// A separator was requested; it is only added if more content follows, so none is trailing or doubled.
    separator_pending: Cell<bool>,
}

impl ContextMenu {
    pub fn new(area: &gtk::DrawingArea) -> Self {
        let rows = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .accessible_role(gtk::AccessibleRole::Menu)
            .build();
        rows.update_property(&[gtk::accessible::Property::Label("Context menu")]);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .min_content_width(200)
            .child(&rows)
            .build();
        let popover = gtk::Popover::builder()
            .child(&scroller)
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .css_classes(["context-menu"])
            .build();
        popover.set_parent(area);

        // Up/Down/Home/End move focus between rows; Enter/Space and Escape come from the button and the popover.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak_rows = rows.downgrade();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(rows) = weak_rows.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK)
                || ![gdk::Key::Down, gdk::Key::Up, gdk::Key::Home, gdk::Key::End].contains(&key)
            {
                return glib::Propagation::Proceed;
            }
            let items = items(&rows);
            let Some(last) = items.len().checked_sub(1) else {
                return glib::Propagation::Proceed;
            };
            let focused = items.iter().position(|item| item.has_focus());
            let next = match key {
                gdk::Key::Down => focused.map_or(0, |i| if i == last { 0 } else { i + 1 }),
                gdk::Key::Up => focused.map_or(last, |i| if i == 0 { last } else { i - 1 }),
                gdk::Key::Home => 0,
                gdk::Key::End => last,
                _ => return glib::Propagation::Proceed,
            };
            items[next].grab_focus();
            glib::Propagation::Stop
        });
        popover.add_controller(keys);

        // GTK warns when a widget is finalized with a child left, so unparent when the canvas goes away.
        let weak = popover.downgrade();
        area.connect_destroy(move |_| {
            if let Some(popover) = weak.upgrade()
                && popover.parent().is_some()
            {
                popover.unparent();
            }
        });

        Self {
            popover,
            scroller,
            rows,
            area: area.clone(),
            content: Cell::new(false),
            separator_pending: Cell::new(false),
        }
    }

    /// Removes every row; call before building the rows for a new opening.
    pub fn clear(&self) {
        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        self.content.set(false);
        self.separator_pending.set(false);
    }

    /// Adds a clickable row. The menu closes first, then `callback` runs.
    pub fn action(&self, icon: Icon, label: &str, callback: impl Fn() + 'static) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.append(&chrome::icon(icon));
        let text = gtk::Label::new(Some(label));
        text.set_xalign(0.);
        text.set_hexpand(true);
        row.append(&text);
        let item = gtk::Button::builder()
            .child(&row)
            .accessible_role(gtk::AccessibleRole::MenuItem)
            .css_classes(["tool-button", "context-menu-item"])
            .build();
        item.update_property(&[gtk::accessible::Property::Label(label)]);
        let popover = self.popover.downgrade();
        item.connect_clicked(move |_| {
            if let Some(popover) = popover.upgrade() {
                popover.popdown();
            }
            callback();
        });
        self.push(&item);
    }

    /// Requests a divider before the next row; ignored at the start, at the end and when repeated.
    pub fn separator(&self) {
        if self.content.get() {
            self.separator_pending.set(true);
        }
    }

    /// Adds a non-interactive explanation line, for example why an action is unavailable.
    pub fn status(&self, text: &str) {
        let label = gtk::Label::builder()
            .label(text)
            .xalign(0.)
            .wrap(true)
            .max_width_chars(36)
            .css_classes(["dimension-label", "context-menu-status"])
            .margin_start(8)
            .margin_end(8)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        self.push(&label);
    }

    pub fn popup(&self, x: f64, y: f64) {
        let rect = gdk::Rectangle::new(x.round() as i32, y.round() as i32, 1, 1);
        self.popover.set_pointing_to(Some(&rect));
        let window = self.area.root().map_or(MAX_HEIGHT, |root| {
            root.upcast::<gtk::Widget>().height() - VIEWPORT_MARGIN
        });
        self.scroller
            .set_max_content_height(window.clamp(MIN_HEIGHT, MAX_HEIGHT));
        self.popover.popup();
        if let Some(first) = items(&self.rows).first() {
            first.grab_focus();
        }
    }

    pub fn popdown(&self) {
        self.popover.popdown();
    }

    pub fn is_open(&self) -> bool {
        self.popover.is_visible()
    }

    fn push(&self, child: &impl IsA<gtk::Widget>) {
        if self.separator_pending.replace(false) {
            self.rows
                .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        self.rows.append(child);
        self.content.set(true);
    }
}

impl Drop for ContextMenu {
    fn drop(&mut self) {
        if self.popover.parent().is_some() {
            self.popover.unparent();
        }
    }
}

fn items(rows: &gtk::Box) -> Vec<gtk::Button> {
    let mut buttons = Vec::new();
    let mut child = rows.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(button) = widget.downcast::<gtk::Button>() {
            buttons.push(button);
        }
    }
    buttons
}
