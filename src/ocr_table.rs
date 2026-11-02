//! Table view of the recognised text: rows and columns are inferred from the cached glyph boxes
//! (`ocr::Page`), shown in a native preview where they can be joined or split, and copied as
//! spreadsheet text (TSV), CSV or Markdown. Nothing here touches the image or the project.
//!
//! Only glyphs that are visible right now take part (`ocr_ui::visible`: outside every mask, inside
//! the crop), and the preview never trusts its own copy of them: before every copy and on a short
//! timer it rebuilds the words from the live document and closes when they differ, so text that a
//! mask or crop has since hidden is never copied.

use crate::{
    chrome::{self, Icon},
    ocr::{Page, Rect, rect_of},
    ocr_ui::OcrUi,
};
use gtk::{gdk, glib, prelude::*};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::{Rc, Weak},
    time::Duration,
};

/// Smallest gap that separates two columns, in median word heights; a space is far narrower.
const MIN_COLUMN_GAP: f64 = 0.9;
/// A one-segment line wider than this share of the text's width is a title, not a cell.
const SPANNING: f64 = 0.5;
/// Row pitches must spread by this factor before any line counts as the wrap of the one above.
const WRAP_SPREAD: f64 = 1.35;
/// A line within this factor of the tightest pitch may continue the row above.
const WRAP_PITCH: f64 = 1.15;
/// How often the preview checks that what it shows is still what may be copied.
const WATCH: Duration = Duration::from_millis(300);
const STALE: &str =
    "Table preview closed: the image or the text changed. Open it again to copy the new table.";

/// A run of visible, non-space glyphs of one recognised line.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub rect: Rect,
}

/// The words of `page` that are visible (`vis`) and inside the glyph ordinals `range`. A hidden
/// glyph ends a word, so nothing is ever joined across text that is masked or cropped away.
pub fn words(page: &Page, vis: &[bool], range: (usize, usize)) -> Vec<Word> {
    let union = |a: Rect, b: Rect| {
        let (x, y) = (a.x.min(b.x), a.y.min(b.y));
        Rect {
            x,
            y,
            w: (a.x + a.w).max(b.x + b.w) - x,
            h: (a.y + a.h).max(b.y + b.h) - y,
        }
    };
    let mut out = Vec::new();
    let mut base = 0;
    for (li, line) in page.lines().iter().enumerate() {
        let mut open: Option<Word> = None;
        for (gi, glyph) in line.glyphs.iter().enumerate() {
            let ordinal = base + gi;
            let shown = (range.0..range.1).contains(&ordinal)
                && vis.get(ordinal).copied().unwrap_or(false)
                && !glyph.ch.trim().is_empty();
            if !shown {
                out.extend(open.take());
                continue;
            }
            let rect = rect_of(&page.glyph_quad(li, gi));
            match &mut open {
                Some(word) => {
                    word.text.push_str(&glyph.ch);
                    word.rect = union(word.rect, rect);
                }
                None => {
                    open = Some(Word {
                        text: glyph.ch.clone(),
                        rect,
                    });
                }
            }
        }
        out.extend(open);
        base += line.glyphs.len();
    }
    out
}

/// The words as the live document allows them right now.
pub enum Scene {
    /// The editor holds its state; ask again later.
    Busy,
    /// No recognised page belongs to the document any more.
    Gone,
    Words(Vec<Word>),
}

/// Words sharing one text row, left to right (indices into `Table::words`).
#[derive(Clone, Debug)]
struct Row {
    words: Vec<usize>,
    cy: f64,
}

/// Separator positions in image pixels: `cols` are x values left to right, `rows` y values top to
/// bottom. A word belongs to the column of its left edge and a text row to the row of its centre.
#[derive(Clone, Debug, Default, PartialEq)]
struct Layout {
    cols: Vec<f64>,
    rows: Vec<f64>,
}

