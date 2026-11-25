// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs §2 "Common definitions" and §6.1 "Block formatting command definitions": the
//! node predicates the editing algorithms share (editing hosts and editable nodes, block and inline
//! nodes, visible and invisible nodes, line breaks, allowed children).
//!
//! "Collapsed line break" and "extraneous line break" are defined by their effect on layout; they
//! are decided here from the tree and the computed `display` and `white-space` of the line's nodes
//! (what follows and precedes the `br` on its line), not by laying the block out twice.

use snowlark_dom::{ns, NodeId};

use crate::engine::cssom::Pseudo;
use crate::engine::Engine;

/// "A prohibited paragraph child name".
pub(super) const PROHIBITED_PARAGRAPH_CHILD: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "listing",
    "menu",
    "nav",
    "ol",
    "p",
    "plaintext",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
    "xmp",
];

/// "A name of an element with inline contents".
const INLINE_CONTENTS: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "dfn", "em", "h1", "h2", "h3", "h4", "h5",
    "h6", "i", "kbd", "mark", "p", "pre", "q", "rp", "rt", "ruby", "s", "samp", "small", "span",
    "strong", "sub", "sup", "u", "var", "acronym", "listing", "strike", "xmp", "big", "blink",
    "font", "marquee", "nobr", "tt",
];

const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];

/// A child or parent of "allowed child": a node or an element's local name.
#[derive(Clone, Copy, Debug)]
pub(super) enum Item<'a> {
    Node(NodeId),
    Name(&'a str),
}

/// What a line holds next to a `br`, looking one way from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineHit {
    /// The line's block starts or ends (or a block node does).
    Edge,
    /// Another `br`.
    Br,
    /// Text or a replaced or atomic inline.
    Content,
}

/// The default `display` of an element that has no computed style (not in a document).
fn default_display(name: &str) -> &'static str {
    match name {
        "li" => "list-item",
        "table" => "table",
        "td" | "th" => "table-cell",
        "tr" => "table-row",
        "tbody" | "thead" | "tfoot" => "table-row-group",
        "caption" => "table-caption",
        "col" => "table-column",
        "colgroup" => "table-column-group",
        "head" | "script" | "style" | "template" | "title" | "meta" | "link" => "none",
        "address" | "article" | "aside" | "blockquote" | "body" | "center" | "dd" | "details"
        | "dialog" | "dir" | "div" | "dl" | "dt" | "fieldset" | "figcaption" | "figure"
        | "footer" | "form" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "header" | "hgroup"
        | "hr" | "html" | "legend" | "listing" | "main" | "menu" | "nav" | "ol" | "p"
        | "plaintext" | "pre" | "search" | "section" | "summary" | "ul" | "xmp" => "block",
        _ => "inline",
    }
}

impl Engine {
    // ----- element names ----------------------------------------------------------------------------------

    /// Whether `n` is an HTML element whose local name is one of `names`.
    pub(super) fn ec_is(&self, n: NodeId, names: &[&str]) -> bool {
        let dom = self.dom();
        dom.is_alive(n)
            && dom.is_element(n)
            && dom.namespace(n) == &ns!(html)
            && names.contains(&&**dom.local_name(n))
    }

    /// [`Engine::ec_is`] of a node that may be null.
    pub(super) fn ec_is_o(&self, n: Option<NodeId>, names: &[&str]) -> bool {
        n.is_some_and(|n| self.ec_is(n, names))
    }

    /// "An HTML element".
    pub(super) fn ec_html(&self, n: NodeId) -> bool {
        let dom = self.dom();
        dom.is_alive(n) && dom.is_element(n) && dom.namespace(n) == &ns!(html)
    }

    /// The local name of an HTML element.
    pub(super) fn ec_name(&self, n: NodeId) -> Option<String> {
        self.ec_html(n)
            .then(|| self.dom().local_name(n).to_string())
    }

    /// "A prohibited paragraph child".
    pub(super) fn ec_prohibited_paragraph_child(&self, n: Option<NodeId>) -> bool {
        self.ec_is_o(n, PROHIBITED_PARAGRAPH_CHILD)
    }

    /// "A single-line container" (`li`, `dt`, `dd` and the non-list ones).
    pub(super) fn ec_single_line_container(&self, n: NodeId) -> bool {
        self.ec_is(
            n,
            &[
                "address", "div", "h1", "h2", "h3", "h4", "h5", "h6", "listing", "p", "pre", "xmp",
                "li", "dt", "dd",
            ],
        )
    }

