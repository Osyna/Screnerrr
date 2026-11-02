use super::{RenderAction, View};
use crate::{
    interaction::{State, Tool},
    settings::{Action, Trigger},
};
use gtk::{gdk, gio, glib, prelude::*};
use std::{
    path::PathBuf,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) fn tool_name(tool: Tool) -> &'static str {
    match tool {
        Tool::Select => "select",
        Tool::Smart => "smart",
        Tool::Ink => "freehand",
        Tool::Arrow => "arrow",
        Tool::Line => "line",
        Tool::Rectangle => "rectangle",
        Tool::Ellipse => "ellipse",
        Tool::Text => "text",
        Tool::Crop => "crop",
        Tool::Redact => "privacy",
        Tool::Step => "step",
        Tool::Magnify => "magnifier",
        Tool::Spotlight => "spotlight",
        Tool::Highlight => "highlight",
        Tool::Highlighter => "highlighter",
    }
}
pub(super) fn tool_shortcut<'a>(
    settings: &'a crate::settings::Settings,
    tool: Tool,
    fallback: &'a str,
) -> &'a str {
    if let Some(shortcut) = settings.keybinds.get(tool_name(tool)) {
        return shortcut;
    }
    match tool {
        Tool::Highlight if settings.primary_highlighter == "freehand" => "Shift+G",
        Tool::Highlighter if settings.primary_highlighter == "freehand" => "G",
        _ => fallback,
    }
}