#[derive(Clone, Debug)]
pub struct Table {
    words: Vec<Word>,
    lines: Vec<Row>,
    layout: Layout,
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn center(word: &Word) -> f64 {
    word.rect.y + word.rect.h / 2.0
}

/// Text rows: words whose centres lie within half a line height of the row's first word.
fn group(words: &[Word]) -> Vec<Row> {
    let mut order: Vec<usize> = (0..words.len()).collect();
    order.sort_by(|&a, &b| center(&words[a]).total_cmp(&center(&words[b])));
    let mut groups: Vec<(Vec<usize>, f64, f64)> = Vec::new();
    for i in order {
        let (c, h) = (center(&words[i]), words[i].rect.h);
        match groups.last_mut() {
            Some((row, anchor, anchor_h)) if (c - *anchor).abs() <= 0.5 * h.min(*anchor_h) => {
                row.push(i);
            }
            _ => groups.push((vec![i], c, h)),
        }
    }
    let mut lines: Vec<Row> = groups
        .into_iter()
        .map(|(mut row, ..)| {
            row.sort_by(|&a, &b| words[a].rect.x.total_cmp(&words[b].rect.x));
            let cy = row.iter().map(|&i| center(&words[i])).sum::<f64>() / row.len() as f64;
            Row { words: row, cy }
        })
        .collect();
    lines.sort_by(|a, b| a.cy.total_cmp(&b.cy));
    lines
}

fn column(cols: &[f64], word: &Word) -> usize {
    cols.partition_point(|&c| c < word.rect.x)
}

/// Column separators: the middle of every horizontal gap that no text row crosses. Title-like
/// lines (no gap of their own, spanning most of the width) are ignored so one heading above the
/// table cannot glue its columns together.
fn infer_columns(words: &[Word], lines: &[Row]) -> Vec<f64> {
    let min_gap = MIN_COLUMN_GAP * median(words.iter().map(|w| w.rect.h).collect());
    let left = words.iter().map(|w| w.rect.x).fold(f64::MAX, f64::min);
    let right = words
        .iter()
        .map(|w| w.rect.x + w.rect.w)
        .fold(f64::MIN, f64::max);
    let mut events: Vec<(f64, i32)> = Vec::new();
    for line in lines {
        let ws: Vec<&Rect> = line.words.iter().map(|&i| &words[i].rect).collect();
        let (first, last) = (ws[0], ws[ws.len() - 1]);
        let gapless = ws.windows(2).all(|p| p[1].x - (p[0].x + p[0].w) < min_gap);
        if gapless && last.x + last.w - first.x > SPANNING * (right - left) {
            continue;
        }
        for r in ws {
            events.push((r.x, 1));
            events.push((r.x + r.w, -1));
        }
    }
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut cuts, mut cover, mut i) = (Vec::new(), 0, 0);
    while i < events.len() {
        let x = events[i].0;
        while i < events.len() && events[i].0 == x {
            cover += events[i].1;
            i += 1;
        }
        if cover == 0 && i < events.len() && events[i].0 - x >= min_gap {
            cuts.push((x + events[i].0) / 2.0);
        }
    }
    cuts
}

/// Row separators between text rows. A text row that leaves the first column empty, fills only
/// some of the columns the row above fills, sits at the tightest line pitch and follows a clearly
/// looser one is the wrapped second line of a cell, so it joins that row instead of starting a
/// new one.
fn infer_rows(words: &[Word], lines: &[Row], cols: &[f64]) -> Vec<f64> {
    let occupied = |line: &Row| -> BTreeSet<usize> {
        line.words
            .iter()
            .map(|&i| column(cols, &words[i]))
            .collect()
    };
    let pitch: Vec<f64> = lines.windows(2).map(|p| p[1].cy - p[0].cy).collect();
    let tight = pitch.iter().copied().fold(f64::MAX, f64::min);
    let spread = pitch.iter().copied().fold(f64::MIN, f64::max);
    let wraps = tight > 0.0 && spread >= WRAP_SPREAD * tight;
    let mut cuts = Vec::new();
    let Some(first) = lines.first() else {
        return cuts;
    };
    let mut block = occupied(first);
    for i in 1..lines.len() {
        let here = occupied(&lines[i]);
        let continues = wraps
            && pitch[i - 1] <= WRAP_PITCH * tight
            && !here.contains(&0)
            && here.len() < block.len()
            && here.is_subset(&block);
        if !continues {
            cuts.push((lines[i - 1].cy + lines[i].cy) / 2.0);
            block = here;
        }
    }
    cuts
}

fn insert_sorted(cuts: &mut Vec<f64>, at: f64) {
    let i = cuts.partition_point(|&c| c < at);
    cuts.insert(i, at);
}

impl Table {
    /// Infers the layout of `words`, which must not be empty.
    pub fn new(words: Vec<Word>) -> Self {
        let lines = group(&words);
        let mut table = Self {
            words,
            lines,
            layout: Layout::default(),
        };
        table.reset();
        table
    }

    pub fn words(&self) -> &[Word] {
        &self.words
    }

    /// Back to the inferred rows and columns.
    pub fn reset(&mut self) {
        let cols = infer_columns(&self.words, &self.lines);
        let rows = infer_rows(&self.words, &self.lines, &cols);
        self.layout = Layout { cols, rows };
    }

    pub fn nrows(&self) -> usize {
        self.layout.rows.len() + 1
    }

    pub fn ncols(&self) -> usize {
        self.layout.cols.len() + 1
    }

    fn row_of(&self, line: &Row) -> usize {
        self.layout.rows.partition_point(|&c| c < line.cy)
    }

