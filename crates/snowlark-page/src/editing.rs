// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Editing and the document selection (HTML §6.8 editing hosts, `contenteditable`, `designMode`;
//! Input Events Level 2; the de facto editing conventions browsers share):
//!
//! - the mouse places the document selection (a press collapses it at the nearest text position, Shift
//!   extends, a drag selects, double- and triple-clicks select a word or a block's text) on any page;
//!   Ctrl+A selects the editing host's contents or the body's, Ctrl+C copies the selection's rendered
//!   text;
//! - in an editing host typing inserts text, Enter splits the block (a new `div` when the text sits in
//!   the host itself), Shift+Enter inserts a line break, Backspace and Delete remove a character, a
//!   word with Ctrl, or merge blocks across a block boundary, arrows move by character, by word with
//!   Ctrl and by line (Up/Down, Home/End through layout's line boxes), Shift extends; each edit fires
//!   a cancelable `beforeinput` and then `input` with its `inputType`; the clipboard pastes plain text;
//! - `execCommand` undoes and redoes; the other commands are the HTML Editing APIs'
//!   (`exec_command`);
//! - the selection is painted (highlight in `::selection` colors, the caret of a focused host).

use snowlark_bindings::rt::Cx;
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, Dom, NodeId};
use snowlark_paint::{Color, Point, Rect};

use crate::caret::Granularity;
use crate::engine::events::{EventExt, UiFields};
use crate::engine::ranges::{bp_position, composed_position, focus_in_tree_of, BoundaryPair, Bp};
use crate::engine::{node_handle, Engine};
use crate::input::Modifiers;
use crate::keyboard::{KeyInput, KeyOutcome};

/// A press-and-drag selecting in a document: its document, the anchor (a word or block for double
/// and triple clicks) and the unit it extends by.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DocDrag {
    pub doc: NodeId,
    pub anchor: (Bp, Bp),
    pub granularity: Granularity,
}

/// A user edit's InputEvent fields: its `inputType`, `data`, the ranges `beforeinput`'s
/// `getTargetRanges()` returns and its `dataTransfer`.
pub(crate) struct EditInput<'a> {
    pub input_type: &'a str,
    pub data: Option<&'a str>,
    pub ranges: Vec<BoundaryPair>,
    pub data_transfer: Option<snowlark_bindings::rt::Handle>,
}

impl<'a> EditInput<'a> {
    /// An edit that replaces one range (the selection).
    pub(crate) fn new(input_type: &'a str, data: Option<&'a str>, range: BoundaryPair) -> Self {
        Self {
            input_type,
            data,
            ranges: vec![range],
            data_transfer: None,
        }
    }
}

/// The user agent's selection colors, where the originating element's `::selection` declares
/// neither `color` nor `background-color` (CSS Pseudo-Elements 4 §3.3, §3.3.1): text in
/// `HighlightText` on `Highlight`, here Windows' default highlight (0, 120, 215), drawn translucent
/// so that it looks the same over white, and white text.
const SELECTION_BG: Color = blend_with_white(0x00, 0x78, 0xd7);
const SELECTION_TEXT: Color = Color::rgba(0xff, 0xff, 0xff, 0xff);

/// The elements laid out as replaced boxes whose selection paints a highlight over them.
fn is_replaced_name(name: &snowlark_dom::LocalName) -> bool {
    *name == local_name!("img")
        || *name == local_name!("video")
        || *name == local_name!("canvas")
        || *name == local_name!("svg")
        || *name == local_name!("iframe")
        || *name == local_name!("embed")
        || *name == local_name!("object")
}

/// The translucent color that looks like the opaque `(r, g, b)` over white: the least opacity of
/// 60%, 66.7%, 73.3% and 80% that needs no negative component (at 80%, those still negative are
/// 0), so text and backgrounds under a highlight show through it.
const fn blend_with_white(r: u8, g: u8, b: u8) -> Color {
    const fn component(c: u8, a: i32) -> i32 {
        (c as i32 - (255 - a)) * 255 / a
    }
    const fn clamp(v: i32) -> u8 {
        if v < 0 {
            0
        } else if v > 255 {
            255
        } else {
            v as u8
        }
    }
    let mut a = 153;
    loop {
        let (cr, cg, cb) = (component(r, a), component(g, a), component(b, a));
        if (cr >= 0 && cg >= 0 && cb >= 0) || a >= 204 {
            return Color::rgba(clamp(cr), clamp(cg), clamp(cb), a as u8);
        }
        a += 17;
    }
}

