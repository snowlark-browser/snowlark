// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs "delete the selection" and the `delete` and `forwardDelete` commands.

use snowlark_bindings::rt::Cx;
use snowlark_dom::NodeId;

use super::inline::Recorded;
use super::nodes::Item;
use crate::engine::ranges::Bp;
use crate::engine::Engine;

/// The flags of "delete the selection".
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeleteFlags {
    pub block_merging: bool,
    pub strip_wrappers: bool,
    /// Direction "forward" (else "backward").
    pub forward: bool,
}

impl Default for DeleteFlags {
    fn default() -> Self {
        DeleteFlags {
            block_merging: true,
            strip_wrappers: true,
            forward: true,
        }
    }
}

/// Combining marks `forwardDelete` deletes with the character before them (general category M;
/// the common blocks).
fn is_combining_mark(u: u16) -> bool {
    matches!(u, 0x0300..=0x036f | 0x0483..=0x0489 | 0x0591..=0x05bd | 0x05bf | 0x05c1 | 0x05c2
        | 0x05c4 | 0x05c5 | 0x05c7 | 0x0610..=0x061a | 0x064b..=0x065f | 0x0670
        | 0x06d6..=0x06dc | 0x06df..=0x06e4 | 0x06e7 | 0x06e8 | 0x06ea..=0x06ed
        | 0x0900..=0x0903 | 0x093a..=0x094f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff
        | 0x20d0..=0x20ff | 0xfe20..=0xfe2f)
}

impl Engine {
    /// "The block" of a boundary point's node for "delete the selection": its nearest ancestor in
    /// the same editing host that is not inline, when it is a block node or an editing host that
    /// may contain a `span` and is not a `td` or `th`.
    fn ec_delete_block(&self, node: NodeId) -> Option<NodeId> {
        let mut block = node;
        while let Some(p) = self.ec_parent(block) {
            if self.ec_same_host(block, Some(p)) && self.ec_inline(block) {
                block = p;
            } else {
                break;
            }
        }
        if (!self.ec_block(block) && !self.ec_editing_host(block))
            || !self.ec_allowed_child(Item::Name("span"), Item::Node(block))
            || self.ec_is(block, &["td", "th"])
        {
            return None;
        }
        Some(block)
    }

    /// "Delete the selection", then the states and values its steps recorded restored; the inline
    /// formatting a non-collapsed deletion removed at the caret is also kept for the next
    /// insertion there (the formatting elements, links excepted, as browsers keep a typing style).
    pub(crate) fn ec_delete_selection(&self, cx: &mut Cx, doc: NodeId, flags: DeleteFlags) {
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        if r.start == r.end {
            if let Some(o) = self.ec_delete_selection_steps(cx, doc, flags) {
                self.ec_restore_states_and_values(cx, doc, &o);
            }
            return;
        }
        let before = self.ec_formatting_ancestors(r.start.0);
        if let Some(o) = self.ec_delete_selection_steps(cx, doc, flags) {
            self.ec_restore_states_and_values(cx, doc, &o);
        }
        let Some(after) = self.ec_active(doc) else {
            return;
        };
        let caret = after.start;
        let removed: Vec<(String, Vec<(String, String)>)> = {
            let dom = self.dom();
            before
                .into_iter()
                .filter(|&e| !(dom.is_alive(e) && dom.is_inclusive_ancestor_of(e, caret.0)))
                .filter(|&e| dom.is_alive(e))
                .map(|e| {
                    let attrs = dom
                        .attrs(e)
                        .iter()
                        .map(|a| (a.qualified_name().into_owned(), a.value().to_owned()))
                        .collect();
                    (dom.local_name(e).to_string(), attrs)
                })
                .collect()
        };
        let style = (!removed.is_empty() && after.start == after.end).then_some((caret, removed));
        self.doc_state(doc, |s| s.typing_style = style);
    }

    /// The inline formatting elements that are inclusive ancestors of `node` inside its editing
    /// host, outermost first.
    pub(crate) fn ec_formatting_ancestors(&self, node: NodeId) -> Vec<NodeId> {
        const FORMATTING: &[&str] = &[
            "b", "big", "code", "del", "em", "font", "i", "ins", "kbd", "mark", "s", "samp",
            "small", "span", "strike", "strong", "sub", "sup", "tt", "u", "var",
        ];
        let mut out: Vec<NodeId> = {
            let dom = self.dom();
            dom.inclusive_ancestors(node)
                .skip_while(|&a| !dom.is_element(a))
                .take_while(|&a| dom.is_element(a))
                .collect()
        };
        out.retain(|&a| self.ec_editable(a) && self.ec_is(a, FORMATTING));
        out.reverse();
        out
    }

