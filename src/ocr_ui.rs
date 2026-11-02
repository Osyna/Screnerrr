//! Selectable text layer over the canvas, fed by the local OCR engine (`ocr.rs`).
//!
//! The engine runs once per original screenshot on a worker thread, started by the OCR toggle; the
//! result reaches the UI through an async channel. Boxes are cached in image coordinates, so zoom,
//! crop and output styling never move them. Which glyphs may be shown, selected or copied is
//! checked against the live document (`Document::privacy_snapshot` and the crop) on every use; the
//! result is reused only while the page, crop, size and privacy snapshot are all unchanged, so a
//! mask drawn after recognition immediately hides the text under it and no recognised text is ever
//! stored in the project.

use crate::{
    chrome::{self, Icon},
    document::{Bounds, Crop, Document, Mark, PrivacySnapshot},
    interaction::State,
    ocr::{self, Engine, Page, Pt, Quad},
    ocr_table::{self, Scene},
    shapes::Point,
    smart_redact::{PAD_X as MASK_PAD_X, PAD_Y as MASK_PAD_Y, Review},
};
use anyhow::{Result, anyhow};
use gtk::{cairo::Context, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// Pixels of pointer travel before a press becomes a drag selection instead of a click.
const DRAG_SLOP: f64 = 3.0;
const HIGHLIGHT_ALPHA: f64 = 0.28;
/// A press counts as on text within this fraction of the line height from the nearest glyph.
const CLICK_NEAR: f64 = 0.6;
const READY_HELP: &str = "Text layer ready · drag to select · click a word, click it again for its line · Ctrl+C copies · Ctrl+A selects all";

enum Job {
    Idle,
    Running,
    Ready(Rc<Page>),
}

#[derive(Clone, Copy)]
struct Selection {
    anchor: usize,
    focus: usize,
}

impl Selection {
    fn range(self) -> (usize, usize) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }
}

struct Drag {
    start: (f64, f64),
    moved: bool,
    anchor: usize,
    /// Glyph under a press close enough to the text to count as a click on it.
    click: Option<usize>,
    previous: Option<(usize, usize)>,
}

#[derive(Clone, Copy)]
pub(crate) enum Markup {
    Highlight,
    Underline,
    Strike,
    Redact,
    Pixelate,
}

/// Visibility of the glyphs of one page, valid while its key still matches the document.
struct Cached {
    page: Rc<Page>,
    privacy: Rc<PrivacySnapshot>,
    crop: Option<Crop>,
    size: (u32, u32),
    vis: Rc<Vec<bool>>,
}

#[derive(Default)]
struct VisCache(Option<Cached>);

impl VisCache {
    /// The visibility for `page` against the live `doc`; recomputed when anything it depends on
    /// changed. The privacy snapshot is taken (and validated against the marks) on every call.
    fn get(&mut self, doc: &Document, page: &Rc<Page>) -> Rc<Vec<bool>> {
        let privacy = doc.privacy_snapshot();
        let size = (doc.width, doc.height);
        if let Some(c) = &self.0
            && Rc::ptr_eq(&c.page, page)
            && Rc::ptr_eq(&c.privacy, &privacy)
            && c.crop == doc.crop
            && c.size == size
        {
            return c.vis.clone();
        }
        let vis = Rc::new(compute(doc, page, &privacy));
        self.0 = Some(Cached {
            page: page.clone(),
            privacy,
            crop: doc.crop,
            size,
            vis: vis.clone(),
        });
        vis
    }
}

pub struct OcrUi {
    /// Contextual copy and markup actions; the toggle lives in the main tool strip.
    pub toolbar: gtk::Box,
    area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    help: gtk::Label,
    pub toggle: gtk::ToggleButton,
    actions: Vec<gtk::Button>,
    job: RefCell<Job>,
    selection: Cell<Option<Selection>>,
    drag: RefCell<Option<Drag>>,
    changed: Rc<dyn Fn()>,
    vis: RefCell<VisCache>,
    /// Smart-redaction review window while one is open.
    review: RefCell<Option<Rc<Review>>>,
    /// Table preview window while one is open.
    table: RefCell<Option<glib::WeakRef<gtk::Window>>>,
}

fn point(p: Pt) -> Point {
    Point {
        x: p.0 as f32,
        y: p.1 as f32,
    }
}

fn trace(cr: &Context, q: &Quad) {
    cr.move_to(q[0].0, q[0].1);
    for p in &q[1..] {
        cr.line_to(p.0, p.1);
    }
    cr.close_path();
}

/// Glyphs that may be shown: outside every privacy mask (with a margin for alignment slack) and
/// entirely inside the crop. Recomputed from the live document each time.
pub(crate) fn visible(state: &State, page: &Page) -> Vec<bool> {
    compute(&state.doc, page, &state.doc.privacy_snapshot())
}

