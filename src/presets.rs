//! Named style presets: annotation defaults (colour, width, arrowhead, text) plus the output
//! frame, kept in one private, versioned, bounded JSON file in the user's config folder.
//!
//! The file is user-editable text, so it is untrusted input: every field is range-checked on
//! load and again before each save. A file that cannot be read is reported and never silently
//! overwritten; one written by a newer SharpShot is left exactly as it is.
use crate::{
    chrome::{self, Icon},
    designer::{caption, named},
    document::{MAX_FONT, MAX_HEAD_SIZE, MAX_WIDTH, TextStyle},
    interaction::State,
    presentation::Presentation,
};
use anyhow::{Context, Result, bail, ensure};
use gtk::{gdk, glib, prelude::*};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    rc::Rc,
};

pub const VERSION: u32 = 1;
pub const MAX_PRESETS: usize = 64;
pub const MAX_NAME: usize = 48;
const MAX_FILE: u64 = 256 << 10;

/// Creates `dir` (and parents) owner-only, and tightens an existing folder that is not.
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    match fs::metadata(dir) {
        Ok(meta) => {
            ensure!(meta.is_dir(), "{} is not a folder", dir.display());
            if meta.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("Cannot make {} private", dir.display()))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("Cannot create {}", dir.display())),
        Err(e) => Err(e).with_context(|| format!("Cannot inspect {}", dir.display())),
    }
}

/// Replaces `path` atomically with an owner-only file; a failure leaves the old file intact.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no folder", path.display()))?;
    private_dir(dir)?;
    // NamedTempFile is created 0600.
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("Cannot create a file in {}", dir.display()))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("Cannot save {}", path.display()))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// `None` when the file does not exist; anything but a regular file within `max` is an error.
pub(crate) fn read_bounded(path: &Path, max: u64) -> Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Cannot open {}", path.display())),
    };
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "{} is not a regular file", path.display());
    ensure!(
        meta.len() <= max,
        "{} is larger than the {} KiB limit",
        path.display(),
        max >> 10
    );
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max,
        "{} grew while reading",
        path.display()
    );
    Ok(Some(bytes))
}

pub(crate) fn config_dir() -> PathBuf {
    glib::user_config_dir().join("sharpshot")
}

/// Trimmed, non-empty, at most [`MAX_NAME`] characters, no control characters.
pub fn clean_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    ensure!(!name.is_empty(), "Enter a name for the preset");
    ensure!(
        name.chars().count() <= MAX_NAME,
        "Preset names are limited to {MAX_NAME} characters"
    );
    ensure!(
        !name.chars().any(char::is_control),
        "Preset names cannot contain control characters"
    );
    Ok(name.to_owned())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub name: String,
    pub color: [f64; 4],
    pub width: f64,
    pub head_size: f64,
    pub text_size: f64,
    #[serde(default)]
    pub text_style: TextStyle,
    /// Frame style only: fixed canvas and output sizes belong to one screenshot, never to a preset.
    pub presentation: Presentation,
}

