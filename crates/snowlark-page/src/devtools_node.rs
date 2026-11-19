// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the developer tools do to a node of the element tree, through the same DOM algorithms the
//! page's own scripts reach: the text the "Copy" rows put on the clipboard (outerHTML, a selector, a
//! JS path, XPaths), deleting, duplicating and hiding an element, scrolling it into view, focusing
//! it and storing it in a console variable (`temp1`, `temp2`, ...). Every change goes through the
//! DOM's mutation methods and then [`Engine::after_mutation`], so MutationObservers, custom element
//! reactions and layout see it as they see a script's change.

use snowlark_bindings::rt::{self, Cx};
use snowlark_dom::{NodeData, NodeId, NodeOrString};

use snowlark_style::ElementState;

use crate::engine::cssom::serialize_identifier;
use crate::engine::{node_handle, Engine};

/// The pseudo-classes "Force state" offers, in the order of its menu (the kinds of
/// [`Engine::devtools_force_state`]).
pub const FORCE_STATES: [&str; 7] = [
    ":active",
    ":hover",
    ":focus",
    ":visited",
    ":focus-within",
    ":focus-visible",
    ":target",
];

/// The text a "Copy" row of the element tree's menu copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CopyKind {
    /// The node and its descendants as markup.
    OuterHtml,
    /// A CSS selector that matches the element alone within its tree (the document, or a shadow
    /// tree).
    Selector,
    /// A script expression that evaluates to the element, through `shadowRoot` of each shadow host
    /// above it.
    JsPath,
    /// An XPath from the nearest ancestor with a unique id.
    XPath,
    /// An XPath from the root of the node's tree.
    FullXPath,
}

/// `s` as a JS string literal.
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An XPath string literal for `s`, or `None` when it holds both kinds of quote.
fn xpath_literal(s: &str) -> Option<String> {
    if !s.contains('"') {
        Some(format!("\"{s}\""))
    } else if !s.contains('\'') {
        Some(format!("'{s}'"))
    } else {
        None
    }
}

/// The `style` attribute's declarations, split at top-level semicolons (not inside strings or
/// parentheses).
fn split_declarations(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0usize, None::<char>, 0usize);
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (c, quote) {
            ('\\', _) => escaped = true,
            (q, Some(open)) if q == open => quote = None,
            (_, Some(_)) => {}
            ('"' | '\'', None) => quote = Some(c),
            ('(', None) => depth += 1,
            (')', None) => depth = depth.saturating_sub(1),
            (';', None) if depth == 0 => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out.into_iter().filter(|d| !d.trim().is_empty()).collect()
}

/// Whether `decl` is the declaration "Hide element" adds.
fn is_hide_declaration(decl: &str) -> bool {
    let norm: String = decl
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    norm == "visibility:hidden!important"
}

/// `name="value"` as the attribute editor shows it.
fn attribute_text(name: &str, value: &str) -> String {
    if !value.contains('"') {
        format!("{name}=\"{value}\"")
    } else if !value.contains('\'') {
        format!("{name}='{value}'")
    } else {
        format!("{name}=\"{}\"", value.replace('"', "&quot;"))
    }
}

/// The declaration "Hide element" adds to the `style` attribute.
const HIDE_DECLARATION: &str = "visibility: hidden !important";

impl Engine {
    // ----- the text of the Copy rows --------------------------------------------------------------------

    /// The text of `what` for `node`, or `None` when the node has none (a selector for a text node,
    /// a JS path for a node outside the document).
    pub fn devtools_copy(&self, node: NodeId, what: CopyKind) -> Option<String> {
        if !self.dom().is_alive(node) {
            return None;
        }
        match what {
            CopyKind::OuterHtml => self.devtools_outer_html(node),
            CopyKind::Selector => self.devtools_selector(node),
            CopyKind::JsPath => self.devtools_js_path(node),
            CopyKind::XPath => self.devtools_xpath(node, false),
            CopyKind::FullXPath => self.devtools_xpath(node, true),
        }
    }