    /// Every cell, rectangular; a cell with no text is empty and a wrapped cell holds its lines
    /// separated by `\n`.
    pub fn cells(&self) -> Vec<Vec<String>> {
        let (nr, nc) = (self.nrows(), self.ncols());
        let mut grid: Vec<Vec<Vec<String>>> = vec![vec![Vec::new(); nc]; nr];
        for line in &self.lines {
            let mut parts = vec![String::new(); nc];
            for &i in &line.words {
                let part = &mut parts[column(&self.layout.cols, &self.words[i])];
                if !part.is_empty() {
                    part.push(' ');
                }
                part.push_str(&self.words[i].text);
            }
            let row = self.row_of(line);
            for (c, part) in parts.into_iter().enumerate() {
                if !part.is_empty() {
                    grid[row][c].push(part);
                }
            }
        }
        grid.into_iter()
            .map(|row| row.into_iter().map(|c| c.join("\n")).collect())
            .collect()
    }

    /// Joins `row` with the row above it (its text becomes extra lines of the cells).
    pub fn join_row(&mut self, row: usize) -> bool {
        let ok = row >= 1 && row <= self.layout.rows.len();
        if ok {
            self.layout.rows.remove(row - 1);
        }
        ok
    }

    /// Splits `row` where its text rows are furthest apart; false for a one-line row.
    pub fn split_row(&mut self, row: usize) -> bool {
        let mut centres: Vec<f64> = self
            .lines
            .iter()
            .filter(|l| self.row_of(l) == row)
            .map(|l| l.cy)
            .collect();
        centres.sort_by(f64::total_cmp);
        let Some(p) = centres
            .windows(2)
            .max_by(|a, b| (a[1] - a[0]).total_cmp(&(b[1] - b[0])))
        else {
            return false;
        };
        insert_sorted(&mut self.layout.rows, (p[0] + p[1]) / 2.0);
        true
    }

    /// Joins `col` with the column on its right.
    pub fn join_col(&mut self, col: usize) -> bool {
        let ok = col < self.layout.cols.len();
        if ok {
            self.layout.cols.remove(col);
        }
        ok
    }

    /// Splits `col` at the widest gap between two words of one line; false when no line has two.
    pub fn split_col(&mut self, col: usize) -> bool {
        let mut best: Option<(f64, f64)> = None;
        for line in &self.lines {
            let inside: Vec<&Rect> = line
                .words
                .iter()
                .map(|&i| &self.words[i])
                .filter(|w| column(&self.layout.cols, w) == col)
                .map(|w| &w.rect)
                .collect();
            for p in inside.windows(2) {
                let end = p[0].x + p[0].w;
                let gap = p[1].x - end;
                if gap > 0.0 && best.is_none_or(|b| gap > b.0) {
                    best = Some((gap, end + gap / 2.0));
                }
            }
        }
        let Some((_, at)) = best else { return false };
        insert_sorted(&mut self.layout.cols, at);
        true
    }
}

/// Spreadsheet-style column name: A, B, … Z, AA, AB, …
pub fn column_name(mut index: usize) -> String {
    let mut name = String::new();
    loop {
        name.insert(0, char::from(b'A' + (index % 26) as u8));
        if index < 26 {
            return name;
        }
        index = index / 26 - 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Tsv,
    Csv,
    Markdown,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Tsv => "spreadsheet text (TSV)",
            Self::Csv => "CSV",
            Self::Markdown => "Markdown",
        }
    }
}

/// A spreadsheet runs a cell that starts like a formula, and the text comes from an image anyone
/// may have made: such a cell gets a leading `'`, which spreadsheets read as "text". Risky cells
/// start (after any blanks) with `=`, `+`, `@` or a `-` that is not a plain number, or have a
/// control character (tab, CR, ...) before the first visible character. Negative numbers such as
/// `-5`, `-12.5%` and `-$3,000.00` stay usable.
fn neutralise(text: &str) -> Cow<'_, str> {
    let rest = text.trim_start_matches(|c: char| c.is_whitespace() || c.is_control());
    let blanks = &text[..text.len() - rest.len()];
    let risky = blanks.chars().any(char::is_control)
        || match rest.chars().next() {
            Some('=' | '@' | '+') => true,
            Some('-') => {
                let digits = &rest[1..];
                !(digits.chars().any(|c| c.is_ascii_digit())
                    && digits
                        .chars()
                        .all(|c| c.is_ascii_digit() || " .,%$€£()".contains(c)))
            }
            _ => false,
        };
    if risky {
        Cow::Owned(format!("'{text}"))
    } else {
        Cow::Borrowed(text)
    }
}