impl Preset {
    /// Captures `state`'s drawing defaults together with `presentation`.
    pub fn from_parts(name: &str, state: &State, presentation: Presentation) -> Result<Self> {
        let preset = Self {
            name: clean_name(name)?,
            color: state.color,
            width: state.width,
            head_size: state.head_size,
            text_size: state.text_size,
            text_style: state.text_style,
            presentation: Presentation {
                canvas_size: None,
                output_size: None,
                ..presentation
            },
        };
        preset.validate()?;
        Ok(preset)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            clean_name(&self.name)? == self.name,
            "Preset name is not normalised"
        );
        ensure!(
            self.color.iter().all(|c| (0.0..=1.0).contains(c)),
            "Preset “{}” has a colour out of range",
            self.name
        );
        ensure!(
            (1.0..=MAX_WIDTH).contains(&self.width)
                && (0.0..=MAX_HEAD_SIZE).contains(&self.head_size)
                && (1.0..=MAX_FONT).contains(&self.text_size),
            "Preset “{}” has a width or size out of range",
            self.name
        );
        self.presentation
            .validate()
            .with_context(|| format!("Preset “{}” has an invalid frame", self.name))?;
        ensure!(
            self.presentation.canvas_size.is_none() && self.presentation.output_size.is_none(),
            "Preset “{}” must not fix a canvas or output size",
            self.name
        );
        Ok(())
    }

    /// Frame first: when it is rejected nothing at all changes. The frame is one undo step and
    /// keeps the document's own canvas and output size.
    pub fn apply(&self, state: &mut State, defaults: bool, frame: bool) -> Result<()> {
        if frame {
            let current = &state.doc.presentation;
            let presentation = Presentation {
                canvas_size: current.canvas_size,
                output_size: current.output_size,
                ..self.presentation.clone()
            };
            state.doc.set_presentation(presentation)?;
        }
        if defaults {
            state.color = self.color;
            state.width = self.width;
            state.head_size = self.head_size;
            state.text_size = self.text_size;
            state.text_style = self.text_style;
        }
        Ok(())
    }

    fn summary(&self) -> String {
        let frame = if self.presentation == Presentation::default() {
            "no frame"
        } else {
            "framed"
        };
        format!(
            "{:.0} px · text {:.0} · {frame}",
            self.width, self.text_size
        )
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetFile {
    pub version: u32,
    pub presets: Vec<Preset>,
}

impl Default for PresetFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            presets: vec![],
        }
    }
}

fn key(name: &str) -> String {
    name.to_lowercase()
}

impl PresetFile {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct Head {
            version: u32,
        }
        let head: Head = serde_json::from_slice(bytes).context("Not a SharpShot preset file")?;
        ensure!(
            head.version <= VERSION,
            "This preset file was written by a newer SharpShot (version {}); it is left untouched",
            head.version
        );
        ensure!(
            head.version == VERSION,
            "Unsupported preset file version {}",
            head.version
        );
        let file: Self = serde_json::from_slice(bytes).context("Invalid preset file")?;
        file.validate()?;
        Ok(file)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == VERSION, "Unsupported preset file version");
        ensure!(
            self.presets.len() <= MAX_PRESETS,
            "At most {MAX_PRESETS} presets are supported"
        );
        let mut seen = std::collections::HashSet::new();
        for preset in &self.presets {
            preset.validate()?;
            ensure!(
                seen.insert(key(&preset.name)),
                "Two presets are named “{}”",
                preset.name
            );
        }
        Ok(())
    }

    pub fn find(&self, name: &str) -> Option<&Preset> {
        let k = key(name.trim());
        self.presets.iter().find(|p| key(&p.name) == k)
    }

    /// Adds `preset`, or replaces the one with the same name (case-insensitive); `true` if replaced.
    pub fn insert(&mut self, preset: Preset) -> Result<bool> {
        preset.validate()?;
        let k = key(&preset.name);
        if let Some(slot) = self.presets.iter_mut().find(|p| key(&p.name) == k) {
            *slot = preset;
            return Ok(true);
        }
        ensure!(
            self.presets.len() < MAX_PRESETS,
            "At most {MAX_PRESETS} presets can be saved; delete one first"
        );
        self.presets.push(preset);
        Ok(false)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let k = key(name.trim());
        let before = self.presets.len();
        self.presets.retain(|p| key(&p.name) != k);
        self.presets.len() != before
    }
}

pub fn path() -> PathBuf {
    config_dir().join("presets.json")
}

fn load_from(path: &Path) -> Result<PresetFile> {
    match read_bounded(path, MAX_FILE)? {
        None => Ok(PresetFile::default()),
        Some(bytes) => PresetFile::parse(&bytes)
            .with_context(|| format!("Cannot use presets in {}", path.display())),
    }
}

fn save_to(path: &Path, file: &PresetFile) -> Result<()> {
    file.validate()?;
    let bytes = serde_json::to_vec_pretty(file).context("Cannot encode presets")?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE,
        "Presets are too large to save"
    );
    // Non-finite numbers become null in JSON; a file that cannot be read back is never written.
    ensure!(
        PresetFile::parse(&bytes)? == *file,
        "Presets do not survive saving unchanged"
    );
    write_private(path, &bytes)
}