    /// "An indentation element": a `blockquote`, or a `div` whose style attribute sets `margin` or
    /// one of its longhands.
    pub(super) fn ec_indentation_element(&self, n: NodeId) -> bool {
        if self.ec_is(n, &["blockquote"]) {
            return true;
        }
        if !self.ec_is(n, &["div"]) {
            return false;
        }
        let dom = self.dom();
        dom.get_attribute(n, "style").is_some_and(|s| {
            s.split(';').any(|d| {
                d.split(':')
                    .next()
                    .map(|p| p.trim().to_ascii_lowercase())
                    .is_some_and(|p| p.starts_with("margin"))
            })
        })
    }

    // ----- editing hosts ----------------------------------------------------------------------------------

    /// "An editing host": an HTML element whose `contenteditable` is in the true (or
    /// plaintext-only) state, or the HTML element child of a document in design mode.
    pub(super) fn ec_editing_host(&self, n: NodeId) -> bool {
        let parent = {
            let dom = self.dom();
            if !dom.is_alive(n) || !dom.is_element(n) || dom.namespace(n) != &ns!(html) {
                return false;
            }
            if dom.get_attribute(n, "contenteditable").is_some_and(|v| {
                v.is_empty()
                    || v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("plaintext-only")
            }) {
                return true;
            }
            match dom.parent(n) {
                Some(p) if dom.is_document(p) => p,
                _ => return false,
            }
        };
        self.doc_state(parent, |s| s.design_mode)
    }

    /// "Editable": not an editing host, not `contenteditable=false`, a parent that is an editing
    /// host or editable, and an HTML element, an `svg` or `math` element, or a non-element child of
    /// an HTML element.
    pub(super) fn ec_editable(&self, n: NodeId) -> bool {
        // (a `select`'s options are its own: the select is edited as a whole)
        if self.dom().ancestors(n).any(|a| {
            self.dom()
                .is_html_element_named(a, &snowlark_dom::local_name!("select"))
        }) {
            return false;
        }
        let mut cur = n;
        loop {
            if self.ec_editing_host(cur) {
                return false;
            }
            let parent = {
                let dom = self.dom();
                if !dom.is_alive(cur) {
                    return false;
                }
                let parent = dom.parent(cur);
                let ok = if dom.is_element(cur) {
                    let ns = dom.namespace(cur);
                    let name = &**dom.local_name(cur);
                    let html = ns == &ns!(html);
                    let ce_false = html
                        && dom
                            .get_attribute(cur, "contenteditable")
                            .is_some_and(|v| v.eq_ignore_ascii_case("false"));
                    !ce_false
                        && (html
                            || (ns == &ns!(svg) && name == "svg")
                            || (ns == &ns!(mathml) && name == "math"))
                } else {
                    parent.is_some_and(|p| dom.is_element(p) && dom.namespace(p) == &ns!(html))
                };
                if !ok {
                    return false;
                }
                match parent {
                    Some(p) => p,
                    None => return false,
                }
            };
            if self.ec_editing_host(parent) {
                return true;
            }
            cur = parent;
        }
    }

    /// [`Engine::ec_editable`] of a node that may be null.
    pub(super) fn ec_editable_o(&self, n: Option<NodeId>) -> bool {
        n.is_some_and(|n| self.ec_editable(n))
    }

    /// Editable or an editing host.
    pub(super) fn ec_editable_or_host(&self, n: NodeId) -> bool {
        self.ec_editable(n) || self.ec_editing_host(n)
    }

    /// "The editing host of" a node: itself if it is one, else the nearest editing host ancestor of
    /// an editable node.
    pub(super) fn ec_host_of(&self, n: NodeId) -> Option<NodeId> {
        if self.ec_editing_host(n) {
            return Some(n);
        }
        if !self.ec_editable(n) {
            return None;
        }
        let mut a = self.dom().parent(n)?;
        while !self.ec_editing_host(a) {
            a = self.dom().parent(a)?;
        }
        Some(a)
    }

    /// "In the same editing host".
    pub(super) fn ec_same_host(&self, a: NodeId, b: Option<NodeId>) -> bool {
        let Some(b) = b else {
            return false;
        };
        self.ec_host_of(a)
            .is_some_and(|h| self.ec_host_of(b) == Some(h))
    }

    // ----- style ------------------------------------------------------------------------------------------