fn compute(doc: &Document, page: &Page, privacy: &PrivacySnapshot) -> Vec<bool> {
    let crop = doc.crop.unwrap_or(Crop {
        x: 0,
        y: 0,
        width: doc.width,
        height: doc.height,
    });
    let (x0, y0) = (f64::from(crop.x) - 0.5, f64::from(crop.y) - 0.5);
    let (x1, y1) = (
        f64::from(crop.x + crop.width) + 0.5,
        f64::from(crop.y + crop.height) + 0.5,
    );
    page.visibility(|padded, exact| {
        exact.x >= x0
            && exact.y >= y0
            && exact.x + exact.w <= x1
            && exact.y + exact.h <= y1
            && !privacy.is_private(Bounds {
                x: padded.x as f32,
                y: padded.y as f32,
                width: padded.w as f32,
                height: padded.h as f32,
            })
    })
}

impl OcrUi {
    /// `changed` runs after this layer changed the document (text markup, smart redaction), always
    /// with no `State` borrow held; the editor passes its refresh so the scene layer redraws.
    pub fn new(
        area: &gtk::DrawingArea,
        state: Rc<RefCell<State>>,
        help: &gtk::Label,
        changed: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let toggle = gtk::ToggleButton::builder()
            .child(&chrome::icon(Icon::Ocr))
            .tooltip_text("Recognise text (OCR) · F8 · runs locally on the original screenshot; then drag to select, Ctrl+C copies, Ctrl+A selects all")
            .build();
        toggle.add_css_class("tool-button");
        toggle.update_property(&[gtk::accessible::Property::Label("Recognise text")]);
        let make = |icon, name: &str, tip: &str| {
            let b = chrome::button(icon, name, tip);
            b.set_sensitive(false);
            toolbar.append(&b);
            b
        };
        let copy = make(
            Icon::Copy,
            "Copy recognised text",
            "Copy the selected text, or all recognised text when nothing is selected · Ctrl+C",
        );
        let table = make(
            Icon::Table,
            "Copy as table",
            "Arrange the recognised text as a table (the selection, or all visible text): preview and adjust the rows and columns, then copy as spreadsheet text, CSV or Markdown",
        );
        let highlight = make(
            Icon::Highlight,
            "Highlight selected text",
            "Highlight the selected text with a translucent marker (editable object)",
        );
        let underline = make(
            Icon::Underline,
            "Underline selected text",
            "Underline the selected text (editable object)",
        );
        let strike = make(
            Icon::Strike,
            "Strike through selected text",
            "Strike through the selected text (editable object)",
        );
        let smart = chrome::button(
            Icon::Redact,
            "Smart redaction",
            "Find emails, phone numbers, IP addresses, card and account numbers or your own text in the recognised text, review every match on the canvas, then cover the ones you choose with masks (one undo)",
        );
        toolbar.append(&smart);
        let this = Rc::new(Self {
            toolbar,
            area: area.clone(),
            state,
            help: help.clone(),
            toggle: toggle.clone(),
            actions: vec![
                copy.clone(),
                table.clone(),
                highlight.clone(),
                underline.clone(),
                strike.clone(),
            ],
            job: RefCell::new(Job::Idle),
            selection: Cell::new(None),
            drag: RefCell::new(None),
            changed,
            vis: RefCell::default(),
            review: RefCell::new(None),
            table: RefCell::new(None),
        });
        toggle.connect_toggled(glib::clone!(
            #[weak]
            this,
            move |t| this.toggled(t.is_active())
        ));
        copy.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |_| this.copy_text()
        ));
        for (button, kind) in [
            (highlight, Markup::Highlight),
            (underline, Markup::Underline),
            (strike, Markup::Strike),
        ] {
            button.connect_clicked(glib::clone!(
                #[weak]
                this,
                move |_| this.markup(kind)
            ));
        }
        table.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |b| match b
                .root()
                .and_then(|r| r.downcast::<gtk::ApplicationWindow>().ok())
            {
                Some(window) => this.table_preview(&window),
                None => this
                    .help
                    .set_text("The table view needs the editor window."),
            }
        ));
        smart.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |b| match b
                .root()
                .and_then(|r| r.downcast::<gtk::ApplicationWindow>().ok())
            {
                Some(window) => this.smart_redact(&window),
                None => this
                    .help
                    .set_text("Smart redaction needs the editor window."),
            }
        ));
        this
    }

    pub fn enabled(&self) -> bool {
        self.toggle.is_active()
    }

    /// Selects every visible recognised glyph (Ctrl+A while the layer is on).
    pub fn select_all(&self) {
        let Some((page, vis)) = self.snapshot() else {
            return;
        };
        let Some(first) = vis.iter().position(|&v| v) else {
            return;
        };
        let last = vis.iter().rposition(|&v| v).map_or(first, |i| i + 1);
        self.selection.set(Some(Selection {
            anchor: first,
            focus: last,
        }));
        self.help.set_text(&format!(
            "All text selected · {} characters · Ctrl+C copies",
            page.text(first, last, &vis)
                .chars()
                .filter(|c| !c.is_whitespace())
                .count()
        ));
        self.area.queue_draw();
    }

    /// Clears the selection and turns the layer off so the canvas tools work again.
    pub fn cancel(&self) {
        self.selection.set(None);
        *self.drag.borrow_mut() = None;
        if self.toggle.is_active() {
            self.toggle.set_active(false);
        }
        self.area.queue_draw();
    }

    fn sync(&self) {
        let ready = self.toggle.is_active() && matches!(*self.job.borrow(), Job::Ready(_));
        for b in &self.actions {
            b.set_sensitive(ready);
        }
        self.area.queue_draw();
    }

    fn toggled(self: &Rc<Self>, on: bool) {
        if !on {
            self.selection.set(None);
            *self.drag.borrow_mut() = None;
            self.help.set_text("Text layer off");
            return self.sync();
        }
        let usable = match &*self.job.borrow() {
            Job::Ready(page) => {
                let state = self.state.borrow();
                (page.width, page.height) == (state.doc.width, state.doc.height)
            }
            _ => false,
        };
        let running = matches!(*self.job.borrow(), Job::Running);
        if usable {
            self.help.set_text(READY_HELP);
        } else if running {
            self.help.set_text("Recognising text…");
        } else {
            self.start();
        }
        self.sync();
    }

    /// Reads the original pixels once and recognises them off the UI thread.
    fn start(self: &Rc<Self>) {
        let image = match self.state.borrow().doc.background_image() {
            Ok(image) => image,
            Err(error) => return self.failed(&format!("{error:#}")),
        };
        let (tx, rx) = async_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("ocr".into())
            .spawn(move || {
                let result = Engine::new()
                    .and_then(|mut engine| engine.recognize(&image))
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send_blocking(result);
            });
        if let Err(error) = spawned {
            return self.failed(&error.to_string());
        }
        *self.job.borrow_mut() = Job::Running;
        self.help
            .set_text("Recognising text locally… the screenshot never leaves this computer.");
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            let Some(this) = weak.upgrade() else { return };
            match result {
                Ok(page) => this.ready(page),
                Err(error) => this.failed(&error),
            }
        });
    }

    fn ready(&self, page: Page) {
        let message = if page.is_empty() {
            "No text found in this screenshot.".to_string()
        } else {
            format!(
                "{} text lines found · {}",
                page.lines().len(),
                READY_HELP.trim_start_matches("Text layer ready · ")
            )
        };
        let page = Rc::new(page);
        *self.job.borrow_mut() = Job::Ready(page.clone());
        self.help.set_text(&message);
        self.sync();
        let review = self.review.borrow().clone();
        if let Some(review) = review {
            review.ready(page);
        }
    }

    fn failed(&self, error: &str) {
        *self.job.borrow_mut() = Job::Idle;
        // Untoggling reports "off"; the failure message must come after it.
        if self.toggle.is_active() {
            self.toggle.set_active(false);
        }
        self.help
            .set_text(&format!("Text recognition failed: {error}"));
        self.help.set_tooltip_text(Some(error));
        self.sync();
        let review = self.review.borrow().clone();
        if let Some(review) = review {
            review.failed(error);
        }
    }

    /// The cached page with the glyphs that may be used right now.
    fn snapshot(&self) -> Option<(Rc<Page>, Rc<Vec<bool>>)> {
        let Job::Ready(page) = &*self.job.borrow() else {
            return None;
        };
        let state = self.state.try_borrow().ok()?;
        if (page.width, page.height) != (state.doc.width, state.doc.height) {
            return None;
        }
        let vis = self.vis.borrow_mut().get(&state.doc, page);
        Some((page.clone(), vis))
    }

    fn image_point(&self, x: f64, y: f64) -> Pt {
        let p = self.state.borrow().transform.raw(x, y);
        (f64::from(p.x), f64::from(p.y))
    }

    pub fn begin(&self, x: f64, y: f64, shift: bool) {
        let Some((page, vis)) = self.snapshot() else {
            return;
        };
        let hit = page.hit(self.image_point(x, y), &vis);
        let previous = self
            .selection
            .get()
            .map(Selection::range)
            .filter(|r| r.0 != r.1);
        let anchor = match (shift, self.selection.get(), hit) {
            (true, Some(s), Some(h)) => {
                self.selection.set(Some(Selection {
                    anchor: s.anchor,
                    focus: h.caret,
                }));
                self.area.queue_draw();
                Some((s.anchor, true))
            }
            (_, _, Some(h)) => Some((h.caret, false)),
            _ => None,
        };
        *self.drag.borrow_mut() = Some(Drag {
            start: (x, y),
            moved: anchor.is_some_and(|a| a.1),
            anchor: anchor.map_or(0, |a| a.0),
            click: hit
                .filter(|h| h.distance <= CLICK_NEAR * h.line_height)
                .map(|h| h.ordinal),
            previous,
        });
    }

    pub fn update(&self, x: f64, y: f64) {
        {
            let mut drag = self.drag.borrow_mut();
            let Some(d) = drag.as_mut() else { return };
            if !d.moved && (x - d.start.0).hypot(y - d.start.1) < DRAG_SLOP {
                return;
            }
            d.moved = true;
        }
        let Some((page, vis)) = self.snapshot() else {
            return;
        };
        let Some(hit) = page.hit(self.image_point(x, y), &vis) else {
            return;
        };
        let anchor = self.drag.borrow().as_ref().map_or(hit.caret, |d| d.anchor);
        self.selection.set(Some(Selection {
            anchor,
            focus: hit.caret,
        }));
        self.area.queue_draw();
    }

    pub fn finish(&self) {
        let Some(drag) = self.drag.borrow_mut().take() else {
            return;
        };
        let Some((page, vis)) = self.snapshot() else {
            return;
        };
        if !drag.moved {
            // A click selects the word under it; clicking that word again takes its whole line.
            self.selection.set(drag.click.and_then(|ordinal| {
                let word = page.word(ordinal, &vis);
                let range = if drag.previous == Some(word) {
                    page.line_range(ordinal)
                } else {
                    word
                };
                (range.0 != range.1).then_some(Selection {
                    anchor: range.0,
                    focus: range.1,
                })
            }));
        }
        let selected = self
            .selection
            .get()
            .map(Selection::range)
            .map_or(0, |(a, b)| {
                page.text(a, b, &vis)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .count()
            });
        if selected > 0 {
            self.help.set_text(&format!("{selected} characters selected · Ctrl+C copies · highlight, underline or strike through with the toolbar"));
        } else {
            self.selection.set(None);
        }
        self.area.queue_draw();
    }

    /// Copies the selected text, or everything recognised (and visible) when nothing is selected.
    pub fn copy_text(&self) {
        let Some((page, vis)) = self.snapshot() else {
            return;
        };
        let (range, whole) = match self
            .selection
            .get()
            .map(Selection::range)
            .filter(|r| r.0 != r.1)
        {
            Some(r) => (r, false),
            None => ((0, page.len()), true),
        };
        let text = page.text(range.0, range.1, &vis);
        if text.is_empty() {
            return self.help.set_text("No visible recognised text to copy.");
        }
        self.area.clipboard().set_text(&text);
        self.help.set_text(&format!(
            "Copied {} characters of {} text to the clipboard.",
            text.chars().count(),
            if whole { "all recognised" } else { "selected" }
        ));
    }

    pub(crate) fn markup(&self, kind: Markup) {
        let Some((page, vis)) = self.snapshot() else {
            return;
        };
        let Some((lo, hi)) = live_range(&page, &vis, self.selection.get()) else {
            return self.help.set_text("Select some recognised text first.");
        };
        let runs = page.runs(lo, hi, &vis);
        if runs.is_empty() {
            return self.help.set_text("Nothing visible is selected.");
        }
        let added = {
            let mut state = self.state.borrow_mut();
            let color = state.color;
            let marks = markup_marks(&mut state.doc, color, &runs, kind);
            state.doc.add_many(marks)
        };
        if !added {
            return self
                .help
                .set_text("Could not add the text markup (too many objects?).");
        }
        if matches!(kind, Markup::Redact | Markup::Pixelate) {
            // The covered glyphs are no longer visible, so the range would only be stale.
            self.selection.set(None);
        }
        self.area.queue_draw();
        self.help.set_text(match kind {
            Markup::Highlight => {
                "Highlight added — a normal object: select, recolour, move or delete it."
            }
            Markup::Underline => {
                "Underline added — a normal object: select, recolour, move or delete it."
            }
            Markup::Strike => {
                "Strike-through added — a normal object: select, recolour, move or delete it."
            }
            Markup::Redact => {
                "Redaction added over the selected text — a normal object: move or delete it."
            }
            Markup::Pixelate => {
                "Pixelation added over the selected text — a normal object: move or delete it."
            }
        });
        (self.changed)();
    }

    /// Settles the selection for a right-click at canvas point `at` (`None`: keyboard, keep the
    /// current range) and reports what the context menu may offer, from the live visibility.
    pub(crate) fn prepare_context(&self, at: Option<(f64, f64)>) -> TextContext {
        if !self.enabled() {
            return TextContext::default();
        }
        let Some((page, vis)) = self.snapshot() else {
            return TextContext::default();
        };
        let at = at.map(|(x, y)| self.image_point(x, y));
        let next = context_selection(&page, &vis, self.selection.get(), at);
        self.selection.set(next);
        *self.drag.borrow_mut() = None;
        self.area.queue_draw();
        TextContext {
            ready: true,
            has_text: !page.runs(0, page.len(), &vis).is_empty(),
            selected: next.is_some(),
        }
    }

    /// Extends the selection to the whole visible line it starts on.
    pub(crate) fn select_line(&self) {
        let Some((page, vis)) = self.snapshot() else {
            return;
        };
        let Some(line) = line_selection(&page, &vis, self.selection.get()) else {
            return self.help.set_text("Select some recognised text first.");
        };
        self.selection.set(Some(line));
        self.area.queue_draw();
        self.help
            .set_text("Line selected · Ctrl+C copies · highlight, underline or strike through with the toolbar");
    }

    /// Drops the selection; the text layer stays on.
    pub(crate) fn clear_selection(&self) {
        self.selection.set(None);
        *self.drag.borrow_mut() = None;
        self.area.queue_draw();
    }

    /// The recognised page, when it belongs to the current document.
    fn usable_page(&self) -> Option<Rc<Page>> {
        let Job::Ready(page) = &*self.job.borrow() else {
            return None;
        };
        let state = self.state.try_borrow().ok()?;
        ((page.width, page.height) == (state.doc.width, state.doc.height)).then(|| page.clone())
    }

    /// Opens the reviewed smart-redaction window. Recognition is requested (and its progress shown
    /// in the window) when the text layer is not ready; the list appears once it is, or after a
    /// retry. Nothing changes the image until the user applies.
    pub fn smart_redact(self: &Rc<Self>, parent: &gtk::ApplicationWindow) {
        let open = self.review.borrow().clone();
        if let Some(review) = open {
            return review.present();
        }
        let review = Review::new(self, self.state.clone(), self.area.clone(), parent);
        *self.review.borrow_mut() = Some(review.clone());
        self.review_retry();
        review.present();
    }

    /// Lists matches now, or starts recognition and waits for it.
    pub(crate) fn review_retry(self: &Rc<Self>) {
        let review = self.review.borrow().clone();
        let Some(review) = review else { return };
        match self.usable_page() {
            Some(page) => review.ready(page),
            None => {
                review.waiting();
                let running = matches!(*self.job.borrow(), Job::Running);
                if !running {
                    self.start();
                }
            }
        }
    }

    pub(crate) fn review_closed(&self) {
        *self.review.borrow_mut() = None;
        self.area.queue_draw();
    }

    /// True while `page` is still this document's recognised page.
    pub(crate) fn is_current(&self, page: &Rc<Page>) -> bool {
        self.usable_page().is_some_and(|p| Rc::ptr_eq(&p, page))
    }

    /// Masks were added by the review; call with no `State` borrow held.
    pub(crate) fn review_applied(&self, added: usize, skipped: usize) {
        self.area.queue_draw();
        let skipped = if skipped > 0 {
            format!(" ({skipped} no longer visible, skipped)")
        } else {
            String::new()
        };
        self.help.set_text(&format!(
            "{added} redaction{} added{skipped} · one undo removes them all.",
            if added == 1 { "" } else { "s" }
        ));
        (self.changed)();
    }

    /// Shows `text` in the editor's status line.
    pub(crate) fn say(&self, text: &str) {
        self.help.set_text(text);
    }

    /// The table words the live document allows right now for the glyph ordinals `range`.
    pub(crate) fn table_scene(&self, range: (usize, usize)) -> Scene {
        if self.state.try_borrow().is_err() {
            return Scene::Busy;
        }
        match self.snapshot() {
            Some((page, vis)) => Scene::Words(ocr_table::words(&page, &vis, range)),
            None => Scene::Gone,
        }
    }

    /// Opens the table preview over `parent` for the selected text, or for all visible text when
    /// nothing is selected. Nothing is copied until the user asks; the preview closes by itself
    /// when a mask, crop or new image changes the text it shows.
    pub fn table_preview(self: &Rc<Self>, parent: &gtk::ApplicationWindow) {
        let Some((page, vis)) = self.snapshot() else {
            return self.help.set_text(
                "Turn on the text layer (F8) and wait for recognition before making a table.",
            );
        };
        let range = live_range(&page, &vis, self.selection.get()).unwrap_or((0, page.len()));
        let words = ocr_table::words(&page, &vis, range);
        if words.is_empty() {
            return self
                .help
                .set_text("No visible recognised text to arrange as a table.");
        }
        let old = self.table.borrow_mut().take().and_then(|w| w.upgrade());
        if let Some(old) = old {
            old.close();
        }
        let window = ocr_table::open(self, parent, words, range);
        *self.table.borrow_mut() = Some(window.downgrade());
        self.help
            .set_text("Table preview open · adjust rows and columns, then copy.");
    }

    /// Draws the text layer; `cr` is in image coordinates, after the scene.
    pub fn paint(&self, cr: &Context, state: &State) -> Result<()> {
        self.paint_text(cr, state)?;
        let review = self.review.try_borrow().ok().and_then(|r| r.clone());
        if let Some(review) = review {
            review.paint(cr, state).map_err(|e| anyhow!("{e}"))?;
        }
        Ok(())
    }

    fn paint_text(&self, cr: &Context, state: &State) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        let Job::Ready(page) = &*self.job.borrow() else {
            return Ok(());
        };
        if (page.width, page.height) != (state.doc.width, state.doc.height) {
            return Ok(());
        }
        let vis = self.vis.borrow_mut().get(&state.doc, page);
        let hairline = 1.0 / state.transform.scale.max(1e-6);
        cr.save().map_err(|e| anyhow!("{e}"))?;
        cr.set_line_width(hairline);
        cr.set_source_rgba(0.35, 0.65, 1.0, 0.5);
        for run in page.runs(0, page.len(), &vis) {
            trace(cr, &run.quad);
            cr.stroke().map_err(|e| anyhow!("{e}"))?;
        }
        if let Some((lo, hi)) = self.selection.get().map(Selection::range) {
            cr.set_source_rgba(0.2, 0.5, 1.0, 0.38);
            for run in page.runs(lo, hi, &vis) {
                trace(cr, &run.quad);
                cr.fill().map_err(|e| anyhow!("{e}"))?;
            }
        }
        cr.restore().map_err(|e| anyhow!("{e}"))?;
        Ok(())
    }
}