/// Loads the presets; a missing file is an empty list, an unreadable one an error.
pub fn load() -> Result<PresetFile> {
    load_from(&path())
}

fn update_at<T>(
    path: &Path,
    change: impl FnOnce(&mut PresetFile) -> Result<T>,
) -> Result<(T, PresetFile)> {
    let mut file = load_from(path)?;
    let out = change(&mut file)?;
    save_to(path, &file)?;
    Ok((out, file))
}

/// Reloads from disk, applies `change` and saves, so presets another window saved meanwhile
/// are kept. Nothing is written when loading or `change` fails.
pub fn update<T>(change: impl FnOnce(&mut PresetFile) -> Result<T>) -> Result<(T, PresetFile)> {
    update_at(&path(), change)
}

fn quarantine_at(path: &Path) -> Result<PathBuf> {
    for n in 0..100 {
        let mut name = path.as_os_str().to_owned();
        name.push(if n == 0 {
            ".invalid".to_owned()
        } else {
            format!(".invalid-{n}")
        });
        let target = PathBuf::from(name);
        if !target.exists() {
            fs::rename(path, &target)
                .with_context(|| format!("Cannot move {} aside", path.display()))?;
            return Ok(target);
        }
    }
    bail!("Too many invalid preset files next to {}", path.display())
}

/// Moves an unreadable preset file out of the way (kept, not deleted) so presets can be saved.
pub fn set_aside() -> Result<PathBuf> {
    quarantine_at(&path())
}

struct Ui {
    window: glib::WeakRef<gtk::Window>,
    state: Rc<RefCell<State>>,
    refresh: Rc<dyn Fn()>,
    file: RefCell<PresetFile>,
    /// The file on disk cannot be used; saving is off until it is set aside.
    broken: Cell<bool>,
    list: gtk::ListBox,
    name: gtk::Entry,
    status: gtk::Label,
    defaults: gtk::CheckButton,
    frame: gtk::CheckButton,
    apply: gtk::Button,
    delete: gtk::Button,
    save: gtk::Button,
    aside: gtk::Button,
}

fn swatch(color: [f64; 4]) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(16)
        .content_height(16)
        .can_target(false)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(move |_, cr, w, h| {
        cr.set_source_rgba(color[0], color[1], color[2], color[3]);
        cr.arc(
            f64::from(w) / 2.0,
            f64::from(h) / 2.0,
            f64::from(w.min(h)) / 2.0,
            0.0,
            std::f64::consts::TAU,
        );
        if let Err(error) = cr.fill() {
            eprintln!("Cannot draw preset swatch: {error}");
        }
    });
    area
}

impl Ui {
    fn say(&self, message: &str) {
        self.status.set_text(message);
    }

    fn selected(&self) -> Option<Preset> {
        let row = self.list.selected_row()?;
        self.file
            .borrow()
            .presets
            .get(usize::try_from(row.index()).ok()?)
            .cloned()
    }

    fn sensitivity(&self) {
        let picked = self.selected().is_some();
        let ok = !self.broken.get();
        self.apply.set_sensitive(picked && ok);
        self.delete.set_sensitive(picked && ok);
        self.save.set_sensitive(ok);
        self.name.set_sensitive(ok);
        self.aside.set_visible(self.broken.get());
    }

    fn fill(&self, file: PresetFile, select: Option<&str>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for preset in &file.presets {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.set_margin_top(4);
            line.set_margin_bottom(4);
            line.set_margin_start(6);
            line.set_margin_end(6);
            line.append(&swatch(preset.color));
            let label = gtk::Label::new(Some(&preset.name));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            line.append(&label);
            line.append(&caption(&preset.summary(), "dimension-label"));
            self.list.append(&line);
        }
        let at = select.and_then(|select| {
            file.presets
                .iter()
                .position(|p| key(&p.name) == key(select))
        });
        // Stored first: selecting a row runs `on_select`, which reads the stored list.
        *self.file.borrow_mut() = file;
        if let Some(row) = at.and_then(|at| self.list.row_at_index(at as i32)) {
            self.list.select_row(Some(&row));
        }
        self.sensitivity();
    }

