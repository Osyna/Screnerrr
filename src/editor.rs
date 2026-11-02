#[path = "editor_actions.rs"]
mod actions;

use crate::{
    chrome::{self, Icon},
    context_menu::ContextMenu,
    document::{Document, MarkKind},
    interaction::{self, Gesture, State, Tool, Transform},
    project::Snapshot,
    settings::{Action, Settings, Trigger},
    shapes::{Point, Recognition, Recognizer},
};
use anyhow::Result;
use gtk::{
    cairo::{Content, Context, FontOptions, Format, ImageSurface, RecordingSurface},
    gdk, gio, glib,
    pango::prelude::FontMapExt,
    prelude::*,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    time::{SystemTime, UNIX_EPOCH},
};

const TOOLS: &[(Tool, Icon, &str, &str)] = &[
    (
        Tool::Select,
        Icon::Select,
        "Select",
        "V · Select, move, resize and rotate. Shift-click adds; drag empty space to select several.",
    ),
    (
        Tool::Eraser,
        Icon::Eraser,
        "Eraser",
        "X · Click an annotation or hold the left button and drag to erase whole objects. Privacy masks can also be removed; Undo restores the gesture. Esc stops erasing and keeps removals.",
    ),
    (
        Tool::Smart,
        Icon::Smart,
        "Smart shape",
        "S · Sketch a stroke, then choose a correction or keep the original.",
    ),
    (
        Tool::Ink,
        Icon::Pen,
        "Freehand",
        "P · Draw ink without recognition.",
    ),
    (
        Tool::Arrow,
        Icon::Arrow,
        "Attached arrow",
        "A · Drag an arrow. Endpoints attach to nearby objects and follow them.",
    ),
    (
        Tool::Line,
        Icon::Line,
        "Line",
        "L · Drag a line; change its thickness in the style strip.",
    ),
    (
        Tool::Rectangle,
        Icon::Rectangle,
        "Rectangle",
        "R · Drag between opposite corners.",
    ),
    (
        Tool::Ellipse,
        Icon::Ellipse,
        "Ellipse",
        "E · Drag a bounding box.",
    ),
    (
        Tool::Text,
        Icon::Text,
        "Text",
        "T · Click to type on the image; double-click an existing caption to edit.",
    ),
    (
        Tool::Redact,
        Icon::Redact,
        "Secure redaction",
        "B · Drag an opaque black redaction. Share PNG, not projects, which retain originals.",
    ),
    (
        Tool::Step,
        Icon::Step,
        "Numbered step",
        "N · Click to place, or hold and drag from the centre to size the numbered circle.",
    ),
    (
        Tool::Magnify,
        Icon::Magnify,
        "Magnifier callout",
        "M · Drag a source region, then click to place its zoomed inset.",
    ),
    (
        Tool::Spotlight,
        Icon::Spotlight,
        "Spotlight",
        "H · Drag a clear zone with a rounded border; dim the rest of the image.",
    ),
    (
        Tool::Highlight,
        Icon::Highlight,
        "Block highlighter",
        "G · Drag a translucent rectangle. Ctrl swaps highlighter mode; Alt centres, Shift constrains.",
    ),
    (
        Tool::Highlighter,
        Icon::Highlighter,
        "Freehand highlighter",
        "Shift+G · Paint a translucent stroke. Ctrl swaps mode; Shift draws a 15° aligned segment.",
    ),
    (
        Tool::Crop,
        Icon::Crop,
        "Crop",
        "C · Drag the export region. Reset crop restores the full screenshot.",
    ),
];
const ACCENT: [f64; 3] = [0.388, 0.867, 0.835];

#[derive(Clone, Copy, PartialEq)]
struct ZoomAnchor {
    image: (f64, f64),
    cursor: (f64, f64),
}
impl ZoomAnchor {
    fn offsets(self, t: Transform) -> (f64, f64) {
        (
            t.x + self.image.0 * t.scale - self.cursor.0,
            t.y + self.image.1 * t.scale - self.cursor.1,
        )
    }
}
thread_local! {
    static COPY_REQUEST: Cell<u64> = const { Cell::new(0) };
}

struct SceneCache {
    revision: u64,
    transform: [u64; 3],
    fonts: u32,
    options: FontOptions,
    commands: RecordingSurface,
}

struct View {
    state: Rc<RefCell<State>>,
    settings: Rc<Settings>,
    window: glib::WeakRef<gtk::ApplicationWindow>,
    commands: RefCell<HashMap<&'static str, gtk::Button>>,
    tools: Vec<(Tool, gtk::ToggleButton)>,
    bindings: Vec<(gdk::Key, gdk::ModifierType, String)>,
    toolbars: Vec<gtk::Widget>,
    chrome_visible: Cell<bool>,
    actions: RefCell<VecDeque<Action>>,
    last_saved: RefCell<Option<PathBuf>>,
    auto_revision: Cell<u64>,
    auto_source: RefCell<Option<glib::SourceId>>,
    auto_pending: Cell<bool>,
    exported_revision: Cell<Option<u64>>,
    fill: gtk::ToggleButton,
    round_caps: gtk::ToggleButton,
    corner_radius: gtk::SpinButton,
    area: gtk::DrawingArea,
    menu: ContextMenu,
    scene: gtk::DrawingArea,
    scene_revision: Cell<Option<u64>>,
    scene_cache: RefCell<Option<SceneCache>>,
    scroll: gtk::ScrolledWindow,
    zoom_anchor: Cell<Option<ZoomAnchor>>,
    panning: Cell<Option<(f64, f64)>>,
    rendering: Cell<bool>,
    undo: gtk::Button,
    redo: gtk::Button,
    delete: gtk::Button,
    reset_crop: gtk::Button,
    color: gtk::ColorButton,
    width: gtk::SpinButton,
    opacity: gtk::Scale,
    swatches: Vec<(gtk::ToggleButton, [f64; 4], bool)>,
    presets: Vec<(gtk::ToggleButton, f64)>,
    dimensions: gtk::Label,
    zoom_label: gtk::Label,
    help: gtk::Label,
    syncing: Cell<bool>,
    // Viewport coordinates stay fixed while zoom reallocates and scrolls the canvas.
    pointer: Cell<Option<(f64, f64)>>,
    context: gtk::Box,
    selection_label: gtk::Label,
    group: gtk::Button,
    ungroup: gtk::Button,
    duplicate: gtk::Button,
    edit_text: gtk::Button,
    earlier: gtk::Button,
    later: gtk::Button,
    front: gtk::Button,
    back: gtk::Button,
    privacy_mode: gtk::DropDown,
    lens_zoom: gtk::SpinButton,
    choices: Vec<gtk::Button>,
    keep_ink: gtk::Button,
    caption: Rc<crate::caption::Caption>,
    inspector: Rc<crate::inspector::Inspector>,
    font_size: gtk::SpinButton,
    head_size: gtk::SpinButton,
    ocr: Rc<crate::ocr_ui::OcrUi>,
}
impl View {
    fn refresh(self: &Rc<Self>, state: &State) {
        self.menu.popdown();
        let editing = state.text_draft.is_some();
        let eraser = state.tool == Tool::Eraser;
        self.undo.set_sensitive(state.doc.can_undo() && !editing);
        self.redo.set_sensitive(state.doc.can_redo() && !editing);
        let count = state
            .selected
            .iter()
            .filter(|id| state.doc.mark(**id).is_some())
            .count();
        self.delete.set_sensitive(count > 0 && !editing);
        self.duplicate.set_sensitive(count > 0 && !editing);
        self.front.set_sensitive(count > 0 && !editing);
        self.back.set_sensitive(count > 0 && !editing);
        self.group.set_sensitive(count > 1 && !editing);
        self.ungroup.set_sensitive(
            !editing
                && state
                    .selected
                    .iter()
                    .any(|id| state.doc.mark(*id).is_some_and(|m| m.group.is_some())),
        );
        self.selection_label.set_text(&format!("{count} selected"));
        self.selection_label.set_visible(!editing);
        for button in [
            &self.group,
            &self.ungroup,
            &self.duplicate,
            &self.front,
            &self.back,
        ] {
            button.set_visible(!editing);
        }
        let text = state
            .selected_mark()
            .is_some_and(|m| matches!(m.kind, MarkKind::Text { .. }));
        let step = state
            .selected_mark()
            .is_some_and(|m| matches!(m.kind, MarkKind::Step { .. }));
        self.edit_text.set_visible(text && !editing);
        self.earlier.set_visible(step && !editing);
        self.later.set_visible(step && !editing);
        let lens = state.selected_mark().and_then(|m| {
            if let MarkKind::Magnifier { zoom, .. } = m.kind {
                Some(zoom)
            } else {
                None
            }
        });
        self.syncing.set(true);
        self.font_size
            .set_visible(state.text_draft.is_none() && (text || state.tool == Tool::Text));
        self.font_size.set_value(state.font());
        let connector = state.selected.iter().any(|id| {
            state
                .doc
                .mark(*id)
                .is_some_and(|m| matches!(m.kind, MarkKind::Connector { .. }))
        });
        self.head_size
            .set_visible(connector || state.tool == Tool::Arrow);
        self.head_size.set_value(state.tip());
        self.lens_zoom.set_visible(lens.is_some());
        if let Some(z) = lens {
            self.lens_zoom.set_value(z.into());
        }
        let privacy = state.selected_mark().filter(|m| m.is_redaction());
        self.privacy_mode.set_visible(privacy.is_some());
        if let Some(m) = privacy {
            self.privacy_mode
                .set_selected(u32::from(matches!(m.kind, MarkKind::PrivacyBlur)));
        }
        let correction = state
            .selected
            .first()
            .filter(|_| count == 1)
            .and_then(|id| state.corrections.get(id));
        for (i, button) in self.choices.iter().enumerate() {
            let shape = correction.and_then(|c| c.choices.get(i));
            button.set_visible(shape.is_some());
            if let Some(shape) = shape {
                let icon = match shape.kind.as_str() {
                    "rectangle" => Icon::Rectangle,
                    "ellipse" => Icon::Ellipse,
                    "line" | "curve" => Icon::Line,
                    "arrow" => Icon::Arrow,
                    _ => Icon::Smart,
                };
                button.set_child(Some(&chrome::icon(icon)));
                button.set_tooltip_text(Some(&format!("Replace with {} · undoable", shape.kind)));
                button.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Replace with {}",
                    shape.kind
                ))]);
            }
        }
        self.keep_ink.set_visible(correction.is_some());
        self.context.set_visible(
            count > 0
                || state.text_draft.is_some()
                || self.ocr.enabled()
                || matches!(state.tool, Tool::Text | Tool::Arrow),
        );
        self.ocr.toolbar.set_visible(self.ocr.enabled());
        self.reset_crop.set_sensitive(state.doc.crop.is_some());
        let (color, width) = state.style();
        self.color.set_rgba(&gdk::RGBA::new(
            color[0] as f32,
            color[1] as f32,
            color[2] as f32,
            color[3] as f32,
        ));
        self.width.set_value(width);
        self.opacity.set_value(color[3] * 100.);
        self.width
            .set_sensitive(!eraser && !text && state.tool != Tool::Text);
        self.fill.set_active(state.fill());
        self.round_caps.set_active(state.round_caps());
        self.corner_radius.set_value(state.corner_radius());
        self.fill.set_sensitive(
            !eraser
                && !editing
                && (matches!(state.tool, Tool::Smart | Tool::Rectangle | Tool::Ellipse)
                    || state
                        .selected
                        .iter()
                        .any(|id| state.doc.mark(*id).is_some_and(|m| m.fillable()))),
        );
        self.round_caps.set_sensitive(
            !eraser
                && !editing
                && (matches!(
                    state.tool,
                    Tool::Smart | Tool::Ink | Tool::Arrow | Tool::Line | Tool::Highlighter
                ) || state
                    .selected
                    .iter()
                    .any(|id| state.doc.mark(*id).is_some_and(|m| m.stroked()))),
        );
        self.corner_radius.set_sensitive(
            !eraser
                && !editing
                && (state.tool == Tool::Rectangle
                    || state
                        .selected
                        .iter()
                        .any(|id| state.doc.mark(*id).is_some_and(|m| m.is_rectangle()))),
        );
        let redaction = state
            .selected
            .iter()
            .any(|id| state.doc.mark(*id).is_some_and(|m| m.is_redaction()));
        self.color.set_sensitive(!eraser && !redaction && !editing);
        self.opacity
            .set_sensitive(!eraser && !redaction && !editing);
        for (button, rgb, alpha) in &self.swatches {
            button.set_sensitive(!eraser && !redaction && !editing);
            button.set_active(
                (0..if *alpha { 4 } else { 3 }).all(|i| (color[i] - rgb[i]).abs() < 0.005),
            );
        }
        for (button, value) in &self.presets {
            button.set_sensitive(!eraser && !editing && !text);
            button.set_active((width - value).abs() < 0.01);
        }
        self.syncing.set(false);
        let (w, h) = state
            .crop()
            .map_or((state.doc.width, state.doc.height), |c| (c.width, c.height));
        match state.doc.presentation.dimensions(w, h) {
            Ok((w, h)) => self.dimensions.set_text(&format!("{w} × {h}")),
            Err(_) => self.dimensions.set_text("Invalid output size"),
        }
        self.refresh_cursor(state);
        if self.inspector.widget.is_visible() {
            self.inspector.refresh(state);
        }
        self.inspector
            .widget
            .set_sensitive(state.text_draft.is_none());
        self.caption.sync(state);
        self.refresh_scene(state);
        self.schedule_auto(state);
    }
    fn refresh_scene(&self, state: &State) {
        let dynamic = dynamic_scene(state);
        self.scene.set_visible(!dynamic);
        if !dynamic && self.scene_revision.get() != Some(state.doc.revision()) {
            self.scene.queue_draw();
        }
        self.area.queue_draw();
    }
    fn render_image(
        self: &Rc<Self>,
        state: &State,
        window: &gtk::ApplicationWindow,
        action: RenderAction,
    ) {
        if state.text_draft.is_some() {
            self.actions.borrow_mut().clear();
            self.error("Apply or cancel the caption before rendering.");
            return;
        }
        if self.rendering.replace(true) {
            self.error("An image operation is already running.");
            return;
        }
        let revision = state.doc.revision();
        let automatic = matches!(&action, RenderAction::AutoCopy);
        let trigger = match &action {
            RenderAction::Copy => Some(Trigger::Copy),
            RenderAction::Png(_, trigger) => Some(*trigger),
            _ => None,
        };
        let snapshot = match Snapshot::capture(&state.doc) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.rendering.set(false);
                self.actions.borrow_mut().clear();
                self.error(&format!("Cannot prepare image: {error:#}"));
                return;
            }
        };
        let clipboard_watch =
            matches!(&action, RenderAction::Copy | RenderAction::AutoCopy).then(|| {
                let clipboard = window.clipboard();
                let changed = Rc::new(Cell::new(false));
                let signal = clipboard.connect_changed(glib::clone!(
                    #[strong]
                    changed,
                    move |_| changed.set(true)
                ));
                let request = COPY_REQUEST.with(|current| {
                    let request = current.get().wrapping_add(1);
                    current.set(request);
                    request
                });
                (clipboard, signal, changed, request)
            });
        let hold = window.application().map(|app| app.hold());
        if !automatic {
            self.help.set_text("Rendering image…");
        }
        let view = Rc::downgrade(self);
        let window = window.downgrade();
        glib::spawn_future_local(async move {
            let _hold = hold;
            let result = gio::spawn_blocking(move || render_snapshot(snapshot, action)).await;
            let copy_current =
                clipboard_watch.is_none_or(|(clipboard, signal, changed, request)| {
                    clipboard.disconnect(signal);
                    !changed.get() && COPY_REQUEST.with(|current| current.get() == request)
                });
            let Some(view) = view.upgrade() else {
                return;
            };
            let Some(window) = window.upgrade().filter(|w| w.is_visible()) else {
                view.rendering.set(false);
                view.actions.borrow_mut().clear();
                return;
            };
            let mut success = false;
            let mut durable = false;
            let mut notice = None;
            let mut thumbnail = None;
            match result {
                Ok(Ok(RenderedOutput::Copy(_)))
                    if !copy_current || view.state.borrow().doc.revision() != revision =>
                {
                    view.help.set_text(
                        "Copy skipped: the image or clipboard changed while rendering. Copy again.",
                    );
                }
                Ok(Ok(RenderedOutput::Copy(image))) => {
                    if !automatic
                        && !view.settings.disable_notifications
                        && view.settings.notification_thumbnail
                    {
                        thumbnail = crate::workflow::thumbnail(&image).ok();
                    }
                    let (w, h) = image.dimensions();
                    if let Some(command) = view.settings.copy_command.as_deref() {
                        match crate::workflow::copy_command(image, command).await {
                            Ok(()) => {
                                success = true;
                                durable = true;
                            }
                            Err(error) => view.error(&format!("Cannot copy: {error:#}")),
                        }
                    } else {
                        let bytes = glib::Bytes::from_owned(image.into_raw());
                        let texture = gdk::MemoryTexture::new(
                            w as i32,
                            h as i32,
                            gdk::MemoryFormat::R8g8b8a8,
                            &bytes,
                            w as usize * 4,
                        );
                        let clipboard = window.clipboard();
                        clipboard.set_texture(&texture);
                        if !automatic && view.copy_must_persist() {
                            match crate::workflow::store_clipboard(&clipboard).await {
                                Ok(()) => {
                                    success = true;
                                    durable = true;
                                }
                                Err(error) => view
                                    .error(&format!("Image copied, but exit cancelled: {error:#}")),
                            }
                        } else {
                            clipboard.store_async(
                                glib::Priority::DEFAULT,
                                None::<&gio::Cancellable>,
                                |_| {},
                            );
                            success = true;
                        }
                    }
                    if success && !automatic {
                        let text = format!(
                            "Copied {w} × {h} · Keep this window open until you paste if no clipboard manager is running."
                        );
                        view.help.set_text(&text);
                        notice = Some(text);
                    }
                }
                Ok(Ok(RenderedOutput::Pin(image))) => {
                    view.help.set_text("Pinning screenshot…");
                    match crate::pin::show(&window, image).await {
                        Ok(()) => {
                            view.help.set_text("Reference pinned · Use Hide or show pins to recover click-through controls.");
                            success = true;
                        }
                        Err(error) => view.error(&format!("Cannot pin: {error:#}")),
                    }
                }
                Ok(Ok(RenderedOutput::Saved(path, image))) => {
                    if !view.settings.disable_notifications && view.settings.notification_thumbnail
                    {
                        thumbnail = crate::workflow::thumbnail(&image).ok();
                    }
                    let text = if path.as_os_str() == "-" {
                        "PNG written to stdout".into()
                    } else {
                        format!("Saved {}", path.display())
                    };
                    view.saved_path(path);
                    view.help.set_text(&text);
                    notice = Some(text);
                    success = true;
                    durable = true;
                }
                Ok(Err(error)) => view.error(&format!("Image operation failed: {error:#}")),
                Err(_) => view.error(
                    "Image worker failed. The previous clipboard and document are unchanged.",
                ),
            }
            if success
                && !view.settings.disable_notifications
                && let Some(text) = notice
            {
                if let Some(app) = window.application() {
                    crate::workflow::notify(&app, &text, thumbnail);
                }
            }
            view.rendering.set(false);
            view.complete_action(&window, success, trigger, revision, durable);
        });
    }
    fn refresh_cursor(&self, state: &State) {
        if self.panning.get().is_some() {
            self.caption.set_pan_cursor(true);
        }
        if let Some(viewport) = self.scroll.child() {
            viewport.set_cursor_from_name(self.panning.get().map(|_| "grabbing"));
        }
        self.area
            .set_cursor_from_name(Some(if self.panning.get().is_some() {
                "grabbing"
            } else if self.ocr.enabled() {
                "text"
            } else if matches!(state.gesture, Some(Gesture::Transform { .. })) {
                "grabbing"
            } else if state.tool == Tool::Select {
                if state.hovered.is_some()
                    || self.pointer.get().is_some_and(|(x, y)| {
                        state
                            .handle_at(
                                x + self.scroll.hadjustment().value(),
                                y + self.scroll.vadjustment().value(),
                            )
                            .is_some()
                    })
                {
                    "grab"
                } else {
                    "default"
                }
            } else {
                "crosshair"
            }));
    }
    fn apply_zoom_anchor(&self, state: &State) {
        let Some(anchor) = self.zoom_anchor.get() else {
            return;
        };
        let h = self.scroll.hadjustment();
        let v = self.scroll.vadjustment();
        let width = f64::from(self.area.content_width());
        let height = f64::from(self.area.content_height());
        // Make the new pan range available before GTK lays out and paints the zoom.
        h.set_upper(width);
        v.set_upper(height);
        let scale = state.zoom.unwrap_or(state.transform.scale);
        let (x, y) = anchor.offsets(Transform {
            scale,
            x: ((width - f64::from(state.doc.width) * scale) / 2.).max(0.),
            y: ((height - f64::from(state.doc.height) * scale) / 2.).max(0.),
        });
        h.set_value(x);
        v.set_value(y);
    }
    fn pan(&self, dx: f64, dy: f64) {
        let Some((last_x, last_y)) = self.panning.get() else {
            return;
        };
        let h = self.scroll.hadjustment();
        let v = self.scroll.vadjustment();
        let before = (h.value(), v.value());
        h.set_value(before.0 + last_x - dx);
        v.set_value(before.1 + last_y - dy);
        if let Some(mut anchor) = self.zoom_anchor.get() {
            anchor.cursor.0 -= h.value() - before.0;
            anchor.cursor.1 -= v.value() - before.1;
            self.zoom_anchor.set(Some(anchor));
        }
        self.panning.set(Some((dx, dy)));
    }
    fn end_pan(&self, state: &State) {
        self.panning.set(None);
        self.caption.set_pan_cursor(false);
        self.refresh_cursor(state);
    }
    fn open_text(&self, draft: Option<interaction::TextDraft>, transform: Transform) {
        if draft.is_some() {
            self.scene.set_visible(false);
            self.area.queue_draw();
        }
        self.caption.open(draft, transform);
    }
}
enum Inference {
    Ready(Result<()>),
    Shape(u64, Result<Recognition>),
}