    /// The resolved `display` of an element (its default by tag when it has no computed style).
    pub(super) fn ec_display(&self, el: NodeId) -> String {
        let connected = {
            let dom = self.dom();
            if !dom.is_alive(el) || !dom.is_element(el) {
                return String::new();
            }
            dom.is_connected(el)
        };
        if connected {
            let v = self.computed_value(el, Pseudo::None, "display");
            if !v.is_empty() {
                return v;
            }
        }
        let dom = self.dom();
        if dom.namespace(el) == &ns!(html) {
            default_display(dom.local_name(el)).to_owned()
        } else {
            "inline".to_owned()
        }
    }

    /// The resolved `white-space` of an element.
    pub(super) fn ec_white_space(&self, el: NodeId) -> String {
        let connected = {
            let dom = self.dom();
            dom.is_alive(el) && dom.is_element(el) && dom.is_connected(el)
        };
        let v = if connected {
            self.computed_value(el, Pseudo::None, "white-space")
        } else {
            String::new()
        };
        if !v.is_empty() {
            return v;
        }
        if self.ec_is(el, &["pre", "listing", "xmp", "plaintext"]) {
            "pre".to_owned()
        } else {
            "normal".to_owned()
        }
    }

    /// Whether an author (or user) declaration gives the element `el` its own value for
    /// `property`: a rule of a page's style sheet, its `style` attribute or a presentational hint,
    /// not the user agent's style sheet and not inheritance.
    pub(super) fn ec_author_declares(&self, el: NodeId, property: &str) -> bool {
        let connected = {
            let dom = self.dom();
            dom.is_alive(el) && dom.is_element(el) && dom.is_connected(el)
        };
        connected
            && self
                .with_style(el, |r| r.author_declares(el, property))
                .unwrap_or(false)
    }

    /// Whether line feeds in a Text node are line breaks: its parent's `white-space` preserves
    /// segment breaks (`pre`, `pre-wrap`, `pre-line`, `break-spaces`).
    pub(super) fn ec_preserves_line_feeds(&self, text: NodeId) -> bool {
        self.dom()
            .parent(text)
            .is_some_and(|p| self.ec_keeps_line_feeds(p))
    }

    /// Whether line feeds in the element `el`'s own text are line breaks (its `white-space`
    /// preserves segment breaks).
    pub(super) fn ec_keeps_line_feeds(&self, el: NodeId) -> bool {
        let ws = self.ec_white_space(el);
        !(ws == "normal" || ws == "nowrap" || ws.starts_with("collapse"))
    }

    /// Whether a Text node's white space is preserved: its parent's `white-space` is `pre` or
    /// `pre-wrap`.
    pub(super) fn ec_pre(&self, text: NodeId) -> bool {
        let Some(p) = self.dom().parent(text) else {
            return false;
        };
        matches!(self.ec_white_space(p).as_str(), "pre" | "pre-wrap")
    }

    // ----- block and inline nodes -------------------------------------------------------------------------

    /// "A block node": an element whose `display` is not `inline`, `inline-block`, `inline-table`
    /// or `none`, a document or a document fragment.
    pub(super) fn ec_block(&self, n: NodeId) -> bool {
        let el = {
            let dom = self.dom();
            if !dom.is_alive(n) {
                return false;
            }
            if dom.is_document(n) || dom.is_document_fragment(n) {
                return true;
            }
            dom.is_element(n)
        };
        el && !matches!(
            self.ec_display(n).as_str(),
            "inline" | "inline-block" | "inline-table" | "none"
        )
    }

    /// "An inline node": a node that is not a block node.
    pub(super) fn ec_inline(&self, n: NodeId) -> bool {
        !self.ec_block(n)
    }

    pub(super) fn ec_inline_o(&self, n: Option<NodeId>) -> bool {
        n.is_some_and(|n| self.ec_inline(n))
    }

    /// "The block node of" a node: it or its nearest ancestor that is not an inline node.
    pub(super) fn ec_block_node_of(&self, n: NodeId) -> Option<NodeId> {
        let mut n = n;
        while self.ec_inline(n) {
            n = self.dom().parent(n)?;
        }
        Some(n)
    }

    /// Whether an element or one of its ancestor elements has `display: none`.
    fn ec_display_none_inclusive(&self, n: NodeId) -> bool {
        let chain: Vec<NodeId> = {
            let dom = self.dom();
            dom.inclusive_ancestors(n)
                .filter(|&a| dom.is_element(a))
                .collect()
        };
        chain.into_iter().any(|a| self.ec_display(a) == "none")
    }

