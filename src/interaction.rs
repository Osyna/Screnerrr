use crate::{
    document::{
        Affine, Bounds, Crop, Document, MAX_TEXT, Mark, MarkEdit, MarkKind, TextStyle, step_radius,
    },
    shapes::{Point, Recognition, Recognized},
};
use std::{collections::HashMap, sync::mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Smart,
    Ink,
    Arrow,
    Line,
    Rectangle,
    Ellipse,
    Text,
    Crop,
    Redact,
    Step,
    Spotlight,
    Magnify,
}
#[derive(Clone, Copy)]
pub struct Transform {
    pub scale: f64,
    pub x: f64,
    pub y: f64,
}
impl Transform {
    pub fn raw(self, x: f64, y: f64) -> Point {
        Point {
            x: ((x - self.x) / self.scale) as f32,
            y: ((y - self.y) / self.scale) as f32,
        }
    }
    pub fn point(self, x: f64, y: f64, doc: &Document) -> Point {
        let p = self.raw(x, y);
        Point {
            x: p.x.clamp(0., doc.width as f32),
            y: p.y.clamp(0., doc.height as f32),
        }
    }
    pub fn contains(self, x: f64, y: f64, doc: &Document) -> bool {
        let p = self.raw(x, y);
        p.x >= 0. && p.y >= 0. && p.x <= doc.width as f32 && p.y <= doc.height as f32
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Guide {
    pub a: Point,
    pub b: Point,
    pub spacing: bool,
}
#[derive(Clone, Copy)]
pub enum DragKind {
    Move,
    Resize(usize),
    Rotate,
}
pub enum Gesture {
    Draw(Mark),
    Crop {
        start: Point,
        preview: Option<Crop>,
    },
    Transform {
        kind: DragKind,
        bounds: Bounds,
        matrix: Affine,
        targets: Vec<Bounds>,
        guides: Vec<Guide>,
    },
    Marquee {
        start: Point,
        end: Point,
        base: Vec<u64>,
    },
    Source {
        start: Point,
        end: Point,
    },
}
pub struct Correction {
    pub original: Vec<Point>,
    pub closed: bool,
    pub choices: Vec<Recognized>,
    pub manual: bool,
}
#[derive(Clone)]
pub struct TextDraft {
    pub id: Option<u64>,
    pub at: Point,
    pub value: String,
    pub box_width: f32,
    pub background: bool,
    /// Document-space effective size, independent of `box_width`.
    pub font_size: f64,
    pub text_style: TextStyle,
}
pub struct State {
    pub doc: Document,
    pub tool: Tool,
    pub color: [f64; 4],
    pub width: f64,
    /// Arrowhead multiplier for new arrows (0 = none); lines always use 0.
    pub head_size: f64,
    pub text_size: f64,
    /// Style given to new captions.
    pub text_style: TextStyle,
    pub selected: Vec<u64>,
    pub hovered: Option<u64>,
    pub gesture: Option<Gesture>,
    pub origin: (f64, f64),
    pub anchor: Point,
    pub transform: Transform,
    pub zoom: Option<f64>,
    pub model_ready: bool,
    pub request: mpsc::Sender<(u64, Vec<Point>)>,
    pub corrections: HashMap<u64, Correction>,
    pub text_draft: Option<TextDraft>,
    pub magnifier_source: Option<Bounds>,
}
impl State {
    pub fn new(doc: Document, request: mpsc::Sender<(u64, Vec<Point>)>) -> Self {
        Self {
            doc,
            request,
            tool: Tool::Smart,
            color: [245. / 255., 155. / 255., 52. / 255., 1.],
            width: 4.,
            head_size: 1.,
            text_size: 24.,
            text_style: TextStyle::default(),
            selected: vec![],
            hovered: None,
            gesture: None,
            origin: (0., 0.),
            anchor: pt(0., 0.),
            transform: Transform {
                scale: 1.,
                x: 0.,
                y: 0.,
            },
            zoom: None,
            model_ready: false,
            corrections: HashMap::new(),
            text_draft: None,
            magnifier_source: None,
        }
    }
    pub fn selected_mark(&self) -> Option<&Mark> {
        if self.selected.len() == 1 {
            self.doc.mark(self.selected[0])
        } else {
            None
        }
    }
    pub fn style(&self) -> ([f64; 4], f64) {
        self.selected
            .first()
            .and_then(|id| self.doc.mark(*id))
            .map_or((self.color, self.width), |m| (m.color, m.width))
    }
    /// `color` carries alpha; a field left `None` is not touched on any selected mark, so
    /// changing the width keeps mixed colours (and the reverse).
    pub fn set_style(&mut self, color: Option<[f64; 4]>, width: Option<f64>) {
        let (old_c, old_w) = self.style();
        self.color = color.unwrap_or(old_c);
        self.width = width.unwrap_or(old_w);
        self.doc.edit_marks(
            &self.selected,
            &MarkEdit {
                rgb: color.map(|c| [c[0], c[1], c[2]]),
                alpha: color.map(|c| c[3]),
                width,
                ..MarkEdit::default()
            },
        );
    }
    /// Colour only: every selected mark keeps its own opacity.
    pub fn set_rgb(&mut self, rgb: [f64; 3]) {
        self.color = [rgb[0], rgb[1], rgb[2], self.style().0[3]];
        self.doc.edit_marks(
            &self.selected,
            &MarkEdit {
                rgb: Some(rgb),
                ..MarkEdit::default()
            },
        );
    }
    /// Opacity only: every selected mark keeps its own colour.
    pub fn set_opacity(&mut self, alpha: f64) {
        self.color = [self.color[0], self.color[1], self.color[2], alpha];
        self.doc.edit_marks(
            &self.selected,
            &MarkEdit {
                alpha: Some(alpha),
                ..MarkEdit::default()
            },
        );
    }
    /// Rich text style shown in the UI: the selected text's, else the default for new captions.
    pub fn text_style(&self) -> TextStyle {
        self.selected
            .iter()
            .filter_map(|id| self.doc.mark(*id))
            .find(|m| matches!(m.kind, MarkKind::Text { .. }))
            .map_or(self.text_style, |m| m.text_style)
    }
    /// Sets the style of new captions and of the selected text marks (one undo).
    pub fn set_text_style(&mut self, style: TextStyle) {
        self.text_style = style;
        self.doc.edit_marks(
            &self.selected,
            &MarkEdit {
                bold: Some(style.bold),
                italic: Some(style.italic),
                align: Some(style.align),
                ..MarkEdit::default()
            },
        );
    }
    /// Arrowhead multiplier shown in the UI: the selected connector's, else the tool default.
    pub fn tip(&self) -> f64 {
        self.selected
            .iter()
            .filter_map(|id| self.doc.mark(*id))
            .find(|m| matches!(m.kind, MarkKind::Connector { .. }))
            .map_or(self.head_size, |m| m.head_size)
    }
    pub fn set_tip(&mut self, size: f64) {
        self.head_size = size;
        self.doc.set_head_size(&self.selected, size);
    }
    /// Document-space font size shown in the UI: the selected text's, else the tool default.
    pub fn font(&self) -> f64 {
        self.selected
            .iter()
            .find_map(|id| self.doc.text_size(*id))
            .unwrap_or(self.text_size)
    }
    pub fn set_font(&mut self, size: f64) {
        self.text_size = size;
        self.doc.set_font_size(&self.selected, size);
    }
    pub fn cancel(&mut self) {
        self.gesture = None;
        self.hovered = None;
    }
    pub fn clean_selection(&mut self) {
        self.selected.retain(|id| self.doc.mark(*id).is_some());
    }
    pub fn edit_text(&mut self, id: u64) -> bool {
        let Some(m) = self.doc.mark(id) else {
            return false;
        };
        let MarkKind::Text {
            value,
            box_width,
            background,
        } = &m.kind
        else {
            return false;
        };
        self.text_draft = Some(TextDraft {
            id: Some(id),
            at: m.transform.apply(m.points[0]),
            value: value.clone(),
            box_width: *box_width,
            background: *background,
            font_size: self.doc.text_size(id).unwrap_or(self.text_size),
            text_style: m.text_style,
        });
        self.cancel();
        true
    }
    /// The mark the open caption draft would become, for a live preview (see
    /// `Document::paint_text_preview`): the original with its transform when editing, a plain
    /// mark at `draft.at` in the current colour otherwise. Never touches the document.
    pub fn draft_mark(&self) -> Option<Mark> {
        let d = self.text_draft.as_ref()?;
        if let Some(id) = d.id {
            return self.doc.retexted(
                id,
                &d.value,
                d.box_width,
                d.background,
                d.font_size,
                d.text_style,
            );
        }
        let mut m = Mark::text(0, d.at, d.value.clone(), self.color, self.width);
        m.font_size = d.font_size;
        m.text_style = d.text_style;
        if let MarkKind::Text {
            box_width,
            background,
            ..
        } = &mut m.kind
        {
            *box_width = d.box_width;
            *background = d.background;
        }
        Some(m)
    }
    pub fn commit_text(&mut self, value: String, box_width: f32, background: bool, font_size: f64) {
        let Some(draft) = self.text_draft.take() else {
            return;
        };
        if let Some(id) = draft.id {
            self.doc.set_text_styled(
                id,
                &value,
                box_width,
                background,
                font_size,
                draft.text_style,
            );
        } else if !value.trim().is_empty() && value.len() <= MAX_TEXT {
            let id = self.doc.id();
            let mut m = Mark::text(id, draft.at, value, self.color, self.width);
            m.font_size = font_size;
            m.text_style = draft.text_style;
            if let MarkKind::Text {
                box_width: w,
                background: b,
                ..
            } = &mut m.kind
            {
                *w = box_width;
                *b = background;
            }
            self.doc.add(m);
            self.selected = vec![id];
        }
    }
    pub fn begin(&mut self, x: f64, y: f64, shift: bool) -> Option<&'static str> {
        if self.text_draft.is_some() {
            return None;
        }
        self.cancel();
        self.origin = (x, y);
        self.anchor = self.transform.raw(x, y);
        if self.tool == Tool::Select && !shift {
            if let Some(handle) = self.handle_at(x, y) {
                let bounds = self.doc.selection_bounds(&self.selected)?;
                self.gesture = Some(Gesture::Transform {
                    kind: if handle == 8 {
                        DragKind::Rotate
                    } else {
                        DragKind::Resize(handle)
                    },
                    bounds,
                    matrix: Affine::identity(),
                    targets: self.targets(),
                    guides: vec![],
                });
                return None;
            }
        }
        if !self.transform.contains(x, y, &self.doc) {
            if self.tool == Tool::Select && !shift {
                self.selected.clear()
            }
            return None;
        }
        let p = self.transform.point(x, y, &self.doc);
        self.anchor = p;
        match self.tool {
            Tool::Select => {
                if let Some(id) = self.doc.hit_test(p, (6. / self.transform.scale) as f32) {
                    let group = self.doc.expand_groups(&[id]);
                    if shift {
                        if group.iter().all(|id| self.selected.contains(id)) {
                            self.selected.retain(|id| !group.contains(id));
                            return None;
                        } else {
                            for id in group {
                                if !self.selected.contains(&id) {
                                    self.selected.push(id)
                                }
                            }
                        }
                    } else if !self.selected.contains(&id) {
                        self.selected = group;
                    }
                    if let Some(bounds) = self.doc.selection_bounds(&self.selected) {
                        self.gesture = Some(Gesture::Transform {
                            kind: DragKind::Move,
                            bounds,
                            matrix: Affine::identity(),
                            targets: self.targets(),
                            guides: vec![],
                        });
                    }
                } else {
                    if !shift {
                        self.selected.clear()
                    }
                    self.gesture = Some(Gesture::Marquee {
                        start: p,
                        end: p,
                        base: self.selected.clone(),
                    });
                }
            }
            Tool::Text => {
                if let Some(id) = self.doc.hit_test(p, 0.) {
                    if self.edit_text(id) {
                        self.selected = vec![id];
                        return None;
                    }
                }
                self.text_draft = Some(TextDraft {
                    id: None,
                    at: p,
                    value: String::new(),
                    box_width: 0.,
                    background: false,
                    font_size: self.text_size,
                    text_style: self.text_style,
                });
            }
            Tool::Step => {
                let (id, number) = (self.doc.id(), self.doc.next_step_number());
                self.gesture = Some(Gesture::Draw(Mark::step(
                    id, p, number, self.color, self.width,
                )));
            }
            Tool::Crop => {
                self.gesture = Some(Gesture::Crop {
                    start: p,
                    preview: None,
                })
            }
            Tool::Magnify => {
                if let Some(source) = self.magnifier_source.take() {
                    let id = self.doc.id();
                    let w = (source.width * 2.).min(self.doc.width as f32);
                    let h = (source.height * 2.).min(self.doc.height as f32);
                    let left = p.x.min(self.doc.width as f32 - w);
                    let top = p.y.min(self.doc.height as f32 - h);
                    let mut m = Mark::path(
                        id,
                        vec![pt(left, top), pt(left + w, top + h)],
                        true,
                        self.color,
                        self.width,
                    );
                    m.kind = MarkKind::Magnifier { source, zoom: 2. };
                    self.doc.add(m);
                    self.selected = vec![id];
                    return Some(
                        "Magnifier placed · V moves/resizes it; use Lens zoom in the selection bar.",
                    );
                } else {
                    self.gesture = Some(Gesture::Source { start: p, end: p });
                }
            }
            Tool::Spotlight => {
                self.gesture = Some(Gesture::Draw(Mark::spotlight(self.doc.id(), p, p)));
            }
            _ => {
                let mut m = Mark::path(self.doc.id(), vec![p], false, self.color, self.width);
                if matches!(self.tool, Tool::Arrow | Tool::Line) {
                    m.kind = MarkKind::Connector {
                        start: self.doc.endpoint(p, (10. / self.transform.scale) as f32),
                        end: self.doc.endpoint(p, 0.),
                    };
                    m.points.push(p);
                    m.head_size = if self.tool == Tool::Line {
                        0.
                    } else {
                        self.head_size
                    };
                }
                if self.tool == Tool::Redact {
                    m.kind = MarkKind::Redaction;
                    m.points.push(p);
                    m.closed = true;
                }
                self.gesture = Some(Gesture::Draw(m));
            }
        }
        None
    }
    fn targets(&self) -> Vec<Bounds> {
        self.doc
            .marks
            .iter()
            .filter(|m| !self.selected.contains(&m.id))
            .filter_map(|m| self.doc.selection_bounds(&[m.id]))
            .collect()
    }
    pub fn update(&mut self, dx: f64, dy: f64, shift: bool, alt: bool) {
        let raw = self.transform.raw(self.origin.0 + dx, self.origin.1 + dy);
        let p = self
            .transform
            .point(self.origin.0 + dx, self.origin.1 + dy, &self.doc);
        let start = self.anchor;
        match &mut self.gesture {
            Some(Gesture::Transform {
                kind,
                bounds,
                matrix,
                targets,
                guides,
            }) => {
                guides.clear();
                let tolerance = (6. / self.transform.scale) as f32;
                *matrix = match *kind {
                    DragKind::Move => {
                        let mut delta = pt(
                            (dx / self.transform.scale) as f32,
                            (dy / self.transform.scale) as f32,
                        );
                        if !alt {
                            let result = snap_move(
                                *bounds,
                                delta,
                                targets,
                                self.doc.width as f32,
                                self.doc.height as f32,
                                tolerance,
                            );
                            delta = result.0;
                            *guides = result.1;
                        }
                        Affine::translation(delta)
                    }
                    DragKind::Resize(i) => {
                        let mut q = raw;
                        if !alt {
                            let tiny = Bounds {
                                x: q.x,
                                y: q.y,
                                width: 0.,
                                height: 0.,
                            };
                            let result = snap_move(
                                tiny,
                                pt(0., 0.),
                                targets,
                                self.doc.width as f32,
                                self.doc.height as f32,
                                tolerance,
                            );
                            q.x += result.0.x;
                            q.y += result.0.y;
                            *guides = result.1;
                        }
                        let text = has_text(&self.doc, &self.selected);
                        resize_matrix(*bounds, i, q, shift || text, text)
                    }
                    DragKind::Rotate => {
                        let center =
                            pt(bounds.x + bounds.width / 2., bounds.y + bounds.height / 2.);
                        let mut angle = (raw.y - center.y).atan2(raw.x - center.x)
                            - (start.y - center.y).atan2(start.x - center.x);
                        if shift {
                            let step = std::f32::consts::PI / 12.;
                            angle = (angle / step).round() * step;
                        }
                        Affine::rotation_about(angle, center)
                    }
                };
            }
            Some(Gesture::Marquee { start, end, base }) => {
                *end = p;
                let box_ = between(*start, p);
                let ids: Vec<_> = self
                    .doc
                    .marks
                    .iter()
                    .filter_map(|m| {
                        self.doc
                            .selection_bounds(&[m.id])
                            .filter(|b| intersects(box_, *b))
                            .map(|_| m.id)
                    })
                    .collect();
                self.selected = base.clone();
                for id in self.doc.expand_groups(&ids) {
                    if !self.selected.contains(&id) {
                        self.selected.push(id)
                    }
                }
            }
            Some(Gesture::Crop { start, preview }) => {
                *preview = Crop::between(*start, p, self.doc.width, self.doc.height)
            }
            Some(Gesture::Source { end, .. }) => *end = p,
            Some(Gesture::Draw(mark)) => match self.tool {
                Tool::Ink | Tool::Smart => {
                    let last = mark.points.last().unwrap();
                    if (p.x - last.x).hypot(p.y - last.y) * self.transform.scale as f32 >= 0.75 {
                        mark.points.push(p)
                    }
                }
                Tool::Rectangle => {
                    mark.points = vec![start, pt(p.x, start.y), p, pt(start.x, p.y)];
                    mark.closed = true;
                }
                Tool::Ellipse => {
                    mark.points.clear();
                    mark.points.extend((0..96).map(|i| {
                        let t = i as f32 / 96. * std::f32::consts::TAU;
                        pt(
                            (start.x + p.x) / 2. + (p.x - start.x).abs() / 2. * t.cos(),
                            (start.y + p.y) / 2. + (p.y - start.y).abs() / 2. * t.sin(),
                        )
                    }));
                    mark.closed = true;
                }
                Tool::Arrow | Tool::Line => {
                    if let MarkKind::Connector { end, .. } = &mut mark.kind {
                        *end = self.doc.endpoint(p, (10. / self.transform.scale) as f32);
                    }
                    mark.points[1] = p;
                }
                Tool::Spotlight => {
                    let id = mark.id;
                    *mark = Mark::spotlight(id, start, p);
                }
                Tool::Redact => {
                    mark.points[0] = pt(start.x.min(p.x), start.y.min(p.y));
                    mark.points[1] = pt(start.x.max(p.x), start.y.max(p.y));
                }
                Tool::Step => {
                    // Scale about the centre so the radius is not bound by the 1..24 width.
                    let c = mark.points[0];
                    let base = step_radius(mark.width) as f32;
                    let r = (raw.x - c.x).hypot(raw.y - c.y).max(base);
                    let matrix = Affine::scale_about(r / base, r / base, c);
                    if matrix.is_sane() {
                        mark.transform = matrix;
                    }
                }
                _ => {}
            },
            None => {}
        }
    }
    pub fn finish(&mut self) -> Option<&'static str> {
        match self.gesture.take()? {
            Gesture::Transform { matrix, .. } => self
                .doc
                .transform_marks(&self.selected, matrix)
                .then_some("Selection transformed · Ctrl+Z reverses the whole gesture."),
            Gesture::Crop { preview, .. } => {
                if let Some(crop) = preview {
                    self.doc.set_crop(Some(crop));
                    Some("Crop applied to copy/export. Reset crop and Undo restore it.")
                } else {
                    None
                }
            }
            Gesture::Source { start, end } => {
                let source = between(start, end);
                if source.width >= 2. && source.height >= 2. {
                    self.magnifier_source = Some(source);
                    Some("Source chosen · Click elsewhere on the image to place the 2× magnifier.")
                } else {
                    None
                }
            }
            Gesture::Marquee { .. } => Some(
                "Selection ready · Shift-click adds/removes · Ctrl+G groups · Handles resize/rotate.",
            ),
            Gesture::Draw(mark) => {
                let (a, b) = (mark.points.first().copied(), mark.points.get(1).copied());
                if let (Some(a), Some(b)) = (a, b) {
                    let tiny = match mark.kind {
                        MarkKind::Spotlight { .. } => {
                            (a.x - b.x).abs() < 1. || (a.y - b.y).abs() < 1.
                        }
                        MarkKind::Redaction | MarkKind::Connector { .. } => {
                            (a.x - b.x).hypot(a.y - b.y) < 1.
                        }
                        _ => false,
                    };
                    if tiny {
                        return None;
                    }
                }
                let id = mark.id;
                let smart = self.tool == Tool::Smart && self.model_ready && mark.points.len() >= 2;
                let sent = if smart {
                    self.corrections.insert(
                        id,
                        Correction {
                            original: mark.points.clone(),
                            closed: mark.closed,
                            choices: vec![],
                            manual: false,
                        },
                    );
                    self.request.send((id, mark.points.clone())).is_ok()
                } else {
                    true
                };
                self.doc.add(mark);
                self.selected = vec![id];
                Some(if !sent {
                    "Recognition worker stopped; ink retained."
                } else if smart {
                    "Recognizing shape…"
                } else if self.tool == Tool::Step {
                    "Step placed · Drag from the click to size it · Select it to change its order."
                } else if self.tool == Tool::Redact {
                    "Opaque redaction added · Share PNG, not an editable project (which retains originals)."
                } else if self.tool == Tool::Spotlight {
                    "Spotlight added · Everything outside it is dimmed; V selects and resizes it."
                } else {
                    "Annotation added · V selects · Shift-click adds to the selection."
                })
            }
        }
    }
    pub fn recognize(&mut self, id: u64, result: Recognition) {
        let Some(c) = self.corrections.get_mut(&id) else {
            return;
        };
        if c.manual {
            return;
        }
        c.choices.clear();
        if let Some(shape) = result.accepted {
            self.doc.snap(id, shape.points.clone(), shape.closed);
            c.choices.push(shape)
        }
        c.choices.extend(result.alternatives);
    }
    pub fn correct(&mut self, index: Option<usize>) -> bool {
        let Some(&id) = self.selected.first().filter(|_| self.selected.len() == 1) else {
            return false;
        };
        let Some(c) = self.corrections.get_mut(&id) else {
            return false;
        };
        let (points, closed) = if let Some(i) = index {
            let Some(shape) = c.choices.get(i) else {
                return false;
            };
            (shape.points.clone(), shape.closed)
        } else {
            (c.original.clone(), c.closed)
        };
        c.manual = true;
        self.doc.replace_geometry(id, points, closed)
    }
    pub fn crop(&self) -> Option<Crop> {
        match self.gesture {
            Some(Gesture::Crop {
                preview: Some(c), ..
            }) => Some(c),
            _ => self.doc.crop,
        }
    }
    pub fn selection_bounds(&self) -> Option<Bounds> {
        let b = self.doc.selection_bounds(&self.selected)?;
        Some(
            if let Some(Gesture::Transform { matrix, .. }) = self.gesture {
                transformed_bounds(b, matrix)
            } else {
                b
            },
        )
    }
    pub fn handle_at(&self, x: f64, y: f64) -> Option<usize> {
        let b = self.selection_bounds()?;
        let p = self.transform.raw(x, y);
        let r = (8. / self.transform.scale) as f32;
        handle_points(b, (28. / self.transform.scale) as f32)
            .iter()
            .enumerate()
            .rev()
            .find(|(_, q)| (q.x - p.x).hypot(q.y - p.y) <= r)
            .map(|(i, _)| i)
    }
}
/// True when the expanded selection contains text, which must only ever scale uniformly.
fn has_text(doc: &Document, ids: &[u64]) -> bool {
    doc.expand_groups(ids)
        .iter()
        .any(|id| matches!(doc.mark(*id).map(|m| &m.kind), Some(MarkKind::Text { .. })))
}
pub fn pt(x: f32, y: f32) -> Point {
    Point { x, y }
}
pub fn between(a: Point, b: Point) -> Bounds {
    Bounds {
        x: a.x.min(b.x),
        y: a.y.min(b.y),
        width: (b.x - a.x).abs(),
        height: (b.y - a.y).abs(),
    }
}
fn intersects(a: Bounds, b: Bounds) -> bool {
    a.x <= b.x + b.width && a.x + a.width >= b.x && a.y <= b.y + b.height && a.y + a.height >= b.y
}
pub fn transformed_bounds(b: Bounds, m: Affine) -> Bounds {
    let q = [
        m.apply(pt(b.x, b.y)),
        m.apply(pt(b.x + b.width, b.y)),
        m.apply(pt(b.x + b.width, b.y + b.height)),
        m.apply(pt(b.x, b.y + b.height)),
    ];
    let x = q.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
    let y = q.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
    Bounds {
        x,
        y,
        width: q.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max) - x,
        height: q.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max) - y,
    }
}
pub fn handle_points(b: Bounds, rotation_distance: f32) -> [Point; 9] {
    let (x, y, w, h) = (b.x, b.y, b.width, b.height);
    [
        pt(x, y),
        pt(x + w / 2., y),
        pt(x + w, y),
        pt(x + w, y + h / 2.),
        pt(x + w, y + h),
        pt(x + w / 2., y + h),
        pt(x, y + h),
        pt(x, y + h / 2.),
        pt(x + w / 2., y - rotation_distance),
    ]
}
fn resize_matrix(b: Bounds, index: usize, p: Point, aspect: bool, positive: bool) -> Affine {
    let handles = handle_points(b, 0.);
    let fixed = handles[(index + 4) % 8];
    let original = handles[index];
    let safe = |v: f32| {
        if v.abs() < 0.01 {
            if v < 0. { -0.01 } else { 0.01 }
        } else {
            v
        }
    };
    let mut sx = if index == 1 || index == 5 {
        1.
    } else {
        safe(
            (p.x - fixed.x)
                / (original.x - fixed.x)
                    .abs()
                    .max(0.001_f32)
                    .copysign(original.x - fixed.x),
        )
    };
    let mut sy = if index == 3 || index == 7 {
        1.
    } else {
        safe(
            (p.y - fixed.y)
                / (original.y - fixed.y)
                    .abs()
                    .max(0.001)
                    .copysign(original.y - fixed.y),
        )
    };
    if aspect {
        let s = if index == 1 || index == 5 {
            sy.abs()
        } else if index == 3 || index == 7 {
            sx.abs()
        } else {
            sx.abs().max(sy.abs())
        };
        sx = s.copysign(sx);
        sy = s.copysign(sy);
    }
    if positive {
        (sx, sy) = (sx.abs(), sy.abs());
    }
    Affine::scale_about(sx, sy, fixed)
}
fn anchors(b: Bounds, x: bool) -> [f32; 3] {
    let (a, size) = if x { (b.x, b.width) } else { (b.y, b.height) };
    [a, a + size / 2., a + size]
}
pub fn snap_move(
    b: Bounds,
    delta: Point,
    targets: &[Bounds],
    width: f32,
    height: f32,
    tol: f32,
) -> (Point, Vec<Guide>) {
    let moving = Bounds {
        x: b.x + delta.x,
        y: b.y + delta.y,
        ..b
    };
    let mut out = delta;
    let mut guides = vec![];
    for horizontal in [true, false] {
        let mut best = tol + 1.;
        let mut guide = None;
        for target in targets.iter().copied().chain(std::iter::once(Bounds {
            x: 0.,
            y: 0.,
            width,
            height,
        })) {
            for source in anchors(moving, horizontal) {
                for dest in anchors(target, horizontal) {
                    let d = dest - source;
                    if d.abs() <= tol && d.abs() < best.abs() {
                        best = d;
                        guide = Some(if horizontal {
                            Guide {
                                a: pt(dest, moving.y.min(target.y)),
                                b: pt(
                                    dest,
                                    (moving.y + moving.height).max(target.y + target.height),
                                ),
                                spacing: false,
                            }
                        } else {
                            Guide {
                                a: pt(moving.x.min(target.x), dest),
                                b: pt((moving.x + moving.width).max(target.x + target.width), dest),
                                spacing: false,
                            }
                        })
                    }
                }
            }
        }
        // Equal gaps between the closest neighbours in the same row/column.
        let (start, size, cross, cross_size) = if horizontal {
            (moving.x, moving.width, moving.y, moving.height)
        } else {
            (moving.y, moving.height, moving.x, moving.width)
        };
        let mut before: Option<(f32, f32)> = None;
        let mut after: Option<(f32, f32)> = None;
        for t in targets {
            let (a, s, c, cs) = if horizontal {
                (t.x, t.width, t.y, t.height)
            } else {
                (t.y, t.height, t.x, t.width)
            };
            if c + cs < cross || c > cross + cross_size {
                continue;
            }
            if a + s <= start + tol && before.is_none_or(|v| a + s > v.1) {
                before = Some((a, a + s));
            }
            if a >= start + size - tol && after.is_none_or(|v| a < v.0) {
                after = Some((a, a + s));
            }
        }
        if let (Some(left), Some(right)) = (before, after) {
            let d = (right.0 - size + left.1) / 2. - start;
            if d.abs() <= tol && d.abs() < best.abs() {
                best = d;
                guide = Some(if horizontal {
                    Guide {
                        a: pt(left.1, cross + cross_size / 2.),
                        b: pt(right.0, cross + cross_size / 2.),
                        spacing: true,
                    }
                } else {
                    Guide {
                        a: pt(cross + cross_size / 2., left.1),
                        b: pt(cross + cross_size / 2., right.0),
                        spacing: true,
                    }
                });
            }
        }
        if let Some(g) = guide {
            if horizontal {
                out.x += best
            } else {
                out.y += best
            }
            guides.push(g)
        }
    }
    (out, guides)
}