fn utf16_len(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

fn byte_of(s: &str, utf16: u32) -> usize {
    let mut n = 0u32;
    for (i, c) in s.char_indices() {
        if n >= utf16 {
            return i;
        }
        n += c.len_utf16() as u32;
    }
    s.len()
}

fn utf16_of(s: &str, byte: usize) -> u32 {
    utf16_len(&s[..byte.min(s.len())])
}

/// The block-level elements editing splits and merges (by tag: layout may not have run).
fn is_block_tag(dom: &Dom, el: NodeId) -> bool {
    if !dom.is_element(el) || dom.namespace(el) != &snowlark_dom::ns!(html) {
        return false;
    }
    matches!(
        &**dom.local_name(el),
        "p" | "div"
            | "li"
            | "ul"
            | "ol"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "blockquote"
            | "pre"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "nav"
            | "aside"
            | "address"
            | "dd"
            | "dt"
            | "dl"
            | "td"
            | "th"
            | "table"
            | "tr"
            | "body"
            | "html"
            | "form"
            | "fieldset"
            | "figure"
            | "figcaption"
            | "hr"
    )
}

impl Engine {
    // ----- editing hosts -----------------------------------------------------------------------------------

    /// The `contenteditable` state of an element: `Some(true)` for true and plaintext-only,
    /// `Some(false)` for false, `None` to inherit.
    fn editable_attr(dom: &Dom, el: NodeId) -> Option<bool> {
        if !dom.is_element(el) || dom.namespace(el) != &snowlark_dom::ns!(html) {
            return None;
        }
        match dom
            .get_attribute(el, "contenteditable")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("") | Some("true") | Some("plaintext-only") => Some(true),
            Some("false") => Some(false),
            _ => None,
        }
    }

    /// Whether `node` is editable: the nearest `contenteditable` state among its inclusive
    /// ancestors, else its document's design mode.
    pub(crate) fn is_editable(&self, node: NodeId) -> bool {
        let dom = self.dom();
        if !dom.is_alive(node) || !dom.is_connected(node) {
            return false;
        }
        for a in dom.inclusive_ancestors(node) {
            if let Some(v) = Self::editable_attr(&dom, a) {
                return v;
            }
        }
        let doc = dom.node_document(node);
        drop(dom);
        self.doc_state(doc, |s| s.design_mode)
    }

    /// HTML §6.8.1 the editing host of `node`: the outermost editable element of its editable
    /// ancestors (the document element in design mode).
    pub(crate) fn editing_host(&self, node: NodeId) -> Option<NodeId> {
        let el = {
            let dom = self.dom();
            if dom.is_element(node) {
                node
            } else {
                dom.parent_element(node)?
            }
        };
        if !self.is_editable(el) {
            return None;
        }
        let mut host = el;
        loop {
            let parent = self.dom().parent_element(host);
            match parent {
                Some(p) if self.is_editable(p) => host = p,
                _ => return Some(host),
            }
        }
    }

    /// A resolved style value of an element (empty when it has no style).
    pub(crate) fn style_of_element(&self, el: NodeId, property: &str) -> String {
        self.with_style(el, |r| r.resolved_style_property(el, property))
            .flatten()
            .unwrap_or_default()
    }

    /// When an editing host gains focus and the selection is not in it, the selection collapses at
    /// its first caret position (the de facto behaviour HTML leaves to the user agent: a focused
    /// editor shows a caret, so typing goes into it).
    pub(crate) fn place_caret_on_focus(&self, el: NodeId) {
        let doc = {
            let dom = self.dom();
            if !dom.is_alive(el) || !dom.is_connected(el) {
                return;
            }
            dom.node_document(el)
        };
        // (a text control keeps its own selection, `contenteditable` or not)
        if self.editing_host(el) != Some(el) || self.is_text_control(el) {
            return;
        }
        if let Some((r, _)) = self.doc_selection(doc) {
            if self.dom().is_inclusive_ancestor_of(el, r.start.0) {
                return;
            }
        }
        let at = self.first_caret_position(el);
        self.set_doc_selection(doc, Some((at, at)));
    }

    /// The first caret position in an editing host: before its first visible character or `br`,
    /// else, when an atomic inline (a form control, an image) or a non-editable element comes
    /// first, the start of the block that holds it; else the start of the host.
    fn first_caret_position(&self, host: NodeId) -> Bp {
        let mut cur = self.dom().first_child(host);
        while let Some(node) = cur {
            let (is_element, is_text) = {
                let dom = self.dom();
                (dom.is_element(node), dom.is_text(node))
            };
            let mut descend = false;
            if is_element {
                if self.style_of_element(node, "display") == "none" {
                    // (not rendered: no caret position in it)
                } else if self.dom().is_html_element_named(node, &local_name!("br")) {
                    let dom = self.dom();
                    return (dom.parent(node).unwrap_or(host), dom.index(node) as u32);
                } else if !self.is_editable(node) || self.is_atomic_for_caret(node) {
                    return (self.block_or_host_of(node, host), 0);
                } else {
                    descend = true;
                }
            } else if is_text {
                if let Some(o) = self.first_visible_offset(node) {
                    return (node, o);
                }
            }
            let dom = self.dom();
            cur = match dom.first_child(node).filter(|_| descend) {
                Some(c) => Some(c),
                None => dom.next_skipping_children(node, Some(host)),
            };
        }
        (host, 0)
    }

    /// Elements a caret goes before or after but never into.
    pub(crate) fn is_atomic_for_caret(&self, el: NodeId) -> bool {
        let dom = self.dom();
        dom.namespace(el) != &snowlark_dom::ns!(html)
            || matches!(
                &**dom.local_name(el),
                "input"
                    | "textarea"
                    | "select"
                    | "button"
                    | "img"
                    | "hr"
                    | "video"
                    | "audio"
                    | "iframe"
                    | "object"
                    | "embed"
                    | "canvas"
                    | "meter"
                    | "progress"
            )
    }

    /// The nearest ancestor of `node` that is block-level or the host.
    fn block_or_host_of(&self, node: NodeId, host: NodeId) -> NodeId {
        let mut a = self.dom().parent(node).unwrap_or(host);
        while a != host {
            if !self.style_of_element(a, "display").starts_with("inline") {
                return a;
            }
            a = self.dom().parent(a).unwrap_or(host);
        }
        host
    }

    /// The offset of a text node's first character that is not collapsed white space (CSS Text
    /// §4.1.1: leading spaces and tabs collapse unless `white-space` preserves spaces, line feeds
    /// unless it preserves line breaks); `None` when all of it collapses.
    fn first_visible_offset(&self, text: NodeId) -> Option<u32> {
        let (parent, data) = {
            let dom = self.dom();
            (
                dom.parent_element(text)?,
                dom.character_data(text).unwrap_or("").to_owned(),
            )
        };
        if data.is_empty() {
            return None;
        }
        let ws = self.style_of_element(parent, "white-space");
        let keep_spaces = matches!(ws.as_str(), "pre" | "pre-wrap" | "break-spaces");
        let keep_breaks = keep_spaces || ws == "pre-line";
        let mut offset = 0u32;
        for c in data.chars() {
            let collapses = match c {
                ' ' | '\t' => !keep_spaces,
                '\n' | '\r' => !keep_breaks,
                _ => false,
            };
            if !collapses {
                return Some(offset);
            }
            offset += c.len_utf16() as u32;
        }
        None
    }

    /// Whether `host` is a `contenteditable=plaintext-only` host.
    fn plaintext_only(&self, host: NodeId) -> bool {
        self.dom()
            .get_attribute(host, "contenteditable")
            .is_some_and(|v| v.eq_ignore_ascii_case("plaintext-only"))
    }

    /// The editing host the user is editing: the focused element of the focused document when it is
    /// an editing host (or, in design mode, the document element).
    pub(crate) fn active_editing_host(&self) -> Option<NodeId> {
        let doc = self.focus_document()?;
        if let Some(f) = self.focused_element(doc) {
            if self.editing_host(f) == Some(f) {
                return Some(f);
            }
            if self.is_text_control(f) {
                return None;
            }
        }
        if self.doc_state(doc, |s| s.design_mode) {
            return self.dom().document_element(doc);
        }
        None
    }

    // ----- positions ---------------------------------------------------------------------------------------

    /// A position in a text node where one is next to `bp` (the text at or before an element
    /// position), else `bp`.
    fn text_position(&self, bp: Bp) -> Bp {
        let dom = self.dom();
        let (n, o) = bp;
        if dom.is_text(n) {
            return bp;
        }
        if let Some(c) = dom.child_at(n, o as usize).filter(|&c| dom.is_text(c)) {
            return (c, 0);
        }
        if o > 0 {
            if let Some(c) = dom.child_at(n, o as usize - 1).filter(|&c| dom.is_text(c)) {
                return (c, dom.length(c) as u32);
            }
        }
        bp
    }

    /// The next caret position by character or word (Selection.modify, arrow keys), within the
    /// editing host or the document.
    pub(crate) fn step_position(
        &self,
        doc: NodeId,
        bp: Bp,
        forward: bool,
        word: bool,
    ) -> Option<Bp> {
        let root = self
            .editing_host(bp.0)
            .or_else(|| self.dom().body(doc))
            .or_else(|| self.dom().document_element(doc))?;
        if word {
            self.step_word(bp, root, forward)
        } else {
            self.step_character(bp, root, forward)
        }
    }

    /// A text node's caret rectangle in its document's viewport, at a UTF-16 offset.
    fn caret_rect_of(&self, bp: Bp) -> Option<Rect> {
        let (n, o) = self.text_position(bp);
        let byte = {
            let dom = self.dom();
            if !dom.is_text(n) {
                return None;
            }
            byte_of(dom.character_data(n).unwrap_or(""), o) as u32
        };
        self.with_layout(n, |r, st| r.caret_rect(st.viewport, st.scroll, n, byte))
            .flatten()
    }

    /// The caret position nearest a point of `doc`'s viewport, among the text inside `within`.
    pub(crate) fn caret_at_point(
        &self,
        doc: NodeId,
        p: Point,
        within: Option<NodeId>,
    ) -> Option<Bp> {
        let found = self
            .with_layout(doc, |r, st| {
                let dom = self.dom();
                let keep = |n: NodeId| within.is_none_or(|w| dom.is_inclusive_ancestor_of(w, n));
                r.caret_position(st.viewport, st.scroll, p, &keep)
            })
            .flatten();
        match found {
            Some((n, byte)) => {
                let dom = self.dom();
                let s = dom.character_data(n).unwrap_or("");
                Some((n, utf16_of(s, byte as usize)))
            }
            None => within.map(|w| (w, 0)),
        }
    }

    /// The start or end of the line a position is on (layout's line boxes).
    pub(crate) fn line_boundary(&self, doc: NodeId, bp: Bp, end: bool) -> Option<Bp> {
        let r = self.caret_rect_of(bp)?;
        let host = self.editing_host(bp.0);
        let far = if end { 1.0e6 } else { -1.0e6 };
        let p = match self.vertical_flow(bp, r) {
            None => Point::new(far, r.y + r.height / 2.0),
            Some(_) => Point::new(r.x + r.width / 2.0, far),
        };
        self.caret_at_point(doc, p, host)
    }

    /// For a position in vertical text (its caret rectangle `r` lies across the line, as layout
    /// turns the line), whether lines follow each other rightwards (`vertical-lr`, `sideways-lr`);
    /// `None` in horizontal text.
    fn vertical_flow(&self, bp: Bp, r: Rect) -> Option<bool> {
        if r.width <= r.height {
            return None;
        }
        let el = {
            let dom = self.dom();
            if dom.is_element(bp.0) {
                Some(bp.0)
            } else {
                dom.parent_element(bp.0)
            }
        }?;
        let mode = self.style_of_element(el, "writing-mode");
        Some(mode.ends_with("lr"))
    }

    /// The position a line above or below.
    pub(crate) fn line_step(&self, doc: NodeId, bp: Bp, down: bool) -> Option<Bp> {
        self.line_step_at(doc, bp, None, down)
    }

    /// The position a line above or below `bp`, at the horizontal position of `x_of` (`bp` when
    /// `None`): a selection moved by lines keeps its focus's column.
    pub(crate) fn line_step_at(
        &self,
        doc: NodeId,
        bp: Bp,
        x_of: Option<Bp>,
        down: bool,
    ) -> Option<Bp> {
        let r = self.caret_rect_of(bp)?;
        let column = match x_of {
            Some(f) if f != bp => self.caret_rect_of(f).unwrap_or(r),
            _ => r,
        };
        let host = self.editing_host(bp.0);
        // In vertical writing modes lines stack along x: the next line is to the right in
        // `vertical-lr`, to the left in `vertical-rl`.
        let (p, moved): (Point, fn(Rect, Rect) -> bool) = match self.vertical_flow(bp, r) {
            None => (
                Point::new(column.x, if down { r.bottom() + 1.0 } else { r.y - 1.0 }),
                |t, r| (t.y - r.y).abs() > r.height / 4.0,
            ),
            Some(rightwards) => (
                Point::new(
                    if down == rightwards {
                        r.right() + 1.0
                    } else {
                        r.x - 1.0
                    },
                    column.y,
                ),
                |t, r| (t.x - r.x).abs() > r.width / 4.0,
            ),
        };
        let to = self.caret_at_point(doc, p, host)?;
        // (no line beyond: the caret goes to the text's start or end)
        let moved = self.caret_rect_of(to).is_some_and(|t| moved(t, r));
        if moved {
            Some(to)
        } else {
            self.line_boundary(doc, bp, down)
        }
    }

    /// The word around a position (a text node's).
    fn word_range(&self, bp: Bp) -> (Bp, Bp) {
        let (n, o) = self.text_position(bp);
        let dom = self.dom();
        match dom.character_data(n) {
            Some(s) if dom.is_text(n) => {
                let (a, b) = crate::caret::word_at(s, o);
                ((n, a), (n, b))
            }
            _ => (bp, bp),
        }
    }

    /// The text of the block a position is in: from its first text to its last.
    fn block_range(&self, bp: Bp) -> (Bp, Bp) {
        let block = {
            let dom = self.dom();
            dom.inclusive_ancestors(bp.0)
                .find(|&a| is_block_tag(&dom, a))
                .unwrap_or(bp.0)
        };
        let dom = self.dom();
        let texts: Vec<NodeId> = dom
            .descendants(block)
            .filter(|&n| dom.is_text(n) && dom.length(n) > 0)
            .collect();
        match (texts.first(), texts.last()) {
            (Some(&a), Some(&b)) => ((a, 0), (b, dom.length(b) as u32)),
            _ => (bp, bp),
        }
    }

    // ----- the mouse ---------------------------------------------------------------------------------------

    /// Where `doc`'s viewport origin is in the top-level viewport.
    fn doc_origin_in_top(&self, doc: NodeId) -> Point {
        match self.rendered_container(doc) {
            Some(c) => self
                .top_viewport_box(c, true)
                .map_or(Point::ZERO, |r| Point::new(r.x, r.y)),
            None => Point::ZERO,
        }
    }

    /// A primary press at `p` (`doc`'s viewport) on `target`, not on a form control: the document
    /// selection (a caret, a word, a block; Shift extends) and the drag that follows. `selectstart`
    /// can cancel it; `user-select: none` content takes no selection.
    pub(crate) fn doc_pointer_pressed(
        &self,
        cx: &mut Cx,
        target: NodeId,
        p: Point,
        clicks: u32,
        mods: Modifiers,
    ) {
        let doc = self.dom().node_document(target);
        if self.user_select_none(target) {
            return;
        }
        let host = self.editing_host(target);
        let Some(at) = self
            .caret_at_point(
                doc,
                p,
                host.or(Some(target)).filter(|&w| {
                    let dom = self.dom();
                    dom.descendants(w)
                        .any(|n| dom.is_text(n) && dom.length(n) > 0)
                }),
            )
            .or_else(|| self.caret_at_point(doc, p, None))
        else {
            return;
        };
        let (granularity, anchor) = match clicks {
            0 | 1 => (Granularity::Character, (at, at)),
            2 => (Granularity::Word, self.word_range(at)),
            _ => (Granularity::Line, self.block_range(at)),
        };
        // `selectstart` at the element where the selection starts (cancelable).
        let start_el = {
            let dom = self.dom();
            if dom.is_element(at.0) {
                at.0
            } else {
                dom.parent_element(at.0).unwrap_or(target)
            }
        };
        if !self.fire_event(cx, node_handle(start_el), "selectstart", true, true) {
            return;
        }
        let mut drag = DocDrag {
            doc,
            anchor,
            granularity,
        };
        if granularity == Granularity::Character && mods.shift {
            if let Some((a, _)) = self.doc_anchor_focus(doc) {
                drag.anchor = (a, a);
                self.set_doc_selection(doc, Some((a, at)));
            } else {
                self.set_doc_selection(doc, Some((at, at)));
            }
        } else {
            self.set_doc_selection(doc, Some(anchor));
        }
        self.pointer.borrow_mut().doc_drag = Some(drag);
    }

    /// The element of `doc` under `top` (a top-level viewport point), or its nearest ancestor,
    /// that contains text: where a drag's focus is looked for. `None` where the point is over
    /// nothing of `doc` (outside it, or over another document's frame).
    fn text_scope_under(&self, doc: NodeId, top: Point) -> Option<NodeId> {
        let hit = self.hit_element(top)?;
        let dom = self.dom();
        if dom.node_document(hit.element) != doc {
            return None;
        }
        let mut cur = Some(hit.element);
        while let Some(el) = cur {
            if dom
                .descendants(el)
                .any(|n| dom.is_text(n) && dom.length(n) > 0)
            {
                return Some(el);
            }
            cur = dom.parent_element(el);
        }
        None
    }

    /// The pointer moved to `top` (top-level viewport) with the button held after a press in a
    /// document: the selection follows by its unit.
    pub(crate) fn doc_pointer_dragged(&self, top: Point) {
        let Some(drag) = self.pointer.borrow().doc_drag else {
            return;
        };
        // (the text under the press can go while the button is down, freed or out of the document:
        // the anchors are ids, not a live range; the drag ends with them)
        let anchored = {
            let dom = self.dom();
            dom.is_alive(drag.doc)
                && [drag.anchor.0 .0, drag.anchor.1 .0]
                    .iter()
                    .all(|&n| dom.is_alive(n) && dom.shadow_including_root(n) == drag.doc)
        };
        if !anchored {
            self.pointer.borrow_mut().doc_drag = None;
            return;
        }
        let origin = self.doc_origin_in_top(drag.doc);
        let p = Point::new(top.x - origin.x, top.y - origin.y);
        // The focus is the position for the point in what is under the pointer, as for the press
        // (CSSOM View's caret position for a point hit tests first): text elsewhere on the same
        // line, such as a side column's beside a gap between paragraphs, is not a candidate.
        let within = self
            .editing_host(drag.anchor.0 .0)
            .or_else(|| self.text_scope_under(drag.doc, top));
        let Some(at) = self.caret_at_point(drag.doc, p, within) else {
            return;
        };
        let (f0, f1) = match drag.granularity {
            Granularity::Character => (at, at),
            Granularity::Word => self.word_range(at),
            Granularity::Line => self.block_range(at),
        };
        let (a0, a1) = drag.anchor;
        // (the pointer can be over text of another tree than the press: a shadow tree's, or the
        // document's after a press in a shadow tree)
        let ends = {
            let dom = self.dom();
            let before = composed_position(&dom, f0, a0) == std::cmp::Ordering::Less;
            let (anchor, focus) = if before { (a1, f0) } else { (a0, f1) };
            (anchor, focus_in_tree_of(&dom, anchor, focus))
        };
        if self.doc_anchor_focus(drag.doc) != Some(ends) {
            self.set_doc_selection(drag.doc, Some(ends));
        }
    }

    /// The primary button went up: a document drag ends.
    pub(crate) fn doc_drag_ended(&self) {
        self.pointer.borrow_mut().doc_drag = None;
    }

    // ----- painting ----------------------------------------------------------------------------------------

    /// The rendering of each rendered document's selection: the selected bytes of its text nodes,
    /// the caret of a focused editing host, `::selection` colors. Runs after the rendering
    /// update's style and layout, so that the colors are those of the styles about to be painted.
    pub(crate) fn sync_selection_paint(&self) {
        let editing = self.active_editing_host();
        let version = self.dom().version();
        for doc in self.rendered_documents() {
            // Unchanged selection, focus and DOM, and for selected text no restyle since: the paint
            // stays (a large selection is walked once).
            let range = self.doc_selection(doc).map(|(r, _)| (r.start, r.end));
            let restyled = match range {
                Some((start, end)) if start != end => self
                    .with_doc_renderer(doc, |r, _| r.stats().restyled)
                    .unwrap_or(0),
                _ => 0,
            };
            let key = (range, editing, version, restyled);
            if self.selection_paint_key.borrow().get(&doc) == Some(&key) {
                continue;
            }
            self.selection_paint_key.borrow_mut().insert(doc, key);
            let paint = self.selection_paint(doc, editing);
            self.with_doc_renderer(doc, |r, st| {
                if r.selection() != paint.as_ref() {
                    r.set_selection(paint);
                    st.needs_paint = true;
                }
            });
        }
    }

    fn selection_paint(
        &self,
        doc: NodeId,
        editing: Option<NodeId>,
    ) -> Option<snowlark_layout::TextSelection> {
        let (r, _) = self.doc_selection(doc)?;
        let dom = self.dom();
        if !dom.is_alive(r.start.0)
            || !dom.is_alive(r.end.0)
            || dom.shadow_including_root(r.start.0) != doc
        {
            return None;
        }
        let mut sel = snowlark_layout::TextSelection {
            background: SELECTION_BG,
            color: Some(SELECTION_TEXT),
            ..Default::default()
        };
        if r.start == r.end {
            // A caret, in a focused editing host.
            let host = editing.filter(|&h| dom.is_inclusive_ancestor_of(h, r.start.0))?;
            drop(dom);
            let (n, o) = self.text_position(r.start);
            let dom = self.dom();
            if dom.is_text(n) {
                let s = dom.character_data(n).unwrap_or("");
                sel.caret = Some((n, byte_of(s, o) as u32));
            } else {
                // An empty host or block: the caret at its content box's start.
                let el = if dom.is_element(n) { n } else { host };
                drop(dom);
                let rect = self
                    .with_layout(el, |rr, st| {
                        let g = rr.geometry(el)?;
                        let s = crate::engine::view::scroll_shift(rr, st, &self.dom(), el);
                        Some(g.content_box.translate(-s.x, -s.y))
                    })
                    .flatten();
                sel.caret_box = rect.map(|b| Rect::new(b.x, b.y, 1.0, b.height.clamp(8.0, 40.0)));
            }
            return Some(sel);
        }
        // The text nodes from the start to the end, with the selected bytes of each.
        let first = if dom.is_text(r.start.0) {
            Some(r.start.0)
        } else {
            dom.child_at(r.start.0, r.start.1 as usize)
                .or_else(|| dom.next_skipping_children(r.start.0, None))
        };
        // Where the walk stops: after the end node when the end is in character data, else at the
        // node the end boundary point is before (the child at its offset, or what follows the
        // container). One walk in tree order, no comparison per node.
        let end_in_data = dom.is_text(r.end.0)
            || matches!(
                dom.data(r.end.0),
                snowlark_dom::NodeData::Comment(_)
                    | snowlark_dom::NodeData::ProcessingInstruction { .. }
            );
        let stop = if end_in_data {
            None
        } else {
            dom.child_at(r.end.0, r.end.1 as usize)
                .or_else(|| dom.next_skipping_children(r.end.0, None))
        };
        let mut cur = first;
        // The originating elements of the selected text (each once per run of its text nodes),
        // and each text node's place among them.
        let mut origins: Vec<NodeId> = Vec::new();
        let mut styled: Vec<(NodeId, usize)> = Vec::new();
        // The replaced elements inside it, each its own originating element (only those laid out as
        // replaced boxes are painted).
        let mut replaced: Vec<(NodeId, usize)> = Vec::new();
        let mut last = None;
        while let Some(n) = cur {
            if Some(n) == stop {
                break;
            }
            if dom.is_text(n) {
                let s = dom.character_data(n).unwrap_or("");
                let a = if n == r.start.0 { r.start.1 } else { 0 };
                let b = if n == r.end.0 { r.end.1 } else { utf16_len(s) };
                if a < b {
                    sel.ranges.insert(
                        n,
                        snowlark_layout::SelectedText {
                            start: byte_of(s, a) as u32,
                            end: byte_of(s, b) as u32,
                            highlight: None,
                            continues: true,
                            shadows: 0,
                        },
                    );
                    last = Some(n);
                    if let Some(el) = dom.parent_element(n) {
                        if origins.last() != Some(&el) {
                            origins.push(el);
                        }
                        styled.push((n, origins.len() - 1));
                    }
                }
            }
            if dom.is_element(n) && is_replaced_name(dom.local_name(n)) {
                origins.push(n);
                replaced.push((n, origins.len() - 1));
            }
            if end_in_data && n == r.end.0 {
                break;
            }
            cur = dom.next_in_tree_order(n, None);
        }
        if let Some(r) = last.and_then(|n| sel.ranges.get_mut(&n)) {
            r.continues = false;
        }
        drop(dom);
        // Each text node's highlight colors are its originating element's (CSS Pseudo-Elements 4
        // §3.5).
        if let Some(&el) = origins.first() {
            let colors = self
                .with_style(el, |rr| rr.selection_colors(&origins))
                .unwrap_or_default();
            // and so are its highlight's `text-shadow`s (§3.2), each element's list kept once.
            let shadows = self
                .with_style(el, |rr| rr.selection_shadows(&origins))
                .unwrap_or_default();
            let mut kept: Vec<u32> = vec![0; origins.len()];
            for (n, i) in styled {
                let Some(range) = sel.ranges.get_mut(&n) else {
                    continue;
                };
                if let Some(&Some(c)) = colors.get(i) {
                    range.highlight = Some(c);
                }
                if let Some(list) = shadows.get(i).filter(|l| !l.is_empty()) {
                    if kept[i] == 0 {
                        sel.shadows.push(list.clone());
                        kept[i] = sel.shadows.len() as u32;
                    }
                    range.shadows = kept[i];
                }
            }
            // A replaced element is covered by its highlight's background, drawn translucent if
            // it is opaque (so the content shows through), as the default one is.
            for (n, i) in replaced {
                let background = match colors.get(i) {
                    Some(&Some((background, _))) => background,
                    _ => SELECTION_BG,
                };
                let overlay = if background.a == 0xff {
                    blend_with_white(background.r, background.g, background.b)
                } else {
                    background
                };
                if !overlay.is_transparent() {
                    sel.replaced.insert(n, overlay);
                }
            }
        }
        Some(sel)
    }

    // ----- text -------------------------------------------------------------------------------------------

    /// The selection's text as `innerText` renders it (for copying).
    pub(crate) fn selection_text(&self, doc: NodeId) -> Option<String> {
        let (r, _) = self.doc_selection(doc)?;
        if r.start == r.end {
            return None;
        }
        Some(self.range_inner_text(r))
    }

    // ----- edits -------------------------------------------------------------------------------------------

    /// An InputEvent at an editing host: `beforeinput` (cancelable; returns whether to go on) or
    /// `input`.
    #[allow(clippy::too_many_arguments)]
    fn host_input_event(
        &self,
        cx: &mut Cx,
        host: NodeId,
        type_: &str,
        input_type: &str,
        data: Option<&str>,
        target_ranges: Vec<snowlark_bindings::rt::Handle>,
        data_transfer: Option<snowlark_bindings::rt::Handle>,
    ) -> bool {
        let u = UiFields {
            view: self.window_of_document(self.dom().node_document(host)),
            data: data.map(str::to_owned),
            input_type: input_type.to_owned(),
            target_ranges,
            data_transfer,
            ..Default::default()
        };
        let e = self.new_event(
            InterfaceId::InputEvent,
            type_,
            true,
            type_ == "beforeinput",
            true,
            EventExt::Ui(Box::new(u)),
        );
        self.ev_mut(e, |ev| ev.is_trusted = true);
        self.dispatch(cx, e, node_handle(host), false)
    }

    /// The first caret position in a node: its first text, else its start.
    fn first_position(&self, node: NodeId) -> Bp {
        let dom = self.dom();
        dom.descendants(node)
            .find(|&n| dom.is_text(n))
            .map_or((node, 0), |t| (t, 0))
    }

    /// Runs a user edit (Input Events Level 2): a cancelable `beforeinput` at the host whose
    /// `getTargetRanges()` are `ranges` (what the edit will change), then, unless it was canceled,
    /// the edit and `input`. Returns whether the edit ran.
    pub(crate) fn user_edit(
        &self,
        cx: &mut Cx,
        host: NodeId,
        input: EditInput<'_>,
        edit: impl FnOnce(&Engine, &mut Cx),
    ) -> bool {
        let ranges: Vec<_> = input
            .ranges
            .iter()
            .map(|&r| self.alloc(crate::engine::Object::StaticRange(r)))
            .collect();
        if !self.host_input_event(
            cx,
            host,
            "beforeinput",
            input.input_type,
            input.data,
            ranges,
            input.data_transfer,
        ) {
            return false;
        }
        let doc = self.dom().node_document(host);
        let before = crate::undo::Sel::Doc(self.doc_anchor_focus(doc));
        let recording = self.dom().is_alive(host) && self.undo_begin(host);
        edit(self, cx);
        self.after_mutation(cx);
        if recording {
            let records = self.undo_end();
            if !records.is_empty() {
                self.undo_push(crate::undo::Step {
                    target: host,
                    merge: crate::undo::merge_kind(input.input_type),
                    content: crate::undo::Content::Dom(records),
                    before,
                    after: crate::undo::Sel::Doc(self.doc_anchor_focus(doc)),
                });
            }
        }
        if self.dom().is_alive(host) {
            self.host_input_event(
                cx,
                host,
                "input",
                input.input_type,
                input.data,
                Vec::new(),
                input.data_transfer,
            );
        }
        true
    }

    /// The selection of `doc` as one range, a caret at the host's first position when it has none.
    fn edit_selection(&self, doc: NodeId, host: NodeId) -> BoundaryPair {
        if let Some((r, _)) = self.doc_selection(doc) {
            return r;
        }
        let p = self.first_position(host);
        self.set_doc_selection(doc, Some((p, p)));
        BoundaryPair { start: p, end: p }
    }

    /// Text inserted by the user into the active editing host (typing, an IME commit): the HTML
    /// Editing APIs' `insertText` at the selection.
    pub(crate) fn editing_insert_text(&self, cx: &mut Cx, host: NodeId, text: &str) {
        let doc = self.dom().node_document(host);
        let r = self.edit_selection(doc, host);
        self.user_edit(
            cx,
            host,
            EditInput::new("insertText", Some(text), r),
            |e, cx| {
                e.ec_insert_text(cx, doc, text);
            },
        );
    }

    /// A character typed into the active editing host.
    pub(crate) fn editing_char(&self, cx: &mut Cx, host: NodeId, c: char) {
        let mut buf = [0u8; 4];
        self.editing_insert_text(cx, host, c.encode_utf8(&mut buf));
    }

    /// Backspace (`forward` false) or Delete, by character or (`word`) by word: a non-collapsed
    /// selection goes with the `delete`/`forwardDelete` command; at a caret, a word is selected
    /// first, and a character is the command's own step (a surrogate pair selected whole).
    fn key_delete(&self, cx: &mut Cx, host: NodeId, forward: bool, word: bool) {
        let doc = self.dom().node_document(host);
        let r = self.edit_selection(doc, host);
        let input_type = match (forward, word) {
            (false, false) => "deleteContentBackward",
            (true, false) => "deleteContentForward",
            (false, true) => "deleteWordBackward",
            (true, true) => "deleteWordForward",
        };
        // What goes: the selection, else the step from the caret over rendered content.
        let (target, select) = if r.start != r.end {
            (Some(r), None)
        } else {
            let caret = r.start;
            let mut to = self.step_position(doc, caret, forward, word);
            // (a word deleted forward takes the collapsed, invisible white space after it)
            if forward && word {
                to = to.map(|t| self.collapsed_space_after(t));
            }
            let pair = to.map(|to| {
                if forward {
                    BoundaryPair {
                        start: caret,
                        end: to,
                    }
                } else {
                    BoundaryPair {
                        start: to,
                        end: caret,
                    }
                }
            });
            // A word, or a surrogate pair in one text node, is selected for the command.
            let select = pair.filter(|p| {
                word || (p.start.0 == p.end.0
                    && self.dom().is_text(p.start.0)
                    && p.end.1 - p.start.1 > 1
                    && self.is_surrogate_pair(p.start.0, p.start.1))
            });
            (pair, select)
        };
        self.user_edit(
            cx,
            host,
            EditInput {
                input_type,
                data: None,
                ranges: target.into_iter().collect(),
                data_transfer: None,
            },
            |e, cx| {
                if let Some(p) = select {
                    let (a, f) = if forward {
                        (p.start, p.end)
                    } else {
                        (p.end, p.start)
                    };
                    e.set_doc_selection(doc, Some((a, f)));
                }
                if forward {
                    e.ec_forward_delete(cx, doc);
                } else {
                    e.ec_delete(cx, doc);
                }
            },
        );
    }

    /// Ctrl+Z (`redo` false), Ctrl+Y or Ctrl+Shift+Z in an editing host: `beforeinput`
    /// (`historyUndo`/`historyRedo`, cancelable), the newest step of the host undone or redone,
    /// `input`.
    fn host_history(&self, cx: &mut Cx, host: NodeId, redo: bool) {
        if !self.can_undo(host, redo) {
            return;
        }
        let input_type = if redo { "historyRedo" } else { "historyUndo" };
        if !self.host_input_event(cx, host, "beforeinput", input_type, None, Vec::new(), None) {
            return;
        }
        if !self.undo_apply(cx, host, redo) {
            return;
        }
        self.after_mutation(cx);
        if self.dom().is_alive(host) {
            self.host_input_event(cx, host, "input", input_type, None, Vec::new(), None);
        }
    }

    /// Whether the UTF-16 units at `at` and `at + 1` of a text node are a surrogate pair.
    fn is_surrogate_pair(&self, text: NodeId, at: u32) -> bool {
        let dom = self.dom();
        let s = dom.character_data(text).unwrap_or("");
        let mut units = s.encode_utf16().skip(at as usize);
        matches!(
            (units.next(), units.next()),
            (Some(0xD800..=0xDBFF), Some(0xDC00..=0xDFFF))
        )
    }

    /// Enter (`insertParagraph`) and Shift+Enter or Enter in a plaintext-only host
    /// (`insertLineBreak`), through the HTML Editing APIs' commands.
    fn key_enter(&self, cx: &mut Cx, host: NodeId, line_break: bool) {
        let doc = self.dom().node_document(host);
        let r = self.edit_selection(doc, host);
        let input_type = if line_break {
            "insertLineBreak"
        } else {
            "insertParagraph"
        };
        self.user_edit(cx, host, EditInput::new(input_type, None, r), |e, cx| {
            if line_break {
                e.ec_insert_line_break(cx, doc);
            } else {
                e.ec_insert_paragraph(cx, doc);
            }
        });
    }

    /// The editing keys of an editing host. Returns whether the key was one.
    pub(crate) fn editing_key(
        &self,
        cx: &mut Cx,
        host: NodeId,
        input: &KeyInput,
        out: &mut KeyOutcome,
    ) -> bool {
        let doc = self.dom().node_document(host);
        let ctrl = input.mods.ctrl;
        let shift = input.mods.shift;
        let Some((anchor, focus)) = self.doc_anchor_focus(doc).or_else(|| {
            let p = self.first_position(host);
            self.set_doc_selection(doc, Some((p, p)));
            Some((p, p))
        }) else {
            return false;
        };
        let collapsed = anchor == focus;
        let mv = |e: &Engine, to: Option<Bp>| {
            if let Some(to) = to {
                let anchor = if shift { anchor } else { to };
                e.set_doc_selection(doc, Some((anchor, to)));
            }
        };
        let (start, end) = {
            let dom = self.dom();
            if bp_position(&dom, anchor, focus) == std::cmp::Ordering::Greater {
                (focus, anchor)
            } else {
                (anchor, focus)
            }
        };
        match input.vk {
            0x08 => self.key_delete(cx, host, false, ctrl),
            0x2E if shift && !collapsed => self.cut_selection(cx, host, out),
            0x2E => self.key_delete(cx, host, true, ctrl),
            0x0D => self.key_enter(cx, host, shift || self.plaintext_only(host)),
            0x25 | 0x27 => {
                let forward = input.vk == 0x27;
                if !collapsed && !shift {
                    let to = if forward { end } else { start };
                    self.set_doc_selection(doc, Some((to, to)));
                } else {
                    mv(self, self.step_position(doc, focus, forward, ctrl));
                }
            }
            0x26 | 0x28 => mv(self, self.line_step(doc, focus, input.vk == 0x28)),
            0x24 => mv(
                self,
                if ctrl {
                    Some(self.first_position(host))
                } else {
                    self.line_boundary(doc, focus, false)
                },
            ),
            0x23 => mv(
                self,
                if ctrl {
                    Some(self.last_position(host))
                } else {
                    self.line_boundary(doc, focus, true)
                },
            ),
            k if ctrl && k == b'A' as u16 => {
                let n = self.dom().child_count(host) as u32;
                self.set_doc_selection(doc, Some(((host, 0), (host, n))));
            }
            k if ctrl && (k == b'C' as u16 || k == 0x2D) => self.copy_selection(cx, doc, out),
            k if ctrl && k == b'X' as u16 => self.cut_selection(cx, host, out),
            k if (ctrl && k == b'V' as u16) || (shift && k == 0x2D) => {
                self.paste_into_host(
                    cx,
                    host,
                    input.paste.as_deref(),
                    input.paste_html.as_deref(),
                );
            }
            k if ctrl && k == b'Z' as u16 => self.host_history(cx, host, shift),
            k if ctrl && k == b'Y' as u16 => self.host_history(cx, host, true),
            k if ctrl && (k == b'B' as u16 || k == b'I' as u16 || k == b'U' as u16) => {
                let cmd = match k {
                    0x42 => "bold",
                    0x49 => "italic",
                    _ => "underline",
                };
                self.exec_editing_command(cx, doc, cmd, "");
            }
            _ => return false,
        }
        true
    }

    fn last_position(&self, node: NodeId) -> Bp {
        let dom = self.dom();
        let last = dom.descendants(node).filter(|&n| dom.is_text(n)).last();
        match last {
            Some(t) => (t, dom.length(t) as u32),
            None => (node, dom.child_count(node) as u32),
        }
    }

    /// Keys outside text controls and editing hosts: Ctrl+A selects the body's contents, Ctrl+C
    /// copies the selection. Returns whether the key was one.
    pub(crate) fn document_selection_key(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        input: &KeyInput,
        out: &mut KeyOutcome,
    ) -> bool {
        if !input.mods.ctrl {
            return false;
        }
        match input.vk {
            k if k == b'A' as u16 => {
                let root = self
                    .dom()
                    .body(doc)
                    .or_else(|| self.dom().document_element(doc));
                if let Some(b) = root {
                    let n = self.dom().child_count(b) as u32;
                    self.set_doc_selection(doc, Some(((b, 0), (b, n))));
                }
                true
            }
            k if k == b'C' as u16 || k == 0x2D => {
                self.copy_selection(cx, doc, out);
                out.copy.is_some()
            }
            _ => false,
        }
    }

    // ----- execCommand ---------------------------------------------------------------------------------------

    /// The editing host the selection of `doc` is in.
    fn selection_host(&self, doc: NodeId) -> Option<NodeId> {
        let (r, _) = self.doc_selection(doc)?;
        let a = self.editing_host(r.start.0)?;
        (self.editing_host(r.end.0) == Some(a)).then_some(a)
    }

    /// `execCommand(cmd, false, value)`: `undo` and `redo` here, the others by the HTML Editing
    /// APIs (`exec_command`), with the `input` event they call for. Returns whether it ran.
    pub(crate) fn exec_editing_command(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        cmd: &str,
        value: &str,
    ) -> bool {
        let cmd = cmd.to_ascii_lowercase();
        // `undo` and `redo`: the newest step of the selection's editing host.
        if cmd == "undo" || cmd == "redo" {
            let redo = cmd == "redo";
            let Some(host) = self.selection_host(doc) else {
                return false;
            };
            if !self.undo_apply(cx, host, redo) {
                return false;
            }
            self.after_mutation(cx);
            let t = if redo { "historyRedo" } else { "historyUndo" };
            self.host_input_event(cx, host, "input", t, None, Vec::new(), None);
            return true;
        }
        // (an edit through a command is a step of the undo stack, as the user's are)
        let host = self.selection_host(doc);
        let before = crate::undo::Sel::Doc(self.doc_anchor_focus(doc));
        let recording = host.is_some_and(|h| self.undo_begin(h));
        let out = self.ec_exec(cx, doc, &cmd, value);
        if recording {
            let records = self.undo_end();
            if let (Some(h), false) = (host, records.is_empty()) {
                self.undo_push(crate::undo::Step {
                    target: h,
                    merge: None,
                    content: crate::undo::Content::Dom(records),
                    before,
                    after: crate::undo::Sel::Doc(self.doc_anchor_focus(doc)),
                });
            }
        }
        if let Some((host, input_type)) = out.input {
            let data = out.data.as_deref();
            self.host_input_event(cx, host, "input", input_type, data, Vec::new(), None);
        }
        out.ran
    }

    /// `queryCommandEnabled` / `queryCommandSupported` / `queryCommandState` /
    /// `queryCommandIndeterm`.
    pub(crate) fn query_editing_command(&self, doc: NodeId, cmd: &str, what: &str) -> bool {
        let cmd = cmd.to_ascii_lowercase();
        match what {
            "supported" => Self::ec_supported(&cmd),
            "enabled" => self.ec_enabled(doc, &cmd),
            "state" => self.ec_query_state(doc, &cmd),
            "indeterm" => self.ec_query_indeterm(doc, &cmd),
            _ => false,
        }
    }
}
