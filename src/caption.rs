use crate::{
    chrome::{self, Icon},
    document::{Mark, TextAlign, TextEffect, TextStyle, font_description, installed_families},
    interaction::{State, TextDraft, Transform},
};
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

pub struct Caption {
    pub toolbar: gtk::Box,
    pub apply: gtk::Button,
    pub cancel: gtk::Button,
    input: gtk::TextView,
    area: gtk::DrawingArea,
    font: gtk::SpinButton,
    width: gtk::SpinButton,
    background: gtk::CheckButton,
    bold: gtk::ToggleButton,
    italic: gtk::ToggleButton,
    alignment: gtk::DropDown,
    family: gtk::DropDown,
    families: Vec<String>,
    effect: gtk::DropDown,
    error: gtk::Label,
    tag: gtk::TextTag,
    state: Rc<RefCell<State>>,
    changed: Rc<dyn Fn()>,
    updating: Cell<bool>,
    active: Cell<bool>,
    preview: RefCell<Option<Mark>>,
    revision: Cell<u64>,
    metrics: RefCell<(f64, bool, bool, String, String)>,
    placement: Cell<(f64, f64, f64)>,
    text_scale: Cell<f64>,
}

impl Caption {
    pub fn new(
        overlay: &gtk::Overlay,
        area: &gtk::DrawingArea,
        state: Rc<RefCell<State>>,
        changed: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        toolbar.set_visible(false);
        let label = gtk::Label::new(Some("Caption"));
        toolbar.append(&label);
        let font = gtk::SpinButton::with_range(6., 8192., 1.);
        font.update_property(&[gtk::accessible::Property::Label(
            "Caption font size in pixels",
        )]);
        font.set_tooltip_text(Some("Live caption size in image pixels"));
        toolbar.append(&font);
        let width = gtk::SpinButton::with_range(0., 8192., 8.);
        width.update_property(&[gtk::accessible::Property::Label(
            "Text box width in image pixels",
        )]);
        width.set_tooltip_text(Some(
            "Wrap width in image pixels · 0 automatically fits the text",
        ));
        toolbar.append(&width);
        let bold = gtk::ToggleButton::builder()
            .child(&chrome::icon(Icon::Bold))
            .tooltip_text("Bold · Ctrl+B")
            .build();
        bold.update_property(&[gtk::accessible::Property::Label("Bold caption")]);
        toolbar.append(&bold);
        let italic = gtk::ToggleButton::builder()
            .child(&chrome::icon(Icon::Italic))
            .tooltip_text("Italic · Ctrl+I")
            .build();
        italic.update_property(&[gtk::accessible::Property::Label("Italic caption")]);
        toolbar.append(&italic);
        let alignment = gtk::DropDown::from_strings(&["Left", "Centre", "Right"]);
        alignment.update_property(&[gtk::accessible::Property::Label("Caption alignment")]);
        toolbar.append(&alignment);
        let (family, families) = family_picker(
            "Caption font family",
            "Font family of this caption · type to search installed fonts",
        );
        toolbar.append(&family);
        let effect = gtk::DropDown::from_strings(&TextEffect::ALL.map(TextEffect::label));
        effect.set_tooltip_text(Some("Caption effect · Alt cycles outline and shadow"));
        effect.update_property(&[gtk::accessible::Property::Label("Caption effect")]);
        toolbar.append(&effect);
        let background = gtk::CheckButton::with_label("Readable background");
        toolbar.append(&background);
        let apply = chrome::button(
            Icon::Check,
            "Apply text",
            "Apply caption as one undo step · Enter or Ctrl+Enter · Shift+Enter for a new line",
        );
        let cancel = chrome::button(
            Icon::Close,
            "Cancel text edit",
            "Cancel caption without changing the document · Escape",
        );
        toolbar.append(&apply);
        toolbar.append(&cancel);
        let error = gtk::Label::new(None);
        error.set_wrap(true);
        toolbar.append(&error);
        let input = gtk::TextView::new();
        input.add_css_class("canvas-caption");
        input.set_halign(gtk::Align::Start);
        input.set_valign(gtk::Align::Start);
        input.set_visible(false);
        input.set_accepts_tab(false);
        input.set_left_margin(0);
        input.set_right_margin(0);
        input.set_top_margin(0);
        input.set_bottom_margin(0);
        input.update_property(&[gtk::accessible::Property::Label("Caption text")]);
        let tag = gtk::TextTag::builder()
            .name("live-caption")
            .foreground_rgba(&gdk::RGBA::new(0., 0., 0., 0.))
            .build();
        input.buffer().tag_table().add(&tag);
        overlay.add_overlay(&input);
        let this = Rc::new(Self {
            toolbar,
            apply,
            cancel,
            input,
            area: area.clone(),
            font,
            width,
            background,
            bold,
            italic,
            alignment,
            family,
            families,
            effect,
            error,
            tag,
            state,
            changed,
            updating: Cell::new(false),
            active: Cell::new(false),
            preview: RefCell::new(None),
            revision: Cell::new(0),
            metrics: RefCell::new((0., false, false, String::new(), String::new())),
            placement: Cell::new((f64::NAN, f64::NAN, f64::NAN)),
            text_scale: Cell::new(1.),
        });
        this.input.buffer().connect_changed(glib::clone!(
            #[weak]
            this,
            move |_| this.update()
        ));
        for spin in [&this.font, &this.width] {
            spin.connect_value_changed(glib::clone!(
                #[weak]
                this,
                move |_| this.update()
            ));
        }
        for toggle in [&this.bold, &this.italic] {
            toggle.connect_toggled(glib::clone!(
                #[weak]
                this,
                move |_| this.update()
            ));
        }
        this.background.connect_toggled(glib::clone!(
            #[weak]
            this,
            move |_| this.update()
        ));
        this.alignment.connect_selected_notify(glib::clone!(
            #[weak]
            this,
            move |_| this.update()
        ));
        for picker in [&this.family, &this.effect] {
            picker.connect_selected_notify(glib::clone!(
                #[weak]
                this,
                move |_| this.update()
            ));
        }
        this.apply.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |_| this.commit()
        ));
        this.cancel.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |_| {
                this.state.borrow_mut().text_draft = None;
                this.close();
                (this.changed)();
                this.area.grab_focus();
            }
        ));
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(glib::clone!(
            #[weak]
            this,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |controller, key, _, mods| {
                if key == gdk::Key::Escape {
                    this.cancel.emit_clicked();
                    return glib::Propagation::Stop;
                }
                if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter)
                    && (!mods.contains(gdk::ModifierType::SHIFT_MASK)
                        || mods.contains(gdk::ModifierType::CONTROL_MASK))
                {
                    // Let the input method finish composition before applying the caption.
                    if let Some(event) = controller.current_event()
                        && this.input.im_context_filter_keypress(&event)
                    {
                        return glib::Propagation::Stop;
                    }
                    this.apply.emit_clicked();
                    return glib::Propagation::Stop;
                }
                if mods.contains(gdk::ModifierType::CONTROL_MASK) {
                    if key.to_lower() == gdk::Key::b {
                        this.bold.set_active(!this.bold.is_active());
                        return glib::Propagation::Stop;
                    }
                    if key.to_lower() == gdk::Key::i {
                        this.italic.set_active(!this.italic.is_active());
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            }
        ));
        this.input.add_controller(keys);
        this
    }
    pub fn set_pan_cursor(&self, panning: bool) {
        if !panning || self.input.is_visible() {
            self.input
                .set_cursor_from_name(Some(if panning { "grabbing" } else { "text" }));
        }
    }
    /// Steps the open caption's effect: none, inverted outline, contrast outline, shadow, none.
    /// Does nothing while no caption is being edited. Bound to Alt by the editor.
    pub fn cycle_effect(&self) {
        if self.active.get() {
            let at = self.effect.selected() as usize % TextEffect::ALL.len();
            self.effect
                .set_selected(((at + 1) % TextEffect::ALL.len()) as u32);
        }
    }
    pub fn open(&self, draft: Option<TextDraft>, transform: Transform) {
        let Some(d) = draft else { return };
        let scale =
            d.id.and_then(|id| {
                self.state
                    .borrow()
                    .doc
                    .mark(id)
                    .map(|m| m.transform.det().abs().sqrt())
            })
            .unwrap_or(1.);
        self.text_scale.set(scale);
        self.width
            .adjustment()
            .set_upper((f64::from(d.box_width) * scale).max(8192.));
        self.font.adjustment().set_upper(d.font_size.max(8192.));
        self.updating.set(true);
        self.active.set(true);
        self.font.set_value(d.font_size);
        self.width.set_value(f64::from(d.box_width) * scale);
        self.background.set_active(d.background);
        self.bold.set_active(d.text_style.bold);
        self.italic.set_active(d.text_style.italic);
        self.alignment.set_selected(match d.text_style.align {
            TextAlign::Left => 0,
            TextAlign::Center => 1,
            TextAlign::Right => 2,
        });
        self.family
            .set_selected(family_position(&self.families, &d.font_family));
        self.effect.set_selected(
            TextEffect::ALL
                .iter()
                .position(|e| *e == d.text_effect)
                .unwrap_or(0) as u32,
        );
        self.input.buffer().set_text(&d.value);
        let buffer = self.input.buffer();
        buffer.apply_tag(&self.tag, &buffer.start_iter(), &buffer.end_iter());
        self.toolbar.set_visible(true);
        self.input.set_visible(true);
        self.error.set_text("");
        self.apply.set_sensitive(true);
        self.updating.set(false);
        self.refresh_preview();
        self.position(&d, transform);
        self.input.grab_focus();
        let end = self.input.buffer().end_iter();
        self.input.buffer().place_cursor(&end);
    }
    pub fn close(&self) {
        self.updating.set(true);
        self.active.set(false);
        self.toolbar.set_visible(false);
        self.input.set_visible(false);
        *self.preview.borrow_mut() = None;
        self.updating.set(false);
        self.area.queue_draw();
    }
    pub fn owns_widget(&self, widget: &gtk::Widget) -> bool {
        widget == self.input.upcast_ref::<gtk::Widget>()
            || widget.is_ancestor(&self.input)
            || widget == self.toolbar.upcast_ref::<gtk::Widget>()
            || widget.is_ancestor(&self.toolbar)
    }

    pub fn finish(&self) -> bool {
        self.area.grab_focus();
        self.commit();
        if self.active.get() {
            self.input.grab_focus();
            false
        } else {
            true
        }
    }

    pub fn needs_sync(&self, transform: Transform) -> bool {
        self.active.get() && self.placement.get() != (transform.x, transform.y, transform.scale)
    }
    /// Rebuilds the live preview and the input's position from `state`'s open draft, for changes
    /// made to the draft outside the toolbar (keyboard nudges via `State::move_text_draft`).
    /// `state` is the caller's own borrow of the shared state. Does nothing while no caption is
    /// being edited; the unchanged-draft path of `sync` stays cached.
    pub fn refresh_draft(&self, state: &State) {
        if !self.active.get() {
            return;
        }
        *self.preview.borrow_mut() = state.draft_mark();
        self.revision.set(state.doc.revision());
        if let Some(d) = &state.text_draft {
            self.position(d, state.transform);
        }
        self.area.queue_draw();
    }
    pub fn sync(&self, state: &State) {
        if state.text_draft.is_none() {
            if self.active.get() {
                self.close();
            }
            return;
        }
        if self.active.get() {
            if self.revision.get() != state.doc.revision() {
                *self.preview.borrow_mut() = state.draft_mark();
                self.revision.set(state.doc.revision());
            }
            if let Some(d) = &state.text_draft {
                self.position(d, state.transform);
            }
        }
    }
    pub fn paint(&self, state: &State, cr: &gtk::cairo::Context) -> anyhow::Result<()> {
        if let Some(mark) = self.preview.borrow().as_ref() {
            state
                .doc
                .paint_text_preview(cr, state.text_draft.as_ref().and_then(|d| d.id), mark)
        } else {
            state.doc.paint(cr)
        }
    }
    fn refresh_preview(&self) {
        let state = self.state.borrow();
        *self.preview.borrow_mut() = state.draft_mark();
        self.revision.set(state.doc.revision());
        if let Some(d) = &state.text_draft {
            self.position(d, state.transform);
        }
    }
    fn position(&self, draft: &TextDraft, t: Transform) {
        let size = (draft.font_size * t.scale).clamp(1., 32767.);
        let metrics = (
            size,
            draft.text_style.bold,
            draft.text_style.italic,
            draft.font_family.clone(),
            draft.font_style.clone(),
        );
        self.updating.set(true);
        if *self.metrics.borrow() != metrics {
            let font = font_description(
                &draft.font_family,
                &draft.font_style,
                size,
                draft.text_style,
            );
            self.tag.set_property("font-desc", &font);
            *self.metrics.borrow_mut() = metrics;
        }
        self.input.set_justification(match draft.text_style.align {
            TextAlign::Left => gtk::Justification::Left,
            TextAlign::Center => gtk::Justification::Center,
            TextAlign::Right => gtk::Justification::Right,
        });
        self.input.set_wrap_mode(if draft.box_width > 0. {
            gtk::WrapMode::WordChar
        } else {
            gtk::WrapMode::None
        });
        let bounds = self
            .preview
            .borrow()
            .as_ref()
            .and_then(|m| m.local_bounds());
        let w = if draft.box_width > 0. {
            f64::from(draft.box_width) * self.text_scale.get() * t.scale
        } else {
            bounds.map_or(size * 8., |b| {
                f64::from(b.width) * self.text_scale.get() * t.scale
            })
        };
        let h = bounds.map_or(size * 1.5, |b| {
            f64::from(b.height) * self.text_scale.get() * t.scale
        });
        self.input.set_size_request(
            (w.ceil() as i32 + 2).clamp(18, 32767),
            (h.ceil() as i32 + 8).clamp(28, 32767),
        );
        self.input.set_margin_start(
            (t.x + f64::from(draft.at.x) * t.scale)
                .round()
                .clamp(0., 32767.) as i32,
        );
        self.input.set_margin_top(
            (t.y + f64::from(draft.at.y) * t.scale)
                .round()
                .clamp(0., 32767.) as i32,
        );
        self.placement.set((t.x, t.y, t.scale));
        self.updating.set(false);
    }
    fn update(&self) {
        if self.updating.get() || !self.active.get() {
            return;
        }
        let b = self.input.buffer();
        let value = b.text(&b.start_iter(), &b.end_iter(), true).to_string();
        if value.chars().count() > 4096 {
            self.error.set_text("Keep captions within 4096 characters.");
            self.apply.set_sensitive(false);
            return;
        }
        let Some(box_width) = local_wrap(self.width.value(), self.text_scale.get()) else {
            self.error.set_text(&format!(
                "Wrap width must be 0 (auto), or at least {:.1} image pixels.",
                f64::from(crate::document::MIN_BOX_WIDTH) * self.text_scale.get()
            ));
            self.apply.set_sensitive(false);
            return;
        };
        self.error.set_text("");
        self.apply.set_sensitive(true);
        self.updating.set(true);
        b.apply_tag(&self.tag, &b.start_iter(), &b.end_iter());
        self.updating.set(false);
        {
            let mut s = self.state.borrow_mut();
            let Some(d) = s.text_draft.as_mut() else {
                return;
            };
            d.value = value;
            d.font_size = self.font.value();
            d.box_width = box_width;
            d.background = self.background.is_active();
            if let Some(f) = self.families.get(self.family.selected() as usize) {
                d.font_family.clone_from(f);
            }
            if let Some(e) = TextEffect::ALL.get(self.effect.selected() as usize) {
                d.text_effect = *e;
            }
            d.text_style = TextStyle {
                bold: self.bold.is_active(),
                italic: self.italic.is_active(),
                align: match self.alignment.selected() {
                    1 => TextAlign::Center,
                    2 => TextAlign::Right,
                    _ => TextAlign::Left,
                },
            };
        }
        self.refresh_preview();
        self.area.queue_draw();
    }
    fn commit(&self) {
        self.update();
        if !self.apply.is_sensitive() {
            return;
        }
        {
            let mut s = self.state.borrow_mut();
            let Some(d) = s.text_draft.clone() else {
                return;
            };
            s.commit_text(d.value, d.box_width, d.background, d.font_size);
        }
        self.close();
        (self.changed)();
        self.area.grab_focus();
    }
}