enum RenderAction {
    Copy,
    AutoCopy,
    Pin,
    Png(PathBuf, Trigger),
}

enum RenderedOutput {
    Copy(image::RgbaImage),
    Pin(image::RgbaImage),
    Saved(PathBuf, image::RgbaImage),
}

fn render_snapshot(snapshot: Snapshot, action: RenderAction) -> Result<RenderedOutput> {
    let image = snapshot.into_document()?.render()?;
    match action {
        RenderAction::Copy | RenderAction::AutoCopy => Ok(RenderedOutput::Copy(image)),
        RenderAction::Pin => Ok(RenderedOutput::Pin(image)),
        RenderAction::Png(path, _) => {
            crate::workflow::save(&image, &path)?;
            Ok(RenderedOutput::Saved(path, image))
        }
    }
}

fn select_context_target(state: &mut State, x: f64, y: f64) {
    state.clean_selection();
    let hit = state
        .transform
        .contains(x, y, &state.doc)
        .then(|| {
            state.doc.hit_test(
                state.transform.raw(x, y),
                (6. / state.transform.scale) as f32,
            )
        })
        .flatten();
    match hit {
        Some(id) if !state.selected.contains(&id) => {
            state.selected = state.doc.expand_groups(&[id])
        }
        None => state.selected.clear(),
        _ => {}
    }
    state.hovered = hit;
}

fn layer_moves(state: &State) -> (bool, bool) {
    let (mut selected, mut other) = ([false; 2], [false; 2]);
    let (mut front, mut back) = (false, false);
    for mark in &state.doc.marks {
        let layer = usize::from(mark.is_redaction());
        if state.selected.contains(&mark.id) {
            back |= other[layer];
            selected[layer] = true;
        } else {
            front |= selected[layer];
            other[layer] = true;
        }
    }
    (front, back)
}

fn menu_button(menu: &ContextMenu, icon: Icon, name: &str, button: &gtk::Button) {
    if button.is_sensitive() && button.is_visible() {
        let weak = button.downgrade();
        menu.action(icon, name, move || {
            if let Some(button) = weak.upgrade()
                && button.is_sensitive()
                && button.is_visible()
            {
                button.emit_clicked();
            }
        });
    }
}

fn select_all_objects(state: &mut State, view: &Rc<View>) {
    state.cancel();
    state.selected = state.doc.marks.iter().map(|m| m.id).collect();
    view.refresh(state);
}

pub fn run(doc: Document, settings: Settings) -> Result<()> {
    settings.validate()?;
    let stylesheet = settings.read_stylesheet()?;
    let style_error = Rc::new(RefCell::new(None));
    let activation_error = style_error.clone();
    let settings = Rc::new(settings);
    let app = gtk::Application::builder()
        .application_id(&settings.app_id)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let initial = RefCell::new(Some(doc));
    app.connect_activate(move |app| {
        if let Some(doc) = initial.borrow_mut().take() {
            chrome::install();
            if let Some(css) = &stylesheet {
                if let Err(error) = install_stylesheet(css) {
                    *activation_error.borrow_mut() = Some(error);
                    app.quit();
                    return;
                }
            }
            build(app, doc, settings.clone());
        }
    });
    let code = app.run_with_args::<&str>(&[]);
    if let Some(error) = style_error.borrow_mut().take() {
        return Err(error);
    }
    if code != glib::ExitCode::SUCCESS {
        anyhow::bail!("GTK exited with {code:?}");
    }
    Ok(())
}
fn install_stylesheet(css: &str) -> Result<()> {
    let display = gdk::Display::default()
        .ok_or_else(|| anyhow::anyhow!("No display for the custom stylesheet"))?;
    let provider = gtk::CssProvider::new();
    let errors = Rc::new(RefCell::new(Vec::new()));
    provider.connect_parsing_error(glib::clone!(
        #[strong]
        errors,
        move |_, _, error| errors.borrow_mut().push(error.to_string())
    ));
    provider.load_from_data(css);
    if !errors.borrow().is_empty() {
        anyhow::bail!("Custom stylesheet: {}", errors.borrow().join("; "));
    }
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 2,
    );
    Ok(())
}
fn separator(row: &gtk::Box) {
    row.append(&gtk::Separator::new(gtk::Orientation::Vertical));
}
fn zoom(state: &mut State, view: &View, value: Option<f64>, at: Option<(f64, f64)>) {
    if value == state.zoom {
        return;
    }
    let h = view.scroll.hadjustment();
    let v = view.scroll.vadjustment();
    let cursor = at.unwrap_or((h.page_size() / 2., v.page_size() / 2.));
    let t = state.transform;
    let image = if let Some(anchor) = view.zoom_anchor.get() {
        let scale = state.zoom.unwrap_or(t.scale);
        (
            anchor.image.0 + (cursor.0 - anchor.cursor.0) / scale,
            anchor.image.1 + (cursor.1 - anchor.cursor.1) / scale,
        )
    } else {
        (
            (cursor.0 + h.value() - t.x) / t.scale,
            (cursor.1 + v.value() - t.y) / t.scale,
        )
    };
    state.cancel();
    state.zoom = value;
    view.zoom_anchor
        .set(value.map(|_| ZoomAnchor { image, cursor }));
    // Leave room to keep edge pixels under the cursor even when the image fits.
    view.area.set_content_width(value.map_or(0, |z| {
        (f64::from(state.doc.width) * z).ceil() as i32 + 2 * view.scroll.width()
    }));
    view.area.set_content_height(value.map_or(0, |z| {
        (f64::from(state.doc.height) * z).ceil() as i32 + 2 * view.scroll.height()
    }));
    view.apply_zoom_anchor(state);
    view.area.queue_draw();
}

