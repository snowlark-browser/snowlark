// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §3.2.7 the `innerText` getter: the "rendered text collection steps" over the computed styles,
//! with CSS Text 3 §4 white space processing (collapsible spaces collapse across the text of an inline
//! formatting context and are removed at the starts and ends of lines) and `text-transform`.

use snowlark_dom::{local_name, Dom, NodeData, NodeId};
use snowlark_layout::style::{
    Display, DisplayInternal, DisplayOutside, Style, TextTransform, Visibility, WhiteSpaceCollapse,
};
use snowlark_render::{RenderStyle, Renderer};

use super::Engine;

/// An item of the rendered text collection steps.
enum Item {
    /// Text; `collapsible` when its spaces are collapsible (they go at line starts and ends).
    Text { text: String, collapsible: bool },
    /// Text of a range that is not selectable (`user-select: none`): it is laid out with the rest
    /// (its spaces collapse with theirs, it keeps the line going) but is left out.
    Unselectable { text: String, collapsible: bool },
    /// A `br`: a forced line break.
    Br,
    /// A "required line break count".
    Breaks(u8),
}

fn is_block_level(d: Display) -> bool {
    match d {
        Display::Box { outside, .. } => outside != DisplayOutside::Inline,
        Display::Internal(DisplayInternal::TableCaption) => true,
        _ => false,
    }
}

fn is_none(s: &Option<RenderStyle>) -> bool {
    s.as_ref().is_none_or(|s| s.display() == Display::None)
}

/// The next sibling element with a box of the given internal display (a later cell of the same row, a
/// later row of the same table).
fn has_later(r: &Renderer, dom: &Dom, node: NodeId, kind: DisplayInternal) -> bool {
    let mut n = dom.next_sibling(node);
    while let Some(s) = n {
        if dom.is_element(s)
            && r.element_style(s)
                .is_some_and(|st| st.display() == Display::Internal(kind))
        {
            return true;
        }
        n = dom.next_sibling(s);
    }
    false
}

/// A row's later rows within its table: in its own group, or in a later row group.
fn has_later_row(r: &Renderer, dom: &Dom, row: NodeId) -> bool {
    if has_later(r, dom, row, DisplayInternal::TableRow) {
        return true;
    }
    let Some(group) = dom.parent(row) else {
        return false;
    };
    let grouped = r.element_style(group).is_some_and(|s| {
        matches!(
            s.display(),
            Display::Internal(
                DisplayInternal::TableRowGroup
                    | DisplayInternal::TableHeaderGroup
                    | DisplayInternal::TableFooterGroup
            )
        )
    });
    if !grouped {
        return false;
    }
    let mut n = dom.next_sibling(group);
    while let Some(s) = n {
        if dom.children(s).any(|c| {
            dom.is_element(c)
                && r.element_style(c)
                    .is_some_and(|st| st.display() == Display::Internal(DisplayInternal::TableRow))
        }) {
            return true;
        }
        n = dom.next_sibling(s);
    }
    false
}

/// CSS Text 3 §4.1.1 phase I for one text node: collapsible white space sequences become one space,
/// segment breaks too (`pre-line` keeps them, removing the spaces around them).
fn process(text: &str, collapse: WhiteSpaceCollapse) -> (String, bool) {
    match collapse {
        WhiteSpaceCollapse::Collapse => {
            let mut out = String::with_capacity(text.len());
            let mut space = false;
            for c in text.chars() {
                if matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}') {
                    space = true;
                } else {
                    if space {
                        out.push(' ');
                        space = false;
                    }
                    out.push(c);
                }
            }
            if space {
                out.push(' ');
            }
            (out, true)
        }
        WhiteSpaceCollapse::PreserveBreaks => {
            let mut out = String::with_capacity(text.len());
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push('\n');
                }
                let (l, _) = process(line, WhiteSpaceCollapse::Collapse);
                let l = if i > 0 { l.trim_start_matches(' ') } else { &l };
                out.push_str(l);
            }
            (out, true)
        }
        _ => (text.replace('\r', ""), false),
    }
}