/// One delimited field: quoted, with `"` doubled, when it holds the separator, a quote or a line
/// break (the form spreadsheets read for pasted text and CSV alike).
fn field(text: &str, separator: char) -> String {
    let text = neutralise(text);
    if text.contains([separator, '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.into_owned()
    }
}

/// Markdown cell with the text kept literal: backslash-escapes every character that could start
/// HTML, an entity, a link, emphasis or code (and the table's pipe). Only a real line break
/// becomes our own `<br>`. Bare URLs may still be auto-linked by a renderer; they are not markup.
fn markdown_cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("<br>");
            }
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '|' | '~' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// `cells` (rectangular rows) as text; rows are separated by `\n`, the first row of a Markdown
/// table is its header.
pub fn render(cells: &[Vec<String>], format: Format) -> String {
    let line = |row: &Vec<String>| match format {
        Format::Tsv => row
            .iter()
            .map(|c| field(c, '\t'))
            .collect::<Vec<_>>()
            .join("\t"),
        Format::Csv => row
            .iter()
            .map(|c| field(c, ','))
            .collect::<Vec<_>>()
            .join(","),
        Format::Markdown => format!(
            "| {} |",
            row.iter()
                .map(|c| markdown_cell(c))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
    };
    let mut lines: Vec<String> = cells.iter().map(line).collect();
    if format == Format::Markdown && !lines.is_empty() {
        let width = cells[0].len();
        lines.insert(1, format!("|{}", " --- |".repeat(width)));
    }
    lines.join("\n")
}

// Preview window

#[derive(Clone, Copy)]
enum Sel {
    Row(usize),
    Col(usize),
}

struct Preview {
    window: glib::WeakRef<gtk::Window>,
    ocr: Weak<OcrUi>,
    range: (usize, usize),
    table: RefCell<Table>,
    format: Cell<Format>,
    selected: Cell<Option<Sel>>,
    scroller: gtk::ScrolledWindow,
    output: gtk::TextView,
    summary: gtk::Label,
    status: gtk::Label,
    join_row: gtk::Button,
    split_row: gtk::Button,
    join_col: gtk::Button,
    split_col: gtk::Button,
}

fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.add_css_class(class);
    label
}

fn header(text: &str, name: &str, group: Option<&gtk::ToggleButton>) -> gtk::ToggleButton {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.append(&gtk::Label::new(Some(text)));
    let button = gtk::ToggleButton::builder().child(&content).build();
    button.set_group(group);
    button.set_tooltip_text(Some(name));
    button.update_property(&[gtk::accessible::Property::Label(name)]);
    button
}

/// Opens the preview for `words` (visible text of the glyph ordinals `range`) over `parent`.
pub(crate) fn open(
    ocr: &Rc<OcrUi>,
    parent: &gtk::ApplicationWindow,
    words: Vec<Word>,
    range: (usize, usize),
) -> gtk::Window {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .destroy_with_parent(true)
        .title("Table from recognised text")
        .default_width(760)
        .default_height(640)
        .build();
    window.add_css_class("sharpshot");
    window.set_titlebar(Some(&gtk::HeaderBar::new()));

    let op = |text: &str, tip: &str| {
        let b = gtk::Button::with_label(text);
        b.set_tooltip_text(Some(tip));
        b.set_sensitive(false);
        b
    };
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(200)
        .build();
    let output = gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .left_margin(8)
        .right_margin(8)
        .top_margin(6)
        .bottom_margin(6)
        .build();
    output.update_property(&[gtk::accessible::Property::Label("Table text to copy")]);
    let preview = Rc::new(Preview {
        window: window.downgrade(),
        ocr: Rc::downgrade(ocr),
        range,
        table: RefCell::new(Table::new(words)),
        format: Cell::new(Format::Tsv),
        selected: Cell::new(None),
        scroller,
        output,
        summary: label("", "dimension-label"),
        status: label("", "dimension-label"),
        join_row: op(
            "Join with row above",
            "Make the selected row part of the row above; its text becomes extra lines of the cells",
        ),
        split_row: op(
            "Split row",
            "Split the selected row where its lines are furthest apart",
        ),
        join_col: op(
            "Join with column to the right",
            "Merge the selected column with the one to its right; words of a line are joined with a space",
        ),
        split_col: op(
            "Split column",
            "Split the selected column at its widest gap between two words",
        ),
    });
    let reset = op(
        "Reset rows and columns",
        "Go back to the layout SharpShot inferred",
    );
    reset.set_sensitive(true);
    let copy = chrome::menu_button(
        Icon::Copy,
        "Copy table",
        "Copy the table in the format chosen on the left. For TSV and CSV, a cell that starts with = + @ or a - that is not a plain number gets a leading ' so a spreadsheet shows it as text instead of running it.",
    );

    let formats = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let mut first: Option<gtk::ToggleButton> = None;
    for (text, tip, format) in [
        (
            "TSV",
            "Tab-separated: pastes into spreadsheet cells",
            Format::Tsv,
        ),
        ("CSV", "Comma-separated values", Format::Csv),
        (
            "Markdown",
            "Markdown table; the first row is the header",
            Format::Markdown,
        ),
    ] {
        let b = gtk::ToggleButton::with_label(text);
        b.set_tooltip_text(Some(tip));
        b.set_group(first.as_ref());
        b.set_active(format == Format::Tsv);
        b.connect_toggled(glib::clone!(
            #[weak]
            preview,
            move |b| {
                if b.is_active() {
                    preview.format.set(format);
                    preview.show_text();
                }
            }
        ));
        formats.append(&b);
        first.get_or_insert(b);
    }
    let gap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    gap.set_hexpand(true);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    top.append(&label("Copy as", "field-label"));
    top.append(&formats);
    top.append(&gap);
    top.append(&copy);

    let edits = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    for b in [
        &preview.join_row,
        &preview.split_row,
        &preview.join_col,
        &preview.split_col,
        &reset,
    ] {
        edits.append(b);
    }
    let edit_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&edits)
        .build();
    let text_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(110)
        .child(&preview.output)
        .build();

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.append(&preview.summary);
    body.append(&preview.scroller);
    body.append(&label(
        "Pick a row number or a column letter, then join or split it. Cells shown as “empty” stay empty in the copy. The image is never changed.",
        "dimension-label",
    ));
    body.append(&edit_scroll);
    body.append(&label("Text that will be copied", "title"));
    body.append(&text_scroll);
    body.append(&top);
    body.append(&preview.status);
    window.set_child(Some(&body));

    copy.connect_clicked(glib::clone!(
        #[weak]
        preview,
        move |_| preview.copy()
    ));
    reset.connect_clicked(glib::clone!(
        #[weak]
        preview,
        move |_| {
            preview.table.borrow_mut().reset();
            preview.rebuild();
        }
    ));
    let row_ops: [(&gtk::Button, fn(&mut Table, usize) -> bool, &str); 2] = [
        (
            &preview.join_row,
            Table::join_row,
            "That row is already the first one.",
        ),
        (
            &preview.split_row,
            Table::split_row,
            "That row has a single line of text; there is nothing to split.",
        ),
    ];
    for (button, op, failure) in row_ops {
        button.connect_clicked(glib::clone!(
            #[weak]
            preview,
            move |_| preview.edit(true, op, failure)
        ));
    }
    let col_ops: [(&gtk::Button, fn(&mut Table, usize) -> bool, &str); 2] = [
        (
            &preview.join_col,
            Table::join_col,
            "That is already the last column.",
        ),
        (
            &preview.split_col,
            Table::split_col,
            "No line has two words in that column; there is nothing to split.",
        ),
    ];
    for (button, op, failure) in col_ops {
        button.connect_clicked(glib::clone!(
            #[weak]
            preview,
            move |_| preview.edit(false, op, failure)
        ));
    }

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        window,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key != gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            window.close();
            glib::Propagation::Stop
        }
    ));
    window.add_controller(keys);
    let keep = preview.clone();
    window.connect_destroy(move |_| {
        let _ = &keep;
    });
    glib::timeout_add_local(
        WATCH,
        glib::clone!(
            #[weak]
            preview,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || preview.watch()
        ),
    );
    preview.rebuild();
    window.present();
    window
}