    // ----- white space and visibility ---------------------------------------------------------------------

    /// "A whitespace node".
    pub(super) fn ec_whitespace_node(&self, n: NodeId) -> bool {
        let (parent, any_ws, no_newline) = {
            let dom = self.dom();
            if !dom.is_alive(n) || !dom.is_text(n) {
                return false;
            }
            let data = dom.character_data(n).unwrap_or("");
            if data.is_empty() {
                return true;
            }
            let any_ws = data.chars().all(|c| matches!(c, '\t' | '\n' | '\r' | ' '));
            let no_newline = data.chars().all(|c| matches!(c, '\t' | '\r' | ' '));
            match dom.parent(n) {
                Some(p) if dom.is_element(p) => (p, any_ws, no_newline),
                _ => return false,
            }
        };
        if !any_ws {
            return false;
        }
        match self.ec_white_space(parent).as_str() {
            "normal" | "nowrap" => true,
            "pre-line" => no_newline,
            _ => false,
        }
    }

    /// "A collapsed whitespace node".
    pub(super) fn ec_collapsed_whitespace_node(&self, n: NodeId) -> bool {
        if !self.ec_whitespace_node(n) {
            return false;
        }
        if self.dom().length(n) == 0 {
            return true;
        }
        let Some(mut ancestor) = self.dom().parent(n) else {
            return true;
        };
        let ancestors: Vec<NodeId> = {
            let dom = self.dom();
            dom.ancestors(n).filter(|&a| dom.is_element(a)).collect()
        };
        if ancestors.into_iter().any(|a| self.ec_display(a) == "none") {
            return true;
        }
        while !self.ec_block(ancestor) {
            match self.dom().parent(ancestor) {
                Some(p) => ancestor = p,
                None => break,
            }
        }
        let mut reference = n;
        while reference != ancestor {
            let Some(r) = self.dom().previous_in_tree_order(reference, None) else {
                break;
            };
            reference = r;
            if self.ec_block(reference) || self.ec_is(reference, &["br"]) {
                return true;
            }
            if (self.dom().is_text(reference) && !self.ec_whitespace_node(reference))
                || self.ec_is(reference, &["img"])
            {
                break;
            }
        }
        let stop = self.dom().next_skipping_children(ancestor, None);
        let mut reference = Some(n);
        while reference != stop {
            reference = reference.and_then(|r| self.dom().next_in_tree_order(r, None));
            let Some(r) = reference else {
                break;
            };
            if self.ec_block(r) || self.ec_is(r, &["br"]) {
                return true;
            }
            if (self.dom().is_text(r) && !self.ec_whitespace_node(r)) || self.ec_is(r, &["img"]) {
                break;
            }
        }
        false
    }

    /// "Visible": a block node, a Text node that is not a collapsed whitespace node, an `img`, a
    /// `br` that is not an extraneous line break, or a node with a visible descendant; never inside
    /// `display: none`.
    pub(super) fn ec_visible(&self, n: NodeId) -> bool {
        if !self.dom().is_alive(n) || self.ec_display_none_inclusive(n) {
            return false;
        }
        self.ec_visible_below(n)
    }

    /// [`Engine::ec_visible`] of a node whose ancestors are known not to be `display: none`.
    fn ec_visible_below(&self, n: NodeId) -> bool {
        let (is_el, is_text) = {
            let dom = self.dom();
            (dom.is_element(n), dom.is_text(n))
        };
        if is_el && self.ec_display(n) == "none" {
            return false;
        }
        if self.ec_block(n)
            || (is_text && !self.ec_collapsed_whitespace_node(n))
            || self.ec_is(n, &["img"])
            || (self.ec_is(n, &["br"]) && !self.ec_extraneous_line_break(n))
        {
            return true;
        }
        let children: Vec<NodeId> = self.dom().children(n).collect();
        children.into_iter().any(|c| self.ec_visible_below(c))
    }

    /// "Invisible": a node that is not visible.
    pub(super) fn ec_invisible(&self, n: NodeId) -> bool {
        !self.ec_visible(n)
    }

    pub(super) fn ec_invisible_o(&self, n: Option<NodeId>) -> bool {
        n.is_some_and(|n| self.ec_invisible(n))
    }

    // ----- line breaks ------------------------------------------------------------------------------------