impl View {
    pub(super) fn error(&self, message: &str) {
        self.help.set_text(message);
        if let Some(status) = self.toolbars.last() {
            status.set_visible(true);
        }
    }
    pub(super) fn invoke(self: &Rc<Self>, name: &str) -> bool {
        if let Some(index) = name
            .strip_prefix("color-")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| n.checked_sub(1))
        {
            self.palette_color(index);
            return true;
        }
        if name == "actual-pixels" {
            let mut state = self.state.borrow_mut();
            super::zoom(&mut state, self, Some(1.), None);
            self.refresh(&state);
            return true;
        }
        if name == "select-all" {
            if self.ocr.enabled() {
                self.ocr.select_all();
            } else {
                super::select_all_objects(&mut self.state.borrow_mut(), self);
            }
            return true;
        }
        if let Some((_, button)) = self.tools.iter().find(|(tool, _)| tool_name(*tool) == name) {
            if button.is_sensitive() {
                button.set_active(true);
                return true;
            }
            return false;
        }
        let button = self.commands.borrow().get(name).cloned();
        if let Some(button) = button.filter(|b| b.is_sensitive()) {
            button.emit_clicked();
            return true;
        }
        false
    }
    pub(super) fn builtin(&self, command: &str) -> bool {
        !self.settings.keybinds.contains_key(command)
    }
    pub(super) fn configured_key(
        self: &Rc<Self>,
        key: gdk::Key,
        modifiers: gdk::ModifierType,
    ) -> bool {
        let relevant = gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::HYPER_MASK
            | gdk::ModifierType::META_MASK;
        if let Some((_, _, command)) = self
            .bindings
            .iter()
            .find(|(k, m, _)| k.to_lower() == key.to_lower() && *m == modifiers & relevant)
        {
            self.invoke(command);
            return true;
        }
        false
    }
    pub(super) fn editable_focus(&self) -> bool {
        self.window
            .upgrade()
            .and_then(|window| gtk::prelude::RootExt::focus(&window))
            .is_some_and(|focus| {
                focus.is::<gtk::Text>()
                    || focus.is::<gtk::Entry>()
                    || focus.is::<gtk::TextView>()
                    || focus.is::<gtk::SpinButton>()
                    || focus.is::<gtk::Scale>()
            })
    }
    pub(super) fn key(self: &Rc<Self>, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
        let alt = modifiers.contains(gdk::ModifierType::ALT_MASK);
        let arrow = match key {
            gdk::Key::Left => Some((-1., 0.)),
            gdk::Key::Right => Some((1., 0.)),
            gdk::Key::Up => Some((0., -1.)),
            gdk::Key::Down => Some((0., 1.)),
            _ => None,
        };
        if ctrl
            && matches!(key, gdk::Key::Return | gdk::Key::KP_Enter)
            && self.state.borrow().text_draft.is_some()
        {
            self.caption.apply.emit_clicked();
            return true;
        }
        if ctrl && alt && self.state.borrow().text_draft.is_some() {
            if let Some((dx, dy)) = arrow {
                let mut state = self.state.borrow_mut();
                let step = self.settings.text_move * if shift { 10. } else { 1. };
                if state.move_text_draft(dx * step, dy * step) {
                    self.caption.refresh_draft(&state);
                    self.refresh(&state);
                }
                return true;
            }
        }
        if key == gdk::Key::Escape {
            if self.panning.take().is_some() {
                self.end_pan(&self.state.borrow());
                return true;
            }
            if self.ocr.enabled() {
                self.ocr.cancel();
                return true;
            }
            let protected = {
                let s = self.state.borrow();
                s.text_draft.is_some() || s.gesture.is_some() || s.magnifier_source.is_some()
            };
            if protected {
                let mut state = self.state.borrow_mut();
                state.cancel();
                state.text_draft = None;
                state.magnifier_source = None;
                self.caption.close();
                self.refresh(&state);
                self.area.grab_focus();
                return true;
            }
        }
        if self.editable_focus() {
            return false;
        }
        if self.configured_key(key, modifiers) {
            return true;
        }
        if key == gdk::Key::Escape {
            if !self.settings.on_escape.is_empty() {
                self.queue_actions(&self.settings.on_escape);
            } else {
                let mut state = self.state.borrow_mut();
                state.cancel();
                state.selected.clear();
                self.refresh(&state);
                drop(state);
                self.invoke("select");
                self.area.grab_focus();
            }
            return true;
        }
        if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
            if self.state.borrow().text_draft.is_some() {
                self.caption.apply.emit_clicked();
            } else {
                self.queue_actions(&self.settings.on_enter);
            }
            return true;
        }
        if modifiers.intersects(
            gdk::ModifierType::SUPER_MASK
                | gdk::ModifierType::HYPER_MASK
                | gdk::ModifierType::META_MASK,
        ) {
            return false;
        }
        if ctrl && alt {
            let command = match key.to_lower() {
                gdk::Key::s => Some("save-as"),
                gdk::Key::c => Some("copy-path"),
                _ => None,
            };
            if let Some(command) = command.filter(|name| self.builtin(name)) {
                self.invoke(command);
                return true;
            }
            return false;
        }
        if alt {
            if let Some((dx, dy)) = arrow {
                self.scroll_by(dx * self.settings.pan_step, dy * self.settings.pan_step);
                return true;
            }
            if let Some(digit) = key.to_unicode().and_then(|c| c.to_digit(10)) {
                let index = if digit == 0 { 9 } else { digit as usize - 1 };
                let command = format!("color-{}", index + 1);
                if self.builtin(&command) {
                    self.palette_color(index);
                    return true;
                }
            }
            return false;
        }
        if ctrl && key.to_lower() == gdk::Key::c && self.ocr.enabled() && self.builtin("copy") {
            self.ocr.copy_text();
            return true;
        }
        let command = if ctrl {
            match key.to_lower() {
                gdk::Key::s => Some(if shift { "save-project" } else { "save" }),
                gdk::Key::o => Some("open-project"),
                gdk::Key::a => Some("select-all"),
                gdk::Key::g => Some(if shift { "ungroup" } else { "group" }),
                gdk::Key::d => Some("duplicate"),
                gdk::Key::Home => Some("front"),
                gdk::Key::End => Some("back"),
                gdk::Key::z => Some(if shift { "redo" } else { "undo" }),
                gdk::Key::y => Some("redo"),
                gdk::Key::c => Some("copy"),
                gdk::Key::plus | gdk::Key::equal => Some("zoom-in"),
                gdk::Key::minus => Some("zoom-out"),
                gdk::Key::_0 => Some("fit"),
                gdk::Key::_1 => Some("actual-pixels"),
                gdk::Key::i => Some("inspector"),
                gdk::Key::k => Some("palette"),
                gdk::Key::t => Some("toolbar"),
                gdk::Key::q => Some("quit"),
                gdk::Key::Delete if shift => Some("clear"),
                _ => None,
            }
        } else {
            match key {
                gdk::Key::_0 => Some("fit"),
                gdk::Key::_1 => Some("actual-pixels"),
                gdk::Key::Delete | gdk::Key::BackSpace => Some("delete"),
                gdk::Key::F8 => Some("ocr"),
                gdk::Key::F11 => Some("fullscreen"),
                _ => None,
            }
        };
        if let Some(command) = command.filter(|name| self.builtin(name)) {
            self.invoke(command);
            return true;
        }
        if ctrl {
            return false;
        }
        if let Some((dx, dy)) = arrow {
            let mut state = self.state.borrow_mut();
            if state.tool == Tool::Select
                && self.area.has_focus()
                && state.gesture.is_none()
                && !state.selected.is_empty()
            {
                let step = if shift { 10. } else { 1. };
                let ids = state.selected.clone();
                state.doc.move_marks(
                    &ids,
                    crate::interaction::pt((dx * step) as f32, (dy * step) as f32),
                );
                self.refresh(&state);
                return true;
            }
        }
        if key.to_lower() == gdk::Key::f && self.builtin("fill") {
            self.invoke("fill");
            return true;
        }
        if key.to_lower() == gdk::Key::k && self.builtin("round-caps") {
            self.invoke("round-caps");
            return true;
        }
        let tool = match key.to_lower() {
            gdk::Key::v => Some(Tool::Select),
            gdk::Key::s => Some(Tool::Smart),
            gdk::Key::p => Some(Tool::Ink),
            gdk::Key::a => Some(Tool::Arrow),
            gdk::Key::l => Some(Tool::Line),
            gdk::Key::h => Some(Tool::Spotlight),
            gdk::Key::r => Some(Tool::Rectangle),
            gdk::Key::e => Some(Tool::Ellipse),
            gdk::Key::t => Some(Tool::Text),
            gdk::Key::c => Some(Tool::Crop),
            gdk::Key::b => Some(Tool::Redact),
            gdk::Key::n => Some(Tool::Step),
            gdk::Key::m => Some(Tool::Magnify),
            gdk::Key::g => Some(
                if (self.settings.primary_highlighter == "freehand") ^ shift {
                    Tool::Highlighter
                } else {
                    Tool::Highlight
                },
            ),
            _ => None,
        };
        if let Some(tool) = tool.filter(|tool| self.builtin(tool_name(*tool))) {
            self.invoke(tool_name(tool));
            return true;
        }
        false
    }
    pub(super) fn queue_actions(self: &Rc<Self>, actions: &[Action]) {
        if self.state.borrow().text_draft.is_some() {
            self.error("Apply or cancel the caption before running image actions.");
            return;
        }
        if self.state.borrow().gesture.is_some() || self.panning.get().is_some() {
            self.error("Finish or cancel the current gesture before running image actions.");
            return;
        }
        if self.rendering.get() || !self.actions.borrow().is_empty() {
            self.error("An image action is already running.");
            return;
        }
        self.actions.borrow_mut().extend(actions.iter().copied());
        self.next_action();
    }
    fn next_action(self: &Rc<Self>) {
        let action = self.actions.borrow_mut().pop_front();
        match action {
            Some(Action::Copy) => {
                if let Some(window) = self.window.upgrade() {
                    self.render_image(&self.state.borrow(), &window, RenderAction::Copy);
                }
            }
            Some(Action::Save) => self.save(false),
            Some(Action::SaveAs) => self.save(true),
            Some(Action::CopyPath) => {
                if !self.copy_path() {
                    self.actions.borrow_mut().clear();
                    return;
                }
                if self.actions.borrow().contains(&Action::Exit) {
                    let Some(window) = self.window.upgrade() else {
                        return;
                    };
                    let hold = window.application().map(|app| app.hold());
                    let clipboard = window.clipboard();
                    let revision = self.state.borrow().doc.revision();
                    let weak = Rc::downgrade(self);
                    let window = window.downgrade();
                    self.rendering.set(true);
                    glib::spawn_future_local(async move {
                        let _hold = hold;
                        let result = crate::workflow::store_clipboard(&clipboard).await;
                        let (Some(view), Some(window)) = (weak.upgrade(), window.upgrade()) else {
                            return;
                        };
                        view.rendering.set(false);
                        if let Err(error) = &result {
                            view.error(&format!("{error:#}"));
                        }
                        view.complete_action(&window, result.is_ok(), None, revision, false);
                    });
                } else {
                    self.next_action();
                }
            }
            Some(Action::Exit) => {
                self.actions.borrow_mut().clear();
                if let Some(window) = self.window.upgrade() {
                    let state = self.state.borrow();
                    let saved = self.exported_revision.get() == Some(state.doc.revision())
                        && state.text_draft.is_none()
                        && state.gesture.is_none();
                    drop(state);
                    if saved {
                        window.destroy();
                    } else {
                        window.close();
                    }
                }
            }
            None => {
                if self.auto_pending.replace(false) {
                    self.start_auto_copy();
                }
            }
        }
    }
    pub(super) fn complete_action(
        self: &Rc<Self>,
        window: &gtk::ApplicationWindow,
        success: bool,
        trigger: Option<Trigger>,
        revision: u64,
        durable: bool,
    ) {
        if !success {
            self.actions.borrow_mut().clear();
            return;
        }
        if trigger.is_some() {
            let state = self.state.borrow();
            if state.doc.revision() != revision
                || state.text_draft.is_some()
                || state.gesture.is_some()
            {
                drop(state);
                self.actions.borrow_mut().clear();
                self.error("Export completed, but the image changed while rendering. The editor stays open; export the new image before exiting.");
                return;
            }
            if durable {
                self.exported_revision.set(Some(revision));
            }
        }
        let exit = trigger.is_some_and(|t| self.settings.early_exit.contains(&t));
        if trigger == Some(Trigger::Copy) && self.settings.save_after_copy {
            let mut pending = self.actions.borrow_mut();
            if exit {
                pending.push_front(Action::Exit);
            }
            pending.push_front(Action::Save);
        } else if exit {
            self.actions.borrow_mut().clear();
            window.destroy();
            return;
        }
        self.next_action();
    }
    pub(super) fn copy_must_persist(&self) -> bool {
        self.settings.early_exit.contains(&Trigger::Copy)
            || self.actions.borrow().contains(&Action::Exit)
    }
    pub(super) fn copy_path(&self) -> bool {
        let path = self.last_saved.borrow();
        let Some(path) = path.as_ref() else {
            self.error("Save a PNG first, then copy its file path.");
            return false;
        };
        let Some(text) = path.to_str() else {
            self.error("This file path is not UTF-8 and cannot be copied as exact text.");
            return false;
        };
        if let Some(window) = self.window.upgrade() {
            window.clipboard().set_text(text);
            self.help.set_text("Saved image path copied.");
            true
        } else {
            false
        }
    }
    pub(super) fn save(self: &Rc<Self>, choose: bool) {
        if self.state.borrow().text_draft.is_some() {
            self.actions.borrow_mut().clear();
            self.error("Apply or cancel the caption before exporting.");
            return;
        }
        if self.rendering.get() {
            self.error("An image operation is already running.");
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let destination = match self.settings.output_path() {
            Ok(path) => path,
            Err(error) => {
                self.actions.borrow_mut().clear();
                self.error(&format!("Cannot resolve output filename: {error:#}"));
                return;
            }
        };
        if !choose && let Some(path) = destination.as_ref() {
            self.render_image(
                &self.state.borrow(),
                &window,
                RenderAction::Png(path.clone(), Trigger::Save),
            );
            return;
        }
        let dialog = gtk::FileChooserNative::new(
            Some("Export PNG"),
            Some(&window),
            gtk::FileChooserAction::Save,
            Some("Export"),
            Some("Cancel"),
        );
        dialog.set_modal(true);
        let name = destination
            .as_ref()
            .filter(|p| p.as_os_str() != "-")
            .and_then(|p| p.file_name())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                format!(
                    "SharpShot-{}.png",
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                )
            });
        dialog.set_current_name(&name);
        if let Some(parent) = destination
            .as_ref()
            .and_then(|p| p.parent())
            .filter(|p| p.is_dir())
        {
            let _ = dialog.set_current_folder(Some(&gio::File::for_path(parent)));
        } else if let Some(parent) = self.last_saved.borrow().as_ref().and_then(|p| p.parent()) {
            let _ = dialog.set_current_folder(Some(&gio::File::for_path(parent)));
        }
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("PNG image"));
        filter.add_pattern("*.png");
        dialog.add_filter(&filter);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let response = dialog.run_future().await;
            let path = dialog.file().and_then(|f| f.path());
            dialog.destroy();
            let Some(view) = weak.upgrade() else {
                return;
            };
            if response != gtk::ResponseType::Accept {
                view.actions.borrow_mut().clear();
                return;
            }
            let Some(path) = path else {
                view.actions.borrow_mut().clear();
                view.error("Choose a local path for the PNG.");
                return;
            };
            if let Some(window) = view.window.upgrade() {
                view.render_image(
                    &view.state.borrow(),
                    &window,
                    RenderAction::Png(path, Trigger::SaveAs),
                );
            }
        });
    }
    pub(super) fn set_toolbars(&self, visible: bool) {
        self.chrome_visible.set(visible);
        for widget in &self.toolbars {
            widget.set_visible(visible);
        }
    }
    pub(super) fn palette_color(&self, index: usize) {
        if let Some((button, _, _)) = self.swatches.get(index) {
            if button.is_sensitive() {
                button.emit_clicked();
            }
        } else {
            self.color.activate();
        }
    }
    pub(super) fn scroll_by(&self, dx: f64, dy: f64) {
        let h = self.scroll.hadjustment();
        let v = self.scroll.vadjustment();
        let old = (h.value(), v.value());
        h.set_value(old.0 + dx);
        v.set_value(old.1 + dy);
        if let Some(mut anchor) = self.zoom_anchor.get() {
            anchor.cursor.0 -= h.value() - old.0;
            anchor.cursor.1 -= v.value() - old.1;
            self.zoom_anchor.set(Some(anchor));
        }
    }
    pub(super) fn schedule_auto(self: &Rc<Self>, state: &State) {
        let revision = state.doc.revision();
        if !self.settings.auto_copy || revision == self.auto_revision.get() {
            return;
        }
        self.auto_revision.set(revision);
        if let Some(source) = self.auto_source.borrow_mut().take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(Duration::from_millis(200), move || {
            if let Some(view) = weak.upgrade() {
                view.auto_source.borrow_mut().take();
                view.start_auto_copy();
            }
        });
        *self.auto_source.borrow_mut() = Some(source);
    }
    fn start_auto_copy(self: &Rc<Self>) {
        if self.rendering.get() || !self.actions.borrow().is_empty() {
            self.auto_pending.set(true);
            return;
        }
        let state = self.state.borrow();
        if state.gesture.is_some() || state.text_draft.is_some() {
            return;
        }
        if let Some(window) = self.window.upgrade().filter(|w| w.is_visible()) {
            self.render_image(&state, &window, RenderAction::AutoCopy);
        }
    }
    pub(super) fn saved_path(&self, path: PathBuf) {
        if path.as_os_str() != "-" {
            *self.last_saved.borrow_mut() = Some(path.canonicalize().unwrap_or(path));
        }
    }
}