fn transform(text: String, t: TextTransform) -> String {
    match t {
        TextTransform::None => text,
        TextTransform::Uppercase => text.to_uppercase(),
        TextTransform::Lowercase => text.to_lowercase(),
        TextTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut start = true;
            for c in text.chars() {
                if start && c.is_alphanumeric() {
                    out.extend(c.to_uppercase());
                    start = false;
                } else {
                    out.push(c);
                    if c.is_whitespace() {
                        start = true;
                    }
                }
            }
            out
        }
    }
}

/// Whether `el` is a `select`'s `option` or `optgroup` (rendered whenever its display is not `none`,
/// and block-level, whatever the control draws).
fn in_select(dom: &Dom, el: NodeId) -> bool {
    (dom.is_html_element_named(el, &local_name!("option"))
        || dom.is_html_element_named(el, &local_name!("optgroup")))
        && dom
            .ancestors(el)
            .any(|a| dom.is_html_element_named(a, &local_name!("select")))
}

/// HTML "rendered text collection steps" for `node`, appending to `out`.
fn collect(r: &Renderer, dom: &Dom, node: NodeId, out: &mut Vec<Item>) {
    let start = out.len();
    for c in dom.children(node) {
        collect(r, dom, c, out);
    }
    match dom.data(node) {
        NodeData::Text(_) | NodeData::CDataSection(_) => {
            let Some(parent) = dom.parent(node).filter(|&p| dom.is_element(p)) else {
                return;
            };
            let style = r.element_style(parent);
            if is_none(&style) && !in_select(dom, parent) {
                return;
            }
            let Some(style) = style else { return };
            if style.visibility() != Visibility::Visible {
                return;
            }
            let data = dom.character_data(node).unwrap_or("");
            let (text, collapsible) = process(data, style.white_space_collapse());
            let text = transform(text, style.text_transform());
            if !text.is_empty() {
                out.push(Item::Text { text, collapsible });
            }
        }
        NodeData::Element(_) => {
            let style = r.element_style(node);
            let select_part = in_select(dom, node);
            if is_none(&style) {
                return;
            }
            let Some(style) = style else { return };
            if style.visibility() != Visibility::Visible {
                return;
            }
            if dom.is_html_element_named(node, &local_name!("br")) {
                out.push(Item::Br);
                return;
            }
            let display = style.display();
            if display == Display::Internal(DisplayInternal::TableCell)
                && has_later(r, dom, node, DisplayInternal::TableCell)
            {
                out.push(Item::Text {
                    text: "\t".into(),
                    collapsible: false,
                });
            }
            if display == Display::Internal(DisplayInternal::TableRow)
                && has_later_row(r, dom, node)
            {
                out.push(Item::Text {
                    text: "\n".into(),
                    collapsible: false,
                });
            }
            let breaks = if dom.is_html_element_named(node, &local_name!("p")) {
                2
            } else if is_block_level(display) || select_part {
                1
            } else {
                0
            };
            if breaks > 0 {
                out.insert(start, Item::Breaks(breaks));
                out.push(Item::Breaks(breaks));
            }
        }
        _ => {}
    }
}

/// The innerText getter's final steps: required line break counts merged, removed at the ends; spaces
/// at the starts and ends of lines removed, adjacent collapsible spaces collapsed.
fn assemble(items: Vec<Item>) -> String {
    let mut out = String::new();
    // Collapsible spaces at the end of `out` (removed when a line ends there).
    let mut trailing = 0usize;
    // At a line start (or the very start): a collapsible space there goes.
    let mut line_start = true;
    // The text before ends in a collapsible space: a collapsible space next collapses with it.
    let mut space_before = false;
    let mut breaks: u8 = 0;
    let mut started = false;
    for item in items {
        match item {
            Item::Breaks(n) => {
                breaks = breaks.max(n);
                out.truncate(out.len() - trailing);
                trailing = 0;
                space_before = false;
                line_start = true;
            }
            Item::Br => {
                out.truncate(out.len() - trailing);
                trailing = 0;
                space_before = false;
                if started && breaks > 0 {
                    out.extend(std::iter::repeat_n('\n', breaks as usize));
                }
                breaks = 0;
                out.push('\n');
                started = true;
                line_start = true;
            }
            Item::Unselectable { text, collapsible } => {
                let mut t = text.as_str();
                if collapsible && (line_start || space_before) {
                    t = t.strip_prefix(' ').unwrap_or(t);
                }
                if t.is_empty() {
                    continue;
                }
                // (the spaces before it are not at a line's end)
                trailing = 0;
                space_before = collapsible && t.ends_with(' ');
                line_start = t.ends_with('\n');
            }
            Item::Text { text, collapsible } => {
                let mut t = text.as_str();
                if collapsible && (line_start || space_before) {
                    t = t.strip_prefix(' ').unwrap_or(t);
                }
                if t.is_empty() {
                    continue;
                }
                if breaks > 0 {
                    if started {
                        out.extend(std::iter::repeat_n('\n', breaks as usize));
                    }
                    breaks = 0;
                }
                out.push_str(t);
                started = true;
                trailing = if collapsible && t.ends_with(' ') {
                    1
                } else {
                    0
                };
                space_before = trailing > 0;
                line_start = t.ends_with('\n');
            }
        }
    }
    out.truncate(out.len() - trailing);
    out
}