#[cfg(test)]
mod tests {
    #[test]
    fn half_zoom_move_and_viewport_change_keep_image_coordinates() {
        let mut s = state();
        let id = rectangle(&mut s, 50.);
        s.transform = Transform {
            scale: 0.5,
            x: 20.,
            y: 30.,
        };
        s.begin(57.5, 55., false);
        s.update(10., 15., false, true);
        s.finish();
        assert_eq!(s.doc.mark(id).unwrap().transform.tx, 20.);
        assert_eq!(s.doc.mark(id).unwrap().transform.ty, 30.);
        s.doc.undo();
        assert_eq!(s.doc.mark(id).unwrap().transform, Affine::identity());
        s.tool = Tool::Rectangle;
        s.begin(30., 40., false);
        s.transform.x = 25.;
        s.transform.y = 35.;
        s.update(10., 15., false, true);
        s.finish();
        let m = s.doc.mark(*s.selected.first().unwrap()).unwrap();
        assert_eq!(m.points[0], pt(20., 20.));
        assert_eq!(m.points[2], pt(30., 40.));
    }
    #[test]
    fn rotation_handle_snaps_to_fifteen_degrees_without_committing_preview() {
        let mut s = state();
        let id = rectangle(&mut s, 100.);
        s.selected = vec![id];
        let b = s.doc.selection_bounds(&s.selected).unwrap();
        let center = pt(b.x + b.width / 2., b.y + b.height / 2.);
        let handle = handle_points(b, 28.)[8];
        let r = (handle.y - center.y).abs();
        s.begin(handle.x.into(), handle.y.into(), false);
        let a = 42_f32.to_radians();
        let target = pt(center.x + r * a.sin(), center.y - r * a.cos());
        s.update(
            (target.x - handle.x).into(),
            (target.y - handle.y).into(),
            true,
            true,
        );
        assert_eq!(s.doc.mark(id).unwrap().transform, Affine::identity());
        s.finish();
        let m = s.doc.mark(id).unwrap().transform;
        assert!((m.b.atan2(m.a) - 45_f32.to_radians()).abs() < 1e-5);
        s.doc.undo();
        assert_eq!(s.doc.mark(id).unwrap().transform, Affine::identity());
    }
    #[test]
    fn alt_drag_bypasses_magnetic_alignment() {
        let mut s = state();
        let id = rectangle(&mut s, 50.);
        rectangle(&mut s, 150.);
        s.selected = vec![id];
        s.begin(60., 50., false);
        s.update(98., 0., false, false);
        let Some(Gesture::Transform { matrix, .. }) = s.gesture else {
            panic!("move preview")
        };
        assert_ne!(matrix.tx, 98.);
        s.cancel();
        s.begin(60., 50., false);
        s.update(98., 0., false, true);
        let Some(Gesture::Transform {
            matrix, ref guides, ..
        }) = s.gesture
        else {
            panic!("move preview")
        };
        assert_eq!(matrix.tx, 98.);
        assert!(guides.is_empty());
    }
    #[test]
    fn correction_is_undoable_preserves_transform_and_ignores_stale_results() {
        let mut s = state();
        s.tool = Tool::Smart;
        s.model_ready = true;
        s.begin(20., 20., false);
        s.update(40., 30., false, true);
        s.finish();
        let id = s.selected[0];
        let original = s.doc.mark(id).unwrap().points.clone();
        s.doc.move_marks(&[id], pt(10., 5.));
        let clean = vec![pt(20., 20.), pt(60., 20.), pt(60., 50.), pt(20., 50.)];
        s.recognize(
            id,
            Recognition {
                accepted: Some(Recognized {
                    kind: "rectangle".into(),
                    points: clean.clone(),
                    closed: true,
                }),
                alternatives: vec![],
            },
        );
        assert_eq!(s.doc.mark(id).unwrap().points, clean);
        assert!(s.correct(None));
        assert_eq!(s.doc.mark(id).unwrap().points, original);
        assert_eq!(s.doc.mark(id).unwrap().transform.tx, 10.);
        s.recognize(
            id,
            Recognition {
                accepted: Some(Recognized {
                    kind: "line".into(),
                    points: vec![pt(0., 0.), pt(1., 1.)],
                    closed: false,
                }),
                alternatives: vec![],
            },
        );
        assert_eq!(s.doc.mark(id).unwrap().points, original);
        s.doc.undo();
        assert_eq!(s.doc.mark(id).unwrap().points, clean);
    }
    use super::*;
    use image::RgbaImage;
    fn state() -> State {
        let (tx, _) = mpsc::channel();
        let mut s = State::new(Document::new(RgbaImage::new(500, 400)).unwrap(), tx);
        s.tool = Tool::Select;
        s
    }
    fn rectangle(s: &mut State, x: f32) -> u64 {
        let id = s.doc.id();
        s.doc.add(Mark::path(
            id,
            vec![pt(x, 50.), pt(x + 50., 50.), pt(x + 50., 100.), pt(x, 100.)],
            true,
            [1., 0., 0., 1.],
            2.,
        ));
        id
    }
    #[test]
    fn gesture_preview_cancel_and_commit_preserve_batch_history() {
        let mut s = state();
        let a = rectangle(&mut s, 50.);
        let b = rectangle(&mut s, 150.);
        s.selected = vec![a, b];
        s.begin(75., 50., false);
        s.update(20., 30., false, true);
        assert_eq!(s.doc.mark(a).unwrap().transform, Affine::identity());
        s.cancel();
        assert_eq!(s.doc.mark(a).unwrap().transform, Affine::identity());
        s.begin(75., 50., false);
        s.update(20., 30., false, true);
        s.finish();
        assert_eq!(s.doc.mark(a).unwrap().transform.tx, 20.);
        assert_eq!(s.doc.mark(b).unwrap().transform.ty, 30.);
        s.doc.undo();
        assert_eq!(s.doc.mark(a).unwrap().transform, Affine::identity());
        assert_eq!(s.doc.mark(b).unwrap().transform, Affine::identity());
    }
    #[test]
    fn shift_selection_marquee_and_group_expand() {
        let mut s = state();
        let a = rectangle(&mut s, 50.);
        let b = rectangle(&mut s, 150.);
        s.begin(75., 50., false);
        s.finish();
        s.begin(175., 50., true);
        s.finish();
        assert_eq!(s.selected.len(), 2);
        s.doc.group_marks(&[a, b]);
        s.selected.clear();
        s.begin(75., 50., false);
        assert_eq!(s.selected.len(), 2);
        s.cancel();
        s.begin(10., 10., false);
        s.update(220., 110., false, true);
        s.finish();
        assert!(s.selected.contains(&a) && s.selected.contains(&b));
    }
    #[test]
    fn resize_opposite_anchor_and_shift_aspect() {
        let b = Bounds {
            x: 20.,
            y: 20.,
            width: 80.,
            height: 40.,
        };
        let m = resize_matrix(b, 0, pt(0., 0.), false, false);
        assert_eq!(m.apply(pt(100., 60.)), pt(100., 60.));
        assert_eq!(m.apply(pt(20., 20.)), pt(0., 0.));
        let m = resize_matrix(b, 4, pt(180., 70.), true, false);
        assert!((m.a.abs() - m.d.abs()).abs() < 1e-5);
    }
    #[test]
    fn snap_edges_centers_and_equal_spacing() {
        let b = Bounds {
            x: 41.,
            y: 20.,
            width: 20.,
            height: 20.,
        };
        let targets = [
            Bounds {
                x: 0.,
                y: 20.,
                width: 20.,
                height: 20.,
            },
            Bounds {
                x: 80.,
                y: 20.,
                width: 20.,
                height: 20.,
            },
        ];
        let (d, g) = snap_move(b, pt(0., 0.), &targets, 500., 400., 6.);
        assert_eq!(d.x, -1.);
        assert!(g.iter().any(|g| g.spacing));
        let (d, _) = snap_move(b, pt(36., 0.), &targets, 500., 400., 6.);
        assert_eq!(d.x, 39.);
    }
    #[test]
    fn text_commit_and_cancel_do_not_create_empty_marks() {
        let mut s = state();
        s.tool = Tool::Text;
        s.begin(40., 40., false);
        s.text_draft = None;
        assert!(s.doc.marks.is_empty());
        s.begin(40., 40., false);
        s.commit_text("Caption".into(), 120., true, 24.);
        let id = s.selected[0];
        assert!(s.edit_text(id));
        s.commit_text("Edited".into(), 80., false, 24.);
        s.doc.undo();
        assert!(
            matches!(&s.doc.mark(id).unwrap().kind,MarkKind::Text{value,..} if value=="Caption")
        );
    }
    #[test]
    fn step_click_keeps_default_size_and_drag_grows_past_width_limit_at_zoom() {
        let mut s = state();
        s.tool = Tool::Step;
        s.transform = Transform {
            scale: 0.5,
            x: 20.,
            y: 30.,
        };
        let base = step_radius(s.width) as f32;
        s.begin(70., 80., false);
        s.finish();
        let id = s.selected[0];
        assert_eq!(s.doc.mark(id).unwrap().transform, Affine::identity());
        assert_eq!(s.doc.mark(id).unwrap().points, vec![pt(100., 100.)]);
        s.doc.undo();
        assert!(s.doc.marks.is_empty());
        s.begin(70., 80., false);
        s.update(75., 0., false, false);
        assert!(s.doc.marks.is_empty());
        let Some(Gesture::Draw(m)) = &s.gesture else {
            panic!("step preview")
        };
        assert!((m.transform.a * base - 150.).abs() < 1e-3);
        let center = m.transform.apply(pt(100., 100.));
        assert!((center.x - 100.).hypot(center.y - 100.) < 1e-3);
        s.finish();
        let id = s.selected[0];
        let m = s.doc.mark(id).unwrap();
        assert!(m.transform.a * base > 60.);
        assert!(matches!(m.kind, MarkKind::Step { number: 1 }));
        s.doc.undo();
        assert!(s.doc.marks.is_empty());
    }
    #[test]
    fn step_cancel_leaves_no_mark_or_history() {
        let mut s = state();
        s.tool = Tool::Step;
        s.begin(100., 100., false);
        s.update(80., 0., false, false);
        s.cancel();
        assert!(s.finish().is_none());
        assert!(s.doc.marks.is_empty() && !s.doc.can_undo());
    }
    fn add_text(s: &mut State, size: f64) -> u64 {
        s.tool = Tool::Text;
        s.begin(100., 100., false);
        s.commit_text("Caption".into(), 0., false, size);
        s.tool = Tool::Select;
        s.selected[0]
    }
    fn drag_handle(s: &mut State, index: usize, dx: f64, dy: f64) -> Affine {
        let h = handle_points(s.selection_bounds().unwrap(), 0.)[index];
        let t = s.transform;
        s.begin(
            f64::from(h.x) * t.scale + t.x,
            f64::from(h.y) * t.scale + t.y,
            false,
        );
        s.update(dx, dy, false, true);
        let Some(Gesture::Transform { matrix, .. }) = s.gesture else {
            panic!("resize preview")
        };
        matrix
    }
    fn assert_uniform_positive(m: Affine) {
        assert!(m.a > 0. && m.d > 0. && (m.a - m.d).abs() < 1e-5 && m.b == 0. && m.c == 0.);
    }
    #[test]
    fn new_text_is_unwrapped_with_the_explicit_size() {
        let mut s = state();
        s.text_size = 31.;
        s.tool = Tool::Text;
        s.begin(40., 40., false);
        let d = s.text_draft.as_ref().unwrap();
        assert_eq!((d.box_width, d.font_size), (0., 31.));
        s.commit_text("A".into(), 0., false, 31.);
        assert_eq!(s.doc.text_size(s.selected[0]), Some(31.));
    }
    #[test]
    fn text_resize_is_uniform_and_never_mirrors_at_zoom() {
        let mut s = state();
        let id = add_text(&mut s, 30.);
        s.transform = Transform {
            scale: 0.5,
            x: 20.,
            y: 30.,
        };
        s.selected = vec![id];
        let m = drag_handle(&mut s, 4, 120., 17.);
        assert_uniform_positive(m);
        assert!(m.a > 1.);
        s.finish();
        assert!((s.doc.text_size(id).unwrap() - 30. * f64::from(m.a)).abs() < 1e-3);
        s.cancel();
        // Dragging past the opposite corner or a single edge cannot flip or squash it either.
        assert_uniform_positive(drag_handle(&mut s, 4, -2000., -900.));
        s.cancel();
        assert_uniform_positive(drag_handle(&mut s, 3, 80., 0.));
    }
    #[test]
    fn group_containing_text_resizes_uniformly_but_plain_shapes_still_stretch() {
        let mut s = state();
        let text = add_text(&mut s, 24.);
        let rect = rectangle(&mut s, 200.);
        s.doc.group_marks(&[text, rect]);
        s.selected = vec![rect];
        assert_uniform_positive(drag_handle(&mut s, 4, 90., 10.));
        s.cancel();
        assert_uniform_positive(drag_handle(&mut s, 0, 300., 300.));
        s.cancel();
        let plain = rectangle(&mut s, 300.);
        s.doc.ungroup_marks(&[text]);
        s.selected = vec![plain];
        let m = drag_handle(&mut s, 4, 90., 10.);
        assert!((m.a - m.d).abs() > 0.1);
    }
    #[test]
    fn text_edit_keeps_scale_and_commits_font_in_one_undo() {
        let mut s = state();
        let id = add_text(&mut s, 30.);
        let at = s.doc.mark(id).unwrap().points[0];
        assert!(
            s.doc
                .transform_marks(&[id], Affine::scale_about(2., 2., at))
        );
        assert!(s.edit_text(id));
        let size = s.text_draft.as_ref().unwrap().font_size;
        assert!((size - 60.).abs() < 1e-3);
        s.commit_text("Edited".into(), 0., false, size);
        assert!((s.doc.text_size(id).unwrap() - 60.).abs() < 1e-3);
        assert_eq!(s.doc.mark(id).unwrap().transform.a, 2.);
        assert!(s.edit_text(id));
        s.commit_text("Edited".into(), 0., false, 40.);
        assert!((s.doc.text_size(id).unwrap() - 40.).abs() < 1e-3);
        s.doc.undo();
        assert!((s.doc.text_size(id).unwrap() - 60.).abs() < 1e-3);
    }
    #[test]
    fn line_has_no_head_and_arrow_head_is_independent() {
        let mut s = state();
        s.head_size = 2.5;
        for (tool, head) in [(Tool::Line, 0.), (Tool::Arrow, 2.5)] {
            s.tool = tool;
            s.begin(50., 60., false);
            s.update(100., 40., false, true);
            s.finish();
            let m = s.doc.mark(s.selected[0]).unwrap();
            assert!(matches!(m.kind, MarkKind::Connector { .. }));
            assert_eq!(m.head_size, head);
        }
        s.tool = Tool::Select;
        s.set_tip(0.5);
        assert_eq!(s.tip(), 0.5);
        assert_eq!(s.doc.mark(s.selected[0]).unwrap().head_size, 0.5);
        s.doc.undo();
        assert_eq!(s.doc.mark(s.selected[0]).unwrap().head_size, 2.5);
    }
    #[test]
    fn spotlight_drag_uses_constructor_box_and_tiny_drag_is_dropped() {
        let mut s = state();
        s.tool = Tool::Spotlight;
        s.begin(200., 150., false);
        s.update(-80., -50., false, true);
        assert!(s.doc.marks.is_empty());
        s.finish();
        let m = s.doc.mark(s.selected[0]).unwrap();
        assert!(matches!(m.kind, MarkKind::Spotlight { .. }));
        assert_eq!(m.points, vec![pt(120., 100.), pt(200., 150.)]);
        s.doc.undo();
        assert!(s.doc.marks.is_empty());
        s.begin(200., 150., false);
        s.update(0., 30., false, true);
        assert!(s.finish().is_none() && s.doc.marks.is_empty());
    }
    #[test]
    fn size_controls_update_new_objects_without_switching_tools() {
        let mut s = state();
        s.tool = Tool::Arrow;
        s.begin(50., 60., false);
        s.update(100., 40., false, true);
        s.finish();
        let arrow = s.selected[0];
        s.set_style(None, Some(18.));
        s.set_tip(3.);
        assert_eq!(
            (
                s.doc.mark(arrow).unwrap().width,
                s.doc.mark(arrow).unwrap().head_size
            ),
            (18., 3.)
        );
        s.selected.clear();
        s.tool = Tool::Text;
        s.begin(200., 200., false);
        s.commit_text("Caption".into(), 0., false, 24.);
        let text = s.selected[0];
        s.set_font(52.);
        assert_eq!(s.doc.text_size(text), Some(52.));
        s.doc.undo();
        assert_eq!(s.doc.text_size(text), Some(24.));
    }
    #[test]
    fn width_or_opacity_change_keeps_mixed_colours_of_the_selection() {
        let mut s = state();
        let a = rectangle(&mut s, 50.);
        let b = rectangle(&mut s, 150.);
        s.selected = vec![a, b];
        assert!(s.doc.edit_marks(
            &[b],
            &MarkEdit {
                rgb: Some([0., 0., 1.]),
                alpha: Some(0.5),
                ..MarkEdit::default()
            }
        ));
        s.set_style(None, Some(9.));
        let color = |s: &State, id| s.doc.mark(id).unwrap().color;
        assert_eq!(
            (color(&s, a), color(&s, b)),
            ([1., 0., 0., 1.], [0., 0., 1., 0.5])
        );
        assert_eq!(s.doc.mark(b).unwrap().width, 9.);
        s.set_opacity(0.25);
        assert_eq!(
            (color(&s, a), color(&s, b)),
            ([1., 0., 0., 0.25], [0., 0., 1., 0.25])
        );
        s.set_rgb([0., 1., 0.]);
        assert_eq!(color(&s, a), [0., 1., 0., 0.25]);
        assert_eq!(s.color[3], 0.25, "tool default follows the control");
    }
    #[test]
    fn caption_style_flows_through_draft_preview_commit_edit_and_undo() {
        let mut s = state();
        s.tool = Tool::Text;
        s.text_style = TextStyle {
            bold: true,
            ..TextStyle::default()
        };
        s.begin(40., 40., false);
        assert!(
            s.text_draft.as_ref().unwrap().text_style.bold,
            "new drafts start from the default"
        );
        s.text_draft.as_mut().unwrap().value = "Hello".into();
        s.text_draft.as_mut().unwrap().text_style.italic = true;
        let rev = s.doc.revision();
        let preview = s.draft_mark().unwrap();
        assert_eq!(preview.points, vec![pt(40., 40.)]);
        assert_eq!(preview.color, s.color);
        assert!(preview.text_style.bold && preview.text_style.italic);
        assert_eq!(
            s.doc.revision(),
            rev,
            "previewing never touches the document"
        );
        assert!(s.doc.marks.is_empty());
        s.commit_text("Hello".into(), 0., false, 24.);
        let id = s.selected[0];
        let style = s.doc.mark(id).unwrap().text_style;
        assert!(
            style.bold && style.italic,
            "commit takes the style from the draft"
        );
        assert_eq!(s.text_style(), style);

        // Editing: the preview keeps the original transform and the draft's style, one undo to commit.
        let at = s.doc.mark(id).unwrap().points[0];
        assert!(
            s.doc
                .transform_marks(&[id], Affine::rotation_about(0.4, at))
        );
        assert!(s.edit_text(id));
        assert_eq!(s.text_draft.as_ref().unwrap().text_style, style);
        let d = s.text_draft.as_mut().unwrap();
        d.value = "Changed".into();
        d.text_style.align = crate::document::TextAlign::Right;
        d.box_width = 100.;
        let preview = s.draft_mark().unwrap();
        assert_eq!(preview.transform, s.doc.mark(id).unwrap().transform);
        assert_eq!(preview.id, id);
        assert!(
            matches!(&preview.kind, MarkKind::Text { value, box_width, .. } if value == "Changed" && *box_width == 100.)
        );
        let size = s.text_draft.as_ref().unwrap().font_size;
        s.commit_text("Changed".into(), 100., false, size);
        assert_eq!(
            s.doc.mark(id).unwrap().text_style.align,
            crate::document::TextAlign::Right
        );
        s.doc.undo();
        let m = s.doc.mark(id).unwrap();
        assert!(m.text_style.bold && m.text_style.italic);
        assert_eq!(m.text_style.align, crate::document::TextAlign::Left);
        assert!(s.draft_mark().is_none(), "no draft, no preview");

        // set_text_style styles the selection and the default for new captions together.
        s.selected = vec![id];
        s.set_text_style(TextStyle::default());
        assert_eq!(s.doc.mark(id).unwrap().text_style, TextStyle::default());
        assert_eq!(s.text_style, TextStyle::default());
    }
}