impl Preview {
    fn say(&self, text: &str) {
        self.status.set_text(text);
    }

    /// Redraws the grid and the text from the table; nothing is selected afterwards.
    fn rebuild(self: &Rc<Self>) {
        let this = Rc::clone(self);
        let cells = self.table.borrow().cells();
        let grid = gtk::Grid::builder()
            .column_spacing(16)
            .row_spacing(8)
            .margin_start(8)
            .margin_end(8)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        let mut group: Option<gtk::ToggleButton> = None;
        let columns = cells.first().map_or(0, Vec::len);
        for c in 0..columns {
            let name = column_name(c);
            let b = header(&name, &format!("Select column {name}"), group.as_ref());
            b.connect_toggled(glib::clone!(
                #[weak]
                this,
                move |b| {
                    if b.is_active() {
                        this.select(Sel::Col(c));
                    }
                }
            ));
            grid.attach(&b, c as i32 + 1, 0, 1, 1);
            group.get_or_insert(b);
        }
        for (r, row) in cells.iter().enumerate() {
            let b = header(
                &(r + 1).to_string(),
                &format!("Select row {}", r + 1),
                group.as_ref(),
            );
            b.connect_toggled(glib::clone!(
                #[weak]
                this,
                move |b| {
                    if b.is_active() {
                        this.select(Sel::Row(r));
                    }
                }
            ));
            grid.attach(&b, 0, r as i32 + 1, 1, 1);
            group.get_or_insert(b);
            for (c, text) in row.iter().enumerate() {
                let cell = gtk::Label::builder()
                    .label(if text.is_empty() {
                        "empty"
                    } else {
                        text.as_str()
                    })
                    .xalign(0.0)
                    .valign(gtk::Align::Start)
                    .wrap(true)
                    .max_width_chars(36)
                    .selectable(!text.is_empty())
                    .build();
                if text.is_empty() {
                    cell.add_css_class("dimension-label");
                }
                grid.attach(&cell, c as i32 + 1, r as i32 + 1, 1, 1);
            }
        }
        self.scroller.set_child(Some(&grid));
        self.selected.set(None);
        let words = self.table.borrow().words().len();
        self.summary.set_text(&format!(
            "{} row{} × {} column{} from {words} recognised word{}",
            cells.len(),
            if cells.len() == 1 { "" } else { "s" },
            columns,
            if columns == 1 { "" } else { "s" },
            if words == 1 { "" } else { "s" },
        ));
        self.show_text();
        self.sync();
    }

