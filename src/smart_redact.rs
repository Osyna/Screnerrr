//! Reviewed smart redaction: sensitive-looking strings found in the cached OCR page become
//! candidate masks the user reviews on the canvas, and only an explicit Apply adds them.
//!
//! Detection works on the recognised glyphs of each line, so a match maps back to glyph ordinals and
//! from there to image pixels. Glyphs hidden by an existing mask or the crop are replaced by a
//! separator, hence no match can include (or be completed by) text that is not visible. Matched text
//! is never stored or logged: candidates keep only glyph ordinals, a partly obscured label for the
//! list and the pixel area of the future mask.

use crate::{
    document::{Document, Mark},
    interaction::State,
    ocr::{Page, Rect, rect_of},
    ocr_ui::{OcrUi, visible},
    shapes::Point,
};
use gtk::{
    cairo::{self, Context},
    gdk,
    glib::{self, GString, Regex, RegexCompileFlags, RegexMatchFlags},
    prelude::*,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    net::Ipv6Addr,
    rc::{Rc, Weak},
};

/// Glyph ordinals `lo..hi` of a match in the page.
pub type Span = (usize, usize);

const MIN_TERM: usize = 2;
const MAX_TERM: usize = 200;
/// Stands for glyphs that are hidden or not text; no pattern matches it.
const HIDDEN: char = '\u{1}';
/// Mask margin around the glyph cells, in line heights: decoder alignment is only accurate to about
/// a step, the same slack `ocr_ui::visible` applies.
pub(crate) const PAD_X: f64 = 0.35;
pub(crate) const PAD_Y: f64 = 0.15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Email,
    Phone,
    Ip,
    Account,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub email: bool,
    pub phone: bool,
    pub ip: bool,
    /// Card numbers, IBANs and other account numbers.
    pub account: bool,
    /// Literal, case-insensitive text; ignored below `MIN_TERM` characters.
    pub custom: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            email: true,
            phone: true,
            ip: true,
            account: true,
            custom: String::new(),
        }
    }
}

impl Options {
    fn enabled(&self, category: Category) -> bool {
        match category {
            Category::Email => self.email,
            Category::Phone => self.phone,
            Category::Ip => self.ip,
            Category::Account => self.account,
        }
    }

    fn term(&self) -> Option<String> {
        let term: String = self
            .custom
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_TERM)
            .collect();
        (term.chars().count() >= MIN_TERM).then_some(term)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub kind: &'static str,
    pub span: Span,
    /// Partly obscured text for the list; the real text is never kept.
    pub label: String,
    /// The future mask in image pixels, whole-pixel aligned and inside the image.
    pub area: Rect,
}

// --- detectors -----------------------------------------------------------------------------------

struct Detector {
    category: Category,
    kind: &'static str,
    /// Lower wins when two matches overlap.
    priority: u8,
    group: i32,
    regex: Regex,
    /// Validates the matched text; returns the byte length of the accepted prefix.
    accept: fn(&str) -> Option<usize>,
}

fn whole(s: &str) -> Option<usize> {
    Some(s.len())
}

fn digits(s: &str) -> usize {
    s.bytes().filter(u8::is_ascii_digit).count()
}

fn ipv4(s: &str) -> Option<usize> {
    let parts: Vec<&str> = s.split('.').collect();
    (parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())).then_some(s.len())
}

fn ipv6(s: &str) -> Option<usize> {
    (s.bytes().any(|b| b.is_ascii_hexdigit()) && s.parse::<Ipv6Addr>().is_ok()).then_some(s.len())
}

/// ISO 13616 check: rearranged digits read as one number leave remainder 1 modulo 97.
fn iban_ok(compact: &str) -> bool {
    let b = compact.as_bytes();
    if !(15..=34).contains(&b.len())
        || !b[..2].iter().all(u8::is_ascii_alphabetic)
        || !b[2..4].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    let mut rem = 0u32;
    for &c in b[4..].iter().chain(&b[..4]) {
        let v = match c {
            b'0'..=b'9' => u32::from(c - b'0'),
            b'a'..=b'z' => u32::from(c - b'a') + 10,
            b'A'..=b'Z' => u32::from(c - b'A') + 10,
            _ => return false,
        };
        rem = (rem * if v >= 10 { 100 } else { 10 } + v) % 97;
    }
    rem == 1
}

/// The pattern runs on into following words; keep the longest word-aligned prefix that is a valid
/// IBAN.
fn iban(s: &str) -> Option<usize> {
    s.char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .rev()
        .filter(|&end| {
            s[end..].is_empty() || (s[end..].starts_with(' ') && !s[..end].ends_with(' '))
        })
        .find(|&end| iban_ok(&s[..end].replace(' ', "")))
}

fn luhn(s: &str) -> bool {
    let sum: u32 = s
        .bytes()
        .filter(u8::is_ascii_digit)
        .rev()
        .enumerate()
        .map(|(i, b)| {
            let d = u32::from(b - b'0');
            if i % 2 == 1 {
                let d = d * 2;
                if d > 9 { d - 9 } else { d }
            } else {
                d
            }
        })
        .sum();
    sum % 10 == 0
}

