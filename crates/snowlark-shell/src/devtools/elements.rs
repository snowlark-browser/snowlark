// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The elements panel: the document's element tree (expandable, its rows syntax-coloured), the
//! selected element's box model and computed style (filterable), and the selection that pick mode
//! and "Inspect" make.

use std::collections::HashSet;

use snowlark_dom::NodeId;
use snowlark_page::devtools::{DomRow, DomRowKind, NodeDetails};
use snowlark_paint::window::{keys, Modifiers, MouseButton};
use snowlark_paint::{Color, DisplayListBuilder, Point, Radii, Rect};

use super::protocol::{DevtoolsCommand, EditKind, NodeOp};
use super::{field_style, Action, Colors, FieldId, NodeKind, NodeTarget, Scroll, ROW, TEXT};
use crate::address::TextField;
use crate::chrome::{Face, Fonts};
use crate::driver::Clipboard;
use crate::text_area::{AreaKey, TextArea};
use crate::widgets;

const INDENT: f32 = 14.0;
const BOX_MODEL_H: f32 = 170.0;

#[derive(Default)]
pub struct ElementsView {
    pub rows: Vec<DomRow>,
    pub expanded: HashSet<NodeId>,
    pub selected: Option<NodeId>,
    pub details: Option<(NodeId, NodeDetails)>,
    pub style_filter: TextField,
    hover: Option<usize>,
    /// The element hovered, as last sent to the tab (it highlights it).
    hovered_node: Option<NodeId>,
    scroll: Scroll,
    styles_scroll: Scroll,
    /// Scroll the tree to this element once its row arrives.
    reveal: Option<NodeId>,
    /// The tree's height when last drawn (to scroll a picked element into view).
    tree_height: std::cell::Cell<f32>,
    /// The in-place editor, while one is open.
    edit: Option<ActiveEdit>,
    /// The pseudo-classes forced on elements (Force state), as masks.
    forced: std::collections::HashMap<NodeId, u8>,
}

/// An edit in progress in the tree: what it edits, the editor, the text it started with.
struct ActiveEdit {
    node: NodeId,
    kind: EditKind,
    original: String,
    area: TextArea,
}

/// The most lines the markup editor shows at once.
const EDIT_LINES: usize = 12;

struct Parts {
    tree: Rect,
    styles: Rect,
    filter: Rect,
    box_model: Rect,
    list: Rect,
}

fn parts(body: Rect) -> Parts {
    let (tree, styles) = if body.width >= 560.0 {
        let w = (body.width * 0.38).clamp(240.0, 420.0);
        (
            Rect::new(body.x, body.y, body.width - w, body.height),
            Rect::new(body.right() - w, body.y, w, body.height),
        )
    } else {
        let h = (body.height * 0.45).max(100.0).min(body.height);
        (
            Rect::new(body.x, body.y, body.width, body.height - h),
            Rect::new(body.x, body.bottom() - h, body.width, h),
        )
    };
    let box_model = Rect::new(
        styles.x,
        styles.y,
        styles.width,
        BOX_MODEL_H.min(styles.height),
    );
    let filter = Rect::new(
        styles.x + 8.0,
        box_model.bottom() + 4.0,
        styles.width - 16.0,
        ROW + 4.0,
    );
    let list = Rect::new(
        styles.x,
        filter.bottom() + 4.0,
        styles.width,
        (styles.bottom() - filter.bottom() - 4.0).max(0.0),
    );
    Parts {
        tree,
        styles,
        filter,
        box_model,
        list,
    }
}