    /// The markup of a node: an element with its descendants, a text node's text as markup, a
    /// comment, a doctype.
    fn devtools_outer_html(&self, node: NodeId) -> Option<String> {
        let dom = self.dom();
        match dom.data(node) {
            NodeData::Element(_) => Some(dom.outer_html(node)),
            NodeData::Text(t) | NodeData::CDataSection(t) => {
                let mut out = String::new();
                for c in t.chars() {
                    match c {
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        '\u{a0}' => out.push_str("&nbsp;"),
                        c => out.push(c),
                    }
                }
                Some(out)
            }
            NodeData::Comment(c) => Some(format!("<!--{c}-->")),
            NodeData::DocumentType(_) => dom.doctype(node).map(|(name, public, system)| {
                match (public.is_empty(), system.is_empty()) {
                    (true, true) => format!("<!DOCTYPE {name}>"),
                    (false, true) => format!("<!DOCTYPE {name} PUBLIC \"{public}\">"),
                    (false, false) => {
                        format!("<!DOCTYPE {name} PUBLIC \"{public}\" \"{system}\">")
                    }
                    (true, false) => format!("<!DOCTYPE {name} SYSTEM \"{system}\">"),
                }
            }),
            _ => None,
        }
    }

    /// The tree `node` is in: its root (the document, a shadow root, or the top of a detached
    /// tree).
    fn devtools_root(&self, node: NodeId) -> NodeId {
        self.dom().root(node)
    }

    /// The elements of `root`'s tree that `selector` matches (none when it does not parse).
    fn devtools_query(&self, root: NodeId, selector: &str) -> Vec<NodeId> {
        match self.parse_selectors(selector) {
            Ok(list) => self.query_all(root, &list),
            Err(_) => Vec::new(),
        }
    }

    /// One step of a selector path: the element's type with, unless it is the only one of its type
    /// among its siblings and `plain` allows that, its position.
    fn selector_step(&self, el: NodeId, plain: bool) -> String {
        let dom = self.dom();
        let name = serialize_identifier(dom.local_name(el));
        let Some(parent) = dom.parent(el) else {
            return name;
        };
        let (mut position, mut same, mut seen) = (0usize, 0usize, false);
        for c in dom.children(parent).filter(|&c| dom.is_element(c)) {
            if !seen {
                position += 1;
            }
            if c == el {
                seen = true;
            }
            if dom.local_name(c) == dom.local_name(el) && dom.namespace(c) == dom.namespace(el) {
                same += 1;
            }
        }
        if plain && same == 1 {
            name
        } else {
            format!("{name}:nth-child({position})")
        }
    }