fn build(app: &gtk::Application, doc: Document, settings: Rc<Settings>) {
    let startup = std::time::Instant::now();
    let (request, requests) = mpsc::channel::<(u64, Vec<Point>)>();
    let (result, results) = async_channel::unbounded();
    std::thread::spawn(move || {
        let mut model = match Recognizer::new() {
            Ok(model) => {
                let _ = result.send_blocking(Inference::Ready(Ok(())));
                model
            }
            Err(error) => {
                let _ = result.send_blocking(Inference::Ready(Err(error)));
                return;
            }
        };
        while let Ok((id, points)) = requests.recv() {
            if result
                .send_blocking(Inference::Shape(id, model.recognize_choices(&points)))
                .is_err()
            {
                break;
            }
        }
    });
    let state = Rc::new(RefCell::new(State::new(doc, request)));
    {
        let mut s = state.borrow_mut();
        s.fill = settings.default_fill;
        s.round_caps = settings.default_round_caps;
        s.corner_radius = settings.corner_radius;
        s.width = (4. * settings.annotation_size_factor)
            .clamp(crate::document::MIN_WIDTH, crate::document::MAX_WIDTH);
        s.head_size = settings
            .annotation_size_factor
            .clamp(0., crate::document::MAX_HEAD_SIZE);
        s.text_size = (24. * settings.annotation_size_factor).clamp(1., crate::document::MAX_FONT);
        s.brush_smoothing = settings.brush_smoothing;
        s.font_family = std::iter::once(settings.font_family.as_str())
            .chain(settings.font_fallback.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(", ");
        s.font_style.clone_from(&settings.font_style);
        s.text_style = crate::document::font_style_defaults(&settings.font_style);
        if let Some(color) = settings
            .palette
            .first()
            .and_then(|c| crate::settings::parse_color(c).ok())
        {
            s.color = color;
        }
    }
    let view_slot = Rc::new(RefCell::new(std::rc::Weak::<View>::new()));
    let refresh: Rc<dyn Fn()> = {
        let slot = view_slot.clone();
        let state = state.clone();
        Rc::new(move || {
            if let Some(view) = slot.borrow().upgrade() {
                view.refresh(&state.borrow());
            }
        })
    };
    let inspector = crate::inspector::Inspector::new(state.clone(), refresh.clone());
    inspector.widget.set_visible(false);
    inspector.widget.set_hexpand(false);
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(&settings.title)
        .default_width(settings.width.unwrap_or(1280))
        .default_height(settings.height.unwrap_or(850))
        .build();
    window.add_css_class("sharpshot");
    window.set_size_request(760, 440);
    if settings.floating {
        window.set_resizable(false);
    }
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = gtk::HeaderBar::new();
    let undo = chrome::button(Icon::Undo, "Undo", "Undo · Ctrl+Z");
    let redo = chrome::button(Icon::Redo, "Redo", "Redo · Ctrl+Shift+Z");
    let delete = chrome::button(
        Icon::Delete,
        "Delete selected object",
        "Delete selection · Delete / Backspace",
    );
    let history = gtk::Box::new(gtk::Orientation::Horizontal, 3);
    history.append(&undo);
    history.append(&redo);
    separator(&history);
    history.append(&delete);
    let inspector_button = gtk::ToggleButton::builder()
        .child(&chrome::icon(Icon::Inspector))
        .tooltip_text("Object inspector · Ctrl+I")
        .build();
    inspector_button.add_css_class("tool-button");
    inspector_button.update_property(&[gtk::accessible::Property::Label("Object inspector")]);
    history.append(&inspector_button);
    let pin_button = chrome::button(
        Icon::Pin,
        "Pin screenshot",
        "Pin this rendered screenshot above other windows",
    );
    history.append(&pin_button);
    let palette_button = chrome::button(
        Icon::Search,
        "Command palette",
        "Search tools and actions · Ctrl+K",
    );
    history.append(&palette_button);
    header.pack_start(&history);
    let tools = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let mut tool_buttons: Vec<(Tool, gtk::ToggleButton)> = vec![];
    for &(tool, icon, name, tip) in TOOLS {
        if tool == Tool::Smart || tool == Tool::Crop {
            separator(&tools);
        }
        let (shortcut, hint) = tip.split_once('·').unwrap_or(("", tip));
        let shortcut = actions::tool_shortcut(&settings, tool, shortcut.trim());
        let button = gtk::ToggleButton::builder()
            .child(&chrome::icon(icon))
            .tooltip_text(format!("{name} · {shortcut} · {}", hint.trim()))
            .build();
        button.add_css_class("tool-button");
        button.update_property(&[gtk::accessible::Property::Label(name)]);
        if let Some((_, first)) = tool_buttons.first() {
            button.set_group(Some(first));
        }
        button.set_active(tool == Tool::Smart);
        button.set_sensitive(tool != Tool::Smart);
        tools.append(&button);
        tool_buttons.push((tool, button));
    }
    let tool_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .min_content_width(140)
        .min_content_height(44)
        .propagate_natural_width(true)
        .child(&tools)
        .build();
    let tool_wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let tool_adjustment = tool_scroll.hadjustment();
    // External policy hides the bar; map either wheel axis onto the tool strip.
    tool_wheel.connect_scroll(move |controller, dx, dy| {
        let delta = if dx.abs() > dy.abs() { dx } else { dy };
        let step = if controller.unit() == gdk::ScrollUnit::Surface {
            1.
        } else {
            tool_adjustment.step_increment()
        };
        tool_adjustment.set_value(tool_adjustment.value() + delta * step);
        glib::Propagation::Stop
    });
    tools.add_controller(tool_wheel);
    header.set_title_widget(Some(&tool_scroll));
    let copy = chrome::button(
        Icon::Copy,
        "Copy image",
        "Copy rendered image · Ctrl+C outside OCR mode",
    );
    let export = chrome::button(Icon::Save, "Export PNG", "Export PNG · Ctrl+S");
    let designer = chrome::button(
        Icon::Designer,
        "Screenshot designer",
        "Designer · Background, padding, border, corners and shadow",
    );
    let resize = chrome::button(
        Icon::Resize,
        "Crop and resize",
        "Crop and resize · Set crop coordinates and output dimensions",
    );
    export.add_css_class("export-button");
    let help_button = gtk::MenuButton::builder()
        .child(&chrome::icon(Icon::Help))
        .tooltip_text("Tools and keyboard shortcuts")
        .build();
    help_button.add_css_class("tool-button");
    help_button.update_property(&[gtk::accessible::Property::Label("Keyboard shortcuts")]);
    let popover = gtk::Popover::new();
    let legend = gtk::Box::new(gtk::Orientation::Vertical, 8);
    legend.add_css_class("help-popover");
    for text in [
        "V  Select / move       S  Smart shape",
        "X  Eraser: click / hold left button and drag; removes whole annotations, including privacy masks",
        "Eraser: Ctrl+Z restores one gesture; Esc stops and keeps removals",
        "P  Freehand    A  Arrow    L  Line    R  Rectangle",
        "E  Ellipse      T  Text      C  Crop",
        "B  Privacy zone    N  Numbered step    M  Magnifier    H  Spotlight",
        "G  Primary highlighter    Shift+G  Other highlighter    Ctrl-drag  Swap style",
        "F  Toggle fill    K  Round line ends    Alt+1…0  Palette / custom color",
        "Alt-drag shapes  Draw from center    Shift  Square / circle",
        "Shift while highlighting  Straight 15° segments    Alt on a step  Extra ring",
        "Shift-click / marquee  Multi-select    Ctrl+A  Select all",
        "Handles  Resize / rotate    Shift  Proportions / 15° rotation",
        "Alt while dragging  Disable magnetic guides",
        "Ctrl+G  Group    Ctrl+Shift+G  Ungroup    Ctrl+D  Duplicate",
        "Ctrl+Home  Bring to front    Ctrl+End  Send to back",
        "Text: Enter  Apply    Shift+Enter  New line    Esc  Cancel",
        "Text: Alt  Cycle effect    Ctrl+Alt+arrows  Move caption    Shift  10×",
        "Ctrl+O  Open project    Ctrl+Shift+S  Save editable project",
        "Arrows  Nudge 1 px    Shift+arrows 10 px",
        "Delete  Remove selection    Esc  Cancel / deselect",
        "Ctrl+Z  Undo    Ctrl+Shift+Z  Redo",
        "0  Fit image    1  Actual pixels    Wheel  Zoom at cursor",
        "Middle-button drag  Pan the canvas",
        "Touchpad  Pan    Ctrl+touchpad / pinch  Zoom    Alt+arrows  Pan",
        "Right-click / Menu / Shift+F10  Context actions",
        "F8  OCR text layer    Ctrl+C  Copy image / recognised text    Ctrl+S  Export PNG",
        "Ctrl+K  Command palette    Ctrl+I  Object inspector",
        "Enter  Copy image    Ctrl+Alt+S  Save PNG as…    Ctrl+Alt+C  Copy saved path",
        "Ctrl+T  Toggle toolbars    F11  Fullscreen    Ctrl+Q  Close screenshot",
        "Ctrl+Shift+Delete  Clear all annotations (undoable)",
        "While editing text: Ctrl+B Bold    Ctrl+I Italic",
    ] {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.);
        legend.append(&label);
    }
    if !settings.keybinds.is_empty() {
        legend.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let note = gtk::Label::new(Some("Configured shortcuts replace the defaults above:"));
        note.set_xalign(0.);
        legend.append(&note);
        for (command, accelerator) in &settings.keybinds {
            let label = gtk::Label::new(Some(&format!("{accelerator}  ·  {command}")));
            label.set_xalign(0.);
            legend.append(&label);
        }
    }
    let shortcuts_scroll = gtk::ScrolledWindow::builder()
        .child(&legend)
        .max_content_height(600)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .build();
    popover.set_child(Some(&shortcuts_scroll));
    help_button.set_popover(Some(&popover));
    let project_button = gtk::MenuButton::builder()
        .child(&chrome::icon(Icon::Project))
        .tooltip_text("Projects and actions · Presets, recovery, smart redaction, scrolling capture and vector export")
        .build();
    project_button.add_css_class("tool-button");
    project_button.update_property(&[gtk::accessible::Property::Label("Projects and actions")]);
    let project_popover = gtk::Popover::new();
    let project_actions = gtk::Box::new(gtk::Orientation::Vertical, 8);
    project_actions.add_css_class("help-popover");
    let open_project =
        chrome::menu_button(Icon::Open, "Open project", "Open editable project · Ctrl+O");
    let save_project = chrome::menu_button(
        Icon::Save,
        "Save project",
        "Save editable project · Ctrl+Shift+S · Contains original pixels",
    );
    project_actions.append(&open_project);
    project_actions.append(&save_project);
    let more = |icon, name, tip| {
        let button = chrome::menu_button(icon, name, tip);
        project_actions.append(&button);
        button
    };
    let style_presets = more(
        Icon::Designer,
        "Saved styles",
        "Save and reuse annotation styles and designer frames",
    );
    let recovery_button = more(
        Icon::Recovery,
        "Autosave and recovery",
        "Private local recovery, retention, restore and deletion controls",
    );
    let smart_redact = more(
        Icon::Redact,
        "Smart redaction",
        "Review sensitive text matches before applying opaque masks",
    );
    let table_copy = more(
        Icon::Table,
        "Copy OCR table",
        "Preview recognised text as a table and copy TSV, CSV or Markdown",
    );
    let toggle_pins = more(
        Icon::Pin,
        "Hide or show pins",
        "Hide pinned references, or show them with click-through cleared",
    );
    let scroll_capture = more(
        Icon::Scroll,
        "Scrolling capture",
        "Capture a long page with guided scrolling and seam review",
    );
    let record_capture = more(
        Icon::Record,
        "Record screen region",
        "Record a short clip, preview, trim and export GIF or WebM",
    );
    let export_svg = more(
        Icon::Save,
        "Export SVG",
        "Privacy-safe vector export with sanitised image assets",
    );
    let export_pdf = more(
        Icon::Save,
        "Export PDF",
        "Privacy-safe single-page PDF with sanitised image assets",
    );
    let save_as = more(
        Icon::Save,
        "Save PNG as…",
        "Always choose a PNG filename · Ctrl+Alt+S",
    );
    let copy_path = more(
        Icon::Copy,
        "Copy saved file path",
        "Copy the last saved PNG path · Ctrl+Alt+C",
    );
    let toolbar_toggle = more(
        Icon::Toolbars,
        "Show or hide toolbars",
        "Toggle all toolbars · Ctrl+T",
    );
    let fullscreen_toggle = more(
        Icon::Fullscreen,
        "Toggle fullscreen",
        "Enter or leave fullscreen · F11",
    );
    let clear_all = more(
        Icon::Delete,
        "Clear annotations",
        "Remove all annotations, including privacy masks · Undoable · Ctrl+Shift+Delete",
    );
    let quit = more(
        Icon::Close,
        "Close screenshot",
        "Close this editor · Ctrl+Q · Unsaved work is confirmed",
    );
    let project_warning = gtk::Label::new(Some(
        "Projects contain the ORIGINAL screenshot,\nincluding redacted pixels. Share PNG instead.",
    ));
    project_warning.set_xalign(0.);
    project_actions.append(&project_warning);
    project_popover.set_child(Some(&project_actions));
    project_button.set_popover(Some(&project_popover));
    header.pack_end(&project_button);
    header.pack_end(&help_button);
    header.pack_end(&export);
    header.pack_end(&copy);
    header.pack_end(&designer);
    header.pack_end(&resize);
    if settings.no_window_decoration {
        window.set_decorated(false);
        header.set_show_title_buttons(false);
        root.prepend(&header);
    } else {
        window.set_titlebar(Some(&header));
    }
    let area = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .focusable(true)
        .accessible_role(gtk::AccessibleRole::Img)
        .build();
    area.add_css_class("canvas");
    area.add_css_class("annotation-overlay");
    let scene = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .can_target(false)
        .build();
    scene.add_css_class("canvas");
    scene.add_css_class("document-scene");
    area.update_property(&[gtk::accessible::Property::Label(
        "Vector annotation canvas; V selects objects, arrows move the selection, middle-button drag pans",
    )]);
    let canvas_overlay = gtk::Overlay::new();
    canvas_overlay.set_child(Some(&scene));
    canvas_overlay.add_overlay(&area);
    canvas_overlay.set_measure_overlay(&area, true);
    let viewport = gtk::Viewport::builder()
        .scroll_to_focus(false)
        .child(&canvas_overlay)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .kinetic_scrolling(false)
        .child(&viewport)
        .build();
    // GTK's wheel inertia is separate from the touch kinetic-scrolling setting.
    for controller in scroll
        .observe_controllers()
        .iter::<glib::Object>()
        .flatten()
    {
        if let Ok(controller) = controller.downcast::<gtk::EventControllerScroll>() {
            controller.set_flags(controller.flags() & !gtk::EventControllerScrollFlags::KINETIC);
        }
    }
    scroll.add_css_class("canvas");
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.set_vexpand(true);
    body.append(&scroll);
    body.append(&inspector.widget);
    root.append(&body);

    let context = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    context.add_css_class("context-bar");
    let selection_label = gtk::Label::new(None);
    context.append(&selection_label);
    let group = chrome::button(Icon::Group, "Group", "Group selection · Ctrl+G");
    let ungroup = chrome::button(Icon::Ungroup, "Ungroup", "Ungroup selection · Ctrl+Shift+G");
    let duplicate = chrome::button(Icon::Duplicate, "Duplicate", "Duplicate selection · Ctrl+D");
    let edit_text = chrome::button(Icon::Text, "Edit text", "Edit caption · Double-click text");
    let earlier = chrome::button(
        Icon::Earlier,
        "Earlier step",
        "Move step earlier in numbering",
    );
    let later = chrome::button(Icon::Later, "Later step", "Move step later in numbering");
    let front = chrome::button(
        Icon::Front,
        "Bring to front",
        "Bring to front · Ctrl+Home · Privacy masks stay above annotations",
    );
    let back = chrome::button(
        Icon::Back,
        "Send to back",
        "Send to back · Ctrl+End · Privacy masks stay above annotations",
    );
    for button in [
        &group, &ungroup, &duplicate, &front, &back, &edit_text, &earlier, &later,
    ] {
        context.append(button);
    }
    let privacy_mode = gtk::DropDown::from_strings(&["Black redaction", "Pixelated privacy"]);
    privacy_mode.set_tooltip_text(Some(
        "Pixelated privacy replaces pixels with a source-independent mosaic, not averages of sensitive content. Projects retain originals.",
    ));
    privacy_mode.update_property(&[gtk::accessible::Property::Label("Privacy zone appearance")]);
    context.append(&privacy_mode);
    let font_size = gtk::SpinButton::with_range(6., 512., 1.);
    font_size.set_tooltip_text(Some(
        "Text size in image pixels · Resizing text keeps its proportions",
    ));
    font_size.update_property(&[gtk::accessible::Property::Label("Text size in pixels")]);
    context.append(&font_size);
    let head_size = gtk::SpinButton::with_range(0., crate::document::MAX_HEAD_SIZE, 0.25);
    head_size.set_digits(2);
    head_size.set_tooltip_text(Some(
        "Arrow tip scale · 1× = 12 image pixels · Independent of line thickness · 0 makes a plain line",
    ));
    head_size.update_property(&[gtk::accessible::Property::Label("Arrow tip size")]);
    context.append(&head_size);
    let lens_zoom = gtk::SpinButton::with_range(1., 8., 0.5);
    lens_zoom.set_digits(1);
    lens_zoom.set_tooltip_text(Some("Lens zoom factor"));
    lens_zoom.update_property(&[gtk::accessible::Property::Label("Lens zoom")]);
    context.append(&lens_zoom);
    let mut choices = vec![];
    for _ in 0..4 {
        let button = gtk::Button::new();
        context.append(&button);
        choices.push(button);
    }
    let keep_ink = chrome::button(
        Icon::Pen,
        "Keep original stroke",
        "Keep original hand-drawn stroke · Undoable",
    );
    context.append(&keep_ink);
    let context_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&context)
        .build();
    root.append(&context_scroll);
    let caption =
        crate::caption::Caption::new(&canvas_overlay, &area, state.clone(), refresh.clone());
    context.append(&caption.toolbar);
    let styles = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    styles.add_css_class("style-bar");
    styles.set_halign(gtk::Align::Center);
    let mut swatches = vec![];
    for (index, name) in settings
        .palette
        .iter()
        .chain(&settings.custom_colors)
        .enumerate()
    {
        let rgb = crate::settings::parse_color(name).expect("validated annotation palette");
        let paint = gtk::DrawingArea::builder()
            .content_width(18)
            .content_height(18)
            .can_target(false)
            .build();
        paint.set_draw_func(move |_, cr, _, _| {
            cr.set_source_rgba(rgb[0], rgb[1], rgb[2], rgb[3]);
            cr.rectangle(1., 1., 16., 16.);
            let _ = cr.fill();
        });
        let button = gtk::ToggleButton::builder()
            .child(&paint)
            .tooltip_text(if index < 10 {
                let command = format!("color-{}", index + 1);
                let shortcut = settings
                    .keybinds
                    .get(&command)
                    .cloned()
                    .unwrap_or_else(|| format!("Alt+{}", (index + 1) % 10));
                format!("{name} annotation color · {shortcut}")
            } else {
                format!("{name} annotation color")
            })
            .build();
        button.add_css_class("tool-button");
        button.add_css_class("swatch");
        button.update_property(&[gtk::accessible::Property::Label(&format!(
            "{name} annotation color"
        ))]);
        styles.append(&button);
        swatches.push((button, rgb, name.len() == 9));
    }
    let color = gtk::ColorButton::new();
    color.set_title("Annotation color");
    color.set_use_alpha(true);
    color.set_tooltip_text(Some("Custom color and opacity"));
    color.update_property(&[gtk::accessible::Property::Label("Custom annotation color")]);
    styles.append(&color);
    let fill = gtk::ToggleButton::builder()
        .child(&chrome::icon(Icon::Fill))
        .tooltip_text("Fill shapes · F · Transparent shapes remain click-through")
        .css_classes(["tool-button"])
        .build();
    fill.update_property(&[gtk::accessible::Property::Label("Fill shapes")]);
    styles.append(&fill);
    let round_caps = gtk::ToggleButton::builder()
        .child(&chrome::icon(Icon::RoundCaps))
        .tooltip_text("Round line ends · K")
        .css_classes(["tool-button"])
        .build();
    round_caps.update_property(&[gtk::accessible::Property::Label("Round line ends")]);
    styles.append(&round_caps);
    let corner_radius = gtk::SpinButton::with_range(0., crate::document::MAX_RADIUS, 1.);
    corner_radius.set_width_chars(3);
    corner_radius.set_tooltip_text(Some("Rectangle corner radius in image pixels"));
    corner_radius.update_property(&[gtk::accessible::Property::Label("Rectangle corner radius")]);
    styles.append(&corner_radius);
    separator(&styles);
    let mut presets = vec![];
    for (name, value) in [("S", 2.), ("M", 4.), ("L", 8.)] {
        let button = gtk::ToggleButton::with_label(name);
        button.add_css_class("tool-button");
        button.add_css_class("size-preset");
        button.set_tooltip_text(Some(&format!("Stroke width {value} px")));
        button.update_property(&[gtk::accessible::Property::Label(&format!(
            "Stroke width {value} pixels"
        ))]);
        styles.append(&button);
        presets.push((button, value));
    }
    let width =
        gtk::SpinButton::with_range(crate::document::MIN_WIDTH, crate::document::MAX_WIDTH, 0.1);
    width.set_digits(2);
    width.set_width_chars(4);
    width.set_tooltip_text(Some(
        "Line thickness in image pixels · Wheel or arrows: 0.1 px, Shift: 0.01 px, Ctrl: 1 px · Text has its own font size",
    ));
    width.update_property(&[gtk::accessible::Property::Label("Stroke width in pixels")]);
    styles.append(&width);
    separator(&styles);
    let opacity_label = gtk::Label::new(Some("Opacity"));
    opacity_label.add_css_class("field-label");
    styles.append(&opacity_label);
    let opacity = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0., 100., 1.);
    opacity.set_draw_value(false);
    opacity.set_width_request(90);
    opacity.set_tooltip_text(Some("Annotation opacity"));
    opacity.update_property(&[gtk::accessible::Property::Label(
        "Annotation opacity percent",
    )]);
    styles.append(&opacity);
    separator(&styles);
    let minus = chrome::button(Icon::Minus, "Zoom out", "Zoom out · Ctrl+minus");
    let plus = chrome::button(Icon::Plus, "Zoom in", "Zoom in · Ctrl+plus");
    let fit = chrome::button(Icon::Fit, "Fit image", "Fit image · 0; actual pixels · 1");
    let zoom_label = gtk::Label::new(Some("Fit"));
    zoom_label.set_width_chars(9);
    styles.append(&minus);
    styles.append(&zoom_label);
    styles.append(&plus);
    styles.append(&fit);
    separator(&styles);
    let dimensions = gtk::Label::new(None);
    dimensions.set_width_chars(12);
    dimensions.add_css_class("dimension-label");
    styles.append(&dimensions);
    let reset_crop = chrome::button(
        Icon::ResetCrop,
        "Reset crop",
        "Restore the full screenshot; undoable",
    );
    styles.append(&reset_crop);
    let style_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .min_content_height(50)
        .child(&styles)
        .build();
    root.append(&style_scroll);
    let status = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    status.add_css_class("status-bar");
    let help = gtk::Label::new(Some("Loading smart shapes…"));
    help.set_hexpand(true);
    help.set_xalign(0.);
    help.set_ellipsize(gtk::pango::EllipsizeMode::End);
    status.append(&help);
    let credit = gtk::Label::new(None);
    credit.set_markup("<a href=\"https://desertant.com\">Powered by Desert Ant Labs</a>");
    status.append(&credit);
    root.append(&status);
    let ocr = crate::ocr_ui::OcrUi::new(&area, state.clone(), &help, refresh.clone());
    tools.append(&ocr.toggle);
    context.append(&ocr.toolbar);
    window.set_child(Some(&root));
    let menu = ContextMenu::new(&area);
    let view = Rc::new(View {
        state: state.clone(),
        settings: settings.clone(),
        window: window.downgrade(),
        commands: RefCell::new(HashMap::new()),
        tools: tool_buttons.clone(),
        bindings: settings
            .keybinds
            .iter()
            .filter_map(|(name, value)| {
                crate::settings::parse_accelerator(value)
                    .map(|(key, mods)| (key, mods, name.clone()))
            })
            .collect(),
        toolbars: vec![
            header.clone().upcast(),
            context_scroll.clone().upcast(),
            style_scroll.clone().upcast(),
            status.clone().upcast(),
        ],
        chrome_visible: Cell::new(true),
        actions: RefCell::new(VecDeque::new()),
        last_saved: RefCell::new(None),
        auto_revision: Cell::new(state.borrow().doc.revision()),
        auto_source: RefCell::new(None),
        auto_pending: Cell::new(false),
        exported_revision: Cell::new(None),
        fill,
        round_caps,
        corner_radius,
        area,
        menu,
        scene,
        scene_revision: Cell::new(None),
        scene_cache: RefCell::new(None),
        scroll,
        zoom_anchor: Cell::new(None),
        panning: Cell::new(None),
        rendering: Cell::new(false),
        undo,
        redo,
        delete,
        reset_crop,
        color,
        width,
        opacity,
        swatches,
        presets,
        dimensions,
        zoom_label,
        help,
        context,
        selection_label,
        group,
        ungroup,
        duplicate,
        edit_text,
        earlier,
        later,
        front,
        back,
        privacy_mode,
        lens_zoom,
        choices,
        keep_ink,
        caption,
        inspector,
        font_size,
        head_size,
        ocr,
        syncing: Cell::new(false),
        pointer: Cell::new(None),
    });
    *view_slot.borrow_mut() = Rc::downgrade(&view);
    view.refresh(&state.borrow());
    {
        let mut commands = view.commands.borrow_mut();
        for (name, button) in [
            ("copy", &copy),
            ("save", &export),
            ("save-as", &save_as),
            ("copy-path", &copy_path),
            ("undo", &view.undo),
            ("redo", &view.redo),
            ("toolbar", &toolbar_toggle),
            ("fullscreen", &fullscreen_toggle),
            ("fit", &fit),
            ("open-project", &open_project),
            ("save-project", &save_project),
            ("zoom-in", &plus),
            ("zoom-out", &minus),
            ("delete", &view.delete),
            ("group", &view.group),
            ("ungroup", &view.ungroup),
            ("duplicate", &view.duplicate),
            ("front", &view.front),
            ("back", &view.back),
            ("clear", &clear_all),
            ("quit", &quit),
            ("palette", &palette_button),
        ] {
            commands.insert(name, button.clone());
        }
        commands.insert("fill", view.fill.clone().upcast());
        commands.insert("round-caps", view.round_caps.clone().upcast());
        commands.insert("inspector", inspector_button.clone().upcast());
        commands.insert("ocr", view.ocr.toggle.clone().upcast());
    }
    for (command, button) in view.commands.borrow().iter() {
        if let Some(shortcut) = settings.keybinds.get(*command) {
            let tip = button.tooltip_text();
            let name = tip
                .as_deref()
                .and_then(|tip| tip.split(" · ").next())
                .unwrap_or(command);
            button.set_tooltip_text(Some(&format!("{name} · {shortcut}")));
        }
    }
    save_as.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| view.save(true)
    ));
    copy_path.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| {
            view.copy_path();
        }
    ));
    toolbar_toggle.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| view.set_toolbars(!view.chrome_visible.get())
    ));
    fullscreen_toggle.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| {
            if window.is_fullscreen() {
                window.unfullscreen();
            } else {
                window.fullscreen();
            }
        }
    ));
    quit.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| window.close()
    ));
    clear_all.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut s = state.borrow_mut();
            if s.text_draft.is_some() {
                view.error("Apply or cancel the caption before clearing annotations.");
                return;
            }
            s.cancel();
            let ids: Vec<_> = s.doc.marks.iter().map(|m| m.id).collect();
            s.doc.remove_many(&ids);
            s.selected.clear();
            view.refresh(&s);
        }
    ));
    view.fill.connect_toggled(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |button| {
            if view.syncing.get() {
                return;
            }
            let mut s = state.borrow_mut();
            s.set_fill(button.is_active());
            view.refresh(&s);
        }
    ));
    view.round_caps.connect_toggled(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |button| {
            if view.syncing.get() {
                return;
            }
            let mut s = state.borrow_mut();
            s.set_round_caps(button.is_active());
            view.refresh(&s);
        }
    ));
    view.corner_radius.connect_value_changed(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |spin| {
            if view.syncing.get() {
                return;
            }
            let mut s = state.borrow_mut();
            s.set_corner_radius(spin.value());
            view.refresh(&s);
        }
    ));
    view.width.adjustment().set_page_increment(1.);
    let width_scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    width_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    width_scroll.connect_scroll(glib::clone!(
        #[weak]
        view,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |controller, _, dy| {
            let modifiers = controller.current_event_state();
            let step = if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                1.
            } else if modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
                0.01
            } else {
                0.1
            };
            view.width.set_value(view.width.value() - dy * step);
            glib::Propagation::Stop
        }
    ));
    view.width.add_controller(width_scroll);
    let width_keys = gtk::EventControllerKey::new();
    width_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    width_keys.connect_key_pressed(glib::clone!(
        #[weak]
        view,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, modifiers| {
            let direction = match key {
                gdk::Key::Up | gdk::Key::Page_Up => 1.,
                gdk::Key::Down | gdk::Key::Page_Down => -1.,
                _ => return glib::Propagation::Proceed,
            };
            let step = if modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                || matches!(key, gdk::Key::Page_Up | gdk::Key::Page_Down)
            {
                1.
            } else if modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
                0.01
            } else {
                0.1
            };
            view.width.set_value(view.width.value() + direction * step);
            glib::Propagation::Stop
        }
    ));
    view.width.add_controller(width_keys);
    inspector_button.connect_toggled(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |button| {
            if button.is_active() {
                view.inspector.refresh(&state.borrow());
            }
            view.inspector.widget.set_visible(button.is_active());
        }
    ));
    style_presets.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[strong]
        refresh,
        #[weak]
        window,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            crate::presets::open(&window, state.clone(), refresh.clone());
        }
    ));
    let recovery = crate::recovery::RecoveryUi::attach(&window, state.clone(), refresh.clone());
    recovery.set_on_restore(Rc::new(glib::clone!(
        #[weak]
        app,
        #[strong]
        settings,
        move |doc| build(&app, doc, settings.clone())
    )));
    recovery_button.connect_clicked(glib::clone!(
        #[strong]
        recovery,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            recovery.open();
        }
    ));
    recovery.set_on_change(Rc::new(glib::clone!(
        #[weak]
        recovery_button,
        #[weak]
        recovery,
        move || {
            recovery_button.set_tooltip_text(Some(&format!(
                "{} · {} interrupted sessions available",
                recovery.status(),
                recovery.available()
            )));
        }
    )));
    smart_redact.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[weak]
        window,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            view.ocr.smart_redact(&window);
        }
    ));
    pin_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        #[weak]
        window,
        move |_| view.render_image(&state.borrow(), &window, RenderAction::Pin)
    ));
    toggle_pins.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            if let Err(error) = crate::pin::toggle_all() {
                view.help
                    .set_text(&format!("Cannot toggle pins: {error:#}"));
            }
        }
    ));
    scroll_capture.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        view,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            match std::env::current_exe()
                .and_then(|exe| std::process::Command::new(exe).arg("--scroll").spawn())
            {
                Ok(_) => window.minimize(),
                Err(error) => view
                    .help
                    .set_text(&format!("Cannot start scrolling capture: {error}")),
            }
        }
    ));
    record_capture.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        view,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            match std::env::current_exe()
                .and_then(|exe| std::process::Command::new(exe).arg("--record").spawn())
            {
                Ok(_) => window.minimize(),
                Err(error) => view
                    .help
                    .set_text(&format!("Cannot start recording: {error}")),
            }
        }
    ));
    table_copy.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        view,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            view.ocr.table_preview(&window);
        }
    ));
    for (button, format) in [
        (&export_svg, crate::vector_export::Format::Svg),
        (&export_pdf, crate::vector_export::Format::Pdf),
    ] {
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[weak]
            window,
            #[weak]
            view,
            #[weak]
            project_popover,
            move |_| {
                project_popover.popdown();
                export_vector(&window, &state, &view, format);
            }
        ));
    }
    let mut commands = Vec::new();
    let shortcut_for = |command: &str, fallback: &str| {
        settings
            .keybinds
            .get(command)
            .cloned()
            .unwrap_or_else(|| fallback.into())
    };
    for ((tool, _, name, tip), (_, button)) in TOOLS.iter().zip(&tool_buttons) {
        commands.push(crate::palette::Command {
            name: (*name).into(),
            shortcut: actions::tool_shortcut(
                &settings,
                *tool,
                tip.split('·').next().unwrap_or("").trim(),
            )
            .into(),
            description: button
                .tooltip_text()
                .map_or(String::new(), |tip| tip.to_string()),
            widget: button.clone().upcast(),
        });
    }
    for (name, command, shortcut, button) in [
        ("Undo", "undo", "Ctrl+Z", &view.undo),
        ("Redo", "redo", "Ctrl+Shift+Z", &view.redo),
        ("Delete selection", "delete", "Delete", &view.delete),
        ("Group", "group", "Ctrl+G", &view.group),
        ("Ungroup", "ungroup", "Ctrl+Shift+G", &view.ungroup),
        ("Duplicate", "duplicate", "Ctrl+D", &view.duplicate),
        ("Bring to front", "front", "Ctrl+Home", &view.front),
        ("Send to back", "back", "Ctrl+End", &view.back),
        ("Edit text", "", "Double-click", &view.edit_text),
        ("Earlier step", "", "", &view.earlier),
        ("Later step", "", "", &view.later),
        ("Copy image", "copy", "Ctrl+C", &copy),
        ("Export PNG", "save", "Ctrl+S", &export),
        ("Save PNG as…", "save-as", "Ctrl+Alt+S", &save_as),
        (
            "Copy saved file path",
            "copy-path",
            "Ctrl+Alt+C",
            &copy_path,
        ),
        (
            "Show or hide toolbars",
            "toolbar",
            "Ctrl+T",
            &toolbar_toggle,
        ),
        ("Toggle fullscreen", "fullscreen", "F11", &fullscreen_toggle),
        (
            "Clear annotations",
            "clear",
            "Ctrl+Shift+Delete",
            &clear_all,
        ),
        ("Close screenshot", "quit", "Ctrl+Q", &quit),
        ("Export SVG", "", "", &export_svg),
        ("Export PDF", "", "", &export_pdf),
        ("Screenshot designer", "", "", &designer),
        ("Crop and resize", "", "", &resize),
        ("Reset crop", "", "", &view.reset_crop),
        ("Open project", "open-project", "Ctrl+O", &open_project),
        (
            "Save project",
            "save-project",
            "Ctrl+Shift+S",
            &save_project,
        ),
        ("Saved styles", "", "", &style_presets),
        ("Autosave and recovery", "", "", &recovery_button),
        ("Smart redaction", "", "", &smart_redact),
        ("Copy OCR table", "", "", &table_copy),
        ("Pin screenshot", "", "", &pin_button),
        ("Hide or show pins", "", "", &toggle_pins),
        ("Scrolling capture", "", "", &scroll_capture),
        ("Record screen region", "", "", &record_capture),
        ("Fit image", "fit", "0", &fit),
        ("Zoom in", "zoom-in", "Ctrl+plus", &plus),
        ("Zoom out", "zoom-out", "Ctrl+minus", &minus),
    ] {
        commands.push(crate::palette::Command {
            name: name.into(),
            shortcut: shortcut_for(command, shortcut),
            description: button
                .tooltip_text()
                .map_or(String::new(), |s| s.to_string()),
            widget: button.clone().upcast(),
        });
    }
    commands.push(crate::palette::Command {
        name: "Object inspector".into(),
        shortcut: shortcut_for("inspector", "Ctrl+I"),
        description: "Precise geometry, appearance, alignment and distribution".into(),
        widget: inspector_button.clone().upcast(),
    });
    commands.push(crate::palette::Command {
        name: "Recognise text".into(),
        shortcut: shortcut_for("ocr", "F8"),
        description: "Selectable OCR text layer and markup".into(),
        widget: view.ocr.toggle.clone().upcast(),
    });
    for (name, command, shortcut, button) in [
        ("Fill shapes", "fill", "F", &view.fill),
        ("Round line ends", "round-caps", "K", &view.round_caps),
    ] {
        commands.push(crate::palette::Command {
            name: name.into(),
            shortcut: shortcut_for(command, shortcut),
            description: button
                .tooltip_text()
                .map_or(String::new(), |s| s.to_string()),
            widget: button.clone().upcast(),
        });
    }
    let palette = crate::palette::Palette::new(&window, commands);
    palette_button.connect_clicked(glib::clone!(
        #[strong]
        palette,
        move |_| palette.open()
    ));
    let open_context: Rc<dyn Fn(Option<(f64, f64)>)> = Rc::new(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        #[weak]
        window,
        #[weak]
        copy,
        #[weak]
        export,
        #[weak]
        pin_button,
        #[weak]
        designer,
        #[weak]
        fit,
        #[weak]
        inspector_button,
        #[strong]
        tool_buttons,
        move |point: Option<(f64, f64)>| {
            if state.borrow().gesture.is_some() || view.panning.get().is_some() {
                return;
            }
            let h = view.scroll.hadjustment();
            let v = view.scroll.vadjustment();
            let at = point.unwrap_or_else(|| {
                let s = state.borrow();
                s.doc.selection_bounds(&s.selected).map_or(
                    (
                        h.value() + h.page_size() / 2.,
                        v.value() + v.page_size() / 2.,
                    ),
                    |b| {
                        (
                            s.transform.x + f64::from(b.x + b.width / 2.) * s.transform.scale,
                            s.transform.y + f64::from(b.y + b.height / 2.) * s.transform.scale,
                        )
                    },
                )
            });
            let menu = &view.menu;
            menu.clear();
            if state.borrow().text_draft.is_some() {
                menu_button(menu, Icon::Check, "Apply text", &view.caption.apply);
                menu_button(
                    menu,
                    Icon::Close,
                    "Cancel text editing",
                    &view.caption.cancel,
                );
            } else if view.ocr.enabled() {
                use crate::ocr_ui::Markup;
                let text = view.ocr.prepare_context(point);
                if text.has_text {
                    menu.action(
                        Icon::Copy,
                        if text.selected {
                            "Copy selected text"
                        } else {
                            "Copy all recognised text"
                        },
                        glib::clone!(
                            #[weak]
                            view,
                            move || view.ocr.copy_text()
                        ),
                    );
                    menu.action(
                        Icon::Table,
                        "Copy as table…",
                        glib::clone!(
                            #[weak]
                            view,
                            #[weak]
                            window,
                            move || view.ocr.table_preview(&window)
                        ),
                    );
                    if text.selected {
                        menu.action(
                            Icon::Text,
                            "Select whole line",
                            glib::clone!(
                                #[weak]
                                view,
                                move || view.ocr.select_line()
                            ),
                        );
                    }
                    menu.action(
                        Icon::Select,
                        "Select all recognised text",
                        glib::clone!(
                            #[weak]
                            view,
                            move || view.ocr.select_all()
                        ),
                    );
                    if text.selected {
                        menu.action(
                            Icon::Close,
                            "Clear text selection",
                            glib::clone!(
                                #[weak]
                                view,
                                move || view.ocr.clear_selection()
                            ),
                        );
                        menu.separator();
                        for (icon, label, kind) in [
                            (
                                Icon::Highlight,
                                "Highlight selected text",
                                Markup::Highlight,
                            ),
                            (
                                Icon::Underline,
                                "Underline selected text",
                                Markup::Underline,
                            ),
                            (Icon::Strike, "Strike through selected text", Markup::Strike),
                            (Icon::Redact, "Redact selected text", Markup::Redact),
                            (Icon::Redact, "Pixelate selected text", Markup::Pixelate),
                        ] {
                            menu.action(
                                icon,
                                label,
                                glib::clone!(
                                    #[weak]
                                    view,
                                    move || view.ocr.markup(kind)
                                ),
                            );
                        }
                    }
                    menu.separator();
                    menu.action(
                        Icon::Redact,
                        "Review sensitive text…",
                        glib::clone!(
                            #[weak]
                            view,
                            #[weak]
                            window,
                            move || view.ocr.smart_redact(&window)
                        ),
                    );
                } else {
                    menu.status(if text.ready {
                        "No visible recognised text here"
                    } else {
                        "Recognising text…"
                    });
                }
                menu.action(
                    Icon::Close,
                    "Exit text layer",
                    glib::clone!(
                        #[weak]
                        view,
                        move || view.ocr.cancel()
                    ),
                );
                menu.separator();
                menu_button(menu, Icon::Undo, "Undo", &view.undo);
                menu_button(menu, Icon::Redo, "Redo", &view.redo);
            } else {
                if let Some((x, y)) = point {
                    let mut s = state.borrow_mut();
                    select_context_target(&mut s, x, y);
                    view.refresh(&s);
                }
                let s = state.borrow();
                if !s.selected.is_empty() {
                    if s.selected.len() > 1 {
                        menu.status(&format!("{} objects selected", s.selected.len()));
                    }
                    menu_button(menu, Icon::Text, "Edit text", &view.edit_text);
                    menu_button(menu, Icon::Duplicate, "Duplicate", &view.duplicate);
                    let group = s
                        .doc
                        .marks
                        .iter()
                        .find(|m| Some(&m.id) == s.selected.first())
                        .and_then(|m| m.group);
                    if group.is_none()
                        || s.doc
                            .marks
                            .iter()
                            .any(|m| s.selected.contains(&m.id) && m.group != group)
                    {
                        menu_button(menu, Icon::Group, "Group", &view.group);
                    }
                    menu_button(menu, Icon::Ungroup, "Ungroup", &view.ungroup);
                    let single = s.selected_mark();
                    if let Some(mark) = single {
                        match mark.kind {
                            MarkKind::Step { number } => {
                                let before = s.doc.marks.iter().any(|m| matches!(m.kind, MarkKind::Step { number: n } if n < number));
                                let after = s.doc.marks.iter().any(|m| matches!(m.kind, MarkKind::Step { number: n } if n > number));
                                if before {
                                    menu_button(
                                        menu,
                                        Icon::Earlier,
                                        "Move number earlier",
                                        &view.earlier,
                                    );
                                }
                                if after {
                                    menu_button(
                                        menu,
                                        Icon::Later,
                                        "Move number later",
                                        &view.later,
                                    );
                                }
                            }
                            MarkKind::Redaction | MarkKind::PrivacyBlur => {
                                let pixelated = matches!(mark.kind, MarkKind::PrivacyBlur);
                                menu.action(
                                    Icon::Redact,
                                    if pixelated {
                                        "Use opaque redaction"
                                    } else {
                                        "Use pixelated redaction"
                                    },
                                    glib::clone!(
                                        #[weak]
                                        view,
                                        move || {
                                            if view.privacy_mode.is_visible() {
                                                view.privacy_mode
                                                    .set_selected(u32::from(!pixelated));
                                            }
                                        }
                                    ),
                                );
                            }
                            _ => {}
                        }
                    }
                    for (i, button) in view.choices.iter().enumerate() {
                        if let Some(choice) = s
                            .selected
                            .first()
                            .and_then(|id| s.corrections.get(id))
                            .and_then(|c| c.choices.get(i))
                        {
                            menu_button(
                                menu,
                                Icon::Smart,
                                &format!("Change to {}", choice.kind),
                                button,
                            );
                        }
                    }
                    menu_button(menu, Icon::Pen, "Keep original stroke", &view.keep_ink);
                    menu.separator();
                    let (front, back) = layer_moves(&s);
                    if front {
                        menu_button(menu, Icon::Front, "Bring to front", &view.front);
                    }
                    if back {
                        menu_button(menu, Icon::Back, "Send to back", &view.back);
                    }
                    let label = match single.map(|m| &m.kind) {
                        Some(MarkKind::Text { .. }) => "Text properties…",
                        Some(MarkKind::Connector { .. }) => "Arrow and line settings…",
                        Some(MarkKind::Step { .. }) => "Numbered circle properties…",
                        Some(MarkKind::Magnifier { .. }) => "Magnifier properties…",
                        Some(MarkKind::Spotlight { .. }) => "Spotlight properties…",
                        Some(MarkKind::Redaction | MarkKind::PrivacyBlur) => {
                            "Privacy zone properties…"
                        }
                        _ => "Object properties…",
                    };
                    menu.action(
                        Icon::Inspector,
                        label,
                        glib::clone!(
                            #[weak]
                            inspector_button,
                            move || inspector_button.set_active(true)
                        ),
                    );
                    menu.separator();
                } else {
                    if !view.rendering.get() {
                        menu_button(menu, Icon::Copy, "Copy image", &copy);
                        menu_button(menu, Icon::Save, "Export PNG…", &export);
                        menu_button(menu, Icon::Pin, "Pin screenshot", &pin_button);
                    } else {
                        menu.status("Rendering image…");
                    }
                    menu.separator();
                    if !s.doc.marks.is_empty() {
                        menu.action(
                            Icon::Select,
                            "Select all objects",
                            glib::clone!(
                                #[strong]
                                state,
                                #[weak]
                                view,
                                move || select_all_objects(&mut state.borrow_mut(), &view)
                            ),
                        );
                    }
                    menu.action(
                        Icon::Ocr,
                        "Recognise text",
                        glib::clone!(
                            #[weak]
                            view,
                            move || view.ocr.toggle.set_active(true)
                        ),
                    );
                    menu_button(menu, Icon::Designer, "Screenshot designer…", &designer);
                    let crop_tool = tool_buttons
                        .iter()
                        .find(|(t, _)| *t == Tool::Crop)
                        .unwrap()
                        .1
                        .clone();
                    menu.action(Icon::Crop, "Crop screenshot", move || {
                        crop_tool.set_active(true)
                    });
                    menu_button(menu, Icon::ResetCrop, "Reset crop", &view.reset_crop);
                    menu_button(menu, Icon::Fit, "Fit image", &fit);
                    menu.action(
                        Icon::Resize,
                        "Actual pixels",
                        glib::clone!(
                            #[strong]
                            state,
                            #[weak]
                            view,
                            move || {
                                let mut s = state.borrow_mut();
                                zoom(&mut s, &view, Some(1.), None);
                                view.refresh(&s);
                            }
                        ),
                    );
                    menu.separator();
                }
                menu_button(menu, Icon::Undo, "Undo", &view.undo);
                menu_button(menu, Icon::Redo, "Redo", &view.redo);
                if !s.selected.is_empty() {
                    menu.separator();
                    menu_button(menu, Icon::Delete, "Delete", &view.delete);
                }
            }
            menu.separator();
            menu.action(
                Icon::Toolbars,
                if view.chrome_visible.get() {
                    "Hide toolbars"
                } else {
                    "Show toolbars"
                },
                glib::clone!(
                    #[weak]
                    view,
                    move || {
                        view.invoke("toolbar");
                    }
                ),
            );
            menu.action(
                Icon::Fullscreen,
                if window.is_fullscreen() {
                    "Leave fullscreen"
                } else {
                    "Enter fullscreen"
                },
                glib::clone!(
                    #[weak]
                    view,
                    move || {
                        view.invoke("fullscreen");
                    }
                ),
            );
            if state.borrow().text_draft.is_none() && !view.rendering.get() {
                menu.action(
                    Icon::Save,
                    "Save PNG as…",
                    glib::clone!(
                        #[weak]
                        view,
                        move || view.save(true)
                    ),
                );
                if view.last_saved.borrow().is_some() {
                    menu.action(
                        Icon::Copy,
                        "Copy saved file path",
                        glib::clone!(
                            #[weak]
                            view,
                            move || {
                                view.copy_path();
                            }
                        ),
                    );
                }
            }
            menu.popup(
                at.0.clamp(h.value(), h.value() + h.page_size().max(1.) - 1.),
                at.1.clamp(v.value(), v.value() + v.page_size().max(1.) - 1.),
            );
        }
    ));
    let secondary = gtk::GestureClick::new();
    secondary.set_button(3);
    secondary.set_propagation_phase(gtk::PropagationPhase::Capture);
    secondary.connect_pressed(glib::clone!(
        #[strong]
        open_context,
        #[weak]
        view,
        move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if view.state.borrow().tool == Tool::Crop
                && gesture
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
            {
                let mut state = view.state.borrow_mut();
                state.cancel();
                state.doc.set_crop(None);
                view.refresh(&state);
            } else if !view.settings.on_right_click.is_empty() {
                view.queue_actions(&view.settings.on_right_click);
            } else {
                open_context(Some((x, y)));
            }
        }
    ));
    view.area.add_controller(secondary);
    view.ocr.toggle.connect_toggled(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |toggle| {
            let mut state = state.borrow_mut();
            if toggle.is_active() {
                state.cancel();
            }
            view.refresh(&state);
        }
    ));
    for button in [&designer, &resize] {
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[weak]
            view,
            #[weak]
            window,
            move |_| {
                view.ocr.cancel();
                let weak_view = Rc::downgrade(&view);
                let refresh_state = state.clone();
                crate::designer::open(
                    &window,
                    state.clone(),
                    Rc::new(move || {
                        if let Some(view) = weak_view.upgrade() {
                            view.refresh(&refresh_state.borrow());
                        }
                    }),
                );
            }
        ));
    }
    view.head_size.connect_value_changed(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |spin| {
            if view.syncing.get() {
                return;
            }
            let mut s = state.borrow_mut();
            s.set_tip(spin.value());
            view.refresh(&s);
        }
    ));
    view.font_size.connect_value_changed(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |spin| {
            if view.syncing.get() {
                return;
            }
            let mut s = state.borrow_mut();
            s.set_font(spin.value());
            view.refresh(&s);
        }
    ));
    for (button, action) in [
        (&view.group, 0),
        (&view.ungroup, 1),
        (&view.duplicate, 2),
        (&view.earlier, 3),
        (&view.later, 4),
        (&view.front, 5),
        (&view.back, 6),
    ] {
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[weak]
            view,
            move |_| {
                let mut s = state.borrow_mut();
                s.cancel();
                let ids = s.selected.clone();
                match action {
                    0 => {
                        s.doc.group_marks(&ids);
                    }
                    1 => {
                        s.doc.ungroup_marks(&ids);
                    }
                    2 => {
                        s.selected = s.doc.duplicate_marks(&ids);
                    }
                    5 | 6 => {
                        s.doc.reorder_marks(&ids, action == 5);
                    }
                    _ => {
                        if let Some(id) = ids.first() {
                            s.doc.reorder_step(*id, if action == 3 { -1 } else { 1 });
                        }
                    }
                }
                view.refresh(&s);
                view.area.grab_focus();
            }
        ));
    }
    view.privacy_mode.connect_selected_notify(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |mode| {
            if view.syncing.get() {
                return;
            }
            let mut s = state.borrow_mut();
            s.cancel();
            let ids = s.selected.clone();
            s.doc.set_privacy_blur(&ids, mode.selected() == 1);
            view.refresh(&s);
            view.area.grab_focus();
        }
    ));
    for (i, button) in view.choices.iter().enumerate() {
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[weak]
            view,
            move |_| {
                let mut s = state.borrow_mut();
                s.correct(Some(i));
                view.refresh(&s);
                view.area.grab_focus();
            }
        ));
    }
    view.keep_ink.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut s = state.borrow_mut();
            s.correct(None);
            view.refresh(&s);
            view.area.grab_focus();
        }
    ));
    view.lens_zoom.connect_value_changed(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |spin| {
            if view.syncing.get() {
                return;
            }
            let mut s = state.borrow_mut();
            if let Some(&id) = s.selected.first() {
                s.doc.set_magnifier_zoom(id, spin.value() as f32);
            }
            view.refresh(&s);
        }
    ));
    view.edit_text.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut s = state.borrow_mut();
            if let Some(&id) = s.selected.first() {
                s.edit_text(id);
            }
            let draft = s.text_draft.clone();
            let transform = s.transform;
            drop(s);
            view.open_text(draft, transform);
        }
    ));
    let double_click = gtk::GestureClick::new();
    double_click.set_button(1);
    double_click.connect_pressed(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_, n, x, y| {
            if n != 2 {
                return;
            }
            if view.ocr.enabled() {
                return;
            }
            let mut s = state.borrow_mut();
            if s.tool == Tool::Eraser {
                return;
            }
            let p = s.transform.point(x, y, &s.doc);
            if let Some(id) = s.doc.hit_test(p, 0.) {
                if s.edit_text(id) {
                    s.selected = vec![id];
                    view.refresh(&s);
                }
            }
            let draft = s.text_draft.clone();
            let transform = s.transform;
            drop(s);
            view.open_text(draft, transform);
        }
    ));
    view.area.add_controller(double_click);
    save_project.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        #[weak]
        window,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            save_project_dialog(&window, &state, &view);
        }
    ));
    open_project.connect_clicked(glib::clone!(
        #[weak]
        app,
        #[weak]
        view,
        #[weak]
        window,
        #[weak]
        project_popover,
        move |_| {
            project_popover.popdown();
            open_project_dialog(&app, &window, &view);
        }
    ));

    view.scene.set_draw_func(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_, cr, w, h| {
            let mut state = state.borrow_mut();
            state.transform = canvas_transform(&state, w, h);
            match paint_stable_scene(&view, &state, cr) {
                Ok(()) => view.scene_revision.set(Some(state.doc.revision())),
                Err(error) => {
                    view.scene_revision.set(None);
                    cr.identity_matrix();
                    cr.reset_clip();
                    cr.set_operator(gtk::cairo::Operator::Clear);
                    let _ = cr.paint();
                    eprintln!("Cannot draw document scene: {error:#}");
                }
            }
        }
    ));
    for adjustment in [view.scroll.hadjustment(), view.scroll.vadjustment()] {
        adjustment.connect_changed(glib::clone!(
            #[weak]
            view,
            move |_| view.scene.queue_draw()
        ));
        adjustment.connect_value_changed(glib::clone!(
            #[weak]
            view,
            move |_| view.scene.queue_draw()
        ));
    }
    view.scene
        .connect_scale_factor_notify(|scene| scene.queue_draw());
    view.area.set_draw_func(glib::clone!(
        #[strong(rename_to = draw_state)]
        state,
        #[weak]
        view,
        move |_, cr, w, h| {
            let mut state = draw_state.borrow_mut();
            state.transform = canvas_transform(&state, w, h);
            let scale = state.transform.scale;
            view.zoom_anchor.set(None);
            let label = format!(
                "{}{}%",
                if state.zoom.is_none() { "Fit · " } else { "" },
                (scale * 100.).round() as u32
            );
            if view.zoom_label.text() != label {
                // Widget changes during snapshot invalidate siblings before GTK can paint them.
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    view,
                    move || view.zoom_label.set_text(&label)
                ));
            }
            if view.caption.needs_sync(state.transform) {
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    view,
                    #[strong]
                    draw_state,
                    move || view.caption.sync(&draw_state.borrow())
                ));
            }
            let result = (|| {
                if dynamic_scene(&state) {
                    paint_document_scene(&state, cr, &view.caption)?;
                }
                paint_canvas(&state, cr, &view.ocr)
            })();
            if let Err(error) = result {
                cr.identity_matrix();
                cr.reset_clip();
                cr.set_operator(gtk::cairo::Operator::Clear);
                let _ = cr.paint();
                eprintln!("Cannot draw canvas overlay: {error:#}");
            }
        }
    ));
    for (tool, button) in &tool_buttons {
        let tool = *tool;
        button.connect_toggled(glib::clone!(
            #[strong]
            state,
            #[weak]
            view,
            move |button| {
                if !button.is_active() {
                    return;
                }
                view.ocr.cancel();
                let mut state = state.borrow_mut();
                state.cancel();
                state.magnifier_source = None;
                state.tool = tool;
                state.text_draft = None;
                view.caption.close();
                view.help
                    .set_text(TOOLS.iter().find(|t| t.0 == tool).unwrap().3);
                view.refresh(&state);
                view.area.grab_focus();
            }
        ));
    }
    for (button, rgb, alpha) in &view.swatches {
        let rgb = *rgb;
        let alpha = *alpha;
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[weak]
            view,
            move |_| {
                if view.syncing.get() {
                    return;
                }
                let mut state = state.borrow_mut();
                if alpha {
                    state.set_style(Some(rgb), None);
                } else {
                    state.set_rgb([rgb[0], rgb[1], rgb[2]]);
                }
                view.refresh(&state);
            }
        ));
    }
    for (button, width) in &view.presets {
        let width = *width;
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[weak]
            view,
            move |_| {
                if view.syncing.get() {
                    return;
                }
                let mut state = state.borrow_mut();
                state.set_style(None, Some(width));
                view.refresh(&state);
            }
        ));
    }
    view.color.connect_color_set(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |button| {
            if view.syncing.get() {
                return;
            }
            let c = button.rgba();
            let mut state = state.borrow_mut();
            let rgb = [
                f64::from(c.red()),
                f64::from(c.green()),
                f64::from(c.blue()),
            ];
            if (f64::from(c.alpha()) - state.style().0[3]).abs() < 1e-6 {
                state.set_rgb(rgb);
            } else {
                state.set_style(Some([rgb[0], rgb[1], rgb[2], f64::from(c.alpha())]), None);
            }
            view.refresh(&state);
        }
    ));
    view.width.connect_value_changed(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |spin| {
            if view.syncing.get() {
                return;
            }
            let mut state = state.borrow_mut();
            state.set_style(None, Some(spin.value()));
            view.refresh(&state);
        }
    ));
    view.opacity.connect_value_changed(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |scale| {
            if view.syncing.get() {
                return;
            }
            let mut state = state.borrow_mut();
            state.set_opacity(scale.value() / 100.);
            view.refresh(&state);
        }
    ));
    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    drag.connect_drag_begin(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |controller, x, y| {
            view.area.grab_focus();
            if view.panning.get().is_some() {
                return;
            }
            if state.borrow().tool == Tool::Eraser {
                view.ocr.cancel();
            }
            if view.ocr.enabled() {
                view.ocr.begin(
                    x,
                    y,
                    controller
                        .current_event_state()
                        .contains(gdk::ModifierType::SHIFT_MASK),
                );
                return;
            }
            let mut state = state.borrow_mut();
            let shift = controller
                .current_event_state()
                .contains(gdk::ModifierType::SHIFT_MASK);
            let modifiers = controller.current_event_state();
            if let Some(message) = state.begin(
                x,
                y,
                shift,
                modifiers.contains(gdk::ModifierType::ALT_MASK),
                modifiers.contains(gdk::ModifierType::CONTROL_MASK),
            ) {
                view.help.set_text(message);
            }
            view.refresh(&state);
            let draft = state.text_draft.clone();
            let transform = state.transform;
            drop(state);
            view.open_text(draft, transform);
        }
    ));
    drag.connect_drag_update(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |controller, dx, dy| {
            if view.ocr.enabled() {
                if let Some((x, y)) = controller.start_point() {
                    view.ocr.update(x + dx, y + dy);
                }
                return;
            }
            let mut state = state.borrow_mut();
            let mods = controller.current_event_state();
            state.update(
                dx,
                dy,
                mods.contains(gdk::ModifierType::SHIFT_MASK),
                mods.contains(gdk::ModifierType::ALT_MASK),
            );
            view.refresh(&state);
        }
    ));
    drag.connect_drag_end(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |controller, dx, dy| {
            if view.ocr.enabled() {
                if let Some((x, y)) = controller.start_point() {
                    view.ocr.update(x + dx, y + dy);
                }
                view.ocr.finish();
                return;
            }
            let mut state = state.borrow_mut();
            let mods = controller.current_event_state();
            state.update(
                dx,
                dy,
                mods.contains(gdk::ModifierType::SHIFT_MASK),
                mods.contains(gdk::ModifierType::ALT_MASK),
            );
            if let Some(message) = state.finish() {
                view.help.set_text(message);
            }
            view.refresh(&state);
        }
    ));
    drag.connect_cancel(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_, _| {
            if view.ocr.enabled() {
                view.ocr.cancel();
                return;
            }
            let mut state = state.borrow_mut();
            state.cancel();
            view.refresh(&state);
        }
    ));
    view.area.add_controller(drag);
    let pan = gtk::GestureDrag::new();
    pan.set_button(2);
    pan.set_propagation_phase(gtk::PropagationPhase::Capture);
    pan.connect_drag_begin(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |controller, x, y| {
            controller.set_state(gtk::EventSequenceState::Claimed);
            view.ocr.cancel();
            let mut state = state.borrow_mut();
            state.cancel();
            if state.zoom.is_none() {
                // Panning must preserve Fit's scale, even below the wheel's 10% minimum.
                let scale = state.transform.scale;
                zoom(&mut state, &view, Some(scale), Some((x, y)));
            }
            view.panning.set(Some((0., 0.)));
            view.refresh(&state);
        }
    ));
    pan.connect_drag_update(glib::clone!(
        #[weak]
        view,
        move |_, dx, dy| view.pan(dx, dy)
    ));
    pan.connect_drag_end(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_, dx, dy| {
            view.pan(dx, dy);
            view.end_pan(&state.borrow());
        }
    ));
    pan.connect_cancel(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_, _| {
            view.end_pan(&state.borrow());
        }
    ));
    viewport.add_controller(pan);
    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter(glib::clone!(
        #[weak]
        view,
        move |_, x, y| view.pointer.set(Some((x, y)))
    ));
    motion.connect_motion(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_, x, y| {
            view.pointer.set(Some((x, y)));
            if view.panning.get().is_some() {
                view.refresh_cursor(&state.borrow());
                return;
            }
            if view.ocr.enabled() {
                view.area.set_cursor_from_name(Some("text"));
                return;
            }
            let mut state = state.borrow_mut();
            if state.tool != Tool::Select || state.gesture.is_some() {
                return;
            }
            let (x, y) = (
                x + view.scroll.hadjustment().value(),
                y + view.scroll.vadjustment().value(),
            );
            let hit = if state.transform.contains(x, y, &state.doc) {
                state.doc.hit_test(
                    state.transform.point(x, y, &state.doc),
                    (6. / state.transform.scale) as f32,
                )
            } else {
                None
            };
            if state.hovered != hit {
                state.hovered = hit;
                view.refresh_cursor(&state);
                view.area.queue_draw();
            } else {
                view.refresh_cursor(&state);
            }
        }
    ));
    motion.connect_leave(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut state = state.borrow_mut();
            view.pointer.set(None);
            state.hovered = None;
            if state.gesture.is_none() {
                view.refresh_cursor(&state);
                view.area.queue_draw();
            }
        }
    ));
    viewport.add_controller(motion);
    view.undo.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut state = state.borrow_mut();
            state.cancel();
            state.doc.undo();
            state.clean_selection();
            view.refresh(&state);
        }
    ));
    view.redo.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut state = state.borrow_mut();
            state.cancel();
            state.doc.redo();
            state.clean_selection();
            view.refresh(&state);
        }
    ));
    view.delete.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut state = state.borrow_mut();
            state.cancel();
            let ids = std::mem::take(&mut state.selected);
            if state.doc.remove_many(&ids) {
                view.help
                    .set_text("Selection deleted · Ctrl+Z restores it and its connections.");
            }
            view.refresh(&state);
            view.area.grab_focus();
        }
    ));
    view.reset_crop.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut state = state.borrow_mut();
            state.cancel();
            state.doc.set_crop(None);
            view.refresh(&state);
        }
    ));
    fit.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        move |_| {
            let mut state = state.borrow_mut();
            zoom(&mut state, &view, None, None);
            view.refresh(&state);
        }
    ));
    for (button, factor) in [(&minus, 1. / 1.25), (&plus, 1.25)] {
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[weak]
            view,
            move |_| {
                let mut state = state.borrow_mut();
                let z = (state.zoom.unwrap_or(state.transform.scale) * factor).clamp(0.1, 4.);
                zoom(&mut state, &view, Some(z), None);
                view.refresh(&state);
            }
        ));
    }
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    wheel.connect_scroll(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |controller, dx, dy| {
            if view.menu.is_open()
                || view.panning.get().is_some()
                || state.borrow().gesture.is_some()
            {
                return glib::Propagation::Stop;
            }
            let modifiers = controller.current_event_state();
            if modifiers.intersects(
                gdk::ModifierType::SHIFT_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            ) {
                return glib::Propagation::Proceed;
            }
            let smooth = controller.unit() == gdk::ScrollUnit::Surface;
            if smooth && !modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                view.scroll_by(dx, dy);
                return glib::Propagation::Stop;
            }
            if dy == 0. {
                return glib::Propagation::Proceed;
            }
            let Some(at) = view.pointer.get() else {
                return glib::Propagation::Proceed;
            };
            let mut state = state.borrow_mut();
            let factor = if smooth { 1.0025_f64 } else { 1.25_f64 };
            let z = (state.zoom.unwrap_or(state.transform.scale) * factor.powf(-dy)).clamp(0.1, 4.);
            if state.zoom != Some(z) {
                zoom(&mut state, &view, Some(z), Some(at));
                view.refresh(&state);
            }
            glib::Propagation::Stop
        }
    ));
    view.area.add_controller(wheel);
    let pinch = gtk::GestureZoom::new();
    let pinch_start = Rc::new(Cell::new(1.));
    pinch.connect_begin(glib::clone!(
        #[strong]
        state,
        #[strong]
        pinch_start,
        #[weak]
        view,
        move |gesture, _| {
            if view.menu.is_open() {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let mut state = state.borrow_mut();
            state.cancel();
            pinch_start.set(state.zoom.unwrap_or(state.transform.scale));
        }
    ));
    pinch.connect_scale_changed(glib::clone!(
        #[strong]
        state,
        #[strong]
        pinch_start,
        #[weak]
        view,
        move |gesture, scale| {
            if !scale.is_finite() || scale <= 0. {
                return;
            }
            let Some((x, y)) = gesture.bounding_box_center() else {
                return;
            };
            let at = (
                x - view.scroll.hadjustment().value(),
                y - view.scroll.vadjustment().value(),
            );
            let mut state = state.borrow_mut();
            let z = (pinch_start.get() * scale).clamp(0.1, 4.);
            if state.zoom != Some(z) {
                zoom(&mut state, &view, Some(z), Some(at));
                view.refresh(&state);
            }
        }
    ));
    view.area.add_controller(pinch);

    copy.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        #[weak]
        window,
        move |_| view.render_image(&state.borrow(), &window, RenderAction::Copy)
    ));
    export.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| view.save(false)
    ));
    let click_away = gtk::GestureClick::new();
    click_away.set_button(1);
    click_away.set_propagation_phase(gtk::PropagationPhase::Capture);
    let select_tool = tool_buttons[0].1.clone();
    click_away.connect_pressed(glib::clone!(
        #[strong]
        state,
        #[weak]
        view,
        #[weak]
        window,
        #[weak]
        select_tool,
        move |gesture, _, x, y| {
            if view.menu.is_open() {
                return;
            }
            if state.borrow().text_draft.is_none() {
                return;
            }
            let Some(target) = window.pick(x, y, gtk::PickFlags::DEFAULT) else {
                return;
            };
            if view.caption.owns_widget(&target) {
                return;
            }
            if !view.caption.finish() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                return;
            }
            select_tool.set_active(true);
            if target == *view.area.upcast_ref::<gtk::Widget>() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        }
    ));
    window.add_controller(click_away);

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let alt_tap = Rc::new(Cell::new(false));
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        view,
        #[weak]
        help_button,
        #[strong]
        open_context,
        #[strong]
        alt_tap,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, modifiers| {
            if view.menu.is_open() {
                return glib::Propagation::Proceed;
            }
            if matches!(key, gdk::Key::Alt_L | gdk::Key::Alt_R) {
                let state = view.state.borrow();
                alt_tap.set(state.text_draft.is_some() || state.tool == Tool::Text);
                return glib::Propagation::Proceed;
            }
            alt_tap.set(false);
            if view.key(key, modifiers) {
                return glib::Propagation::Stop;
            }
            if !view.editable_focus() {
                if key == gdk::Key::Menu
                    || (modifiers.contains(gdk::ModifierType::SHIFT_MASK) && key == gdk::Key::F10)
                {
                    open_context(None);
                    return glib::Propagation::Stop;
                }
                if key == gdk::Key::question {
                    help_button.popup();
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        }
    ));
    keys.connect_key_released(glib::clone!(
        #[weak]
        view,
        #[strong]
        alt_tap,
        move |_, key, _, _| {
            if matches!(key, gdk::Key::Alt_L | gdk::Key::Alt_R) && alt_tap.replace(false) {
                if view.state.borrow().text_draft.is_some() {
                    view.caption.cycle_effect();
                } else {
                    let mut state = view.state.borrow_mut();
                    let effect = state.text_effect().next();
                    state.set_text_effect(effect);
                    view.refresh(&state);
                }
            }
        }
    ));
    window.add_controller(keys);
    window.connect_close_request(glib::clone!(#[strong] state, #[strong] view, move |window| {
        let mut state=state.borrow_mut();state.cancel();view.refresh(&state);
        if !state.doc.can_undo() && state.text_draft.is_none() { return glib::Propagation::Proceed; }
        drop(state);
        let dialog=gtk::MessageDialog::builder().transient_for(window).modal(true).message_type(gtk::MessageType::Question)
            .text("Close this screenshot?").secondary_text("Editable objects and unfinished text will be discarded. Save a project to keep editing later, or export/copy a flattened image.").build();
        dialog.add_button("Keep editing",gtk::ResponseType::Cancel);dialog.add_button("Close screenshot",gtk::ResponseType::Accept);dialog.set_default_response(gtk::ResponseType::Cancel);
        dialog.connect_response(glib::clone!(#[weak] window, move |dialog,response| { dialog.close();if response==gtk::ResponseType::Accept { window.destroy(); } }));
        dialog.present();glib::Propagation::Stop
    }));
    let weak_view = Rc::downgrade(&view);
    let weak_state = Rc::downgrade(&state);
    let weak_smart = tool_buttons[1].1.downgrade();
    let weak_ink = tool_buttons[2].1.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(result) = results.recv().await {
            let (Some(view), Some(state), Some(smart)) = (
                weak_view.upgrade(),
                weak_state.upgrade(),
                weak_smart.upgrade(),
            ) else {
                break;
            };
            match result {
                Inference::Ready(Ok(())) => {
                    state.borrow_mut().model_ready = true;
                    smart.set_sensitive(true);
                    view.help.set_text("Sketch → snap → select and move · S smart shapes · V select · ? shows shortcuts.");
                }
                Inference::Ready(Err(error)) => {
                    if let Some(ink) = weak_ink.upgrade() {
                        ink.set_active(true);
                    }
                    view.help.set_text(&format!(
                        "Smart shapes unavailable: {error}. Freehand and manual tools still work."
                    ));
                    view.help.set_tooltip_text(Some(&format!("{error:#}")));
                }
                Inference::Shape(id, result) => {
                    let mut state = state.borrow_mut();
                    match result {
                        Ok(recognition) => {
                            let accepted = recognition.accepted.is_some();
                            state.recognize(id, recognition);
                            view.help.set_text(if accepted {
                                "Shape fitted · Choose a correction below, or Keep original stroke."
                            } else {
                                "Ink retained · Choose a suggested shape below, or keep the stroke."
                            });
                            view.refresh(&state);
                        }
                        Err(error) => view
                            .help
                            .set_text(&format!("Recognition failed; ink retained: {error:#}")),
                    }
                }
            }
        }
    });
    view.invoke(&settings.initial_tool);
    view.set_toolbars(!settings.hide_toolbars);
    if settings.focus_toggle_toolbars {
        let initial_focus = Cell::new(true);
        window.connect_is_active_notify(glib::clone!(
            #[weak]
            view,
            move |window| {
                if initial_focus.replace(false) {
                    return;
                }
                view.set_toolbars(window.is_active());
            }
        ));
    }
    if settings.profile_startup {
        let first_map = Cell::new(true);
        window.connect_map(move |_| {
            if first_map.replace(false) {
                eprintln!(
                    "startup: GTK window mapped in {:.1} ms",
                    startup.elapsed().as_secs_f64() * 1000.
                );
            }
        });
    }
    window.present();
    if let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() {
        toplevel.set_fullscreen_mode(if settings.fullscreen_mode == "all" {
            gdk::FullscreenMode::AllMonitors
        } else {
            gdk::FullscreenMode::CurrentMonitor
        });
    }
    if settings.fullscreen {
        window.fullscreen();
    }
    if settings.floating {
        glib::idle_add_local_once(glib::clone!(
            #[weak]
            window,
            move || window.set_resizable(true)
        ));
    }
    let initial_zoom = (settings.zoom_factor / settings.input_scale).clamp(0.1, 4.);
    if settings.zoom_factor != 1. || settings.input_scale != 1. {
        view.area.add_tick_callback(glib::clone!(
            #[weak]
            view,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move |_, _| {
                if view.scroll.hadjustment().page_size() <= 0.
                    || view.scroll.vadjustment().page_size() <= 0.
                {
                    return glib::ControlFlow::Continue;
                }
                let mut state = view.state.borrow_mut();
                state.transform = canvas_transform(&state, view.area.width(), view.area.height());
                zoom(&mut state, &view, Some(initial_zoom), None);
                view.refresh(&state);
                glib::ControlFlow::Break
            }
        ));
    }
    view.area.grab_focus();
}