/// The coloured pieces of a row's text.
pub fn row_segments<'a>(row: &'a DomRow, c: &Colors) -> Vec<(std::borrow::Cow<'a, str>, Color)> {
    use std::borrow::Cow;
    let mut out: Vec<(Cow<'a, str>, Color)> = Vec::new();
    match &row.kind {
        DomRowKind::Open {
            tag,
            attrs,
            inline_text,
            empty,
        } => {
            out.push((Cow::Owned(format!("<{tag}")), c.tag));
            for (n, v) in attrs {
                out.push((Cow::Owned(format!(" {n}")), c.attr));
                out.push((Cow::Borrowed("=\""), c.tag));
                out.push((Cow::Borrowed(v.as_str()), c.value));
                out.push((Cow::Borrowed("\""), c.tag));
            }
            out.push((Cow::Borrowed(">"), c.tag));
            if let Some(t) = inline_text {
                out.push((Cow::Borrowed(t.as_str()), c.text));
                out.push((Cow::Owned(format!("</{tag}>")), c.tag));
            } else if row.expandable && !row.expanded {
                out.push((Cow::Borrowed("…"), c.dim));
                out.push((Cow::Owned(format!("</{tag}>")), c.tag));
            } else if *empty && !is_void(tag) {
                out.push((Cow::Owned(format!("</{tag}>")), c.tag));
            }
        }
        DomRowKind::Close { tag } => out.push((Cow::Owned(format!("</{tag}>")), c.tag)),
        DomRowKind::Text(t) => out.push((Cow::Owned(format!("\"{t}\"")), c.text)),
        DomRowKind::Comment(t) => out.push((Cow::Owned(format!("<!--{t}-->")), c.comment)),
        DomRowKind::Doctype(n) => out.push((Cow::Owned(format!("<!DOCTYPE {n}>")), c.dim)),
    }
    out
}

/// HTML's void elements (no end tag).
fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

impl ElementsView {
    pub fn set_rows(&mut self, rows: Vec<DomRow>) {
        // (the tab's expanded set is the truth: it adds the defaults and a pick's ancestors)
        self.expanded = rows.iter().filter(|r| r.expanded).map(|r| r.node).collect();
        self.rows = rows;
        if let Some(i) = self.reveal.and_then(|n| self.row_index(n)) {
            let h = self.tree_height.get().max(ROW * 4.0);
            self.scroll.reveal(4.0 + i as f32 * ROW, ROW * 3.0, h);
            self.reveal = None;
        }
        if let Some(sel) = self.selected {
            if !self.rows.iter().any(|r| r.node == sel) {
                self.selected = None;
            }
        }
        self.hover = None;
    }

    pub fn set_details(&mut self, d: Option<(NodeId, NodeDetails)>) {
        self.details = d;
        self.styles_scroll = Scroll::default();
    }

    pub fn picked(&mut self, node: NodeId, path: Vec<NodeId>) {
        self.expanded.extend(path);
        self.selected = Some(node);
        self.reveal = Some(node);
    }

    pub fn navigated(&mut self) {
        self.edit = None;
        self.forced.clear();
        self.rows.clear();
        self.expanded.clear();
        self.selected = None;
        self.details = None;
        self.hover = None;
        self.hovered_node = None;
        self.scroll = Scroll::default();
    }

    /// What the tab says is forced on `node`.
    pub fn set_forced(&mut self, node: NodeId, kinds: u8) {
        if kinds == 0 {
            self.forced.remove(&node);
        } else {
            self.forced.insert(node, kinds);
        }
    }

    pub fn editing(&self) -> bool {
        self.edit.is_some()
    }

    /// Opens the editor on `node` (`text` is what it starts with) and shows its row.
    pub fn begin_edit(&mut self, node: NodeId, kind: EditKind, text: &str) {
        let single = matches!(kind, EditKind::Attribute(_) | EditKind::AddAttribute);
        self.edit = Some(ActiveEdit {
            node,
            kind,
            original: text.to_owned(),
            area: TextArea::new(text, single),
        });
        if let Some(i) = self.row_index(node) {
            let h = self.tree_height.get().max(ROW * 4.0);
            self.scroll.reveal(4.0 + i as f32 * ROW, ROW * 3.0, h);
        }
    }

    /// Closes the editor; with `commit` the change goes to the page.
    pub fn finish_edit(&mut self, commit: bool) -> Vec<Action> {
        let Some(e) = self.edit.take() else {
            return Vec::new();
        };
        let text = e.area.text().to_owned();
        if !commit || text == e.original {
            return Vec::new();
        }
        let op = match e.kind {
            EditKind::Html => NodeOp::SetOuterHtml(text),
            EditKind::Text => NodeOp::SetText(text),
            EditKind::Attribute(name) => NodeOp::SetAttributes {
                replacing: Some(name),
                text,
            },
            EditKind::AddAttribute if text.trim().is_empty() => return Vec::new(),
            EditKind::AddAttribute => NodeOp::SetAttributes {
                replacing: None,
                text,
            },
        };
        vec![Action::Send(DevtoolsCommand::Node(e.node, op))]
    }

    /// A key while the editor has the keyboard.
    pub fn edit_key(&mut self, key: u16, mods: Modifiers, clip: &dyn Clipboard) -> Vec<Action> {
        let Some(e) = &mut self.edit else {
            return Vec::new();
        };
        match e.area.key(key, mods, clip) {
            AreaKey::Commit => self.finish_edit(true),
            AreaKey::Cancel => self.finish_edit(false),
            AreaKey::Edited | AreaKey::Ignored => Vec::new(),
        }
    }

    pub fn edit_typed(&mut self, c: char) -> bool {
        self.edit.as_mut().is_some_and(|e| e.area.typed(c))
    }

    /// The attributes of a row with where each is drawn: name, left and right edge (from the
    /// row's text start), and where the tag ends (before its `>`).
    fn attr_spans(row: &DomRow, fonts: &Fonts) -> (Vec<(String, f32, f32)>, f32) {
        let DomRowKind::Open { tag, attrs, .. } = &row.kind else {
            return (Vec::new(), 0.0);
        };
        let w = |s: &str| fonts.width_in(Face::Mono, s, TEXT);
        let mut x = w(&format!("<{tag}"));
        let mut out = Vec::new();
        for (n, v) in attrs {
            let x0 = x;
            x += w(&format!(" {n}=\"")) + w(v) + w("\"");
            out.push((n.clone(), x0, x));
        }
        (out, x)
    }

    /// The editor's box in the tree (`tree`, DIPs), where it opens for its node's row.
    fn edit_rect(&self, tree: Rect, fonts: &Fonts) -> Option<Rect> {
        let e = self.edit.as_ref()?;
        let i = self.row_index(e.node)?;
        let row = &self.rows[i];
        let y = tree.y + 4.0 + i as f32 * ROW - self.scroll.y;
        let text_x = tree.x + 6.0 + row.depth as f32 * INDENT + 12.0;
        let (spans, end) = Self::attr_spans(row, fonts);
        let (x, h, top) = match &e.kind {
            EditKind::Attribute(name) => {
                let x0 = spans.iter().find(|(n, ..)| n == name).map_or(end, |s| s.1);
                (text_x + x0, TextArea::height_for(1, 1, TEXT), y - 5.0)
            }
            EditKind::AddAttribute => (text_x + end, TextArea::height_for(1, 1, TEXT), y - 5.0),
            EditKind::Html | EditKind::Text => (
                text_x,
                TextArea::height_for(e.area.lines(), EDIT_LINES, TEXT),
                y - 3.0,
            ),
        };
        let w = (tree.right() - x - 6.0).max(220.0).min(tree.width - 12.0);
        let x = x.min(tree.right() - w - 6.0).max(tree.x + 4.0);
        let top = top.min(tree.bottom() - h - 2.0).max(tree.y + 2.0);
        Some(Rect::new(x, top, w, h))
    }

    fn row_index(&self, node: NodeId) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| r.node == node && !matches!(r.kind, DomRowKind::Close { .. }))
    }

    fn select(&mut self, node: Option<NodeId>) -> Vec<Action> {
        if self.selected == node {
            return Vec::new();
        }
        self.selected = node;
        vec![Action::Send(DevtoolsCommand::Select(node))]
    }

    fn toggle(&mut self, node: NodeId) -> Vec<Action> {
        if !self.expanded.remove(&node) {
            self.expanded.insert(node);
        }
        vec![Action::Send(DevtoolsCommand::Tree(
            self.expanded.iter().copied().collect(),
        ))]
    }

    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        b: &mut DisplayListBuilder,
        body: Rect,
        fonts: &Fonts,
        c: &Colors,
        focus: Option<FieldId>,
        caret_on: bool,
    ) {
        let p = parts(body);
        let mut scroll = self.scroll;
        let content = self.rows.len() as f32 * ROW + 8.0;
        scroll.clamp(content, p.tree.height);
        self.tree_height.set(p.tree.height);
        b.push_clip_rect(p.tree);
        if self.rows.is_empty() {
            fonts.label(
                b,
                "Loading the document…",
                p.tree.x + 12.0,
                p.tree.y + 16.0,
                TEXT,
                c.dim,
                p.tree.width,
            );
        }
        let first = (scroll.y / ROW).floor().max(0.0) as usize;
        let n = (p.tree.height / ROW).ceil() as usize + 1;
        for (k, row) in self.rows.iter().enumerate().skip(first).take(n) {
            let y = p.tree.y + 4.0 + k as f32 * ROW - scroll.y;
            let r = Rect::new(p.tree.x, y, p.tree.width, ROW);
            let selected =
                Some(row.node) == self.selected && !matches!(row.kind, DomRowKind::Close { .. });
            if selected {
                b.fill_rect(r, c.selected);
            } else if self.hover == Some(k) {
                b.fill_rect(r, c.hover);
            }
            let cy = y + ROW / 2.0;
            let x0 = p.tree.x + 6.0 + row.depth as f32 * INDENT;
            if row.expandable {
                let tri = if row.expanded { "▾" } else { "▸" };
                fonts.label(b, tri, x0, cy, 11.0, c.dim, 12.0);
            }
            let mut x = x0 + 12.0;
            for (text, color) in row_segments(row, c) {
                if x > p.tree.right() {
                    break;
                }
                x += fonts.label_in(
                    b,
                    Face::Mono,
                    &text,
                    x,
                    cy,
                    TEXT,
                    color,
                    (p.tree.right() - x).max(0.0),
                );
            }
        }
        if let (Some(e), Some(r)) = (&self.edit, self.edit_rect(p.tree, fonts)) {
            let theme = crate::chrome::theme(c.dark);
            let on = caret_on && focus == Some(FieldId::ElementEdit);
            e.area.draw(b, fonts, &theme, Face::Mono, r, TEXT, on);
        }
        b.pop_clip();
        // The styles pane.
        b.fill_rect(p.styles, c.bg);
        match body.width >= 560.0 {
            true => b.fill_rect(
                Rect::new(p.styles.x, p.styles.y, 1.0, p.styles.height),
                c.border,
            ),
            false => b.fill_rect(
                Rect::new(p.styles.x, p.styles.y, p.styles.width, 1.0),
                c.border,
            ),
        }
        b.push_clip_rect(p.styles);
        match &self.details {
            None => {
                let msg = if self.selected.is_some() {
                    "This element has no box."
                } else {
                    "Select an element to see its styles."
                };
                fonts.label(
                    b,
                    msg,
                    p.styles.x + 12.0,
                    p.styles.y + 18.0,
                    TEXT,
                    c.dim,
                    p.styles.width - 24.0,
                );
            }
            Some((_, d)) => {
                self.paint_box_model(b, p.box_model, d, fonts, c);
                let theme = crate::chrome::theme(c.dark);
                widgets::draw_field(
                    b,
                    fonts,
                    &theme,
                    p.filter,
                    &self.style_filter,
                    &super::FieldStyle {
                        boxed: true,
                        ..field_style(
                            focus == Some(FieldId::StyleFilter),
                            caret_on,
                            "Filter computed styles",
                            Face::Ui,
                        )
                    },
                );
                let props = self.computed(d);
                let mut s = self.styles_scroll;
                s.clamp(props.len() as f32 * ROW, p.list.height);
                b.push_clip_rect(p.list);
                let first = (s.y / ROW).floor().max(0.0) as usize;
                let n = (p.list.height / ROW).ceil() as usize + 1;
                let name_w = (p.list.width * 0.48).min(200.0);
                for (k, (name, value)) in props.iter().enumerate().skip(first).take(n) {
                    let y = p.list.y + k as f32 * ROW - s.y;
                    let cy = y + ROW / 2.0;
                    fonts.label_in(
                        b,
                        Face::Mono,
                        name,
                        p.list.x + 10.0,
                        cy,
                        TEXT,
                        c.attr,
                        name_w - 14.0,
                    );
                    let x = p.list.x + 10.0 + name_w;
                    fonts.label_in(
                        b,
                        Face::Mono,
                        value,
                        x,
                        cy,
                        TEXT,
                        c.text,
                        (p.list.right() - x - 8.0).max(0.0),
                    );
                }
                b.pop_clip();
            }
        }
        b.pop_clip();
    }

    /// The computed properties shown (filtered by name or value).
    fn computed<'a>(&self, d: &'a NodeDetails) -> Vec<&'a (String, String)> {
        let f = self.style_filter.text().trim().to_lowercase();
        d.computed
            .iter()
            // (vendor-prefixed properties only when asked for)
            .filter(|(n, v)| {
                if f.is_empty() {
                    !n.starts_with('-')
                } else {
                    n.contains(&f) || v.to_lowercase().contains(&f)
                }
            })
            .collect()
    }

    /// The box model: nested margin, border, padding and content boxes with their sizes.
    fn paint_box_model(
        &self,
        b: &mut DisplayListBuilder,
        area: Rect,
        d: &NodeDetails,
        fonts: &Fonts,
        c: &Colors,
    ) {
        fonts.label_in(
            b,
            Face::Bold,
            &d.title,
            area.x + 10.0,
            area.y + 12.0,
            TEXT,
            c.text,
            area.width - 20.0,
        );
        let Some(m) = &d.box_model else {
            return;
        };
        let outer = Rect::new(
            area.x + 10.0,
            area.y + 26.0,
            (area.width - 20.0).max(60.0),
            (area.height - 34.0).max(60.0),
        );
        let step_x = (outer.width / 8.0).min(44.0);
        let step_y = (outer.height / 8.0).min(24.0);
        let layers = [
            ("margin", m.margin, Color::rgba(246, 178, 107, 150)),
            ("border", m.border, Color::rgba(255, 229, 153, 170)),
            ("padding", m.padding, Color::rgba(147, 196, 125, 150)),
        ];
        let mut r = outer;
        let ink = Color::hex(0x202124);
        let num = |v: f32| {
            if v == 0.0 {
                "-".to_owned()
            } else {
                let r = (v * 100.0).round() / 100.0;
                format!("{r}")
            }
        };
        for (label, e, color) in layers {
            b.fill_rect(r, color);
            b.stroke_rect(r.inflate(-0.5, -0.5), 1.0, Color::rgba(0, 0, 0, 60));
            fonts.label(b, label, r.x + 4.0, r.y + 8.0, 10.0, ink, 60.0);
            let cx = r.x + r.width / 2.0;
            let mid = r.y + r.height / 2.0;
            let center = |b: &mut DisplayListBuilder, s: &str, x: f32, y: f32| {
                let w = fonts.width(s, 10.0);
                fonts.label(b, s, x - w / 2.0, y, 10.0, ink, f32::INFINITY);
            };
            center(b, &num(e[0]), cx, r.y + step_y / 2.0 + 1.0);
            center(b, &num(e[2]), cx, r.bottom() - step_y / 2.0);
            center(b, &num(e[3]), r.x + step_x / 2.0, mid);
            center(b, &num(e[1]), r.right() - step_x / 2.0, mid);
            r = r.inflate(-step_x, -step_y);
        }
        b.fill_rect(r, Color::rgba(111, 168, 220, 170));
        b.stroke_rect(r.inflate(-0.5, -0.5), 1.0, Color::rgba(0, 0, 0, 60));
        let s = format!("{} × {}", num(m.content.0), num(m.content.1));
        let w = fonts.width(&s, 10.0);
        fonts.label(
            b,
            &s,
            r.x + (r.width - w) / 2.0,
            r.y + r.height / 2.0,
            10.0,
            ink,
            r.width,
        );
        let _ = Radii::ZERO;
    }

    fn tree_row_at(&self, tree: Rect, p: Point) -> Option<usize> {
        if !tree.contains(p) {
            return None;
        }
        let k = ((p.y - tree.y - 4.0 + self.scroll.y) / ROW).floor();
        (k >= 0.0 && (k as usize) < self.rows.len()).then_some(k as usize)
    }

    pub fn mouse_move(&mut self, body: Rect, p: Point, _fonts: &Fonts) -> (bool, Vec<Action>) {
        self.reveal = None;
        let parts = parts(body);
        let hover = self.tree_row_at(parts.tree, p);
        let changed = hover != self.hover;
        self.hover = hover;
        let node = hover
            .and_then(|k| self.rows.get(k))
            .filter(|r| matches!(r.kind, DomRowKind::Open { .. } | DomRowKind::Close { .. }))
            .map(|r| r.node);
        let mut actions = Vec::new();
        if node != self.hovered_node {
            self.hovered_node = node;
            actions.push(Action::Send(DevtoolsCommand::Hover(node)));
        }
        (changed, actions)
    }

    pub fn mouse_leave(&mut self) -> (bool, Vec<Action>) {
        let changed = self.hover.take().is_some();
        let mut actions = Vec::new();
        if self.hovered_node.take().is_some() {
            actions.push(Action::Send(DevtoolsCommand::Hover(None)));
        }
        (changed, actions)
    }

    pub fn mouse_down(
        &mut self,
        body: Rect,
        p: Point,
        button: MouseButton,
        clicks: u32,
        fonts: &Fonts,
    ) -> (Option<FieldId>, Vec<Action>) {
        // A press in the editor places its caret; anywhere else ends it (the change goes in).
        let mut before = Vec::new();
        if self.edit.is_some() {
            let tree = parts(body).tree;
            match self.edit_rect(tree, fonts) {
                Some(r) if r.contains(p) && button == MouseButton::Left => {
                    if let Some(e) = &mut self.edit {
                        e.area.press(fonts, Face::Mono, r, p, TEXT, clicks, false);
                    }
                    return (Some(FieldId::ElementEdit), Vec::new());
                }
                _ => before = self.finish_edit(true),
            }
        }
        let (focus, mut actions) = self.press_in_tree(body, p, button, clicks, fonts);
        before.append(&mut actions);
        (focus, before)
    }

    fn press_in_tree(
        &mut self,
        body: Rect,
        p: Point,
        button: MouseButton,
        clicks: u32,
        fonts: &Fonts,
    ) -> (Option<FieldId>, Vec<Action>) {
        self.reveal = None;
        let parts = parts(body);
        if parts.filter.contains(p) && self.details.is_some() {
            let style = super::FieldStyle {
                boxed: true,
                ..field_style(true, true, "", Face::Ui)
            };
            let at = widgets::field_index_at(fonts, &self.style_filter, parts.filter, p.x, &style);
            self.style_filter.set_caret(at, false);
            return (Some(FieldId::StyleFilter), Vec::new());
        }
        let Some(k) = self.tree_row_at(parts.tree, p) else {
            return (None, Vec::new());
        };
        let row = self.rows[k].clone();
        if button == MouseButton::Right {
            // The context menu: the node is selected first when it is an element, as in Chrome.
            let kind = match row.kind {
                DomRowKind::Open { .. } | DomRowKind::Close { .. } => NodeKind::Element,
                DomRowKind::Text(_) => NodeKind::Text,
                DomRowKind::Comment(_) => NodeKind::Comment,
                DomRowKind::Doctype(_) => NodeKind::Doctype,
            };
            let mut actions = Vec::new();
            if kind == NodeKind::Element {
                actions.extend(self.select(Some(row.node)));
            }
            let (spans, _) = Self::attr_spans(&row, fonts);
            let text_x = parts.tree.x + 6.0 + row.depth as f32 * INDENT + 12.0;
            let attr = spans
                .into_iter()
                .find(|(_, x0, x1)| p.x >= text_x + x0 && p.x < text_x + x1)
                .map(|(n, ..)| n);
            actions.push(Action::NodeMenu {
                target: NodeTarget {
                    node: row.node,
                    kind,
                    attr,
                    root: row.depth == 0 && kind == NodeKind::Element,
                    forced: self.forced.get(&row.node).copied().unwrap_or(0),
                },
                at: p,
            });
            return (None, actions);
        }
        let x0 = parts.tree.x + 6.0 + row.depth as f32 * INDENT;
        let on_triangle = p.x >= x0 - 2.0 && p.x < x0 + 12.0;
        let mut actions = Vec::new();
        if row.expandable && (on_triangle || clicks >= 2) && button == MouseButton::Left {
            actions.extend(self.toggle(row.node));
        }
        if !matches!(
            row.kind,
            DomRowKind::Text(_) | DomRowKind::Comment(_) | DomRowKind::Doctype(_)
        ) {
            actions.extend(self.select(Some(row.node)));
        }
        (None, actions)
    }

    pub fn wheel(&mut self, body: Rect, p: Point, dy: f32) -> bool {
        self.reveal = None;
        let parts = parts(body);
        if parts.styles.contains(p) {
            let n = self
                .details
                .as_ref()
                .map_or(0, |(_, d)| self.computed(d).len());
            return self.styles_scroll.by(dy, n as f32 * ROW, parts.list.height);
        }
        let content = self.rows.len() as f32 * ROW + 8.0;
        self.scroll.by(dy, content, parts.tree.height)
    }

    /// Up/Down move the selection along the elements; Right expands, Left collapses; Ctrl+C
    /// copies the selected node's markup (Chrome's `DOMTreeWidget.onCopyOrCut`: `copyOuterHTML`).
    pub fn key(&mut self, body: Rect, key: u16, mods: Modifiers) -> Option<Vec<Action>> {
        if mods.ctrl && !mods.alt && key == b'C' as u16 {
            return Some(
                self.selected
                    .map(|n| {
                        Action::Send(DevtoolsCommand::Node(
                            n,
                            NodeOp::Copy(snowlark_page::devtools_node::CopyKind::OuterHtml),
                        ))
                    })
                    .into_iter()
                    .collect(),
            );
        }
        self.reveal = None;
        let elements: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.kind, DomRowKind::Open { .. }))
            .map(|(i, _)| i)
            .collect();
        let cur = self
            .selected
            .and_then(|n| self.row_index(n))
            .and_then(|i| elements.iter().position(|&e| e == i));
        let tree = parts(body).tree;
        match key {
            keys::UP | keys::DOWN => {
                let next = match (cur, key) {
                    (None, _) => elements.first().copied(),
                    (Some(c), keys::UP) => elements.get(c.saturating_sub(1)).copied(),
                    (Some(c), _) => elements
                        .get((c + 1).min(elements.len().saturating_sub(1)))
                        .copied(),
                };
                let Some(i) = next else {
                    return Some(Vec::new());
                };
                self.scroll.reveal(4.0 + i as f32 * ROW, ROW, tree.height);
                let node = self.rows[i].node;
                Some(self.select(Some(node)))
            }
            keys::RIGHT | keys::LEFT => {
                let Some(i) = self.selected.and_then(|n| self.row_index(n)) else {
                    return Some(Vec::new());
                };
                let row = &self.rows[i];
                let want_open = key == keys::RIGHT;
                if row.expandable && row.expanded != want_open {
                    let node = row.node;
                    return Some(self.toggle(node));
                }
                Some(Vec::new())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> NodeId {
        NodeId::from_bits(n << 32 | 1).unwrap()
    }

    fn open(n: u64, depth: u16, tag: &str, expandable: bool, expanded: bool) -> DomRow {
        DomRow {
            node: id(n),
            depth,
            kind: DomRowKind::Open {
                tag: tag.into(),
                attrs: vec![("class".into(), "x".into())],
                inline_text: None,
                empty: !expandable,
            },
            expandable,
            expanded,
        }
    }

    #[test]
    fn rows_selection_and_expansion() {
        let c = super::super::colors(false);
        let rows = vec![
            open(1, 0, "html", true, true),
            open(2, 1, "body", true, false),
            DomRow {
                node: id(1),
                depth: 0,
                kind: DomRowKind::Close { tag: "html".into() },
                expandable: false,
                expanded: false,
            },
        ];
        let text: String = row_segments(&rows[1], &c)
            .iter()
            .map(|(t, _)| t.as_ref())
            .collect();
        assert_eq!(text, "<body class=\"x\">…</body>");
        let img = open(9, 0, "img", false, false);
        let text: String = row_segments(&img, &c)
            .iter()
            .map(|(t, _)| t.as_ref())
            .collect();
        assert_eq!(text, "<img class=\"x\">");
        let mut v = ElementsView::default();
        v.set_rows(rows);
        assert!(v.expanded.contains(&id(1)));
        let body = Rect::new(0.0, 0.0, 900.0, 400.0);
        // Down selects the first element, then the next; Right expands it.
        assert_eq!(
            v.key(body, keys::DOWN, Modifiers::default()),
            Some(vec![Action::Send(DevtoolsCommand::Select(Some(id(1))))])
        );
        v.key(body, keys::DOWN, Modifiers::default());
        assert_eq!(v.selected, Some(id(2)));
        let a = v.key(body, keys::RIGHT, Modifiers::default()).unwrap();
        assert!(matches!(&a[0], Action::Send(DevtoolsCommand::Tree(e)) if e.contains(&id(2))));
        // Ctrl+C copies the selected node's outerHTML.
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        assert_eq!(
            v.key(body, b'C' as u16, ctrl),
            Some(vec![Action::Send(DevtoolsCommand::Node(
                id(2),
                NodeOp::Copy(snowlark_page::devtools_node::CopyKind::OuterHtml)
            ))])
        );
        // A pick selects and expands the path, and its row scrolls into view when it comes.
        v.picked(id(7), vec![id(1), id(2)]);
        assert_eq!(v.selected, Some(id(7)));
        assert!(v.expanded.contains(&id(2)));
        v.tree_height.set(100.0);
        let mut rows: Vec<DomRow> = (10..60).map(|n| open(n, 2, "p", false, false)).collect();
        rows.push(open(7, 2, "em", false, false));
        v.set_rows(rows);
        assert_eq!(v.selected, Some(id(7)));
        assert!(v.scroll.y > 0.0 && v.reveal.is_none());
    }

    #[test]
    fn a_right_press_selects_an_element_and_asks_for_the_menu() {
        let mut v = ElementsView::default();
        let text = DomRow {
            node: id(5),
            depth: 2,
            kind: DomRowKind::Text("hello".into()),
            expandable: false,
            expanded: false,
        };
        v.set_rows(vec![
            open(1, 0, "html", true, true),
            open(2, 1, "body", true, false),
            text,
        ]);
        let body = Rect::new(0.0, 0.0, 900.0, 400.0);
        let fonts = Fonts::new(std::sync::Arc::new(
            snowlark_paint::TextEngine::new().unwrap(),
        ));
        // Row 1 (the body) is at y = 4 + 20 .. 44.
        let p = Point::new(34.0, 4.0 + ROW * 1.5);
        let (focus, actions) = v.mouse_down(body, p, MouseButton::Right, 1, &fonts);
        assert_eq!(focus, None);
        assert_eq!(
            actions,
            [
                Action::Send(DevtoolsCommand::Select(Some(id(2)))),
                Action::NodeMenu {
                    target: NodeTarget {
                        node: id(2),
                        kind: NodeKind::Element,
                        attr: None,
                        root: false,
                        forced: 0
                    },
                    at: p
                }
            ]
        );
        assert_eq!(v.selected, Some(id(2)));
        // A text node asks for its menu too, without becoming the selection.
        let p = Point::new(100.0, 4.0 + ROW * 2.5);
        let (_, actions) = v.mouse_down(body, p, MouseButton::Right, 1, &fonts);
        assert_eq!(
            actions,
            [Action::NodeMenu {
                target: NodeTarget {
                    node: id(5),
                    kind: NodeKind::Text,
                    attr: None,
                    root: false,
                    forced: 0
                },
                at: p
            }]
        );
        assert_eq!(v.selected, Some(id(2)));
        // A left press does not.
        let (_, actions) = v.mouse_down(body, p, MouseButton::Left, 1, &fonts);
        assert!(actions
            .iter()
            .all(|a| !matches!(a, Action::NodeMenu { .. })));
    }

    fn editor_view() -> (ElementsView, Rect, Fonts) {
        let mut v = ElementsView::default();
        v.set_rows(vec![
            open(1, 0, "html", true, true),
            open(2, 1, "body", true, false),
        ]);
        let fonts = Fonts::new(std::sync::Arc::new(
            snowlark_paint::TextEngine::new().unwrap(),
        ));
        (v, Rect::new(0.0, 0.0, 900.0, 400.0), fonts)
    }

    fn press(a: &mut ElementsView, k: u16, ctrl: bool) -> Vec<Action> {
        let clip = crate::driver::MemoryClipboard::default();
        a.edit_key(
            k,
            Modifiers {
                ctrl,
                ..Default::default()
            },
            &clip,
        )
    }

    #[test]
    fn an_attribute_edit_goes_to_the_page_on_enter_and_not_on_escape_or_when_unchanged() {
        let (mut v, _, _) = editor_view();
        v.begin_edit(id(2), EditKind::Attribute("class".into()), "class=\"x\"");
        assert!(v.editing());
        for c in " id=b".chars() {
            assert!(v.edit_typed(c));
        }
        let a = press(&mut v, keys::ENTER, false);
        assert_eq!(
            a,
            [Action::Send(DevtoolsCommand::Node(
                id(2),
                NodeOp::SetAttributes {
                    replacing: Some("class".into()),
                    text: "class=\"x\" id=b".into()
                }
            ))]
        );
        assert!(!v.editing());
        // Escape throws the edit away; so does an edit that changed nothing.
        v.begin_edit(id(2), EditKind::Attribute("class".into()), "class=\"x\"");
        v.edit_typed('z');
        assert!(press(&mut v, keys::ESCAPE, false).is_empty() && !v.editing());
        v.begin_edit(id(2), EditKind::Attribute("class".into()), "class=\"x\"");
        assert!(press(&mut v, keys::ENTER, false).is_empty() && !v.editing());
        // A new attribute with nothing written adds nothing.
        v.begin_edit(id(2), EditKind::AddAttribute, "");
        assert!(press(&mut v, keys::ENTER, false).is_empty());
        v.begin_edit(id(2), EditKind::AddAttribute, "");
        for c in "data-a=1".chars() {
            v.edit_typed(c);
        }
        let a = press(&mut v, keys::ENTER, false);
        assert!(matches!(
            &a[..],
            [Action::Send(DevtoolsCommand::Node(
                _,
                NodeOp::SetAttributes { replacing: None, text }
            ))] if text == "data-a=1"
        ));
    }

    #[test]
    fn the_markup_editor_takes_lines_and_ctrl_enter_commits() {
        let (mut v, _, _) = editor_view();
        v.begin_edit(id(2), EditKind::Html, "<body>");
        press(&mut v, keys::ENTER, false);
        assert!(v.editing(), "Enter is a new line");
        for c in "</body>".chars() {
            v.edit_typed(c);
        }
        let a = press(&mut v, keys::ENTER, true);
        assert_eq!(
            a,
            [Action::Send(DevtoolsCommand::Node(
                id(2),
                NodeOp::SetOuterHtml("<body>\n</body>".into())
            ))]
        );
    }

    #[test]
    fn a_press_outside_the_editor_commits_and_inside_places_the_caret() {
        let (mut v, body, fonts) = editor_view();
        v.begin_edit(id(2), EditKind::Html, "<body>");
        v.edit_typed('!');
        let tree = parts(body).tree;
        let r = v.edit_rect(tree, &fonts).expect("a box");
        assert!(r.height >= 20.0 && r.right() <= tree.right() && r.bottom() <= tree.bottom());
        // Inside: the editor keeps the keyboard.
        let inside = Point::new(r.x + 20.0, r.y + 10.0);
        let (focus, actions) = v.mouse_down(body, inside, MouseButton::Left, 1, &fonts);
        assert_eq!((focus, actions.len()), (Some(FieldId::ElementEdit), 0));
        assert!(v.editing());
        // Outside: the edit commits and the press goes on to the tree.
        let outside = Point::new(tree.right() - 5.0, tree.bottom() - 5.0);
        let (focus, actions) = v.mouse_down(body, outside, MouseButton::Left, 1, &fonts);
        assert_eq!(focus, None);
        assert!(matches!(
            &actions[..],
            [Action::Send(DevtoolsCommand::Node(_, NodeOp::SetOuterHtml(t)))] if t == "<body>!"
        ));
        assert!(!v.editing());
    }

    #[test]
    fn a_right_press_on_an_attribute_names_it() {
        let (mut v, body, fonts) = editor_view();
        // `<body class="x">` at depth 1: the row's text starts at 6 + 14 + 12.
        let text_x = 6.0 + INDENT + 12.0;
        let tag = fonts.width_in(Face::Mono, "<body", TEXT);
        let y = 4.0 + ROW * 1.5;
        let on_attr = Point::new(text_x + tag + 12.0, y);
        let (_, a) = v.mouse_down(body, on_attr, MouseButton::Right, 1, &fonts);
        assert!(matches!(
            a.last(),
            Some(Action::NodeMenu { target, .. }) if target.attr.as_deref() == Some("class")
        ));
        let on_tag = Point::new(text_x + 2.0, y);
        let (_, a) = v.mouse_down(body, on_tag, MouseButton::Right, 1, &fonts);
        assert!(matches!(
            a.last(),
            Some(Action::NodeMenu { target, .. }) if target.attr.is_none()
        ));
        // (the html row is the document element)
        let top = Point::new(100.0, 4.0 + ROW * 0.5);
        let (_, a) = v.mouse_down(body, top, MouseButton::Right, 1, &fonts);
        assert!(matches!(
            a.last(),
            Some(Action::NodeMenu { target, .. }) if target.root
        ));
    }
}