    /// A selector for `el` within `root`'s tree built from `ids` (stop at the nearest ancestor with
    /// an id that is the only one) and positions, `plain` leaving them out where the type is
    /// unique among siblings.
    fn selector_candidate(&self, root: NodeId, el: NodeId, ids: bool, plain: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut cur = el;
        loop {
            if ids {
                let id = self
                    .dom()
                    .get_attribute(cur, "id")
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned);
                if let Some(id) = id {
                    let candidate = format!("#{}", serialize_identifier(&id));
                    if self.devtools_query(root, &candidate) == [cur] {
                        parts.push(candidate);
                        break;
                    }
                }
            }
            parts.push(self.selector_step(cur, plain));
            let parent = self.dom().parent(cur);
            match parent {
                Some(p) if self.dom().is_element(p) => cur = p,
                _ => break,
            }
        }
        parts.reverse();
        parts.join(" > ")
    }

    /// A selector that matches `node` alone within its own tree (`root.querySelector(selector)`
    /// for the document or the shadow root it is in is `node`). Tried shortest first and checked
    /// against the engine's own matching before it is returned.
    pub fn devtools_selector(&self, node: NodeId) -> Option<String> {
        if !self.dom().is_element(node) {
            return None;
        }
        let root = self.devtools_root(node);
        let unique = |s: &str| self.devtools_query(root, s) == [node];
        for (ids, plain) in [(true, true), (true, false), (false, true), (false, false)] {
            let s = self.selector_candidate(root, node, ids, plain);
            if unique(&s) {
                return Some(s);
            }
        }
        // (the top of a shadow tree is not anchored by an `html` element: a step like
        // `div:nth-child(2)` also matches deeper ones, so keep the first step to the top level)
        let s = self.selector_candidate(root, node, false, false);
        let anchored = match s.split_once(" > ") {
            Some((first, rest)) => format!("{first}:not(* *) > {rest}"),
            None => format!("{s}:not(* *)"),
        };
        unique(&anchored).then_some(anchored)
    }

    /// A script expression for `node` (`document.querySelector("...")`, through `.shadowRoot` of
    /// each shadow host above it); `None` outside a document.
    pub fn devtools_js_path(&self, node: NodeId) -> Option<String> {
        let dom = self.dom();
        if !dom.is_element(node) {
            return None;
        }
        let root = dom.root(node);
        let query = |sel: &str| format!("querySelector({})", js_string(sel));
        if dom.is_document(root) {
            drop(dom);
            let sel = self.devtools_selector(node)?;
            return Some(format!("document.{}", query(&sel)));
        }
        if dom.is_shadow_root(root) {
            let host = dom.host(root)?;
            drop(dom);
            let outer = self.devtools_js_path(host)?;
            let sel = self.devtools_selector(node)?;
            return Some(format!("{outer}.shadowRoot.{}", query(&sel)));
        }
        None
    }

    /// An XPath for `node`: from the tree's root (`full`), or from the nearest ancestor (or the
    /// node) with an id no other element of the tree has.
    pub fn devtools_xpath(&self, node: NodeId, full: bool) -> Option<String> {
        let root = self.devtools_root(node);
        let mut steps: Vec<String> = Vec::new();
        let mut cur = node;
        let mut anchored = false;
        loop {
            let (step, id_anchor) = {
                let dom = self.dom();
                let step = match dom.data(cur) {
                    NodeData::Element(_) => {
                        let html = dom.namespace(cur) == &snowlark_dom::ns!(html);
                        let local = dom.local_name(cur).to_string();
                        let name_test = if html {
                            local.to_ascii_lowercase()
                        } else {
                            let lit = xpath_literal(&dom.qualified_name(cur))?;
                            format!("*[name()={lit}]")
                        };
                        let siblings = |c: NodeId| {
                            dom.is_element(c)
                                && dom.local_name(c) == dom.local_name(cur)
                                && dom.namespace(c) == dom.namespace(cur)
                        };
                        let (mut index, mut count) = (0usize, 0usize);
                        if let Some(p) = dom.parent(cur) {
                            for c in dom.children(p).filter(|&c| siblings(c)) {
                                count += 1;
                                if c == cur {
                                    index = count;
                                }
                            }
                        } else {
                            index = 1;
                            count = 1;
                        }
                        if count > 1 {
                            format!("{name_test}[{index}]")
                        } else {
                            name_test
                        }
                    }
                    NodeData::Text(_) | NodeData::CDataSection(_) => {
                        let parent = dom.parent(cur)?;
                        let kind = |c: NodeId| {
                            matches!(dom.data(c), NodeData::Text(_) | NodeData::CDataSection(_))
                        };
                        let all: Vec<NodeId> = dom.children(parent).filter(|&c| kind(c)).collect();
                        let index = all.iter().position(|&c| c == cur)? + 1;
                        if all.len() > 1 {
                            format!("text()[{index}]")
                        } else {
                            "text()".to_owned()
                        }
                    }
                    NodeData::Comment(_) => {
                        let parent = dom.parent(cur)?;
                        let all: Vec<NodeId> = dom
                            .children(parent)
                            .filter(|&c| matches!(dom.data(c), NodeData::Comment(_)))
                            .collect();
                        let index = all.iter().position(|&c| c == cur)? + 1;
                        if all.len() > 1 {
                            format!("comment()[{index}]")
                        } else {
                            "comment()".to_owned()
                        }
                    }
                    _ => return None,
                };
                let id = (!full && dom.is_element(cur))
                    .then(|| dom.get_attribute(cur, "id").map(str::to_owned))
                    .flatten()
                    .filter(|v| !v.is_empty());
                (step, id)
            };
            if let Some(id) = id_anchor {
                if let Some(lit) = xpath_literal(&id) {
                    let probe = format!("#{}", serialize_identifier(&id));
                    if self.devtools_query(root, &probe) == [cur] {
                        steps.push(format!("//*[@id={lit}]"));
                        anchored = true;
                        break;
                    }
                }
            }
            steps.push(step);
            let parent = self.dom().parent(cur);
            match parent {
                Some(p) if self.dom().is_element(p) => cur = p,
                _ => break,
            }
        }
        steps.reverse();
        let mut out = String::new();
        for (i, s) in steps.iter().enumerate() {
            if i == 0 && anchored {
                out.push_str(s);
            } else {
                out.push('/');
                out.push_str(s);
            }
        }
        Some(out)
    }

    // ----- changes ----------------------------------------------------------------------------------------

    /// Deletes `node` from its parent (`ChildNode.remove()`); whether it had a parent.
    pub fn devtools_delete(&self, cx: &mut Cx, node: NodeId) -> bool {
        if !self.dom().is_alive(node) || self.dom().parent(node).is_none() {
            return false;
        }
        self.dom_mut().remove_node(node);
        self.after_mutation(cx);
        true
    }

    /// Inserts a deep copy of the element after it (`cloneNode(true)` and `after()`); the copy.
    pub fn devtools_duplicate(&self, cx: &mut Cx, node: NodeId) -> Option<NodeId> {
        if !self.dom().is_element(node) || self.dom().parent(node).is_none() {
            return None;
        }
        let copy = self.clone_with_steps(cx, node, None, true).ok()?;
        let r = self.dom_mut().after(node, vec![NodeOrString::Node(copy)]);
        self.after_mutation(cx);
        r.ok().map(|_| copy)
    }

    /// Whether "Hide element" is on for `node` (its `style` attribute has the declaration).
    pub fn devtools_is_hidden(&self, node: NodeId) -> bool {
        self.dom()
            .get_attribute(node, "style")
            .is_some_and(|s| split_declarations(s).into_iter().any(is_hide_declaration))
    }

    /// Hides the element with `visibility: hidden !important` in its `style` attribute, or shows it
    /// again (removes just that declaration); whether it is hidden now.
    pub fn devtools_toggle_hidden(&self, cx: &mut Cx, node: NodeId) -> bool {
        if !self.dom().is_element(node) {
            return false;
        }
        let old = self
            .dom()
            .get_attribute(node, "style")
            .unwrap_or("")
            .to_owned();
        let hidden = split_declarations(&old)
            .into_iter()
            .any(is_hide_declaration);
        let mut decls: Vec<String> = split_declarations(&old)
            .into_iter()
            .filter(|d| !is_hide_declaration(d))
            .map(|d| d.trim().to_owned())
            .collect();
        if !hidden {
            decls.push(HIDE_DECLARATION.to_owned());
        }
        if decls.is_empty() {
            self.dom_mut().remove_attribute(node, "style");
        } else {
            let text = decls.join("; ") + ";";
            let _ = self.dom_mut().set_attribute(node, "style", &text);
        }
        self.after_mutation(cx);
        !hidden
    }

    /// `scrollIntoView()` for `node`.
    pub fn devtools_scroll_into_view(&self, cx: &mut Cx, node: NodeId) {
        if !self.dom().is_element(node) {
            return;
        }
        let _ = cx;
        self.view_scroll_into_view(node, "start", "nearest");
    }

    /// `focus()` for `node`.
    pub fn devtools_focus(&self, cx: &mut Cx, node: NodeId) {
        if self.dom().is_element(node) {
            self.focus_element(cx, node);
        }
    }

    /// The descendant elements of `node` that have children to show (to expand it recursively).
    pub fn devtools_expandable_descendants(&self, node: NodeId) -> Vec<NodeId> {
        let dom = self.dom();
        dom.descendants(node)
            .filter(|&d| dom.is_element(d) && dom.children(d).next().is_some())
            .collect()
    }

    // ----- editing ------------------------------------------------------------------------------------------

    /// The text the attribute editor starts with: `name="value"` (single quotes when the value has
    /// a double quote and no single quote; `&quot;` otherwise).
    pub fn devtools_attribute_text(&self, node: NodeId, name: &str) -> Option<String> {
        let dom = self.dom();
        if !dom.is_element(node) {
            return None;
        }
        let value = dom.get_attribute(node, name)?;
        Some(attribute_text(name, value))
    }

    /// The text of a text or comment node (what "Edit text" edits).
    pub fn devtools_text_value(&self, node: NodeId) -> Option<String> {
        let dom = self.dom();
        match dom.data(node) {
            NodeData::Text(t) | NodeData::CDataSection(t) | NodeData::Comment(t) => {
                Some(t.to_string())
            }
            _ => None,
        }
    }

    /// The markup "Edit as HTML" starts with: an element's outerHTML.
    pub fn devtools_html_text(&self, node: NodeId) -> Option<String> {
        self.dom()
            .is_element(node)
            .then(|| self.dom().outer_html(node))
    }

    /// The attributes `text` writes (as the HTML parser reads the attributes of a start tag:
    /// quoted or bare values, character references decoded), in order.
    pub fn devtools_parse_attributes(&self, node: NodeId, text: &str) -> Vec<(String, String)> {
        let mut dom = self.dom_mut();
        let context = dom
            .parent(node)
            .filter(|&p| dom.is_element(p))
            .unwrap_or(node);
        let Ok(fragment) = dom.fragment_parse(context, &format!("<x {text}></x>")) else {
            return Vec::new();
        };
        let attrs = dom
            .children(fragment)
            .find(|&c| dom.is_element(c))
            .map(|el| {
                dom.attrs(el)
                    .iter()
                    .map(|a| (a.qualified_name().into_owned(), a.value().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        dom.free_subtree(fragment);
        attrs
    }

    /// Applies the attribute editor's `text` (`name="value" other`) to `node`: the attribute
    /// being edited (`replacing`) is replaced in its place by the attributes the text has (when
    /// the text no longer has it, it goes), and the others are set (`setAttribute`, which keeps an
    /// existing one where it is). The attributes after a replaced one are taken off and put back
    /// behind its replacements, as Chrome does, so the order stays. Whether anything changed.
    pub fn devtools_set_attributes(
        &self,
        cx: &mut Cx,
        node: NodeId,
        replacing: Option<&str>,
        text: &str,
    ) -> bool {
        if !self.dom().is_element(node) {
            return false;
        }
        let parsed = self.devtools_parse_attributes(node, text);
        let mut changed = false;
        {
            let mut dom = self.dom_mut();
            // The attributes behind the one that goes (only when the text renames it).
            let mut behind: Vec<(String, String)> = Vec::new();
            if let Some(old) = replacing {
                let kept = parsed.iter().any(|(n, _)| n == old);
                if !kept && dom.get_attribute(node, old).is_some() {
                    let all: Vec<(String, String)> = dom
                        .attrs(node)
                        .iter()
                        .map(|a| (a.qualified_name().into_owned(), a.value().to_owned()))
                        .collect();
                    let at = all.iter().position(|(n, _)| n == old).unwrap_or(0);
                    behind = all[at + 1..]
                        .iter()
                        .filter(|(n, _)| !parsed.iter().any(|(p, _)| p == n))
                        .cloned()
                        .collect();
                    for (n, _) in &behind {
                        dom.remove_attribute(node, n);
                    }
                    dom.remove_attribute(node, old);
                    changed = true;
                }
            }
            for (name, value) in parsed.iter().chain(behind.iter()) {
                if dom.get_attribute(node, name) != Some(value.as_str())
                    && dom.set_attribute(node, name, value).is_ok()
                {
                    changed = true;
                }
            }
        }
        self.after_mutation(cx);
        changed
    }

    /// `outerHTML = markup` for `node`: the element the first node of the markup makes (when it is
    /// an element), or `None` when the element was not replaced (a document element).
    pub fn devtools_set_outer_html(
        &self,
        cx: &mut Cx,
        node: NodeId,
        markup: &str,
    ) -> Option<Option<NodeId>> {
        let (parent, prev) = {
            let dom = self.dom();
            if !dom.is_element(node) {
                return None;
            }
            (dom.parent(node)?, dom.previous_sibling(node))
        };
        let r = self.dom_mut().set_outer_html(node, markup);
        self.after_mutation(cx);
        r.ok()?;
        let dom = self.dom();
        let first = match prev {
            Some(p) => dom.next_sibling(p),
            None => dom.first_child(parent),
        };
        Some(first.filter(|&f| dom.is_element(f)))
    }

    /// The text or comment `node`'s data set to `value` (`nodeValue = value`).
    pub fn devtools_set_text(&self, cx: &mut Cx, node: NodeId, value: &str) -> bool {
        if self.devtools_text_value(node).is_none() {
            return false;
        }
        self.dom_mut().set_node_value(node, value);
        self.after_mutation(cx);
        true
    }

    /// Appends `markup` (HTML) as the last children of `target` (`insertAdjacentHTML("beforeend")`
    /// on an element, the DOM's own steps); the first element it made.
    pub fn devtools_paste(&self, cx: &mut Cx, target: NodeId, markup: &str) -> Option<NodeId> {
        let last = {
            let dom = self.dom();
            if !dom.is_element(target) {
                return None;
            }
            dom.children(target).last()
        };
        let r = self.dom_mut().insert_adjacent_html(
            target,
            snowlark_dom::AdjacentPosition::BeforeEnd,
            markup,
        );
        self.after_mutation(cx);
        r.ok()?;
        let dom = self.dom();
        let first = match last {
            Some(l) => dom.next_sibling(l),
            None => dom.first_child(target),
        };
        let mut cur = first;
        while let Some(c) = cur {
            if dom.is_element(c) {
                return Some(c);
            }
            cur = dom.next_sibling(c);
        }
        None
    }

    // ----- forced states -------------------------------------------------------------------------------

    /// The bits `kind` forces on the element itself and on its ancestors.
    fn forced_bits(kind: u8) -> (ElementState, ElementState) {
        let e = ElementState::empty();
        match kind {
            0 => (ElementState::ACTIVE, ElementState::ACTIVE),
            1 => (ElementState::HOVER, ElementState::HOVER),
            2 => (
                ElementState::FOCUS | ElementState::FOCUS_WITHIN,
                ElementState::FOCUS_WITHIN,
            ),
            3 => (ElementState::VISITED, e),
            4 => (ElementState::FOCUS_WITHIN, ElementState::FOCUS_WITHIN),
            5 => (ElementState::FOCUSRING, e),
            _ => (ElementState::URLTARGET, e),
        }
    }

    /// The state bits the tools force on `el` (its own, and its descendants' that reach it).
    pub(crate) fn forced_mask(&self, el: NodeId) -> ElementState {
        let forced = self.forced_states.borrow();
        if forced.is_empty() {
            return ElementState::empty();
        }
        let dom = self.dom();
        let mut mask = ElementState::empty();
        for &(n, kind) in forced.iter() {
            if !dom.is_alive(n) {
                continue;
            }
            let (own, up) = Self::forced_bits(kind);
            if n == el {
                mask |= own;
            } else if dom.is_shadow_including_inclusive_ancestor_of(el, n) {
                mask |= up;
            }
        }
        mask
    }

    /// Whether the tools force any of `state` on `el`.
    pub(crate) fn devtools_forced(&self, el: NodeId, state: ElementState) -> bool {
        self.forced_mask(el).intersects(state)
    }

    /// The kinds forced on `node` itself, as a bit mask (bit 0 `:active` ... bit 6 `:target`).
    pub fn devtools_forced_kinds(&self, node: NodeId) -> u8 {
        self.forced_states
            .borrow()
            .iter()
            .filter(|(n, _)| *n == node)
            .fold(0, |m, (_, k)| m | 1 << k)
    }

    /// Forces (or stops forcing) the pseudo-class `kind` (see [`FORCE_STATES`]) on `node`: the
    /// style engine and selector matching see the element, and for `:hover`, `:active` and
    /// `:focus-within` its ancestors, in that state whatever the pointer and focus do.
    pub fn devtools_force_state(&self, node: NodeId, kind: u8, on: bool) {
        if !self.dom().is_alive(node) || kind as usize >= FORCE_STATES.len() {
            return;
        }
        {
            let mut forced = self.forced_states.borrow_mut();
            forced.retain(|&(n, k)| !(n == node && k == kind));
            if on {
                forced.push((node, kind));
            }
        }
        let chain = self.chain(Some(node));
        let doc = self.dom().node_document(node);
        let focused = self.focused_element(doc);
        for n in chain {
            let natural = |bit: ElementState| -> bool {
                match bit {
                    b if b == ElementState::HOVER => self.pointer.borrow().hovered.contains(&n),
                    b if b == ElementState::ACTIVE => self.pointer.borrow().active.contains(&n),
                    b if b == ElementState::FOCUS => self.has_focus(&self.dom(), n),
                    b if b == ElementState::FOCUS_WITHIN => focused.is_some_and(|f| {
                        self.dom().is_shadow_including_inclusive_ancestor_of(n, f)
                    }),
                    b if b == ElementState::FOCUSRING => {
                        focused == Some(n)
                            && (self.focus_from_keyboard.get() || self.is_text_control(n))
                    }
                    _ => false,
                }
            };
            for bit in [
                ElementState::HOVER,
                ElementState::ACTIVE,
                ElementState::FOCUS,
                ElementState::FOCUS_WITHIN,
                ElementState::FOCUSRING,
                ElementState::URLTARGET,
            ] {
                // (`set_element_state` adds what is forced)
                self.set_element_state(n, bit, natural(bit));
            }
            if self.forced_mask(n).intersects(ElementState::VISITED) {
                self.set_element_state(n, ElementState::VISITED, true);
                self.set_element_state(n, ElementState::UNVISITED, false);
            } else if kind == 3 && n == node {
                self.reset_element_state(n, ElementState::VISITED | ElementState::UNVISITED);
            }
        }
    }

    /// Stops forcing every state (a new document).
    pub fn devtools_clear_forced(&self) {
        self.forced_states.borrow_mut().clear();
    }

    /// Defines a global `tempN` (the first unused) holding `node`'s wrapper in the top-level realm;
    /// its name and the console's formatting of the value.
    pub fn devtools_store_global(&self, cx: &mut Cx, node: NodeId) -> Option<(String, String)> {
        if !self.dom().is_alive(node) {
            return None;
        }
        // (the console shows a node as its markup, on one line)
        let text = crate::devtools::clip(&self.devtools_outer_html(node).unwrap_or_default(), 200);
        self.enter_script();
        let out = {
            let scope = cx.scope();
            let obj = rt::wrap(scope, node_handle(node));
            obj.and_then(|obj| define_temp(scope, obj.into()))
        };
        self.leave_script(cx);
        out.map(|name| (name, text))
    }
}

/// Defines a global `tempN` (the first unused) holding `v` in the current realm; its name.
pub(crate) fn define_temp(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Option<String> {
    let global = scope.get_current_context().global(scope);
    let mut n = 1;
    let (name, key) = loop {
        let name = format!("temp{n}");
        let key = v8::String::new(scope, &name)?;
        if !global.has(scope, key.into()).unwrap_or(false) {
            break (name, key);
        }
        n += 1;
    };
    global.set(scope, key.into(), v)?;
    Some(name)
}