    /// What the line holds next to `from` (forward: after it), within its block.
    fn ec_line_scan(&self, from: NodeId, forward: bool) -> LineHit {
        let mut cur = from;
        loop {
            let sib = {
                let dom = self.dom();
                if forward {
                    dom.next_sibling(cur)
                } else {
                    dom.previous_sibling(cur)
                }
            };
            match sib {
                None => {
                    let Some(p) = self.dom().parent(cur) else {
                        return LineHit::Edge;
                    };
                    if !self.dom().is_element(p) || self.ec_display(p) != "inline" {
                        return LineHit::Edge;
                    }
                    cur = p;
                }
                Some(s) => {
                    if let Some(hit) = self.ec_line_subtree(s, forward) {
                        return hit;
                    }
                    cur = s;
                }
            }
        }
    }

    /// The first thing in `n`'s subtree that a line holds, looking from one side.
    fn ec_line_subtree(&self, n: NodeId, forward: bool) -> Option<LineHit> {
        let (is_text, is_el) = {
            let dom = self.dom();
            (dom.is_text(n), dom.is_element(n))
        };
        if is_text {
            let (empty, ws, no_newline) = {
                let dom = self.dom();
                let d = dom.character_data(n).unwrap_or("");
                (
                    d.is_empty(),
                    d.chars().all(|c| matches!(c, '\t' | '\n' | '\r' | ' ')),
                    d.chars().all(|c| matches!(c, '\t' | '\r' | ' ')),
                )
            };
            if empty {
                return None;
            }
            let p = self.dom().parent(n)?;
            let collapses = match self.ec_white_space(p).as_str() {
                "normal" | "nowrap" => ws,
                "pre-line" => no_newline,
                _ => false,
            };
            return (!collapses).then_some(LineHit::Content);
        }
        if !is_el {
            return None;
        }
        if self.ec_is(n, &["br"]) {
            return Some(LineHit::Br);
        }
        match self.ec_display(n).as_str() {
            "none" => None,
            "inline" => {
                if self.ec_is(
                    n,
                    &[
                        "img", "input", "button", "select", "textarea", "iframe", "video", "audio",
                        "canvas", "object", "embed", "meter", "progress",
                    ],
                ) || {
                    let dom = self.dom();
                    dom.namespace(n) == &ns!(svg) && &**dom.local_name(n) == "svg"
                } {
                    return Some(LineHit::Content);
                }
                let children: Vec<NodeId> = {
                    let dom = self.dom();
                    if forward {
                        dom.children(n).collect()
                    } else {
                        dom.children_rev(n).collect()
                    }
                };
                children
                    .into_iter()
                    .find_map(|c| self.ec_line_subtree(c, forward))
            }
            d if d.starts_with("inline") => Some(LineHit::Content),
            _ => Some(LineHit::Edge),
        }
    }

    /// "A collapsed line break": a `br` that begins a line box with nothing else in it (nothing
    /// follows it before its block's end or a block).
    pub(super) fn ec_collapsed_line_break(&self, n: NodeId) -> bool {
        self.ec_is(n, &["br"]) && self.ec_line_scan(n, true) == LineHit::Edge
    }

    /// "An extraneous line break": a `br` whose removal would not change layout (one that ends a
    /// line that has content, with nothing after it in its block), except the only child of an
    /// `li`.
    pub(super) fn ec_extraneous_line_break(&self, n: NodeId) -> bool {
        if !self.ec_is(n, &["br"]) {
            return false;
        }
        let parent = self.dom().parent(n);
        if self.ec_is_o(parent, &["li"]) && parent.is_some_and(|p| self.dom().child_count(p) == 1) {
            return false;
        }
        self.ec_line_scan(n, true) == LineHit::Edge
            && self.ec_line_scan(n, false) == LineHit::Content
    }

    /// "A collapsed block prop".
    pub(super) fn ec_collapsed_block_prop(&self, n: NodeId) -> bool {
        if self.ec_collapsed_line_break(n) && !self.ec_extraneous_line_break(n) {
            return true;
        }
        if !self.ec_inline(n) || !self.dom().is_element(n) {
            return false;
        }
        let children: Vec<NodeId> = self.dom().children(n).collect();
        let mut has = false;
        for c in children {
            let prop = self.ec_collapsed_block_prop(c);
            if !self.ec_invisible(c) && !prop {
                return false;
            }
            has |= prop;
        }
        has
    }

    pub(super) fn ec_collapsed_block_prop_o(&self, n: Option<NodeId>) -> bool {
        n.is_some_and(|n| self.ec_collapsed_block_prop(n))
    }

    // ----- allowed children -------------------------------------------------------------------------------