    /// The steps of "delete the selection"; returns the states and values to restore once they
    /// end, when they recorded them.
    fn ec_delete_selection_steps(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        flags: DeleteFlags,
    ) -> Option<Vec<(&'static str, Recorded)>> {
        let r = self.ec_active(doc)?;
        self.ec_canonicalize_whitespace(cx, r.start.0, r.start.1, true);
        let r = self.ec_active(doc)?;
        self.ec_canonicalize_whitespace(cx, r.end.0, r.end.1, true);
        let r = self.ec_active(doc)?;
        let selection_start = r.start;
        let (mut start_node, mut start_offset) = self.ec_last_equivalent_point(r.start);
        let (mut end_node, mut end_offset) = self.ec_first_equivalent_point(r.end);
        if self.ec_bp_cmp((end_node, end_offset), (start_node, start_offset))
            != std::cmp::Ordering::Greater
        {
            self.ec_collapse_to(doc, flags.forward);
            return None;
        }
        if self.dom().is_text(start_node) && start_offset == 0 {
            if let Some(p) = self.ec_parent(start_node) {
                start_offset = self.ec_index(start_node);
                start_node = p;
            }
        }
        if self.dom().is_text(end_node) && end_offset == self.ec_len(end_node) {
            if let Some(p) = self.ec_parent(end_node) {
                end_offset = 1 + self.ec_index(end_node);
                end_node = p;
            }
        }
        self.ec_select(doc, (start_node, start_offset), (end_node, end_offset));
        let r = self.ec_active(doc)?;
        let start_block = self.ec_delete_block(r.start.0);
        let end_block = self.ec_delete_block(r.end.0);
        // (for the delete commands the states and values are those at the selection's start, of
        // the Text node it is in or the node before it, when there is one: what is typed next
        // continues the text before the deleted part, as in browsers. Text typed over a selection
        // takes the first selected node's, as the spec has it.)
        let at = flags.strip_wrappers.then(|| {
            let (n, o) = selection_start;
            let before = if self.dom().is_text(n) {
                Some(n)
            } else {
                self.ec_child_before(n, o).map(|mut c| {
                    while let Some(l) = self.dom().last_child(c) {
                        c = l;
                    }
                    c
                })
            };
            before.filter(|&b| self.ec_formattable(b))
        });
        let at = self
            .ec_first_formattable(r)
            .map(|first| at.flatten().unwrap_or(first));
        let overrides = self.ec_record_current_states_and_values(doc, at);
        let finish = |e: &Engine| e.ec_collapse_to(doc, flags.forward);
        if start_node == end_node && self.ec_editable(start_node) && self.dom().is_text(start_node)
        {
            self.ec_delete_data(cx, start_node, start_offset, end_offset - start_offset);
            self.ec_canonicalize_whitespace(cx, start_node, start_offset, false);
            finish(self);
            return Some(overrides);
        }
        if self.ec_editable(start_node) && self.dom().is_text(start_node) {
            let len = self.ec_len(start_node);
            self.ec_delete_data(cx, start_node, start_offset, len - start_offset);
        }
        let Some(r) = self.ec_active(doc) else {
            return Some(overrides);
        };
        let list = self.ec_contained_nodes(r, false, |e, n| {
            e.ec_editable(n) && !e.ec_is(n, &["thead", "tbody", "tfoot", "tr", "th", "td"])
        });
        for node in list {
            let Some(mut parent) = self.ec_parent(node) else {
                continue;
            };
            self.ec_remove(cx, node);
            let block = self.ec_block_node_of(parent);
            // (not for a block outside the editing host: an inline editing host gets no `br`)
            let no_visible = block.is_some_and(|b| {
                self.ec_editable_or_host(b)
                    && !self.ec_children(b).into_iter().any(|c| self.ec_visible(c))
            });
            if no_visible && self.ec_editable_or_host(parent) {
                let br = self.ec_create(doc, "br");
                self.ec_append(cx, parent, br);
            }
            if flags.strip_wrappers || !self.dom().is_inclusive_ancestor_of(parent, start_node) {
                while self.ec_editable(parent) && self.ec_inline(parent) && self.ec_len(parent) == 0
                {
                    let Some(gp) = self.ec_parent(parent) else {
                        break;
                    };
                    self.ec_remove(cx, parent);
                    parent = gp;
                }
            }
        }
        if self.dom().is_alive(end_node)
            && self.ec_editable(end_node)
            && self.dom().is_text(end_node)
        {
            self.ec_delete_data(cx, end_node, 0, end_offset.min(self.ec_len(end_node)));
        }
        if let Some(r) = self.ec_active(doc) {
            self.ec_canonicalize_whitespace(cx, r.start.0, r.start.1, false);
        }
        if let Some(r) = self.ec_active(doc) {
            self.ec_canonicalize_whitespace(cx, r.end.0, r.end.1, false);
        }
        let (Some(start_block), Some(mut end_block)) = (start_block, end_block) else {
            finish(self);
            return Some(overrides);
        };
        if !flags.block_merging
            || !self.ec_same_host(start_block, Some(end_block))
            || start_block == end_block
        {
            finish(self);
            return Some(overrides);
        }
        let values;
        let children = self.ec_children(start_block);
        if children.len() == 1 && self.ec_collapsed_block_prop(children[0]) {
            self.ec_remove(cx, children[0]);
        }
        if self.dom().is_ancestor_of(start_block, end_block) {
            let mut reference = end_block;
            while let Some(p) = self.ec_parent(reference) {
                if p == start_block {
                    break;
                }
                reference = p;
            }
            let i = self.ec_index(reference);
            self.ec_collapse(doc, (start_block, i));
            if !self.dom().has_child_nodes(end_block) {
                while self.ec_editable(end_block) {
                    let Some(p) = self.ec_parent(end_block) else {
                        break;
                    };
                    if self.dom().child_count(p) != 1 || p == start_block {
                        break;
                    }
                    self.ec_remove(cx, end_block);
                    end_block = p;
                }
                let (prev, next) = {
                    let dom = self.dom();
                    (dom.previous_sibling(end_block), dom.next_sibling(end_block))
                };
                if self.ec_editable(end_block)
                    && !self.ec_inline(end_block)
                    && self.ec_inline_o(prev)
                    && self.ec_inline_o(next)
                {
                    let br = self.ec_create(doc, "br");
                    self.ec_insert_after(cx, end_block, br);
                }
                if self.ec_editable(end_block) {
                    self.ec_remove(cx, end_block);
                }
                return Some(overrides);
            }
            if !self.ec_inline_o(self.dom().first_child(end_block)) {
                return Some(overrides);
            }
            let mut children = vec![self.dom().first_child(end_block).unwrap()];
            loop {
                let last = *children.last().unwrap();
                if self.ec_is(last, &["br"]) {
                    break;
                }
                match self.ec_next_sibling(last) {
                    Some(n) if self.ec_inline(n) => children.push(n),
                    _ => break,
                }
            }
            values = self.ec_record_values(&children);
            while self.ec_parent(children[0]) != Some(start_block) {
                let before = self.ec_parent(children[0]);
                self.ec_split_parent(cx, &children);
                if self.ec_parent(children[0]) == before {
                    break;
                }
            }
            let prev = self.dom().previous_sibling(children[0]);
            if let Some(prev) = prev.filter(|&p| self.ec_editable(p) && self.ec_is(p, &["br"])) {
                self.ec_remove(cx, prev);
            }
        } else if self.dom().is_ancestor_of(end_block, start_block) {
            let len = self.ec_len(start_block);
            self.ec_collapse(doc, (start_block, len));
            let mut reference = start_block;
            while let Some(p) = self.ec_parent(reference) {
                if p == end_block {
                    break;
                }
                reference = p;
            }
            let next = self.dom().next_sibling(reference);
            let last = self.dom().last_child(start_block);
            if self.ec_inline_o(next) && self.ec_is_o(last, &["br"]) {
                self.ec_remove(cx, last.unwrap());
            }
            let mut nodes_to_move = Vec::new();
            let next = self.dom().next_sibling(reference);
            if let Some(n) = next.filter(|&n| !self.ec_block(n)) {
                nodes_to_move.push(n);
            }
            // (as the spec's reference implementation: one more node at most)
            if let Some(&last) = nodes_to_move.last() {
                let next = self.dom().next_sibling(last);
                if !self.ec_is(last, &["br"]) && next.is_some_and(|n| !self.ec_block(n)) {
                    nodes_to_move.push(next.unwrap());
                }
            }
            values = self.ec_record_values(&nodes_to_move);
            for n in nodes_to_move {
                let len = self.dom().child_count(start_block) as u32;
                self.ec_move(cx, n, start_block, len);
            }
        } else {
            let len = self.ec_len(start_block);
            self.ec_collapse(doc, (start_block, len));
            let first = self.dom().first_child(end_block);
            let last = self.dom().last_child(start_block);
            if self.ec_inline_o(first) && self.ec_is_o(last, &["br"]) {
                self.ec_remove(cx, last.unwrap());
            }
            values = self.ec_record_values(&self.ec_children(end_block));
            while let Some(c) = self.ec_first_child(end_block) {
                let len = self.dom().child_count(start_block) as u32;
                self.ec_move(cx, c, start_block, len);
            }
            while !self.dom().has_child_nodes(end_block) {
                let Some(p) = self.ec_parent(end_block) else {
                    break;
                };
                self.ec_remove(cx, end_block);
                end_block = p;
            }
        }
        // Adjacent lists of one kind merge.
        let mut ancestor = start_block;
        loop {
            let chain: Vec<NodeId> = self.dom().inclusive_ancestors(ancestor).collect();
            let mergeable = |e: &Engine, n: NodeId| {
                let next = e.dom().next_sibling(n);
                ((e.ec_is(n, &["ol"]) && e.ec_is_o(next, &["ol"]))
                    || (e.ec_is(n, &["ul"]) && e.ec_is_o(next, &["ul"])))
                    && e.ec_same_host(n, next)
            };
            if !chain
                .iter()
                .any(|&n| self.ec_same_host(ancestor, Some(n)) && mergeable(self, n))
            {
                break;
            }
            while !mergeable(self, ancestor) {
                match self.ec_parent(ancestor) {
                    Some(p) => ancestor = p,
                    None => return Some(overrides),
                }
            }
            let Some(next) = self.dom().next_sibling(ancestor) else {
                break;
            };
            while let Some(c) = self.ec_first_child(next) {
                let len = self.dom().child_count(ancestor) as u32;
                self.ec_move(cx, c, ancestor, len);
            }
            self.ec_remove(cx, next);
        }
        self.ec_restore_values(cx, &values);
        if !self.dom().has_child_nodes(start_block) {
            let br = self.ec_create(doc, "br");
            self.ec_append(cx, start_block, br);
        }
        self.ec_remove_extraneous_line_breaks_at_end(cx, start_block);
        Some(overrides)
    }