fn lerp(a: Pt, b: Pt, t: f64) -> Pt {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// What the right-click menu may offer for the text layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextContext {
    pub ready: bool,
    pub has_text: bool,
    pub selected: bool,
}

/// The selected range when it still covers visible text (privacy and crop may have hidden it).
fn live_range(page: &Page, vis: &[bool], current: Option<Selection>) -> Option<(usize, usize)> {
    current
        .map(Selection::range)
        .filter(|&(lo, hi)| lo != hi && !page.runs(lo, hi, vis).is_empty())
}

/// Selection after a right-click at image point `at`: inside the current range it is kept whole,
/// on another word it becomes that word, anywhere else (blank, masked, cropped) it is cleared.
/// `None` is the keyboard: the current range is kept while it is still visible.
fn context_selection(
    page: &Page,
    vis: &[bool],
    current: Option<Selection>,
    at: Option<Pt>,
) -> Option<Selection> {
    let live = live_range(page, vis, current);
    let keep = current.filter(|_| live.is_some());
    let Some(at) = at else { return keep };
    let hit = page
        .hit(at, vis)
        .filter(|h| h.distance <= CLICK_NEAR * h.line_height)?;
    if live.is_some_and(|(lo, hi)| (lo..hi).contains(&hit.ordinal)) {
        return keep;
    }
    let (a, b) = page.word(hit.ordinal, vis);
    (a != b).then_some(Selection {
        anchor: a,
        focus: b,
    })
}