fn canvas_transform(state: &State, w: i32, h: i32) -> Transform {
    let scale = state.zoom.unwrap_or_else(|| {
        ((f64::from(w) - 48.) / f64::from(state.doc.width))
            .min((f64::from(h) - 48.) / f64::from(state.doc.height))
            .clamp(0.001, 1.)
    });
    Transform {
        scale,
        x: ((f64::from(w) - f64::from(state.doc.width) * scale) / 2.).max(0.),
        y: ((f64::from(h) - f64::from(state.doc.height) * scale) / 2.).max(0.),
    }
}

fn dynamic_scene(state: &State) -> bool {
    state.text_draft.is_some()
        || match &state.gesture {
            Some(Gesture::Transform { .. }) => true,
            Some(Gesture::Draw(mark)) => matches!(mark.kind, MarkKind::Spotlight { .. }),
            _ => false,
        }
}

// Keep vector commands across pans; rasterise only the visible viewport at native density.
fn paint_stable_scene(view: &View, state: &State, cr: &Context) -> Result<()> {
    let t = state.transform;
    let transform = [t.scale.to_bits(), t.x.to_bits(), t.y.to_bits()];
    let fonts = pangocairo::FontMap::default().serial();
    let options = cr.font_options()?;
    let mut cache = view.scene_cache.borrow_mut();
    if cache.as_ref().is_none_or(|cache| {
        cache.revision != state.doc.revision()
            || cache.transform != transform
            || cache.fonts != fonts
            || cache.options != options
    }) {
        let commands = RecordingSurface::create(Content::ColorAlpha, None)?;
        let scene = Context::new(&commands)?;
        scene.set_font_options(&options);
        paint_document_scene(state, &scene, &view.caption)?;
        *cache = Some(SceneCache {
            revision: state.doc.revision(),
            transform,
            fonts,
            options,
            commands,
        });
    }
    let h = view.scroll.hadjustment();
    let v = view.scroll.vadjustment();
    let density = f64::from(view.scene.scale_factor().max(1));
    // Align with the canvas pixel grid, so fractional panning does not resample the cache twice.
    let (x, y) = (
        (h.value() * density).floor() / density,
        (v.value() * density).floor() / density,
    );
    let pixels = |size: f64| ((size.max(1.).ceil() + 2.) * density) as i32;
    let surface =
        ImageSurface::create(Format::ARgb32, pixels(h.page_size()), pixels(v.page_size()))?;
    surface.set_device_scale(density, density);
    {
        let raster = Context::new(&surface)?;
        raster.translate(-x, -y);
        raster.set_source_surface(&cache.as_ref().unwrap().commands, 0., 0.)?;
        raster.paint()?;
    }
    cr.set_source_surface(&surface, x, y)?;
    cr.paint()?;
    Ok(())
}