    /// The `delete` command's action (deleting backwards from the caret).
    pub(crate) fn ec_delete(&self, cx: &mut Cx, doc: NodeId) -> bool {
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        if r.start != r.end {
            self.ec_delete_selection(cx, doc, DeleteFlags::default());
            return true;
        }
        self.ec_canonicalize_whitespace(cx, r.start.0, r.start.1, true);
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        let (mut node, mut offset) = r.start;
        loop {
            let prev_sib = self.dom().previous_sibling(node);
            let before = self.ec_child_before(node, offset);
            if offset == 0 && self.ec_editable_o(prev_sib) && self.ec_invisible_o(prev_sib) {
                self.ec_remove(cx, prev_sib.unwrap());
            } else if self.ec_editable_o(before) && self.ec_invisible_o(before) {
                self.ec_remove(cx, before.unwrap());
                offset -= 1;
            } else if (offset == 0 && self.ec_inline(node)) || self.ec_invisible(node) {
                let Some(p) = self.ec_parent(node) else {
                    break;
                };
                offset = self.ec_index(node);
                node = p;
            } else if self.ec_editable_o(before) && self.ec_is_o(before, &["a"]) {
                self.ec_remove_preserving_descendants(cx, before.unwrap());
                return true;
            } else if before.is_some_and(|b| !self.ec_block(b) && !self.ec_is(b, &["br", "img"])) {
                node = before.unwrap();
                offset = self.ec_len(node);
            } else {
                break;
            }
        }
        let before = self.ec_child_before(node, offset);
        if (self.dom().is_text(node) && offset != 0)
            || (self.ec_block(node) && self.ec_is_o(before, &["br", "hr", "img"]))
        {
            self.ec_select(doc, (node, offset), (node, offset - 1));
            self.ec_delete_selection(cx, doc, DeleteFlags::default());
            return true;
        }
        if self.ec_inline(node) {
            return true;
        }
        let parent = self.ec_parent(node);
        if self.ec_is(node, &["li", "dt", "dd"])
            && parent.is_some_and(|p| self.dom().first_child(p) == Some(node))
            && offset == 0
        {
            let mut items: Vec<NodeId> = {
                let dom = self.dom();
                dom.ancestors(node).collect()
            };
            items.retain(|&a| self.ec_is(a, &["li"]));
            items.reverse();
            for item in items {
                self.ec_normalize_sublists(cx, item);
            }
            self.ec_split_parent(cx, &[node]);
            let mut node = node;
            if self.ec_is(node, &["dd", "dt"]) && self.ec_disallowed_everywhere(node) {
                let name = self.ec_default_single_line_container(doc);
                node = self.ec_set_tag_name(cx, node, name);
            }
            self.ec_fix_disallowed_ancestors(cx, doc, node);
            return true;
        }
        let (mut start_node, mut start_offset) = (node, offset);
        loop {
            if start_offset == 0 {
                let Some(p) = self.ec_parent(start_node) else {
                    break;
                };
                start_offset = self.ec_index(start_node);
                start_node = p;
                continue;
            }
            let before = self.ec_child_before(start_node, start_offset);
            if self.ec_editable_o(before) && self.ec_invisible_o(before) {
                self.ec_remove(cx, before.unwrap());
                start_offset -= 1;
            } else {
                break;
            }
        }
        if offset == 0 {
            let chain: Vec<NodeId> = self.dom().inclusive_ancestors(node).collect();
            if chain.into_iter().any(|a| {
                self.ec_editable(a)
                    && self.ec_same_host(a, Some(node))
                    && self.ec_indentation_element(a)
            }) {
                // Outdenting (the `outdent` command's algorithm) is not done yet: the indented
                // block stays.
                return true;
            }
        }
        let at = self.ec_child(start_node, start_offset);
        if self.ec_is_o(at, &["table"]) {
            return true;
        }
        let before = self.ec_child_before(start_node, start_offset);
        if self.ec_is_o(before, &["table"]) {
            self.ec_select(
                doc,
                (start_node, start_offset - 1),
                (start_node, start_offset),
            );
            return true;
        }
        if offset == 0
            && (self.ec_is_o(before, &["hr"])
                || (self.ec_is_o(before, &["br"]) && {
                    let prev = before.and_then(|b| self.dom().previous_sibling(b));
                    self.ec_is_o(prev, &["br"]) || !self.ec_inline_o(prev)
                }))
        {
            self.ec_select(
                doc,
                (start_node, start_offset - 1),
                (start_node, start_offset),
            );
            self.ec_delete_selection(cx, doc, DeleteFlags::default());
            self.ec_collapse(doc, (node, offset));
            return true;
        }
        let at = self.ec_child(start_node, start_offset);
        if self.ec_is_o(at, &["li", "dt", "dd"])
            && self.ec_inline_o(at.and_then(|a| self.dom().first_child(a)))
            && start_offset != 0
        {
            if let Some(previous_item) = self.ec_child_before(start_node, start_offset) {
                let doc_ = doc;
                let last = self.dom().last_child(previous_item);
                if self.ec_inline_o(last) && !self.ec_is_o(last, &["br"]) {
                    let br = self.ec_create(doc_, "br");
                    self.ec_append(cx, previous_item, br);
                }
                let last = self.dom().last_child(previous_item);
                if self.ec_inline_o(last) {
                    let br = self.ec_create(doc_, "br");
                    self.ec_append(cx, previous_item, br);
                }
            }
        }
        let at = self.ec_child(start_node, start_offset);
        if self.ec_is_o(at, &["li", "dt", "dd"])
            && self.ec_is_o(
                at.and_then(|a| self.dom().previous_sibling(a)),
                &["li", "dt", "dd"],
            )
        {
            // (the original range, kept as a live range while the items merge)
            let original = self.new_range(doc);
            if let Some(r) = self.ec_active(doc) {
                self.set_range(original, r);
            }
            let Some(prev) = self.ec_child_before(start_node, start_offset) else {
                return true;
            };
            let len = self.ec_len(prev);
            let next = self.dom().next_sibling(prev);
            if let Some(next) = next {
                self.ec_select(doc, (prev, len), (next, 0));
                self.ec_delete_selection(cx, doc, DeleteFlags::default());
            }
            let r = self.range_bounds(original);
            self.ranges.borrow_mut().retain(|&h| h != original);
            self.ec_select(doc, r.start, r.end);
            return true;
        }
        while let Some(c) = self.ec_child_before(start_node, start_offset) {
            if self.ec_editable(c) && self.ec_invisible(c) {
                self.ec_remove(cx, c);
                start_offset -= 1;
            } else {
                start_node = c;
                start_offset = self.ec_len(c);
            }
        }
        self.ec_select(doc, (start_node, start_offset), (node, offset));
        self.ec_delete_selection(
            cx,
            doc,
            DeleteFlags {
                forward: false,
                ..Default::default()
            },
        );
        true
    }