    /// "An allowed child" of `parent`.
    pub(super) fn ec_allowed_child(&self, child: Item<'_>, parent: Item<'_>) -> bool {
        const TABLEISH: &[&str] = &["colgroup", "table", "tbody", "tfoot", "thead", "tr"];
        const RAW: &[&str] = &["script", "style", "plaintext", "xmp"];
        let parent_named = |names: &[&str]| match parent {
            Item::Name(p) => names.contains(&p),
            Item::Node(p) => self.ec_is(p, names),
        };
        let (child_text, child_data_ws, child_bad) = match child {
            Item::Node(c) => {
                let dom = self.dom();
                (
                    dom.is_text(c),
                    dom.character_data(c)
                        .unwrap_or("")
                        .chars()
                        .all(|ch| matches!(ch, ' ' | '\t' | '\n' | '\x0c' | '\r')),
                    dom.is_document(c)
                        || dom.is_document_fragment(c)
                        || dom.node_type(c) == snowlark_dom::NodeType::DocumentType,
                )
            }
            Item::Name(_) => (false, false, false),
        };
        if parent_named(TABLEISH) && child_text && !child_data_ws {
            return false;
        }
        if parent_named(RAW) && !child_text {
            return false;
        }
        if child_bad {
            return false;
        }
        let child_name: String = match child {
            Item::Name(c) => c.to_owned(),
            Item::Node(c) => match self.ec_name(c) {
                Some(n) => n,
                None => return true,
            },
        };
        let child = child_name.as_str();
        let parent_name: String = match parent {
            Item::Name(p) => p.to_owned(),
            Item::Node(p) => {
                if self.ec_html(p) {
                    let chain: Vec<NodeId> = self.dom().inclusive_ancestors(p).collect();
                    for a in chain {
                        if child == "a" && self.ec_is(a, &["a"]) {
                            return false;
                        }
                        if PROHIBITED_PARAGRAPH_CHILD.contains(&child)
                            && self.ec_is(a, INLINE_CONTENTS)
                        {
                            return false;
                        }
                        if HEADINGS.contains(&child) && self.ec_is(a, HEADINGS) {
                            return false;
                        }
                    }
                    self.dom().local_name(p).to_string()
                } else {
                    let dom = self.dom();
                    return dom.is_element(p) || dom.is_document_fragment(p);
                }
            }
        };
        let parent = parent_name.as_str();
        match parent {
            "colgroup" => return child == "col",
            "table" => {
                return [
                    "caption", "col", "colgroup", "tbody", "td", "tfoot", "th", "thead", "tr",
                ]
                .contains(&child)
            }
            "tbody" | "thead" | "tfoot" => return ["td", "th", "tr"].contains(&child),
            "tr" => return ["td", "th"].contains(&child),
            "dl" => return ["dt", "dd"].contains(&child),
            "dir" | "ol" | "ul" => return ["dir", "li", "ol", "ul"].contains(&child),
            "hgroup" => return HEADINGS.contains(&child),
            _ => {}
        }
        if [
            "body", "caption", "col", "colgroup", "frame", "frameset", "head", "html", "tbody",
            "td", "tfoot", "th", "thead", "tr",
        ]
        .contains(&child)
        {
            return false;
        }
        if ["dd", "dt"].contains(&child) && parent != "dl" {
            return false;
        }
        if child == "li" && parent != "ol" && parent != "ul" {
            return false;
        }
        let table: [(&[&str], &[&str]); 7] = [
            (&["a"], &["a"]),
            (&["dd", "dt"], &["dd", "dt"]),
            (HEADINGS, HEADINGS),
            (&["li"], &["li"]),
            (&["nobr"], &["nobr"]),
            (INLINE_CONTENTS, PROHIBITED_PARAGRAPH_CHILD),
            (
                &["td", "th"],
                &[
                    "caption", "col", "colgroup", "tbody", "td", "tfoot", "th", "thead", "tr",
                ],
            ),
        ];
        !table
            .iter()
            .any(|(ps, cs)| ps.contains(&parent) && cs.contains(&child))
    }

    /// Whether `node` is not an allowed child of any of its ancestors in the same editing host.
    pub(super) fn ec_disallowed_everywhere(&self, node: NodeId) -> bool {
        let ancestors: Vec<NodeId> = self.dom().ancestors(node).collect();
        ancestors.into_iter().all(|a| {
            !self.ec_same_host(node, Some(a))
                || !self.ec_allowed_child(Item::Node(node), Item::Node(a))
        })
    }
}