fn paint_document_scene(
    state: &State,
    cr: &Context,
    caption: &crate::caption::Caption,
) -> Result<()> {
    cr.save()?;
    cr.translate(state.transform.x, state.transform.y);
    cr.scale(state.transform.scale, state.transform.scale);
    cr.rectangle(0., 0., state.doc.width.into(), state.doc.height.into());
    cr.clip();
    if let Some(Gesture::Transform { matrix, .. }) = state.gesture {
        state.doc.paint_preview(cr, &state.selected, matrix)?;
    } else if state.text_draft.is_some() {
        caption.paint(state, cr)?;
    } else {
        state.doc.paint(cr)?;
    }
    cr.restore()?;
    Ok(())
}

fn paint_canvas(state: &State, cr: &Context, ocr: &crate::ocr_ui::OcrUi) -> Result<()> {
    cr.save()?;
    cr.translate(state.transform.x, state.transform.y);
    cr.scale(state.transform.scale, state.transform.scale);
    cr.save()?;
    cr.rectangle(0., 0., state.doc.width.into(), state.doc.height.into());
    cr.clip();
    if let Some(Gesture::Draw(mark)) = &state.gesture {
        state.doc.paint_mark(cr, mark)?;
    }
    ocr.paint(cr, state)?;
    if let Some(crop) = state.crop() {
        cr.set_source_rgba(0., 0., 0., 0.58);
        cr.rectangle(0., 0., state.doc.width.into(), state.doc.height.into());
        cr.rectangle(
            crop.x.into(),
            crop.y.into(),
            crop.width.into(),
            crop.height.into(),
        );
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        cr.fill()?;
        cr.set_fill_rule(gtk::cairo::FillRule::Winding);
        cr.set_source_rgb(ACCENT[0], ACCENT[1], ACCENT[2]);
        cr.set_line_width(1. / state.transform.scale);
        cr.rectangle(
            crop.x.into(),
            crop.y.into(),
            crop.width.into(),
            crop.height.into(),
        );
        cr.stroke()?;
    }
    cr.restore()?;
    let scale = state.transform.scale;
    if let Some(Gesture::Marquee { start, end, .. } | Gesture::Source { start, end }) =
        state.gesture
    {
        let b = interaction::between(start, end);
        cr.rectangle(b.x.into(), b.y.into(), b.width.into(), b.height.into());
        cr.set_source_rgba(ACCENT[0], ACCENT[1], ACCENT[2], 0.12);
        cr.fill_preserve()?;
        cr.set_source_rgb(ACCENT[0], ACCENT[1], ACCENT[2]);
        cr.set_line_width(1. / scale);
        cr.stroke()?;
    }
    if let Some(source) = state.magnifier_source {
        cr.set_source_rgb(ACCENT[0], ACCENT[1], ACCENT[2]);
        cr.set_line_width(1. / scale);
        cr.set_dash(&[4. / scale, 3. / scale], 0.);
        cr.rectangle(
            source.x.into(),
            source.y.into(),
            source.width.into(),
            source.height.into(),
        );
        cr.stroke()?;
        cr.set_dash(&[], 0.);
    }
    if let Some(Gesture::Transform { guides, .. }) = &state.gesture {
        for guide in guides {
            cr.set_source_rgb(ACCENT[0], ACCENT[1], ACCENT[2]);
            cr.set_line_width(1. / scale);
            cr.set_dash(&[4. / scale, 3. / scale], 0.);
            cr.move_to(guide.a.x.into(), guide.a.y.into());
            cr.line_to(guide.b.x.into(), guide.b.y.into());
            cr.stroke()?;
            if guide.spacing {
                let mid =
                    interaction::pt((guide.a.x + guide.b.x) / 2., (guide.a.y + guide.b.y) / 2.);
                cr.set_dash(&[], 0.);
                cr.arc(
                    mid.x.into(),
                    mid.y.into(),
                    3. / scale,
                    0.,
                    std::f64::consts::TAU,
                );
                cr.fill()?;
            }
        }
        cr.set_dash(&[], 0.);
    }
    if state.tool == Tool::Select && state.text_draft.is_none() {
        if let Some(bounds) = state.selection_bounds() {
            cr.rectangle(
                bounds.x.into(),
                bounds.y.into(),
                bounds.width.into(),
                bounds.height.into(),
            );
            cr.set_source_rgba(0.02, 0.04, 0.05, 0.95);
            cr.set_line_width(3. / scale);
            cr.stroke_preserve()?;
            cr.set_source_rgb(ACCENT[0], ACCENT[1], ACCENT[2]);
            cr.set_line_width(1. / scale);
            cr.stroke()?;
            let handles = interaction::handle_points(bounds, (28. / scale) as f32);
            cr.move_to(handles[1].x.into(), handles[1].y.into());
            cr.line_to(handles[8].x.into(), handles[8].y.into());
            cr.stroke()?;
            for (i, p) in handles.iter().enumerate() {
                let r = 4. / scale;
                if i == 8 {
                    cr.arc(p.x.into(), p.y.into(), r, 0., std::f64::consts::TAU);
                } else {
                    cr.rectangle(f64::from(p.x) - r, f64::from(p.y) - r, r * 2., r * 2.);
                }
                cr.set_source_rgb(0.05, 0.07, 0.08);
                cr.fill_preserve()?;
                cr.set_source_rgb(ACCENT[0], ACCENT[1], ACCENT[2]);
                cr.stroke()?;
            }
        }
    }
    cr.restore()?;
    Ok(())
}