    fn show_text(&self) {
        let format = self.format.get();
        let text = render(&self.table.borrow().cells(), format);
        self.output.buffer().set_text(&text);
        self.say(match format {
            Format::Markdown => "Markdown: the first row becomes the table header.",
            _ => "Cells starting with = + @ or a non-numeric - get a leading ' when copied, so a spreadsheet shows them as text and never runs them.",
        });
    }

    fn select(&self, sel: Sel) {
        self.selected.set(Some(sel));
        self.say("");
        self.sync();
    }

    fn sync(&self) {
        let columns = self.table.borrow().ncols();
        let sel = self.selected.get();
        self.join_row
            .set_sensitive(matches!(sel, Some(Sel::Row(r)) if r > 0));
        self.split_row
            .set_sensitive(matches!(sel, Some(Sel::Row(_))));
        self.join_col
            .set_sensitive(matches!(sel, Some(Sel::Col(c)) if c + 1 < columns));
        self.split_col
            .set_sensitive(matches!(sel, Some(Sel::Col(_))));
    }

    fn edit(self: &Rc<Self>, row: bool, op: fn(&mut Table, usize) -> bool, failure: &str) {
        let index = match (self.selected.get(), row) {
            (Some(Sel::Row(i)), true) | (Some(Sel::Col(i)), false) => i,
            _ => return,
        };
        if op(&mut self.table.borrow_mut(), index) {
            self.rebuild();
        } else {
            self.say(failure);
        }
    }

    /// The live words equal the previewed ones, `Some(false)` when they differ, `None` while the
    /// editor is busy.
    fn current(&self, ocr: &OcrUi) -> Option<bool> {
        match ocr.table_scene(self.range) {
            Scene::Busy => None,
            Scene::Gone => Some(false),
            Scene::Words(live) => Some(live[..] == *self.table.borrow().words()),
        }
    }

    fn stale(&self, ocr: &OcrUi) {
        if let Some(window) = self.window.upgrade() {
            window.close();
        }
        ocr.say(STALE);
    }