/// Valid by Luhn, or a 16-digit number in four groups: one misread digit must not leave a card
/// visible.
fn card(s: &str) -> Option<usize> {
    let n = digits(s);
    let groups: Vec<&str> = s.split([' ', '-']).collect();
    let shaped = n == 16 && groups.len() == 4 && groups.iter().all(|g| g.len() == 4);
    ((13..=19).contains(&n) && (luhn(s) || shaped)).then_some(s.len())
}

fn labelled(s: &str) -> Option<usize> {
    (digits(s) >= 6).then_some(s.len())
}

fn date_like(s: &str) -> bool {
    let parts: Vec<&str> = s.split(['.', '-']).collect();
    if parts.len() != 3 || !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let [a, b, c] = [parts[0].len(), parts[1].len(), parts[2].len()];
    (a == 4 && b <= 2 && c <= 2) || (a <= 2 && b <= 2 && (c == 2 || c == 4))
}

/// Needs a phone-like shape: some separator (plain digit runs are account numbers), enough digits
/// for the style, and no date or dotted-quad/version look.
fn phone(s: &str) -> Option<usize> {
    let n = digits(s);
    let min = if s.starts_with('+') {
        8
    } else if s.contains(['(', '-']) {
        7
    } else if s.split('.').count() <= 3 {
        10
    } else {
        return None;
    };
    ((min..=15).contains(&n) && n < s.len() && !date_like(s)).then_some(s.len())
}

fn detectors() -> Vec<Detector> {
    type Row = (
        Category,
        &'static str,
        u8,
        i32,
        &'static str,
        fn(&str) -> Option<usize>,
    );
    let rows: [Row; 8] = [
        (
            Category::Email,
            "Email",
            1,
            0,
            r"(?<![A-Za-z0-9._%+\-])[A-Za-z0-9][A-Za-z0-9._%+\-]{0,63}@[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?)*\.[A-Za-z]{2,24}(?![A-Za-z0-9\-])",
            whole,
        ),
        (
            Category::Ip,
            "IP address",
            2,
            0,
            r"(?<![0-9.])(?:[0-9]{1,3}\.){3}[0-9]{1,3}(?![0-9]|\.[0-9])",
            ipv4,
        ),
        (
            Category::Ip,
            "IP address",
            2,
            0,
            r"(?<![0-9A-Fa-f:.])(?:[0-9A-Fa-f]{0,4}:){2,7}[0-9A-Fa-f]{0,4}(?![0-9A-Fa-f:]|\.[0-9])",
            ipv6,
        ),
        (
            Category::Account,
            "IBAN",
            3,
            0,
            r"(?<![A-Za-z0-9])[A-Z]{2}[0-9]{2}(?: ?[A-Za-z0-9]){11,30}",
            iban,
        ),
        (
            Category::Account,
            "Card number",
            4,
            0,
            r"(?<![0-9])[0-9](?:[ \-]?[0-9]){12,18}(?![0-9])",
            card,
        ),
        (
            Category::Account,
            "Account number",
            5,
            1,
            r"(?i)\b(?:account|acct|a/c|iban|routing|card)(?:[ \t]*(?:number|no\.?|num|#))?[ \t]*[:#]?[ \t]*([0-9](?:[ \-]?[0-9]){4,24})(?![0-9])",
            labelled,
        ),
        (
            Category::Phone,
            "Phone number",
            6,
            0,
            r"(?<![0-9A-Za-z])(?:\+|00)?(?:\([0-9]{1,4}\)|[0-9]{1,4})(?:[ .\-]?(?:\([0-9]{1,4}\)|[0-9]{1,4})){2,6}(?![0-9A-Za-z])",
            phone,
        ),
        (
            Category::Account,
            "Account number",
            7,
            0,
            r"(?<![0-9A-Za-z])[0-9]{8,17}(?![0-9A-Za-z])",
            whole,
        ),
    ];
    rows.into_iter()
        .filter_map(|(category, kind, priority, group, pattern, accept)| {
            let regex = Regex::new(
                pattern,
                RegexCompileFlags::OPTIMIZE,
                RegexMatchFlags::DEFAULT,
            )
            .ok()
            .flatten()?;
            Some(Detector {
                category,
                kind,
                priority,
                group,
                regex,
                accept,
            })
        })
        .collect()
}

thread_local! {
    static DETECTORS: Vec<Detector> = detectors();
}

/// Byte spans of `group` for every non-overlapping match of `regex` in `text`.
fn spans(regex: &Regex, text: &str, group: i32) -> Vec<(usize, usize)> {
    let text = GString::from(text);
    let Ok(info) = regex.match_(text.as_gstr(), RegexMatchFlags::DEFAULT) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while info.matches() {
        if let Some((s, e)) = info.fetch_pos(group)
            && s >= 0
            && e > s
        {
            out.push((s as usize, e as usize));
        }
        if !matches!(info.next(), Ok(true)) {
            break;
        }
    }
    out
}

struct Hit {
    kind: &'static str,
    priority: u8,
    /// Char indices into the line.
    start: usize,
    end: usize,
}