    /// The `forwardDelete` command's action.
    pub(crate) fn ec_forward_delete(&self, cx: &mut Cx, doc: NodeId) -> bool {
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        if r.start != r.end {
            self.ec_delete_selection(cx, doc, DeleteFlags::default());
            return true;
        }
        self.ec_canonicalize_whitespace(cx, r.start.0, r.start.1, true);
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        let (mut node, mut offset) = r.start;
        loop {
            let next_sib = self.dom().next_sibling(node);
            let at = self.ec_child(node, offset);
            let len = self.ec_len(node);
            if offset == len && self.ec_editable_o(next_sib) && self.ec_invisible_o(next_sib) {
                self.ec_remove(cx, next_sib.unwrap());
            } else if self.ec_editable_o(at) && self.ec_invisible_o(at) {
                self.ec_remove(cx, at.unwrap());
            } else if (offset == len && self.ec_inline(node)) || self.ec_invisible(node) {
                let Some(p) = self.ec_parent(node) else {
                    break;
                };
                offset = 1 + self.ec_index(node);
                node = p;
            } else if at.is_some_and(|c| {
                !self.ec_block(c)
                    && !self.ec_is(c, &["br", "img"])
                    && !self.ec_collapsed_block_prop(c)
            }) {
                node = at.unwrap();
                offset = 0;
            } else {
                break;
            }
        }
        if self.dom().is_text(node) && offset != self.ec_len(node) {
            let len = self.ec_len(node);
            let mut end_offset = offset + 1;
            while end_offset != len
                && self
                    .ec_unit(node, end_offset)
                    .is_some_and(is_combining_mark)
            {
                end_offset += 1;
            }
            self.ec_select(doc, (node, offset), (node, end_offset));
            self.ec_delete_selection(cx, doc, DeleteFlags::default());
            return true;
        }
        if self.ec_inline(node) {
            return true;
        }
        let at = self.ec_child(node, offset);
        if self.ec_is_o(at, &["br", "hr", "img"]) && !self.ec_collapsed_block_prop_o(at) {
            self.ec_select(doc, (node, offset), (node, offset + 1));
            self.ec_delete_selection(cx, doc, DeleteFlags::default());
            return true;
        }
        let (mut end_node, mut end_offset): Bp = (node, offset);
        if self.ec_collapsed_block_prop_o(self.ec_child(end_node, end_offset)) {
            end_offset += 1;
        }
        loop {
            if end_offset == self.ec_len(end_node) {
                let Some(p) = self.ec_parent(end_node) else {
                    break;
                };
                end_offset = 1 + self.ec_index(end_node);
                end_node = p;
                continue;
            }
            let at = self.ec_child(end_node, end_offset);
            if self.ec_editable_o(at) && self.ec_invisible_o(at) {
                self.ec_remove(cx, at.unwrap());
            } else {
                break;
            }
        }
        if self.ec_is_o(self.ec_child_before(end_node, end_offset), &["table"]) {
            return true;
        }
        if self.ec_is_o(self.ec_child(end_node, end_offset), &["table"]) {
            self.ec_select(doc, (end_node, end_offset), (end_node, end_offset + 1));
            return true;
        }
        if offset == self.ec_len(node)
            && self.ec_is_o(self.ec_child(end_node, end_offset), &["br", "hr"])
        {
            self.ec_select(doc, (end_node, end_offset), (end_node, end_offset + 1));
            self.ec_delete_selection(cx, doc, DeleteFlags::default());
            self.ec_collapse(doc, (node, offset));
            return true;
        }
        while let Some(c) = self.ec_child(end_node, end_offset) {
            if self.ec_editable(c) && self.ec_invisible(c) {
                self.ec_remove(cx, c);
            } else {
                end_node = c;
                end_offset = 0;
            }
        }
        self.ec_select(doc, (node, offset), (end_node, end_offset));
        self.ec_delete_selection(cx, doc, DeleteFlags::default());
        true
    }
}