    fn reload(&self) {
        match load() {
            Ok(file) => {
                self.broken.set(false);
                self.fill(file, None);
                self.say("");
            }
            Err(error) => {
                self.broken.set(true);
                self.fill(PresetFile::default(), None);
                self.say(&format!("{error:#}"));
            }
        }
    }

    fn on_select(&self) {
        if let Some(preset) = self.selected() {
            self.name.set_text(&preset.name);
        }
        self.sensitivity();
    }

    fn save(&self) {
        let preset = match self.state.try_borrow() {
            Ok(state) => Preset::from_parts(
                self.name.text().as_str(),
                &state,
                state.doc.presentation.clone(),
            ),
            Err(error) => Err(error.into()),
        };
        let name = preset.as_ref().map(|p| p.name.clone()).unwrap_or_default();
        match preset.and_then(|p| update(|f| f.insert(p))) {
            Ok((replaced, file)) => {
                self.fill(file, Some(&name));
                self.say(&format!(
                    "{} “{name}”",
                    if replaced { "Replaced" } else { "Saved" }
                ));
            }
            Err(error) => self.say(&format!("{error:#}")),
        }
    }

    fn apply(&self) {
        let Some(preset) = self.selected() else {
            return;
        };
        let (defaults, frame) = (self.defaults.is_active(), self.frame.is_active());
        if !defaults && !frame {
            return self.say("Choose what to apply: pen defaults, frame, or both");
        }
        let result = match self.state.try_borrow_mut() {
            Ok(mut state) => preset.apply(&mut state, defaults, frame),
            Err(error) => Err(error.into()),
        };
        match result {
            Ok(()) => {
                (self.refresh)();
                self.say(&format!("Applied “{}”", preset.name));
            }
            Err(error) => self.say(&format!("{error:#}")),
        }
    }

    fn delete(self: &Rc<Self>) {
        let (Some(preset), Some(window)) = (self.selected(), self.window.upgrade()) else {
            return;
        };
        let weak = Rc::downgrade(self);
        let name = preset.name.clone();
        confirm_delete(&window, &name, move || {
            let Some(ui) = weak.upgrade() else { return };
            match update(|f| Ok(f.remove(&preset.name))) {
                Ok((_, file)) => {
                    ui.fill(file, None);
                    ui.say(&format!("Deleted “{}”", preset.name));
                }
                Err(error) => ui.say(&format!("{error:#}")),
            }
        });
    }

    fn set_aside(&self) {
        match set_aside() {
            Ok(moved) => {
                self.reload();
                self.say(&format!("Invalid file kept as {}", moved.display()));
            }
            Err(error) => self.say(&format!("{error:#}")),
        }
    }
}
/// Asks before `accept` runs; Escape and "Keep" leave everything as it was.
pub(crate) fn confirm(
    window: &gtk::Window,
    text: &str,
    detail: &str,
    accept_label: &str,
    accept: impl Fn() + 'static,
) {
    let dialog = gtk::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .message_type(gtk::MessageType::Question)
        .text(text)
        .secondary_text(detail)
        .build();
    dialog.add_button("Keep", gtk::ResponseType::Cancel);
    dialog.add_button(accept_label, gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Cancel);
    dialog.connect_response(move |dialog, response| {
        dialog.close();
        if response == gtk::ResponseType::Accept {
            accept();
        }
    });
    dialog.present();
}

pub(crate) fn confirm_delete(window: &gtk::Window, name: &str, accept: impl Fn() + 'static) {
    confirm(
        window,
        &format!("Delete “{name}”?"),
        "This cannot be undone.",
        "Delete",
        accept,
    );
}