/// The whole line of the first visible selected glyph.
fn line_selection(page: &Page, vis: &[bool], current: Option<Selection>) -> Option<Selection> {
    let (lo, hi) = live_range(page, vis, current)?;
    let first = (lo..hi).find(|&o| vis.get(o).copied().unwrap_or(false))?;
    let (a, b) = page.line_range(first);
    Some(Selection {
        anchor: a,
        focus: b,
    })
}

/// One normal, source-independent mark per run. Masks are padded like smart redaction's so the
/// covered glyphs count as private (see `ocr_ui::compute`) and are whole-pixel, inside the image.
fn markup_marks(doc: &mut Document, color: [f64; 4], runs: &[ocr::Run], kind: Markup) -> Vec<Mark> {
    let (width, height) = (f64::from(doc.width), f64::from(doc.height));
    runs.iter()
        .map(|run| {
            let id = doc.id();
            let q = &run.quad;
            let h = ((q[3].0 - q[0].0).hypot(q[3].1 - q[0].1)).max(1.0);
            let across = |v: f64| [point(lerp(q[0], q[3], v)), point(lerp(q[1], q[2], v))];
            let b = ocr::rect_of(q);
            match kind {
                Markup::Highlight => {
                    let tint = [color[0], color[1], color[2], HIGHLIGHT_ALPHA];
                    Mark::highlight(id, point((b.x, b.y)), point((b.x + b.w, b.y + b.h)), tint)
                }
                Markup::Underline => Mark::path(
                    id,
                    across(0.94).to_vec(),
                    false,
                    color,
                    (h * 0.08).clamp(2.0, 8.0),
                ),
                Markup::Strike => Mark::path(
                    id,
                    across(0.55).to_vec(),
                    false,
                    color,
                    (h * 0.08).clamp(2.0, 8.0),
                ),
                Markup::Redact | Markup::Pixelate => {
                    let a = point((
                        (b.x - MASK_PAD_X * h).floor().max(0.0),
                        (b.y - MASK_PAD_Y * h).floor().max(0.0),
                    ));
                    let c = point((
                        (b.x + b.w + MASK_PAD_X * h).ceil().min(width),
                        (b.y + b.h + MASK_PAD_Y * h).ceil().min(height),
                    ));
                    if matches!(kind, Markup::Redact) {
                        Mark::redaction(id, a, c)
                    } else {
                        Mark::privacy_blur(id, a, c)
                    }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document::{Affine, Document, MarkKind},
        ocr::{Glyph, Line},
    };

    fn line(x: f64, y: f64, text: &str) -> Line {
        let n = text.chars().count() as f64;
        Line {
            quad: [
                (x, y),
                (x + 10.0 * n, y),
                (x + 10.0 * n, y + 14.0),
                (x, y + 14.0),
            ],
            glyphs: text
                .chars()
                .enumerate()
                .map(|(i, c)| Glyph {
                    ch: c.to_string(),
                    score: 0.9,
                    u0: i as f64 / n,
                    u1: (i + 1) as f64 / n,
                })
                .collect(),
        }
    }

    /// Masks drawn after recognition and crops chosen later hide text at once; undo brings it back.
    #[test]
    fn live_document_hides_masked_and_cropped_text() -> Result<()> {
        let page = Page::new(
            400,
            300,
            vec![
                line(10.0, 10.0, "Hello world"),
                line(10.0, 60.0, "Second line"),
            ],
        );
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut state = State::new(Document::new(image::RgbaImage::new(400, 300))?, tx);
        let all = |state: &State| {
            let vis = visible(state, &page);
            page.text(0, page.len(), &vis)
        };
        assert_eq!(all(&state), "Hello world\n\nSecond line");

        let id = state.doc.id();
        state.doc.add(Mark::redaction(
            id,
            Point { x: 10.0, y: 5.0 },
            Point { x: 60.0, y: 30.0 },
        ));
        let text = all(&state);
        assert_eq!(
            text, "world\n\nSecond line",
            "only glyphs clear of the mask remain"
        );

        state.doc.set_crop(Some(Crop {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        }));
        assert_eq!(
            all(&state),
            "wor",
            "second line and the cut-off glyphs are outside the crop"
        );

        state.doc.undo();
        state.doc.undo();
        assert_eq!(all(&state), "Hello world\n\nSecond line");
        Ok(())
    }

    /// The cache is reused only while page, crop, size and the actual mask geometry are unchanged;
    /// every kind of change is seen by the next lookup, including edits made straight to the marks.
    #[test]
    fn visibility_cache_follows_page_crop_and_every_mask_change() -> Result<()> {
        let page = Rc::new(Page::new(400, 300, vec![line(10.0, 10.0, "Hello world")]));
        let mut doc = Document::new(image::RgbaImage::new(400, 300))?;
        let mut cache = VisCache::default();
        let shown = |v: &Rc<Vec<bool>>| v.iter().filter(|&&v| v).count();

        let all = cache.get(&doc, &page);
        assert_eq!(shown(&all), 11);
        assert!(
            Rc::ptr_eq(&all, &cache.get(&doc, &page)),
            "an unchanged document reuses the result"
        );

        let other = Rc::new(Page::new(400, 300, vec![line(10.0, 10.0, "Hello")]));
        assert_eq!(
            cache.get(&doc, &other).len(),
            5,
            "a replaced page is recomputed"
        );
        assert_eq!(cache.get(&doc, &page).len(), 11);

        // Two masks with a gap between them: the glyphs in the gap stay visible.
        let (a, b) = (doc.id(), doc.id() + 1);
        doc.add(Mark::redaction(
            a,
            Point { x: 10.0, y: 5.0 },
            Point { x: 30.0, y: 30.0 },
        ));
        doc.add(Mark::redaction(
            b,
            Point { x: 60.0, y: 5.0 },
            Point { x: 80.0, y: 30.0 },
        ));
        let gap = cache.get(&doc, &page);
        let flags: Vec<bool> = gap.to_vec();
        assert_eq!(
            flags,
            [
                false, false, false, true, false, false, false, false, true, true, true
            ],
            "text between disjoint masks is not swallowed by their union"
        );

        assert!(doc.move_marks(&[b], Point { x: 200.0, y: 0.0 }));
        assert_eq!(
            shown(&cache.get(&doc, &page)),
            8,
            "a moved mask uncovers its text"
        );

        // Writing the public marks directly must not leave a stale answer either.
        doc.marks[1].transform = Affine::translation(Point { x: 0.0, y: 0.0 });
        assert_eq!(cache.get(&doc, &page).to_vec(), flags);

        assert!(doc.remove_many(&[b]));
        assert_eq!(shown(&cache.get(&doc, &page)), 8);
        doc.undo();
        assert_eq!(
            cache.get(&doc, &page).to_vec(),
            flags,
            "undo restores the mask"
        );

        doc.set_crop(Some(Crop {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        }));
        assert_eq!(
            shown(&cache.get(&doc, &page)),
            2,
            "the crop cuts off the right half"
        );
        doc.set_crop(None);
        assert_eq!(cache.get(&doc, &page).to_vec(), flags);
        Ok(())
    }

    fn sel(a: usize, b: usize) -> Option<Selection> {
        Some(Selection {
            anchor: a,
            focus: b,
        })
    }

    fn range(s: Option<Selection>) -> Option<(usize, usize)> {
        s.map(Selection::range)
    }

    /// "Hello world" at x 10..120, y 10..24; glyph `n` spans x 10+10n..20+10n.
    fn hello() -> Page {
        Page::new(400, 300, vec![line(10.0, 10.0, "Hello world")])
    }

    #[test]
    fn right_click_keeps_a_range_selects_another_word_and_clears_blank_space() {
        let page = hello();
        let vis = vec![true; page.len()];
        let at = |x: f64| Some((x, 17.0));

        let kept = context_selection(&page, &vis, sel(0, 11), at(25.0));
        assert_eq!(range(kept), Some((0, 11)), "inside the range: whole range");
        let kept = context_selection(&page, &vis, sel(0, 11), at(65.0));
        assert_eq!(
            range(kept),
            Some((0, 11)),
            "the space inside it is part of it"
        );

        let other = context_selection(&page, &vis, sel(0, 5), at(85.0));
        assert_eq!(
            range(other),
            Some((6, 11)),
            "another word replaces the range"
        );
        let fresh = context_selection(&page, &vis, None, at(25.0));
        assert_eq!(range(fresh), Some((0, 5)), "no range: the word under it");

        let space = context_selection(&page, &vis, None, at(65.0));
        assert!(space.is_none(), "a space is not a word");
        for blank in [(300.0, 200.0), (50.0, 60.0), (130.0, 17.0)] {
            let cleared = context_selection(&page, &vis, sel(0, 11), Some(blank));
            assert!(cleared.is_none(), "{blank:?} is too far from the text");
        }
        let keyboard = context_selection(&page, &vis, sel(0, 5), None);
        assert_eq!(range(keyboard), Some((0, 5)), "keyboard keeps the range");
        assert!(context_selection(&page, &vis, None, None).is_none());
    }

    #[test]
    fn masked_and_cropped_text_cannot_be_the_target_of_a_context_click() -> Result<()> {
        let page = hello();
        let mut doc = Document::new(image::RgbaImage::new(400, 300))?;
        let id = doc.id();
        doc.add(Mark::redaction(
            id,
            Point { x: 10.0, y: 5.0 },
            Point { x: 60.0, y: 30.0 },
        ));
        let vis = compute(&doc, &page, &doc.privacy_snapshot());
        let masked = context_selection(&page, &vis, None, Some((30.0, 17.0)));
        assert!(masked.is_none(), "the masked word is not selectable");
        let far = context_selection(&page, &vis, sel(6, 11), Some((30.0, 17.0)));
        assert!(far.is_none(), "and its click clears the old range");

        doc.undo();
        doc.set_crop(Some(Crop {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        }));
        let vis = compute(&doc, &page, &doc.privacy_snapshot());
        let cropped = context_selection(&page, &vis, None, Some((115.0, 17.0)));
        assert!(
            cropped.is_none(),
            "text cut off by the crop is not a target"
        );
        let inside = context_selection(&page, &vis, None, Some((85.0, 17.0)));
        assert_eq!(range(inside), Some((6, 9)), "only the glyphs inside remain");
        Ok(())
    }

    /// A range hidden after it was made is dropped, whatever its stale endpoints say.
    #[test]
    fn a_range_hidden_by_privacy_is_no_longer_a_selection() -> Result<()> {
        let page = hello();
        let mut doc = Document::new(image::RgbaImage::new(400, 300))?;
        let id = doc.id();
        doc.add(Mark::privacy_blur(
            id,
            Point { x: 10.0, y: 5.0 },
            Point { x: 60.0, y: 30.0 },
        ));
        let vis = compute(&doc, &page, &doc.privacy_snapshot());
        assert!(context_selection(&page, &vis, sel(0, 5), None).is_none());
        assert!(live_range(&page, &vis, sel(0, 5)).is_none());
        assert!(line_selection(&page, &vis, sel(0, 5)).is_none());
        let partly = context_selection(&page, &vis, sel(0, 11), None);
        assert_eq!(range(partly), Some((0, 11)), "visible text keeps it alive");
        let line = line_selection(&page, &vis, sel(7, 9));
        assert_eq!(
            range(line),
            Some((0, 11)),
            "select line takes the whole line"
        );
        Ok(())
    }

    #[test]
    fn direct_redaction_is_one_undoable_normal_mark_that_hides_the_text() -> Result<()> {
        let page = Rc::new(Page::new(
            400,
            300,
            vec![line(10.0, 10.0, "Hello world"), line(10.0, 60.0, "Second")],
        ));
        for kind in [Markup::Redact, Markup::Pixelate] {
            let mut doc = Document::new(image::RgbaImage::from_pixel(
                400,
                300,
                image::Rgba([255, 255, 255, 255]),
            ))?;
            let vis = compute(&doc, &page, &doc.privacy_snapshot());
            let runs = page.runs(0, page.len(), &vis);
            assert_eq!(runs.len(), 2);
            let marks = markup_marks(&mut doc, [1.0, 0.0, 0.0, 1.0], &runs, kind);
            assert!(doc.add_many(marks));
            assert_eq!(doc.marks.len(), 2, "one normal mark per run");
            assert!(doc.marks.iter().all(|m| match kind {
                Markup::Redact => matches!(m.kind, MarkKind::Redaction),
                _ => matches!(m.kind, MarkKind::PrivacyBlur),
            }));

            let hidden = compute(&doc, &page, &doc.privacy_snapshot());
            assert!(hidden.iter().all(|&v| !v), "every covered glyph is private");
            if matches!(kind, Markup::Redact) {
                let image = doc.render()?;
                assert_eq!(image.get_pixel(50, 17).0, [0, 0, 0, 255]);
                assert_eq!(image.get_pixel(30, 67).0, [0, 0, 0, 255]);
                assert_eq!(image.get_pixel(300, 200).0, [255, 255, 255, 255]);
            }
            doc.undo();
            assert!(doc.marks.is_empty(), "a single undo removes both masks");
            assert!(
                compute(&doc, &page, &doc.privacy_snapshot())
                    .iter()
                    .all(|&v| v)
            );
        }
        Ok(())
    }
}