/// Every enabled detector on one line; overlaps resolved in favour of the lower priority number
/// (custom text first), the result ordered along the line.
fn detect(chars: &[(char, usize)], options: &Options) -> Vec<Hit> {
    let text: String = chars.iter().map(|c| c.0).collect();
    let mut starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    starts.push(text.len());
    let at = |byte: usize| starts.binary_search(&byte).ok();
    let mut hits = Vec::new();
    let mut push = |kind, priority, (s, e): (usize, usize), accept: fn(&str) -> Option<usize>| {
        let Some(n) = accept(&text[s..e]) else { return };
        if let (Some(start), Some(end)) = (at(s), at(s + n)) {
            hits.push(Hit {
                kind,
                priority,
                start,
                end,
            });
        }
    };
    if let Some(term) = options.term()
        && let Ok(Some(regex)) = Regex::new(
            &Regex::escape_string(term.as_str()),
            RegexCompileFlags::CASELESS | RegexCompileFlags::OPTIMIZE,
            RegexMatchFlags::DEFAULT,
        )
    {
        for span in spans(&regex, &text, 0) {
            push("Custom text", 0, span, whole);
        }
    }
    DETECTORS.with(|all| {
        for d in all.iter().filter(|d| options.enabled(d.category)) {
            for span in spans(&d.regex, &text, d.group) {
                push(d.kind, d.priority, span, d.accept);
            }
        }
    });
    hits.sort_by_key(|h| (h.priority, h.start));
    let mut kept: Vec<Hit> = Vec::new();
    for h in hits {
        if kept.iter().all(|k| h.end <= k.start || h.start >= k.end) {
            kept.push(h);
        }
    }
    kept.sort_by_key(|h| h.start);
    kept
}

/// Keeps the first and last characters of longer text so entries can be told apart.
fn obscured(text: &[char]) -> String {
    let n = text.len();
    let keep = if n >= 10 {
        2
    } else if n >= 6 {
        1
    } else {
        0
    };
    text.iter()
        .enumerate()
        .map(|(i, &c)| {
            if i < keep || i + keep >= n || c.is_whitespace() {
                c
            } else {
                '•'
            }
        })
        .collect()
}

fn height(quad: &[(f64, f64); 4]) -> f64 {
    (quad[3].0 - quad[0].0).hypot(quad[3].1 - quad[0].1)
}