/// CSS UI 4 §6.1: the used value of `user-select` at `el` is `none` (`auto` takes the parent's).
/// Text in an inert subtree is not selectable either (HTML §6.3).
fn user_select_none(r: &Renderer, dom: &Dom, el: NodeId) -> bool {
    if r.element_style(el).is_some_and(|s| s.inert()) {
        return true;
    }
    let mut n = Some(el);
    while let Some(id) = n {
        if dom.is_element(id) {
            match r.resolved_property(id, "user-select").as_deref() {
                Some("none") => return true,
                Some("auto") | None => {}
                Some(_) => return false,
            }
        }
        n = dom.parent(id).and_then(|p| {
            if dom.is_shadow_root(p) {
                dom.host(p)
            } else {
                Some(p)
            }
        });
    }
    false
}

/// The rendered text collection steps for the part of `node` inside `r` (text clipped to its
/// boundary points).
fn collect_range(
    r: &Renderer,
    dom: &Dom,
    node: NodeId,
    range: &crate::engine::ranges::BoundaryPair,
    out: &mut Vec<Item>,
) {
    use crate::engine::ranges::bp_position;
    use std::cmp::Ordering;
    // Nodes wholly before the start or after the end add nothing.
    let len = dom.length(node) as u32;
    if bp_position(dom, (node, len), range.start) == Ordering::Less
        && !dom.is_ancestor_of(node, range.start.0)
    {
        return;
    }
    if bp_position(dom, (node, 0), range.end) == Ordering::Greater && node != range.end.0 {
        return;
    }
    if dom.is_text(node) {
        let data = dom.character_data(node).unwrap_or("");
        let units: Vec<u16> = data.encode_utf16().collect();
        let a = if node == range.start.0 {
            range.start.1 as usize
        } else {
            0
        };
        let b = if node == range.end.0 {
            range.end.1 as usize
        } else {
            units.len()
        };
        let (a, b) = (a.min(units.len()), b.min(units.len()));
        if a >= b {
            return;
        }
        // (text at the top of a shadow tree is styled as a child of the host)
        let parent = dom
            .parent(node)
            .and_then(|p| {
                if dom.is_shadow_root(p) {
                    dom.host(p)
                } else {
                    Some(p)
                }
            })
            .filter(|&p| dom.is_element(p));
        let Some(parent) = parent else {
            return;
        };
        let Some(style) = r.element_style(parent) else {
            return;
        };
        if style.display() == Display::None || style.visibility() != Visibility::Visible {
            return;
        }
        let part = String::from_utf16_lossy(&units[a..b]);
        let (text, collapsible) = process(&part, style.white_space_collapse());
        let text = transform(text, style.text_transform());
        if !text.is_empty() {
            out.push(if user_select_none(r, dom, parent) {
                Item::Unselectable { text, collapsible }
            } else {
                Item::Text { text, collapsible }
            });
        }
        return;
    }
    if !dom.is_element(node) && !dom.is_document(node) && !dom.is_shadow_root(node) {
        return;
    }
    let start = out.len();
    for c in dom.children(node) {
        collect_range(r, dom, c, range, out);
    }
    if !dom.is_element(node) {
        return;
    }
    let style = r.element_style(node);
    if is_none(&style) {
        out.truncate(start);
        return;
    }
    let Some(style) = style else { return };
    if dom.is_html_element_named(node, &local_name!("br")) {
        out.push(Item::Br);
        return;
    }
    let display = style.display();
    let breaks = if dom.is_html_element_named(node, &local_name!("p")) {
        2
    } else if is_block_level(display) {
        1
    } else {
        0
    };
    if breaks > 0 && out.len() > start {
        out.insert(start, Item::Breaks(breaks));
        out.push(Item::Breaks(breaks));
    }
}