fn click(ui: &Rc<Ui>, f: fn(&Rc<Ui>)) -> impl Fn(&gtk::Button) + 'static {
    let weak = Rc::downgrade(ui);
    move |_| {
        if let Some(ui) = weak.upgrade() {
            f(&ui);
        }
    }
}

/// Saved styles dialog: load, apply, save and delete named presets.
pub fn open(parent: &gtk::ApplicationWindow, state: Rc<RefCell<State>>, refresh: Rc<dyn Fn()>) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Style presets")
        .default_width(480)
        .default_height(520)
        .build();
    window.add_css_class("sharpshot");
    window.set_titlebar(Some(&gtk::HeaderBar::new()));

    let ui = Rc::new(Ui {
        window: window.downgrade(),
        state,
        refresh,
        file: RefCell::new(PresetFile::default()),
        broken: Cell::new(false),
        list: gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .activate_on_single_click(false)
            .build(),
        name: gtk::Entry::builder()
            .placeholder_text("Preset name")
            .max_length(MAX_NAME as i32)
            .hexpand(true)
            .build(),
        status: caption("", "dimension-label"),
        defaults: gtk::CheckButton::with_label("Pen defaults"),
        frame: gtk::CheckButton::with_label("Frame"),
        apply: chrome::button(
            Icon::Check,
            "Apply preset",
            "Apply the selected preset (double-click also applies)",
        ),
        delete: chrome::button(Icon::Delete, "Delete preset", "Delete the selected preset"),
        save: chrome::button(
            Icon::Save,
            "Save preset",
            "Save the current pen defaults and frame under this name; the same name replaces",
        ),
        aside: chrome::button(
            Icon::Open,
            "Set invalid file aside",
            "Keep the unreadable preset file under a new name so presets can be saved again",
        ),
    });
    ui.status.set_wrap(true);
    ui.defaults.set_active(true);
    ui.frame.set_active(true);
    ui.defaults.set_tooltip_text(Some(
        "Colour, width, arrowhead and text style for new marks",
    ));
    ui.frame.set_tooltip_text(Some(
        "Background, padding, border, corners and shadow (one undo step)",
    ));
    named(&ui.name, "Preset name", "Name for the preset to save");
    named(&ui.list, "Saved presets", "Saved style presets");

    let weak = Rc::downgrade(&ui);
    ui.list.connect_row_selected({
        let weak = weak.clone();
        move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.on_select();
            }
        }
    });
    ui.list.connect_row_activated({
        let weak = weak.clone();
        move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.apply();
            }
        }
    });
    ui.name.connect_activate({
        let weak = weak.clone();
        move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.save();
            }
        }
    });
    ui.apply.connect_clicked(click(&ui, |ui| ui.apply()));
    ui.save.connect_clicked(click(&ui, |ui| ui.save()));
    ui.delete.connect_clicked(click(&ui, Ui::delete));
    ui.aside.connect_clicked(click(&ui, |ui| ui.set_aside()));

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&ui.list)
        .build();
    let apply_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    apply_row.append(&ui.defaults);
    apply_row.append(&ui.frame);
    let gap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    gap.set_hexpand(true);
    apply_row.append(&gap);
    apply_row.append(&ui.delete);
    apply_row.append(&ui.apply);
    let save_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    save_row.append(&ui.name);
    save_row.append(&ui.save);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.append(&caption("Saved styles", "title"));
    body.append(&scroll);
    body.append(&apply_row);
    body.append(&caption("Save current style", "title"));
    body.append(&save_row);
    body.append(&ui.aside);
    body.append(&ui.status);
    window.set_child(Some(&body));

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let weak = ui.window.clone();
        move |_, key, _, _| {
            if key != gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            if let Some(window) = weak.upgrade() {
                window.close();
            }
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);
    let keep = ui.clone();
    window.connect_destroy(move |_| {
        let _ = &keep;
    });
    ui.reload();
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document::{Document, TextAlign},
        presentation::Background,
    };
    use std::sync::mpsc;

    fn state() -> State {
        let (tx, _) = mpsc::channel();
        State::new(Document::new(image::RgbaImage::new(40, 30)).unwrap(), tx)
    }

    fn preset(name: &str) -> Preset {
        Preset::from_parts(name, &state(), Presentation::default()).unwrap()
    }

    #[test]
    fn names_are_trimmed_bounded_and_printable() {
        assert_eq!(clean_name("  Docs ").unwrap(), "Docs");
        assert!(clean_name("   ").is_err());
        assert!(clean_name(&"x".repeat(MAX_NAME + 1)).is_err());
        assert!(clean_name("a\nb").is_err());
        assert!(
            clean_name(&"é".repeat(MAX_NAME)).is_ok(),
            "limit counts characters"
        );
    }

    #[test]
    fn capture_drops_the_fixed_sizes_and_validates() {
        let mut state = state();
        state.text_style.bold = true;
        state.text_style.align = TextAlign::Center;
        let pres = Presentation {
            padding: 8,
            canvas_size: Some([200, 100]),
            output_size: Some([100, 80]),
            ..Presentation::default()
        };
        let p = Preset::from_parts("Blog", &state, pres).unwrap();
        assert_eq!(
            (p.presentation.canvas_size, p.presentation.output_size),
            (None, None)
        );
        assert_eq!(p.presentation.padding, 8);
        assert!(p.text_style.bold && p.text_style.align == TextAlign::Center);
        state.width = f64::NAN;
        assert!(Preset::from_parts("Bad", &state, Presentation::default()).is_err());
    }

    #[test]
    fn insert_replaces_case_insensitively_and_is_bounded() {
        let mut file = PresetFile::default();
        assert!(!file.insert(preset("Docs")).unwrap());
        assert!(file.insert(preset("docs")).unwrap());
        assert_eq!(file.presets.len(), 1);
        assert!(file.find(" DOCS ").is_some());
        for i in 1..MAX_PRESETS {
            file.insert(preset(&format!("p{i}"))).unwrap();
        }
        assert!(file.insert(preset("one too many")).is_err());
        assert!(file.remove("P1") && !file.remove("P1"));
    }

    #[test]
    fn parse_rejects_hostile_or_foreign_files() {
        let good = serde_json::to_vec(&PresetFile {
            version: VERSION,
            presets: vec![preset("A")],
        })
        .unwrap();
        assert_eq!(PresetFile::parse(&good).unwrap().presets.len(), 1);

        let text = String::from_utf8(good).unwrap();
        let edit = |from: &str, to: &str| {
            assert!(text.contains(from), "{from}");
            PresetFile::parse(text.replacen(from, to, 1).as_bytes())
        };
        assert!(edit("\"width\":4.0", "\"width\":0.0").is_err());
        assert!(edit("\"width\":4.0", "\"width\":1e9").is_err());
        assert!(
            edit("\"version\":1", "\"version\":2")
                .unwrap_err()
                .to_string()
                .contains("newer")
        );
        assert!(edit("\"version\":1", "\"version\":0").is_err());
        assert!(edit("\"name\":\"A\"", "\"name\":\"\"").is_err());
        assert!(edit("\"presets\"", "\"extra\":1,\"presets\"").is_err());
        assert!(edit("\"output_size\":null", "\"output_size\":[5,5]").is_err());
        assert!(edit("\"canvas_size\":null", "\"canvas_size\":[5,5]").is_err());
        assert!(PresetFile::parse(b"not json").is_err());
        assert!(PresetFile::parse(b"{}").is_err());
    }

    #[test]
    fn duplicate_names_in_a_file_are_rejected() {
        let file = PresetFile {
            version: VERSION,
            presets: vec![preset("A"), preset("a")],
        };
        assert!(file.validate().is_err());
    }

    #[test]
    fn files_are_private_atomic_and_bad_files_are_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cfg").join("presets.json");
        let (replaced, file) = update_at(&path, |f| f.insert(preset("A"))).unwrap();
        assert!(!replaced && file.presets.len() == 1);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(load_from(&path).unwrap(), file);

        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        update_at(&path, |f| f.insert(preset("B"))).unwrap();
        assert_eq!(
            mode(path.parent().unwrap()),
            0o700,
            "loose folders are tightened"
        );

        let err = update_at(&path, |_| -> Result<()> { bail!("nope") }).unwrap_err();
        assert!(err.to_string().contains("nope"));
        assert_eq!(
            load_from(&path).unwrap().presets.len(),
            2,
            "failed change writes nothing"
        );

        fs::write(&path, b"{ broken").unwrap();
        assert!(update_at(&path, |f| f.insert(preset("C"))).is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            b"{ broken",
            "unreadable file is kept"
        );
        let moved = quarantine_at(&path).unwrap();
        assert_eq!(fs::read(moved).unwrap(), b"{ broken");
        assert!(update_at(&path, |f| f.insert(preset("C"))).is_ok());

        let newer = format!("{{\"version\":{},\"presets\":[]}}", VERSION + 1);
        fs::write(&path, &newer).unwrap();
        assert!(update_at(&path, |f| f.insert(preset("D"))).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), newer);
    }

    #[test]
    fn oversized_files_are_refused_before_parsing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("presets.json");
        fs::write(&path, vec![b' '; MAX_FILE as usize + 1]).unwrap();
        assert!(load_from(&path).unwrap_err().to_string().contains("limit"));
    }

    #[test]
    fn applying_a_frame_keeps_the_documents_canvas_and_output_size() {
        let (tx, _) = mpsc::channel();
        let blue = image::Rgba([0, 0, 255, 255]);
        let red = image::Rgba([255, 0, 0, 255]);
        let mut state = State::new(
            Document::new(image::RgbaImage::from_pixel(40, 30, blue)).unwrap(),
            tx,
        );
        state
            .doc
            .set_presentation(Presentation {
                canvas_size: Some([160, 90]),
                output_size: Some([80, 45]),
                ..Presentation::default()
            })
            .unwrap();
        let before = state.doc.render().unwrap();
        let mut p = preset("Pad");
        p.presentation.padding = 4;
        p.presentation.background = Background::Solid([1., 0., 0., 1.]);
        p.apply(&mut state, false, true).unwrap();
        let rendered = state.doc.render().unwrap();
        assert_eq!(rendered.dimensions(), (80, 45));
        assert_eq!(*rendered.get_pixel(12, 22), red);
        assert_eq!(*rendered.get_pixel(20, 22), blue);
        assert_eq!(*rendered.get_pixel(67, 22), red);
        state.doc.undo();
        assert_eq!(state.doc.render().unwrap(), before);
    }

    #[test]
    fn apply_sets_defaults_and_one_undoable_frame() {
        let mut state = state();
        let mut p = preset("Look");
        p.color = [0.1, 0.2, 0.3, 1.0];
        p.width = 9.0;
        p.text_style.italic = true;
        p.presentation = Presentation {
            padding: 12,
            background: Background::Solid([1.0, 0.0, 0.0, 1.0]),
            ..Presentation::default()
        };
        assert_eq!(state.doc.presentation.output_size, None);
        p.apply(&mut state, true, true).unwrap();
        assert_eq!(state.width, 9.0);
        assert!(state.text_style.italic);
        assert_eq!(state.doc.presentation.padding, 12);
        state.doc.undo();
        assert_eq!(state.doc.presentation, Presentation::default());
        assert!(!state.doc.can_undo(), "the frame was a single undo step");

        // A frame the document rejects must leave the defaults alone too.
        let mut bad = p.clone();
        bad.width = 3.0;
        bad.presentation.padding = crate::presentation::MAX_PADDING + 1;
        assert!(bad.apply(&mut state, true, true).is_err());
        assert_eq!(state.width, 9.0);
        bad.presentation.padding = 0;
        bad.apply(&mut state, true, false).unwrap();
        assert_eq!(state.width, 3.0);
        assert_eq!(
            state.doc.presentation,
            Presentation::default(),
            "defaults only"
        );
    }
}