/// The mask for glyphs `lo..hi`: their visible strips' bounds plus alignment slack, snapped outward
/// to whole pixels and kept inside the image.
fn mask_area(page: &Page, lo: usize, hi: usize, vis: &[bool]) -> Option<Rect> {
    let runs = page.runs(lo, hi, vis);
    let boxes = runs.iter().map(|r| rect_of(&r.quad));
    let first = boxes.clone().next()?;
    let (x0, y0, x1, y1) = boxes.fold(
        (first.x, first.y, first.x + first.w, first.y + first.h),
        |a, b| {
            (
                a.0.min(b.x),
                a.1.min(b.y),
                a.2.max(b.x + b.w),
                a.3.max(b.y + b.h),
            )
        },
    );
    let h = runs.iter().map(|r| height(&r.quad)).fold(0.0, f64::max);
    let x0 = (x0 - PAD_X * h).floor().max(0.0);
    let y0 = (y0 - PAD_Y * h).floor().max(0.0);
    let x1 = (x1 + PAD_X * h).ceil().min(f64::from(page.width));
    let y1 = (y1 + PAD_Y * h).ceil().min(f64::from(page.height));
    (x1 > x0 && y1 > y0).then_some(Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Candidate masks for the visible text of `page` (`vis` as from `ocr_ui::visible`), in reading
/// order.
pub fn scan(page: &Page, vis: &[bool], options: &Options) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut base = 0;
    for line in page.lines() {
        let chars: Vec<(char, usize)> = line
            .glyphs
            .iter()
            .enumerate()
            .flat_map(|(gi, g)| {
                let ordinal = base + gi;
                let shown = vis.get(ordinal).copied().unwrap_or(false);
                let cells: Vec<char> = g.ch.chars().collect();
                let cells = if shown && !cells.iter().any(|c| c.is_control()) {
                    cells
                } else {
                    vec![HIDDEN]
                };
                cells.into_iter().map(move |c| (c, ordinal))
            })
            .collect();
        base += line.glyphs.len();
        for hit in detect(&chars, options) {
            let (lo, hi) = (chars[hit.start].1, chars[hit.end - 1].1 + 1);
            if !(lo..hi).all(|o| vis.get(o).copied().unwrap_or(false)) {
                continue;
            }
            let Some(area) = mask_area(page, lo, hi, vis) else {
                continue;
            };
            let text: Vec<char> = chars[hit.start..hit.end].iter().map(|c| c.0).collect();
            out.push(Candidate {
                kind: hit.kind,
                span: (lo, hi),
                label: obscured(&text),
                area,
            });
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied {
        added: usize,
        skipped: usize,
    },
    /// Nothing chosen is (still) visible.
    Nothing,
    /// The document or text layer changed since the list was reviewed; nothing was applied.
    Stale,
    /// The scene limits refused the masks; nothing was applied.
    Rejected,
}

/// Adds one mask per chosen candidate as a single undo step. Refuses a document that changed since
/// `reviewed` (its `revision`), and rescans with the caller's current visibility so a candidate
/// that became hidden or cropped is skipped.
pub fn apply(
    doc: &mut Document,
    page: &Page,
    vis: &[bool],
    options: &Options,
    chosen: &HashSet<Span>,
    blur: bool,
    reviewed: u64,
) -> Outcome {
    if doc.revision() != reviewed || (page.width, page.height) != (doc.width, doc.height) {
        return Outcome::Stale;
    }
    let picked: Vec<Candidate> = scan(page, vis, options)
        .into_iter()
        .filter(|c| chosen.contains(&c.span))
        .collect();
    if picked.is_empty() {
        return Outcome::Nothing;
    }
    let marks: Vec<Mark> = picked
        .iter()
        .map(|c| {
            let id = doc.id();
            let a = Point {
                x: c.area.x as f32,
                y: c.area.y as f32,
            };
            let b = Point {
                x: (c.area.x + c.area.w) as f32,
                y: (c.area.y + c.area.h) as f32,
            };
            if blur {
                Mark::privacy_blur(id, a, b)
            } else {
                Mark::redaction(id, a, b)
            }
        })
        .collect();
    if doc.add_many(marks) {
        Outcome::Applied {
            added: picked.len(),
            skipped: chosen.len().saturating_sub(picked.len()),
        }
    } else {
        Outcome::Rejected
    }
}

// --- review window ---------------------------------------------------------------------------------

/// The review window and its canvas preview. Only `Apply` changes the document.
pub struct Review {
    window: gtk::Window,
    ocr: Weak<OcrUi>,
    state: Rc<RefCell<State>>,
    area: gtk::DrawingArea,
    stack: gtk::Stack,
    spinner: gtk::Spinner,
    progress: gtk::Label,
    retry: gtk::Button,
    email: gtk::CheckButton,
    phone: gtk::CheckButton,
    ip: gtk::CheckButton,
    account: gtk::CheckButton,
    custom: gtk::Entry,
    style: gtk::DropDown,
    list: gtk::ListBox,
    summary: gtk::Label,
    status: gtk::Label,
    apply: gtk::Button,
    page: RefCell<Option<Rc<Page>>>,
    /// `Document::revision` when the list was made.
    reviewed: Cell<u64>,
    found: RefCell<Vec<Candidate>>,
    /// Matches the user switched off; everything else found is chosen.
    unchecked: RefCell<HashSet<Span>>,
    focus: Cell<Option<usize>>,
    building: Cell<bool>,
}

impl Review {
    pub fn new(
        ocr: &Rc<OcrUi>,
        state: Rc<RefCell<State>>,
        area: gtk::DrawingArea,
        parent: &gtk::ApplicationWindow,
    ) -> Rc<Self> {
        let window = gtk::Window::builder()
            .transient_for(parent)
            .destroy_with_parent(true)
            .title("Smart redaction")
            .default_width(460)
            .default_height(640)
            .build();
        window.set_titlebar(Some(&gtk::HeaderBar::new()));
        let check = |label: &str, tip: &str| {
            let b = gtk::CheckButton::with_label(label);
            b.set_active(true);
            b.set_tooltip_text(Some(tip));
            b
        };
        let custom = gtk::Entry::builder()
            .placeholder_text("Custom text to find (not case sensitive)")
            .max_length(MAX_TERM as i32)
            .build();
        custom.update_property(&[gtk::accessible::Property::Label("Custom text to redact")]);
        let style = gtk::DropDown::from_strings(&["Black redaction", "Pixelated privacy"]);
        style.update_property(&[gtk::accessible::Property::Label("Mask appearance")]);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("boxed-list");
        let label = |text: &str| {
            let l = gtk::Label::new(Some(text));
            l.set_xalign(0.0);
            l.set_wrap(true);
            l
        };
        let this = Rc::new(Self {
            window,
            ocr: Rc::downgrade(ocr),
            state,
            area,
            stack: gtk::Stack::new(),
            spinner: gtk::Spinner::new(),
            progress: label(""),
            retry: gtk::Button::with_label("Try again"),
            email: check("Email addresses", "Addresses such as name@example.com"),
            phone: check(
                "Phone numbers",
                "International or national numbers with separators; dates are ignored",
            ),
            ip: check("IP addresses", "IPv4 and IPv6"),
            account: check(
                "Card, IBAN and account numbers",
                "Card numbers, checksum-valid IBANs, long digit runs and numbers after an account label",
            ),
            custom,
            style,
            list,
            summary: label(""),
            status: label(""),
            apply: gtk::Button::with_label("Redact selected"),
            page: RefCell::new(None),
            reviewed: Cell::new(0),
            found: RefCell::new(Vec::new()),
            unchecked: RefCell::new(HashSet::new()),
            focus: Cell::new(None),
            building: Cell::new(false),
        });
        this.build();
        this.connect();
        this
    }

    fn build(self: &Rc<Self>) {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
        root.set_margin_start(14);
        root.set_margin_end(14);
        root.set_margin_top(14);
        root.set_margin_bottom(14);
        // Waiting page: recognition runs off the UI thread.
        let wait = gtk::Box::new(gtk::Orientation::Vertical, 12);
        wait.set_vexpand(true);
        wait.set_valign(gtk::Align::Center);
        self.spinner.set_width_request(32);
        self.spinner.set_height_request(32);
        self.progress.set_justify(gtk::Justification::Center);
        self.retry.set_halign(gtk::Align::Center);
        self.retry.set_visible(false);
        wait.append(&self.spinner);
        wait.append(&self.progress);
        wait.append(&self.retry);
        self.stack.add_named(&wait, Some("wait"));
        // Review page.
        let review = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let intro = gtk::Label::new(Some(
            "Choose what to look for, untick any match you want to keep, then apply. The canvas shows each mask in red; nothing changes until you apply.",
        ));
        intro.set_wrap(true);
        intro.set_xalign(0.0);
        review.append(&intro);
        for b in [&self.email, &self.phone, &self.ip, &self.account] {
            review.append(b);
        }
        review.append(&self.custom);
        let style_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        style_row.append(&gtk::Label::new(Some("Cover with")));
        style_row.append(&self.style);
        review.append(&style_row);
        let tools = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let all = gtk::Button::with_label("Select all");
        let none = gtk::Button::with_label("Select none");
        let refresh = gtk::Button::with_label("Refresh");
        refresh.set_tooltip_text(Some("Search again in the current image"));
        for b in [&all, &none, &refresh] {
            tools.append(b);
        }
        review.append(&tools);
        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .min_content_height(180)
            .child(&self.list)
            .build();
        review.append(&scroller);
        review.append(&self.summary);
        self.stack.add_named(&review, Some("review"));
        root.append(&self.stack);
        self.status.set_visible(false);
        root.append(&self.status);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.set_tooltip_text(Some("Close without changing the image · Esc"));
        self.apply.add_css_class("suggested-action");
        self.apply.set_sensitive(false);
        self.apply.set_tooltip_text(Some(
            "Cover every ticked match with a mask, as one undo step",
        ));
        buttons.append(&cancel);
        buttons.append(&self.apply);
        root.append(&buttons);
        self.window.set_child(Some(&root));
        let window = self.window.clone();
        cancel.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.close()
        ));
        all.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.choose_all(true)
        ));
        none.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.choose_all(false)
        ));
        refresh.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.rescan()
        ));
    }

    fn connect(self: &Rc<Self>) {
        for b in [&self.email, &self.phone, &self.ip, &self.account] {
            b.connect_toggled(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_| this.rescan()
            ));
        }
        self.custom.connect_changed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.rescan()
        ));
        self.style.connect_selected_notify(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.summarise()
        ));
        self.list.connect_row_selected(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, row| {
                if this.building.get() {
                    return;
                }
                this.focus
                    .set(row.and_then(|r| usize::try_from(r.index()).ok()));
                this.area.queue_draw();
            }
        ));
        self.retry.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                if let Some(ocr) = this.ocr.upgrade() {
                    ocr.review_retry();
                }
            }
        ));
        self.apply.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.apply_chosen()
        ));
        self.window.connect_close_request(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                if let Some(ocr) = this.ocr.upgrade() {
                    ocr.review_closed();
                }
                glib::Propagation::Proceed
            }
        ));
        let keys = gtk::EventControllerKey::new();
        let window = self.window.clone();
        keys.connect_key_pressed(glib::clone!(
            #[weak]
            window,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    window.close();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.window.add_controller(keys);
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn waiting(&self) {
        *self.page.borrow_mut() = None;
        self.found.borrow_mut().clear();
        self.stack.set_visible_child_name("wait");
        self.spinner.start();
        self.retry.set_visible(false);
        self.progress
            .set_text("Recognising text locally… this can take a few seconds for a large screenshot.\nThe screenshot never leaves this computer.");
        self.apply.set_sensitive(false);
        self.area.queue_draw();
    }

    pub fn failed(&self, error: &str) {
        *self.page.borrow_mut() = None;
        self.spinner.stop();
        self.stack.set_visible_child_name("wait");
        self.progress
            .set_text(&format!("Text recognition failed: {error}"));
        self.retry.set_visible(true);
        self.apply.set_sensitive(false);
    }

    /// The text layer is ready: list candidates from `page`.
    pub fn ready(self: &Rc<Self>, page: Rc<Page>) {
        self.spinner.stop();
        *self.page.borrow_mut() = Some(page);
        self.stack.set_visible_child_name("review");
        self.rescan();
    }

    fn options(&self) -> Options {
        Options {
            email: self.email.is_active(),
            phone: self.phone.is_active(),
            ip: self.ip.is_active(),
            account: self.account.is_active(),
            custom: self.custom.text().to_string(),
        }
    }

    fn say(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
    }

    /// Searches the current image again and rebuilds the list, keeping earlier choices.
    fn rescan(self: &Rc<Self>) {
        let Some(page) = self.page.borrow().clone() else {
            return;
        };
        let options = self.options();
        let (vis, revision) = match self.state.try_borrow() {
            Ok(state) => (visible(&state, &page), state.doc.revision()),
            Err(_) => return,
        };
        self.reviewed.set(revision);
        let found = scan(&page, &vis, &options);
        self.building.set(true);
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for c in &found {
            let row = gtk::CheckButton::with_label(&format!("{}  ·  {}", c.kind, c.label));
            row.set_active(!self.unchecked.borrow().contains(&c.span));
            row.update_property(&[gtk::accessible::Property::Label(&format!(
                "Redact {}",
                c.kind
            ))]);
            let span = c.span;
            row.connect_toggled(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |b| {
                    if this.building.get() {
                        return;
                    }
                    if b.is_active() {
                        this.unchecked.borrow_mut().remove(&span);
                    } else {
                        this.unchecked.borrow_mut().insert(span);
                    }
                    this.summarise();
                    this.area.queue_draw();
                }
            ));
            self.list.append(&row);
        }
        self.building.set(false);
        *self.found.borrow_mut() = found;
        self.focus.set(None);
        let term = options.custom.trim().chars().count();
        self.say(if (1..MIN_TERM).contains(&term) {
            "Custom text needs at least 2 characters."
        } else {
            ""
        });
        self.summarise();
        self.area.queue_draw();
    }

    fn chosen(&self) -> HashSet<Span> {
        let off = self.unchecked.borrow();
        self.found
            .borrow()
            .iter()
            .map(|c| c.span)
            .filter(|s| !off.contains(s))
            .collect()
    }

    fn choose_all(&self, on: bool) {
        {
            let found = self.found.borrow();
            let mut off = self.unchecked.borrow_mut();
            for c in found.iter() {
                if on {
                    off.remove(&c.span);
                } else {
                    off.insert(c.span);
                }
            }
        }
        self.building.set(true);
        let mut child = self.list.first_child();
        while let Some(row) = child {
            if let Some(check) = row.first_child().and_downcast::<gtk::CheckButton>() {
                check.set_active(on);
            }
            child = row.next_sibling();
        }
        self.building.set(false);
        self.summarise();
        self.area.queue_draw();
    }

    fn summarise(&self) {
        let (chosen, total) = (self.chosen().len(), self.found.borrow().len());
        self.summary.set_text(&if total == 0 {
            "Nothing matching was found in the visible text.".to_string()
        } else {
            format!("{chosen} of {total} selected")
        });
        self.apply.set_label(&format!("Redact {chosen} selected"));
        self.apply.set_sensitive(chosen > 0);
    }

    fn apply_chosen(self: &Rc<Self>) {
        let (Some(page), Some(ocr)) = (self.page.borrow().clone(), self.ocr.upgrade()) else {
            return;
        };
        // The list must come from the text layer that is still current for this document.
        if !ocr.is_current(&page) {
            self.rescan();
            return self.say("The text layer was refreshed. Review the new list and apply again.");
        }
        let blur = self.style.selected() == 1;
        let outcome = {
            let mut state = self.state.borrow_mut();
            let vis = visible(&state, &page);
            apply(
                &mut state.doc,
                &page,
                &vis,
                &self.options(),
                &self.chosen(),
                blur,
                self.reviewed.get(),
            )
        };
        match outcome {
            Outcome::Applied { added, skipped } => {
                ocr.review_applied(added, skipped);
                self.window.close();
            }
            Outcome::Stale => {
                self.rescan();
                self.say("The image changed after this list was made, so nothing was applied. The list is refreshed: review it and apply again.");
            }
            Outcome::Nothing => {
                self.rescan();
                self.say("None of the selected text is visible any more. Nothing was applied.");
            }
            Outcome::Rejected => {
                self.say("The masks could not be added (too many objects?). Nothing was changed.");
            }
        }
    }

    /// Candidate masks over the canvas; `cr` is in image coordinates. Hidden once the document has
    /// changed since the list was made, because the areas may no longer be right.
    pub fn paint(&self, cr: &Context, state: &State) -> Result<(), cairo::Error> {
        if self.reviewed.get() != state.doc.revision() {
            return Ok(());
        }
        let (Ok(found), Ok(off)) = (self.found.try_borrow(), self.unchecked.try_borrow()) else {
            return Ok(());
        };
        let hair = 1.0 / state.transform.scale.max(1e-6);
        cr.save()?;
        for (i, c) in found.iter().enumerate() {
            let focus = self.focus.get() == Some(i);
            let a = c.area;
            cr.rectangle(a.x, a.y, a.w, a.h);
            if off.contains(&c.span) {
                cr.set_source_rgba(0.8, 0.8, 0.8, 0.9);
                cr.set_line_width(hair * if focus { 2.5 } else { 1.0 });
                cr.set_dash(&[4.0 * hair, 3.0 * hair], 0.0);
                cr.stroke()?;
                cr.set_dash(&[], 0.0);
            } else {
                cr.set_source_rgba(0.95, 0.2, 0.2, if focus { 0.55 } else { 0.3 });
                cr.fill_preserve()?;
                cr.set_source_rgba(0.95, 0.2, 0.2, 0.95);
                cr.set_line_width(hair * if focus { 3.0 } else { 1.5 });
                cr.stroke()?;
            }
        }
        cr.restore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document::{Bounds, Crop},
        ocr::{Glyph, Line},
    };
    use anyhow::Result;

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

    fn page(lines: &[&str]) -> Page {
        Page::new(
            1200,
            400,
            lines
                .iter()
                .enumerate()
                .map(|(i, t)| line(10.0, 10.0 + 40.0 * i as f64, t))
                .collect(),
        )
    }

    /// `(kind, matched text)` pairs of a one-line page; the text only exists inside the test.
    fn found(text: &str, options: &Options) -> Vec<(&'static str, String)> {
        let page = page(&[text]);
        let vis = vec![true; page.len()];
        let chars: Vec<char> = text.chars().collect();
        scan(&page, &vis, options)
            .into_iter()
            .map(|c| (c.kind, chars[c.span.0..c.span.1].iter().collect()))
            .collect()
    }

    fn all(text: &str) -> Vec<(&'static str, String)> {
        found(text, &Options::default())
    }

    fn one(text: &str, kind: &str, matched: &str) {
        let hits = all(text);
        assert_eq!(
            hits.len(),
            1,
            "exactly one match expected in {text:?}: {hits:?}"
        );
        assert_eq!((hits[0].0, hits[0].1.as_str()), (kind, matched));
    }

    #[test]
    fn detector_patterns_compile() {
        let n = DETECTORS.with(|d| d.len());
        assert_eq!(n, 8, "every constant pattern must compile");
    }

    #[test]
    fn emails_phones_and_ips() {
        one(
            "Mail jane.doe+x@mail.example.co.uk now",
            "Email",
            "jane.doe+x@mail.example.co.uk",
        );
        one("Mail me at a@b.com.", "Email", "a@b.com");
        one(
            "Call +1 (555) 123-4567 today",
            "Phone number",
            "+1 (555) 123-4567",
        );
        one("Call 555-123-4567.", "Phone number", "555-123-4567");
        one("Call 020 7946 0958", "Phone number", "020 7946 0958");
        one("host 192.168.1.20 up", "IP address", "192.168.1.20");
        one("host 2001:db8::1 up", "IP address", "2001:db8::1");
    }

    #[test]
    fn lookalikes_are_not_matches() {
        for text in [
            "Released 2024-10-31 and 31.10.2024",
            "version 1.2.3 build 300.400.500.600",
            "time 10:30:45 ratio 3:2",
            "Total 1 234 567 items",
            "pi 3.14159",
            "user@localhost and @handle",
            "12345",
        ] {
            assert_eq!(all(text), vec![], "{text:?}");
        }
    }

    #[test]
    fn cards_ibans_and_account_numbers() {
        one(
            "Card 4111 1111 1111 1111 exp",
            "Card number",
            "4111 1111 1111 1111",
        );
        one(
            "Card 4111-1111-1111-1111",
            "Card number",
            "4111-1111-1111-1111",
        );
        // One misread digit breaks the checksum; a four-group 16 digit number still counts.
        one(
            "Card 4111 1111 1111 1112",
            "Card number",
            "4111 1111 1111 1112",
        );
        one(
            "Pay DE89 3704 0044 0532 0130 00 now",
            "IBAN",
            "DE89 3704 0044 0532 0130 00",
        );
        // A wrong checksum is not an IBAN (the digits may still be caught as another number).
        assert!(
            all("DE89 3704 0044 0532 0130 01 x")
                .iter()
                .all(|(kind, _)| *kind != "IBAN")
        );
        one("Account: 12345678", "Account number", "12345678");
        one("Acct No. 987654 closed", "Account number", "987654");
        one("ref 123456789012", "Account number", "123456789012");
        assert_eq!(
            all("ref 1234567").len(),
            0,
            "seven plain digits are not an account number"
        );
    }

    #[test]
    fn categories_and_custom_text_are_chosen_independently() {
        let text = "ACME Corp mail a@b.com call +44 20 7946 0958";
        let only = |email, phone, custom: &str| Options {
            email,
            phone,
            ip: false,
            account: false,
            custom: custom.into(),
        };
        assert_eq!(found(text, &only(true, false, "")).len(), 1);
        assert_eq!(found(text, &only(false, true, "")).len(), 1);
        let hits = found(text, &only(false, false, "acme corp"));
        assert_eq!(hits, vec![("Custom text", "ACME Corp".to_string())]);
        assert!(
            found(text, &only(false, false, "a")).is_empty(),
            "one character is too broad"
        );
        // Regex syntax in custom text is literal.
        assert_eq!(
            found("cost (a+b)* x", &only(false, false, "(a+b)*")).len(),
            1
        );
        assert!(found("aab", &only(false, false, "a+b")).is_empty());
    }

    #[test]
    fn overlaps_go_to_custom_text_then_the_more_specific_kind() {
        let text = "mail jane@example.com";
        let options = Options {
            custom: "example.com".into(),
            ..Options::default()
        };
        let hits = found(text, &options);
        assert_eq!(hits, vec![("Custom text", "example.com".to_string())]);
        // An account label does not make a card number a second match.
        assert_eq!(all("Card: 4111 1111 1111 1111").len(), 1);
    }

    #[test]
    fn hidden_glyphs_break_matches_and_never_complete_them() -> Result<()> {
        let text = "write to jane@example.com please";
        let page = page(&[text]);
        let mut vis = vec![true; page.len()];
        assert_eq!(scan(&page, &vis, &Options::default()).len(), 1);
        // A glyph in the middle of the address is under a mask: no partial or bridged match.
        vis[text.find("example").unwrap()] = false;
        assert!(scan(&page, &vis, &Options::default()).is_empty());
        // A mask drawn after recognition and a crop do the same through the live document.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut state = State::new(Document::new(image::RgbaImage::new(1200, 400))?, tx);
        let live = |state: &State| scan(&page, &visible(state, &page), &Options::default()).len();
        assert_eq!(live(&state), 1);
        let id = state.doc.id();
        state.doc.add(Mark::redaction(
            id,
            Point { x: 100.0, y: 0.0 },
            Point { x: 140.0, y: 40.0 },
        ));
        assert_eq!(live(&state), 0);
        state.doc.undo();
        state.doc.set_crop(Some(Crop {
            x: 0,
            y: 0,
            width: 150,
            height: 400,
        }));
        assert_eq!(live(&state), 0, "the address is cut off by the crop");
        Ok(())
    }

    #[test]
    fn labels_never_show_the_whole_text() {
        let text: Vec<char> = "jane.doe@example.com".chars().collect();
        let label = obscured(&text);
        assert_eq!(label.chars().count(), text.len());
        assert!(label.starts_with("ja") && label.ends_with("om") && label.contains('•'));
        assert_eq!(obscured(&"1234".chars().collect::<Vec<_>>()), "••••");
        // End to end: the listed label differs from the text it stands for.
        let line = "mail jane.doe@example.com now";
        let page = page(&[line]);
        let vis = vec![true; page.len()];
        let c = &scan(&page, &vis, &Options::default())[0];
        assert_ne!(c.label, "jane.doe@example.com");
        assert!(c.label.contains('•'));
    }

    fn setup(lines: &[&str]) -> Result<(Document, Page, Vec<Candidate>)> {
        let page = page(lines);
        let doc = Document::new(image::RgbaImage::new(1200, 400))?;
        let vis = vec![true; page.len()];
        let found = scan(&page, &vis, &Options::default());
        Ok((doc, page, found))
    }

    #[test]
    fn apply_masks_only_the_chosen_matches_as_one_undo() -> Result<()> {
        let (mut doc, page, found) = setup(&["a@b.com and 192.168.1.1", "call +1 555 123 4567"])?;
        assert_eq!(found.len(), 3);
        let vis = vec![true; page.len()];
        let revision = doc.revision();
        let chosen: HashSet<Span> = [found[0].span, found[2].span].into();
        let outcome = apply(
            &mut doc,
            &page,
            &vis,
            &Options::default(),
            &chosen,
            false,
            revision,
        );
        assert_eq!(
            outcome,
            Outcome::Applied {
                added: 2,
                skipped: 0
            }
        );
        assert_eq!(doc.marks.len(), 2);
        assert!(doc.marks.iter().all(|m| m.is_redaction()));
        // Every chosen glyph cell is covered; the unchosen match stays open.
        let cell = |span: Span| {
            let r = page.runs(span.0, span.1, &vis);
            let b = rect_of(&r[0].quad);
            Bounds {
                x: b.x as f32,
                y: b.y as f32,
                width: b.w as f32,
                height: b.h as f32,
            }
        };
        let privacy = doc.privacy_snapshot();
        assert!(privacy.is_private(cell(found[0].span)) && privacy.is_private(cell(found[2].span)));
        assert!(!privacy.is_private(cell(found[1].span)));
        doc.undo();
        assert!(doc.marks.is_empty(), "one undo removes every mask");
        Ok(())
    }

    #[test]
    fn pixelated_style_and_empty_choice() -> Result<()> {
        let (mut doc, page, found) = setup(&["a@b.com"])?;
        let vis = vec![true; page.len()];
        let rev = doc.revision();
        let none = HashSet::new();
        assert_eq!(
            apply(
                &mut doc,
                &page,
                &vis,
                &Options::default(),
                &none,
                false,
                rev
            ),
            Outcome::Nothing
        );
        assert!(doc.marks.is_empty() && !doc.can_undo());
        let chosen: HashSet<Span> = [found[0].span].into();
        apply(
            &mut doc,
            &page,
            &vis,
            &Options::default(),
            &chosen,
            true,
            rev,
        );
        assert!(matches!(
            doc.marks[0].kind,
            crate::document::MarkKind::PrivacyBlur
        ));
        Ok(())
    }

    #[test]
    fn stale_documents_and_hidden_matches_are_not_masked() -> Result<()> {
        let (mut doc, page, found) = setup(&["a@b.com 10.0.0.1"])?;
        let vis = vec![true; page.len()];
        let reviewed = doc.revision();
        let chosen: HashSet<Span> = found.iter().map(|c| c.span).collect();
        // The document changed after review: refuse, change nothing.
        let id = doc.id();
        doc.add(Mark::redaction(
            id,
            Point { x: 0.0, y: 300.0 },
            Point { x: 5.0, y: 305.0 },
        ));
        let before = doc.marks.len();
        assert_eq!(
            apply(
                &mut doc,
                &page,
                &vis,
                &Options::default(),
                &chosen,
                false,
                reviewed
            ),
            Outcome::Stale
        );
        assert_eq!(doc.marks.len(), before);
        // A page from another image size is stale too.
        let small = Page::new(100, 100, Vec::new());
        let now = doc.revision();
        assert_eq!(
            apply(
                &mut doc,
                &small,
                &[],
                &Options::default(),
                &chosen,
                false,
                now
            ),
            Outcome::Stale
        );
        // Current revision, but the first match became hidden: it is skipped, the rest is masked.
        let mut hidden = vis.clone();
        hidden[0] = false;
        assert_eq!(
            apply(
                &mut doc,
                &page,
                &hidden,
                &Options::default(),
                &chosen,
                false,
                now
            ),
            Outcome::Applied {
                added: 1,
                skipped: 1
            }
        );
        assert_eq!(doc.marks.len(), before + 1);
        Ok(())
    }
}