/// Whether non-white-space text follows `node` before the next block boundary (an approximation of
/// "on the same line").
fn text_follows_in_line(dom: &Dom, node: NodeId) -> bool {
    let block = |n: NodeId| {
        dom.is_element(n)
            && matches!(
                &**dom.local_name(n),
                "p" | "div"
                    | "li"
                    | "br"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "td"
                    | "tr"
                    | "blockquote"
                    | "pre"
                    | "section"
                    | "article"
                    | "body"
                    | "html"
            )
    };
    let mut cur = dom.next_skipping_children(node, None);
    let mut steps = 0;
    while let Some(n) = cur {
        steps += 1;
        if steps > 1000 || block(n) || dom.ancestors(node).any(|a| a == n) {
            return false;
        }
        if dom.is_text(n) && dom.character_data(n).is_some_and(|d| !d.trim().is_empty()) {
            return true;
        }
        cur = dom.next_in_tree_order(n, None);
    }
    false
}

impl Engine {
    /// The text of a range as `innerText` renders it (copying a selection).
    pub(crate) fn range_inner_text(&self, range: crate::engine::ranges::BoundaryPair) -> String {
        let doc = self.dom().node_document(range.start.0);
        self.with_style(doc, |r| {
            let dom = self.dom();
            let mut items = Vec::new();
            collect_range(r, &dom, dom.root(range.start.0), &range, &mut items);
            let mut text = assemble(items);
            // A space before the end inside a line stays (it is not at a line end).
            let mid_line = dom.is_text(range.end.0)
                && dom.character_data(range.end.0).is_some_and(|d| {
                    let units: Vec<u16> = d.encode_utf16().collect();
                    let e = (range.end.1 as usize).min(units.len());
                    let before = String::from_utf16_lossy(&units[..e]);
                    let after = String::from_utf16_lossy(&units[e..]);
                    before.ends_with([' ', '\t', '\n'])
                        && (!after.trim_start().is_empty()
                            || text_follows_in_line(&dom, range.end.0))
                });
            if mid_line && !text.is_empty() && !text.ends_with(' ') {
                text.push(' ');
            }
            text
        })
        .unwrap_or_default()
    }

    /// HTML §3.2.7 the `innerText` getter for `el`: `None` when `el` is not being rendered (the
    /// caller answers with its descendant text content).
    pub(crate) fn inner_text_of(&self, el: NodeId) -> Option<String> {
        self.with_style(el, |r| {
            let dom = self.dom();
            if !dom.is_connected(el) {
                return None;
            }
            // Not being rendered: no style (in a `display: none` subtree) or `display: none`; a
            // `select`'s option counts as rendered.
            let style = r.element_style(el);
            if is_none(&style) && !in_select(&dom, el) {
                return None;
            }
            let mut items = Vec::new();
            for c in dom.children(el) {
                collect(r, &dom, c, &mut items);
            }
            Some(assemble(items))
        })
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Item {
        Item::Text {
            text: s.into(),
            collapsible: true,
        }
    }

    #[test]
    fn white_space_and_breaks() {
        assert_eq!(
            process("  a \n\t b  ", WhiteSpaceCollapse::Collapse),
            (" a b ".to_string(), true)
        );
        assert_eq!(
            process("a  \n  b", WhiteSpaceCollapse::PreserveBreaks).0,
            "a\nb"
        );
        assert_eq!(
            assemble(vec![
                Item::Breaks(1),
                text(" a "),
                text(" b "),
                Item::Breaks(2),
                text(" c"),
                Item::Br,
                text(" d "),
                Item::Breaks(1),
            ]),
            "a b\n\nc\nd"
        );
        assert_eq!(
            transform("hello big-world".into(), TextTransform::Capitalize),
            "Hello Big-world"
        );
    }
}