fn project_filter() -> gtk::FileFilter {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("SharpShot editable project (*.sharp)"));
    filter.add_pattern("*.sharp");
    filter
}
fn save_project_dialog(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<State>>,
    view: &Rc<View>,
) {
    if state.borrow().text_draft.is_some() {
        view.help
            .set_text("Apply or cancel the caption before saving a project.");
        return;
    }
    let dialog = gtk::FileChooserNative::new(
        Some("Save editable project — contains original screenshot"),
        Some(window),
        gtk::FileChooserAction::Save,
        Some("Save project"),
        Some("Cancel"),
    );
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    dialog.set_current_name(&format!("SharpShot-{seconds}.sharp"));
    dialog.add_filter(&project_filter());
    let state = state.clone();
    let weak_view = Rc::downgrade(view);
    glib::spawn_future_local(async move {
        let response = dialog.run_future().await;
        let path = dialog.file().and_then(|f| f.path());
        dialog.destroy();
        if response != gtk::ResponseType::Accept {
            return;
        }
        let Some(view) = weak_view.upgrade() else {
            return;
        };
        let Some(path) = path else {
            view.help
                .set_text("Choose a local folder to save the project.");
            return;
        };
        let snapshot = match Snapshot::capture(&state.borrow().doc) {
            Ok(s) => s,
            Err(e) => {
                view.help
                    .set_text(&format!("Cannot prepare project: {e:#}"));
                return;
            }
        };
        view.help
            .set_text("Saving editable project (original screenshot included)…");
        drop(view);
        let result = gio::spawn_blocking(move || snapshot.save_new(&path).map(|()| path)).await;
        if let Some(view) = weak_view.upgrade() {
            match result {
                Ok(Ok(path)) => view.help.set_text(&format!(
                    "Saved {} · Contains original pixels; share PNG for redacted images.",
                    path.display()
                )),
                Ok(Err(error)) => view
                    .help
                    .set_text(&format!("Project save failed: {error:#}")),
                Err(_) => view
                    .help
                    .set_text("Project save worker failed. Choose Save project to retry."),
            }
        }
    });
}
fn open_project_dialog(app: &gtk::Application, window: &gtk::ApplicationWindow, view: &Rc<View>) {
    let dialog = gtk::FileChooserNative::new(
        Some("Open editable project"),
        Some(window),
        gtk::FileChooserAction::Open,
        Some("Open project"),
        Some("Cancel"),
    );
    dialog.add_filter(&project_filter());
    let app = app.downgrade();
    let weak_view = Rc::downgrade(view);
    glib::spawn_future_local(async move {
        let response = dialog.run_future().await;
        let path = dialog.file().and_then(|f| f.path());
        dialog.destroy();
        if response != gtk::ResponseType::Accept {
            return;
        }
        let Some(view) = weak_view.upgrade() else {
            return;
        };
        let Some(path) = path else {
            view.help.set_text("Choose a local project file.");
            return;
        };
        view.help.set_text("Opening project…");
        drop(view);
        let result = gio::spawn_blocking(move || Snapshot::load(&path)).await;
        let Some(view) = weak_view.upgrade() else {
            return;
        };
        match result {
            Ok(Ok(snapshot)) => match snapshot.into_document() {
                Ok(doc) => {
                    if let Some(app) = app.upgrade() {
                        build(&app, doc, view.settings.clone());
                        view.help.set_text(
                            "Project opened in a new window. This screenshot is unchanged.",
                        );
                    }
                }
                Err(error) => view.help.set_text(&format!("Invalid project: {error:#}")),
            },
            Ok(Err(error)) => view
                .help
                .set_text(&format!("Cannot open project: {error:#}")),
            Err(_) => view
                .help
                .set_text("Project load worker failed. Choose Open project to retry."),
        }
    });
}