/// A searchable list of the installed font families; entry `i` of the dropdown is `families[i]`.
pub fn family_picker(label: &str, tip: &str) -> (gtk::DropDown, Vec<String>) {
    use gtk::glib::prelude::StaticType;
    let families = installed_families();
    let names: Vec<&str> = families.iter().map(String::as_str).collect();
    let picker = gtk::DropDown::from_strings(&names);
    picker.set_expression(Some(gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        None::<gtk::Expression>,
        "string",
    )));
    picker.set_enable_search(true);
    picker.set_tooltip_text(Some(tip));
    picker.update_property(&[gtk::accessible::Property::Label(label)]);
    (picker, families)
}

/// Dropdown position of a stored family; none for a fallback list or a name that is not installed.
pub fn family_position(families: &[String], family: &str) -> u32 {
    families
        .iter()
        .position(|f| f == family.trim())
        .map_or(gtk::INVALID_LIST_POSITION, |i| i as u32)
}

// Controls use image pixels; a resized caption stores its wrap width in local units.
fn local_wrap(pixels: f64, scale: f64) -> Option<f32> {
    let width = (pixels / scale) as f32;
    (width.is_finite() && (width == 0. || width >= crate::document::MIN_BOX_WIDTH)).then_some(width)
}

#[cfg(test)]
mod tests {
    use super::local_wrap;
    #[test]
    fn resized_caption_wrap_uses_image_pixels_without_losing_local_layout() {
        assert_eq!(local_wrap(360., 2.), Some(180.));
        assert_eq!(local_wrap(90., 0.5), Some(180.));
        assert_eq!(local_wrap(0., 2.), Some(0.));
        assert_eq!(local_wrap(16., 2.), None);
        assert_eq!(local_wrap(f64::NAN, 1.), None);
    }
}