    fn watch(&self) -> glib::ControlFlow {
        let Some(ocr) = self.ocr.upgrade() else {
            if let Some(window) = self.window.upgrade() {
                window.close();
            }
            return glib::ControlFlow::Break;
        };
        if self.current(&ocr) == Some(false) {
            self.stale(&ocr);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    }

    /// Copies the table, after proving against the live document that every word may still be
    /// copied.
    fn copy(&self) {
        let Some(ocr) = self.ocr.upgrade() else {
            return;
        };
        match self.current(&ocr) {
            Some(true) => {}
            Some(false) => return self.stale(&ocr),
            None => return self.say("The editor is busy; try again."),
        }
        let format = self.format.get();
        let cells = self.table.borrow().cells();
        let text = render(&cells, format);
        let Some(window) = self.window.upgrade() else {
            return;
        };
        window.clipboard().set_text(&text);
        let columns = cells.first().map_or(0, Vec::len);
        self.say(&format!(
            "Copied {} rows × {columns} columns as {}.",
            cells.len(),
            format.name()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document::{Crop, Document, Mark},
        interaction::State,
        ocr::{Glyph, Line},
        ocr_ui::visible,
        shapes::Point,
    };

    fn word(text: &str, x: f64, y: f64) -> Word {
        Word {
            text: text.into(),
            rect: Rect {
                x,
                y,
                w: 10.0 * text.chars().count() as f64,
                h: 14.0,
            },
        }
    }

    fn sales() -> Vec<Word> {
        vec![
            word("Name", 10.0, 10.0),
            word("Qty", 200.0, 10.0),
            word("Price", 300.0, 10.0),
            word("Alice", 10.0, 40.0),
            word("3", 200.0, 40.0),
            word("$4.00", 300.0, 40.0),
            word("Bob", 10.0, 70.0),
            word("$5.50", 300.0, 70.0),
        ]
    }

    #[test]
    fn columns_come_from_gaps_and_missing_cells_stay_empty() {
        let table = Table::new(sales());
        assert_eq!(
            table.cells(),
            [
                ["Name", "Qty", "Price"],
                ["Alice", "3", "$4.00"],
                ["Bob", "", "$5.50"],
            ]
        );
    }

    #[test]
    fn words_of_one_cell_stay_together_and_spaces_do_not_split_columns() {
        let table = Table::new(vec![
            word("Full", 10.0, 10.0),
            word("name", 60.0, 10.0),
            word("Role", 300.0, 10.0),
            word("Ann", 10.0, 40.0),
            word("Lee", 50.0, 40.0),
            word("Dev", 300.0, 40.0),
        ]);
        assert_eq!(table.cells(), [["Full name", "Role"], ["Ann Lee", "Dev"]]);
    }

    fn wrapped() -> Vec<Word> {
        vec![
            word("Item", 10.0, 10.0),
            word("Description", 200.0, 10.0),
            word("Widget", 10.0, 50.0),
            word("Small", 200.0, 50.0),
            word("and", 260.0, 50.0),
            word("cheap", 200.0, 66.0),
            word("Gadget", 10.0, 100.0),
            word("Large", 200.0, 100.0),
        ]
    }

    #[test]
    fn a_wrapped_cell_is_one_multiline_cell_and_rows_can_be_split_and_joined() {
        let mut table = Table::new(wrapped());
        let joined = [
            vec!["Item", "Description"],
            vec!["Widget", "Small and\ncheap"],
            vec!["Gadget", "Large"],
        ];
        assert_eq!(table.cells(), joined);

        assert!(table.split_row(1));
        assert_eq!(table.nrows(), 4);
        assert_eq!(table.cells()[1], ["Widget", "Small and"]);
        assert_eq!(table.cells()[2], ["", "cheap"]);
        assert!(!table.split_row(0), "a one-line row has nothing to split");

        assert!(table.join_row(2));
        assert_eq!(table.cells(), joined);
        assert!(!table.join_row(0), "the first row has no row above");
    }

    #[test]
    fn evenly_spaced_rows_are_never_taken_for_wrapped_lines() {
        // Every pitch equal and the last line fills fewer columns: still three separate rows.
        let table = Table::new(vec![
            word("a", 10.0, 10.0),
            word("b", 200.0, 10.0),
            word("c", 10.0, 26.0),
            word("d", 200.0, 26.0),
            word("e", 10.0, 42.0),
        ]);
        assert_eq!(table.nrows(), 3);
        assert_eq!(table.cells()[2], ["e", ""]);
    }

    #[test]
    fn a_title_over_the_table_does_not_glue_the_columns() {
        let mut words = sales();
        let mut x = 10.0;
        for w in [
            "Quarterly",
            "sales",
            "report",
            "for",
            "all",
            "regions",
            "combined",
        ] {
            let title = word(w, x, -40.0);
            x += title.rect.w + 10.0;
            words.push(title);
        }
        let table = Table::new(words);
        assert_eq!(table.ncols(), 3);
        let cells = table.cells();
        assert_eq!(cells[cells.len() - 3], ["Name", "Qty", "Price"]);
        assert_eq!(cells[cells.len() - 1], ["Bob", "", "$5.50"]);
    }

    #[test]
    fn columns_can_be_joined_split_and_reset() {
        let mut table = Table::new(sales());
        assert!(table.join_col(0));
        assert_eq!(table.cells()[1], ["Alice 3", "$4.00"]);
        assert!(!table.join_col(1), "the last column has none to its right");

        assert!(table.split_col(0));
        assert_eq!(table.cells()[1], ["Alice", "3", "$4.00"]);
        assert!(!table.split_col(0), "one word per line cannot be split");

        assert!(table.join_col(1));
        table.reset();
        assert_eq!(table.ncols(), 3);
    }

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn tsv_and_csv_quote_what_a_spreadsheet_would_misread() {
        let cells = [row(&["a\tb", "say \"hi\"", "one\ntwo", "x,y", "plain", ""])];
        assert_eq!(
            render(&cells, Format::Tsv),
            "\"a\tb\"\t\"say \"\"hi\"\"\"\t\"one\ntwo\"\tx,y\tplain\t"
        );
        assert_eq!(
            render(&cells, Format::Csv),
            "a\tb,\"say \"\"hi\"\"\",\"one\ntwo\",\"x,y\",plain,"
        );
    }

    #[test]
    fn formula_like_cells_are_neutralised_but_negative_numbers_are_not() {
        let cells = [row(&[
            "=SUM(A1)",
            "@x",
            "+cmd|' /C calc'!A0",
            "+1 555",
            "-abc",
            "-",
            "-1+1",
            "\t=1+1",
            "\r@x",
            " =1+1",
        ])];
        assert_eq!(
            render(&cells, Format::Tsv),
            "'=SUM(A1)\t'@x\t'+cmd|' /C calc'!A0\t'+1 555\t'-abc\t'-\t'-1+1\t\"'\t=1+1\"\t\"'\r@x\"\t' =1+1"
        );
        let csv = render(&[row(&["=1", "-9"])], Format::Csv);
        assert_eq!(csv, "'=1,-9");
        let numbers = [row(&["-5", "-12.5%", "-$3,000.00", "(1)", "1+1", "a=b"])];
        assert_eq!(
            render(&numbers, Format::Tsv),
            "-5\t-12.5%\t-$3,000.00\t(1)\t1+1\ta=b"
        );
        let markdown = render(&[row(&["=1"])], Format::Markdown);
        assert!(
            markdown.contains("| =1 |"),
            "Markdown is not formula-sensitive"
        );
    }

    #[test]
    fn markdown_escapes_pipes_backslashes_and_line_breaks() {
        let cells = [
            row(&["H|1", "B"]),
            row(&["x\\y", "l1\nl2"]),
            row(&["", "z"]),
        ];
        assert_eq!(
            render(&cells, Format::Markdown),
            "| H\\|1 | B |\n| --- | --- |\n| x\\\\y | l1<br>l2 |\n|  | z |"
        );
        assert_eq!(render(&[], Format::Markdown), "");
    }

    /// Recognised text is data: markup in it must reach the Markdown as literal characters, while
    /// the `<br>` of a multiline cell stays ours.
    #[test]
    fn markdown_keeps_ocr_text_literal() {
        let cells = [
            row(&["<img src=x onerror=alert(1)>", "[x](http://e.example)"]),
            row(&["a&amp;b `c` *d* _e_ ~f~", "line <b>1</b>\nline 2"]),
        ];
        assert_eq!(
            render(&cells, Format::Markdown),
            "| \\<img src=x onerror=alert(1)\\> | \\[x\\](http://e.example) |\n\
             | --- | --- |\n\
             | a\\&amp;b \\`c\\` \\*d\\* \\_e\\_ \\~f\\~ | line \\<b\\>1\\</b\\><br>line 2 |"
        );
    }

    #[test]
    fn column_names_follow_spreadsheets() {
        let names: Vec<_> = [0, 1, 25, 26, 27, 51, 52, 701, 702].map(column_name).into();
        assert_eq!(names, ["A", "B", "Z", "AA", "AB", "AZ", "BA", "ZZ", "AAA"]);
    }

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

    fn state() -> State {
        let (tx, _rx) = std::sync::mpsc::channel();
        State::new(Document::new(image::RgbaImage::new(400, 300)).unwrap(), tx)
    }

    fn page() -> Page {
        Page::new(
            400,
            300,
            vec![
                line(10.0, 10.0, "Name"),
                line(200.0, 10.0, "Qty"),
                line(10.0, 40.0, "Alice"),
                line(200.0, 40.0, "3"),
            ],
        )
    }

    fn live(state: &State, page: &Page, range: (usize, usize)) -> Vec<Word> {
        words(page, &visible(state, page), range)
    }

    /// Masks and crops drawn after recognition remove text from the table at once; the previewed
    /// words then differ from the live ones, which is what makes a copy refuse.
    #[test]
    fn masked_and_cropped_text_never_reaches_a_table() {
        let page = page();
        let mut state = state();
        let all = (0, page.len());
        let shown = live(&state, &page, all);
        let before = Table::new(shown.clone());
        assert_eq!(before.cells(), [["Name", "Qty"], ["Alice", "3"]]);

        let id = state.doc.id();
        state.doc.add(Mark::redaction(
            id,
            Point { x: 195.0, y: 5.0 },
            Point { x: 235.0, y: 30.0 },
        ));
        let masked = live(&state, &page, all);
        assert_ne!(masked, shown, "the preview can tell its words went stale");
        let after = Table::new(masked);
        assert_eq!(after.cells(), [["Name", ""], ["Alice", "3"]]);
        for format in [Format::Tsv, Format::Csv, Format::Markdown] {
            assert!(!render(&after.cells(), format).contains("Qty"));
        }

        state.doc.set_crop(Some(Crop {
            x: 0,
            y: 0,
            width: 400,
            height: 30,
        }));
        let cropped = live(&state, &page, all);
        assert_eq!(
            cropped.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["Name"],
            "second row is cut away and the first row's masked cell is still hidden"
        );

        state.doc.undo();
        state.doc.undo();
        assert_eq!(live(&state, &page, all), shown, "undo brings the text back");
    }

    #[test]
    fn only_the_selected_range_is_used_and_a_hidden_glyph_splits_a_word() {
        let page = Page::new(
            400,
            300,
            vec![line(10.0, 10.0, "abcXdef"), line(10.0, 60.0, "other")],
        );
        let mut state = state();
        let range = (0, 7);
        let texts = |w: Vec<Word>| w.into_iter().map(|w| w.text).collect::<Vec<_>>();
        assert_eq!(texts(live(&state, &page, range)), ["abcXdef"]);

        let id = state.doc.id();
        state.doc.add(Mark::redaction(
            id,
            Point { x: 40.0, y: 5.0 },
            Point { x: 50.0, y: 30.0 },
        ));
        let words = texts(live(&state, &page, range));
        assert_eq!(words.len(), 2, "text on both sides stays apart: {words:?}");
        assert!(words[0].starts_with('a') && words[1].ends_with('f'));
        assert!(words.iter().all(|w| !w.contains('X')));
    }
}