fn export_vector(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<State>>,
    view: &Rc<View>,
    format: crate::vector_export::Format,
) {
    if state.borrow().text_draft.is_some() {
        view.help
            .set_text("Apply or cancel the caption before exporting.");
        return;
    }
    let extension = match format {
        crate::vector_export::Format::Svg => "svg",
        crate::vector_export::Format::Pdf => "pdf",
    };
    let dialog = gtk::FileChooserNative::new(
        Some(&format!("Export {}", extension.to_uppercase())),
        Some(window),
        gtk::FileChooserAction::Save,
        Some("Export"),
        Some("Cancel"),
    );
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    dialog.set_current_name(&format!("SharpShot-{seconds}.{extension}"));
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(extension));
    filter.add_pattern(&format!("*.{extension}"));
    dialog.add_filter(&filter);
    let state = state.clone();
    let weak = Rc::downgrade(view);
    glib::spawn_future_local(async move {
        let response = dialog.run_future().await;
        let path = dialog.file().and_then(|file| file.path());
        dialog.destroy();
        if response != gtk::ResponseType::Accept {
            return;
        }
        let Some(view) = weak.upgrade() else { return };
        let Some(path) = path else {
            view.help
                .set_text("Choose a local folder for vector export.");
            return;
        };
        let snapshot = match Snapshot::capture(&state.borrow().doc) {
            Ok(s) => s,
            Err(error) => {
                view.help
                    .set_text(&format!("Cannot prepare vector export: {error:#}"));
                return;
            }
        };
        view.help.set_text("Exporting sanitised vector document…");
        drop(view);
        let result = gio::spawn_blocking(move || {
            crate::vector_export::save_new(snapshot, &path, format).map(|()| path)
        })
        .await;
        if let Some(view) = weak.upgrade() {
            match result {
                Ok(Ok(path)) => view.help.set_text(&format!("Saved {}", path.display())),
                Ok(Err(error)) => view
                    .help
                    .set_text(&format!("Vector export failed: {error:#}")),
                Err(_) => view
                    .help
                    .set_text("Vector export worker failed. Choose Export to retry."),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_anchor_preserves_an_off_centre_pixel_at_both_scale_limits() {
        let anchor = ZoomAnchor {
            image: (720., 180.),
            cursor: (900., 120.),
        };
        for (scale, expected) in [(0.1, (172., 598.)), (4., (2980., 1300.))] {
            let t = Transform {
                scale,
                x: 1000.,
                y: 700.,
            };
            assert_eq!(anchor.offsets(t), expected);
        }
    }

    #[test]
    fn worker_render_keeps_the_requested_snapshot_and_privacy_masks() -> Result<()> {
        let mut doc = Document::new(image::RgbaImage::from_pixel(
            10,
            10,
            image::Rgba([220, 30, 40, 255]),
        ))?;
        doc.add(crate::document::Mark::redaction(
            1,
            Point { x: 2., y: 2. },
            Point { x: 6., y: 6. },
        ));
        let snapshot = Snapshot::capture(&doc)?;
        assert!(doc.remove_many(&[1]));
        let result = std::thread::spawn(move || render_snapshot(snapshot, RenderAction::Copy))
            .join()
            .expect("render worker panicked")?;
        let RenderedOutput::Copy(image) = result else {
            panic!("expected clipboard image")
        };
        assert_eq!(image.get_pixel(3, 3).0, [0, 0, 0, 255]);
        assert_eq!(image.get_pixel(0, 0).0, [220, 30, 40, 255]);
        assert_eq!(doc.render()?.get_pixel(3, 3).0, [220, 30, 40, 255]);
        Ok(())
    }

    #[test]
    fn eraser_scene_and_worker_output_restore_original_pixels_and_undo_masks() -> Result<()> {
        let original = image::RgbaImage::from_fn(64, 40, |x, y| {
            image::Rgba([x as u8 * 3, y as u8 * 5, 120, 255])
        });
        let mut doc = Document::new(original.clone())?;
        for (id, x) in [(1, 2.), (2, 40.)] {
            doc.add(crate::document::Mark::redaction(
                id,
                Point { x, y: 2. },
                Point { x: x + 10., y: 12. },
            ));
        }
        let masked = doc.render()?;
        let mut state = State::new(doc, std::sync::mpsc::channel().0);
        state.tool = Tool::Eraser;
        let cached_revision = state.doc.revision();
        state.begin(5., 5., false, false, false);
        assert!(state.gesture.is_some());
        assert_ne!(state.doc.revision(), cached_revision);
        assert!(!dynamic_scene(&state));
        assert_eq!(
            state.doc.render()?.get_pixel(5, 5),
            original.get_pixel(5, 5)
        );
        state.update(40., 0., false, false);
        assert_eq!(state.doc.render()?, original);
        state.finish();
        let RenderedOutput::Copy(erased) =
            render_snapshot(Snapshot::capture(&state.doc)?, RenderAction::AutoCopy)?
        else {
            panic!("expected clipboard image")
        };
        assert_eq!(erased, original);
        state.doc.undo();
        let RenderedOutput::Copy(restored) =
            render_snapshot(Snapshot::capture(&state.doc)?, RenderAction::Copy)?
        else {
            panic!("expected clipboard image")
        };
        assert_eq!(restored, masked);
        Ok(())
    }

    fn context_state() -> Result<State> {
        let mut doc = Document::new(image::RgbaImage::new(160, 120))?;
        for (id, x) in [(1, 10.), (2, 60.), (3, 110.)] {
            doc.add(crate::document::Mark::redaction(
                id,
                Point { x, y: 10. },
                Point { x: x + 25., y: 35. },
            ));
        }
        Ok(State::new(doc, std::sync::mpsc::channel().0))
    }

    #[test]
    fn context_click_preserves_a_selection_or_targets_the_clicked_group() -> Result<()> {
        let mut s = context_state()?;
        s.selected = vec![1, 2];
        select_context_target(&mut s, 15., 15.);
        assert_eq!(s.selected, [1, 2]);
        select_context_target(&mut s, 115., 15.);
        assert_eq!(s.selected, [3]);
        s.doc.group_marks(&[1, 2]);
        select_context_target(&mut s, 65., 15.);
        assert_eq!(s.selected, [1, 2]);
        s.transform = Transform {
            scale: 2.,
            x: 30.,
            y: 40.,
        };
        select_context_target(&mut s, 260., 70.);
        assert_eq!(s.selected, [3]);
        select_context_target(&mut s, 32., 200.);
        assert!(s.selected.is_empty());
        Ok(())
    }

    #[test]
    fn layer_menu_accounts_for_noncontiguous_and_complete_selections() -> Result<()> {
        let mut s = context_state()?;
        for (ids, expected) in [
            (vec![1], (true, false)),
            (vec![2], (true, true)),
            (vec![3], (false, true)),
            (vec![1, 3], (true, true)),
            (vec![1, 2, 3], (false, false)),
            (vec![], (false, false)),
        ] {
            s.selected = ids;
            assert_eq!(layer_moves(&s), expected);
        }
        s.doc.marks[1].kind = MarkKind::Step { number: 1 };
        for (ids, expected) in [
            (vec![2], (false, false)),
            (vec![1], (true, false)),
            (vec![1, 3], (false, false)),
            (vec![1, 2], (true, false)),
        ] {
            s.selected = ids;
            assert_eq!(layer_moves(&s), expected);
        }
        Ok(())
    }
}
