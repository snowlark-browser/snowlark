// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs: the tree operations the commands share — "set the tag name", removing
//! extraneous line breaks, "wrap", "split the parent", "remove a node while preserving its
//! descendants", "fix disallowed ancestors" and "normalize sublists".

use snowlark_bindings::rt::Cx;
use snowlark_dom::{LocalName, NodeId};

use super::nodes::Item;
use crate::engine::{Engine, Object};

impl Engine {
    // ----- mutations (each followed by the engine's reactions, so live ranges are current) -----------

    /// `createElement(name)` on `doc`.
    pub(super) fn ec_create(&self, doc: NodeId, name: &str) -> NodeId {
        self.dom_mut()
            .create_html_element(doc, LocalName::from(name))
    }

    pub(super) fn ec_remove(&self, cx: &mut Cx, n: NodeId) {
        if self.dom().parent(n).is_some() {
            self.dom_mut().remove_node(n);
            self.after_mutation(cx);
        }
    }

    pub(super) fn ec_insert(&self, cx: &mut Cx, parent: NodeId, n: NodeId, before: Option<NodeId>) {
        let _ = self.dom_mut().insert_before(parent, n, before);
        self.after_mutation(cx);
    }

    pub(super) fn ec_append(&self, cx: &mut Cx, parent: NodeId, n: NodeId) {
        self.ec_insert(cx, parent, n, None);
    }

    /// Inserts `n` into `anchor`'s parent right after `anchor`.
    pub(super) fn ec_insert_after(&self, cx: &mut Cx, anchor: NodeId, n: NodeId) {
        let (p, next) = {
            let dom = self.dom();
            (dom.parent(anchor), dom.next_sibling(anchor))
        };
        if let Some(p) = p {
            self.ec_insert(cx, p, n, next);
        }
    }

    pub(super) fn ec_delete_data(&self, cx: &mut Cx, t: NodeId, offset: u32, count: u32) {
        let _ = self
            .dom_mut()
            .delete_data(t, offset as usize, count as usize);
        self.after_mutation(cx);
    }

    pub(super) fn ec_insert_data(&self, cx: &mut Cx, t: NodeId, offset: u32, data: &str) {
        let _ = self.dom_mut().insert_data(t, offset as usize, data);
        self.after_mutation(cx);
    }

    pub(super) fn ec_remove_attribute(&self, cx: &mut Cx, el: NodeId, name: &str) {
        if self.dom().has_attribute(el, name) {
            self.dom_mut().remove_attribute(el, name);
            self.after_mutation(cx);
        }
    }

    /// Copies every attribute of `from` to `to`, in order.
    pub(super) fn ec_copy_attributes(&self, cx: &mut Cx, from: NodeId, to: NodeId) {
        let attrs: Vec<_> = {
            let dom = self.dom();
            dom.attrs(from)
                .iter()
                .map(|a| {
                    (
                        a.local_name().clone(),
                        a.namespace().clone(),
                        a.prefix().cloned(),
                        a.value().to_owned(),
                    )
                })
                .collect()
        };
        for (local, ns, prefix, value) in attrs {
            self.dom_mut()
                .set_attribute_value(to, local, ns, prefix, &value);
        }
        self.after_mutation(cx);
    }

    // ----- set the tag name -------------------------------------------------------------------------------

    /// "Set the tag name" of `element` to `name`; returns the element now in its place.
    pub(super) fn ec_set_tag_name(&self, cx: &mut Cx, element: NodeId, name: &str) -> NodeId {
        if self.ec_is(element, &[name]) {
            return element;
        }
        let Some(parent) = self.ec_parent(element) else {
            return element;
        };
        let doc = self.dom().node_document(element);
        let replacement = self.ec_create(doc, name);
        self.ec_insert(cx, parent, replacement, Some(element));
        self.ec_copy_attributes(cx, element, replacement);
        while let Some(c) = self.ec_first_child(element) {
            let n = self.dom().child_count(replacement) as u32;
            self.ec_move(cx, c, replacement, n);
        }
        self.ec_remove(cx, element);
        replacement
    }

    // ----- extraneous line breaks -------------------------------------------------------------------------

    /// "Remove extraneous line breaks before" a node.
    pub(super) fn ec_remove_extraneous_line_breaks_before(&self, cx: &mut Cx, node: NodeId) {
        let Some(mut r) = self.dom().previous_sibling(node) else {
            return;
        };
        while let Some(l) = self.dom().last_child(r) {
            r = l;
        }
        let parent = self.ec_parent(node);
        while self.ec_invisible(r) && !self.ec_extraneous_line_break(r) && Some(r) != parent {
            match self.dom().previous_in_tree_order(r, None) {
                Some(p) => r = p,
                None => return,
            }
        }
        if self.ec_editable(r) && self.ec_extraneous_line_break(r) {
            self.ec_remove(cx, r);
        }
    }

    /// "Remove extraneous line breaks at the end of" a node.
    pub(super) fn ec_remove_extraneous_line_breaks_at_end(&self, cx: &mut Cx, node: NodeId) {
        let mut r = node;
        while let Some(l) = self.dom().last_child(r) {
            r = l;
        }
        while self.ec_invisible(r) && !self.ec_extraneous_line_break(r) && r != node {
            match self.dom().previous_in_tree_order(r, None) {
                Some(p) => r = p,
                None => return,
            }
        }
        if self.ec_editable(r) && self.ec_extraneous_line_break(r) {
            while let Some(p) = self.ec_parent(r) {
                if self.ec_editable(p) && self.ec_invisible(p) {
                    r = p;
                } else {
                    break;
                }
            }
            self.ec_remove(cx, r);
        }
    }

    /// "Remove extraneous line breaks from" a node.
    pub(super) fn ec_remove_extraneous_line_breaks_from(&self, cx: &mut Cx, node: NodeId) {
        self.ec_remove_extraneous_line_breaks_before(cx, node);
        self.ec_remove_extraneous_line_breaks_at_end(cx, node);
    }

    // ----- wrap -------------------------------------------------------------------------------------------

    /// "Wrap" a list of siblings: into a sibling that `sibling_criteria` accepts, else into what
    /// `new_parent` makes. Returns the new parent.
    pub(super) fn ec_wrap(
        &self,
        cx: &mut Cx,
        mut list: Vec<NodeId>,
        sibling_criteria: &dyn Fn(&Engine, NodeId) -> bool,
        new_parent: &dyn Fn(&Engine, &mut Cx) -> Option<NodeId>,
    ) -> Option<NodeId> {
        if list.iter().all(|&n| self.ec_invisible(n))
            && !list.iter().any(|&n| self.ec_is(n, &["br"]))
        {
            return None;
        }
        self.ec_parent(*list.first()?)?;
        let last = *list.last()?;
        let last_next = self.dom().next_sibling(last);
        if self.ec_inline(last) && !self.ec_is(last, &["br"]) && self.ec_is_o(last_next, &["br"]) {
            list.push(last_next?);
        }
        loop {
            let prev = self.dom().previous_sibling(list[0]);
            match prev {
                Some(p) if self.ec_invisible(p) => list.insert(0, p),
                _ => break,
            }
        }
        loop {
            let next = self.dom().next_sibling(*list.last()?);
            match next {
                Some(n) if self.ec_invisible(n) => list.push(n),
                _ => break,
            }
        }
        let first_prev = self.dom().previous_sibling(list[0]);
        let last_next = self.dom().next_sibling(*list.last()?);
        let new_parent =
            if first_prev.is_some_and(|p| self.ec_editable(p) && sibling_criteria(self, p)) {
                first_prev?
            } else if last_next.is_some_and(|n| self.ec_editable(n) && sibling_criteria(self, n)) {
                last_next?
            } else {
                new_parent(self, cx)?
            };
        if self.ec_parent(new_parent).is_none() {
            let p = self.ec_parent(list[0])?;
            self.ec_insert(cx, p, new_parent, Some(list[0]));
            // (a boundary point at the new parent's index moves past it)
            let i = self.ec_index(new_parent);
            let ranges: Vec<_> = self.ranges.borrow().clone();
            for h in ranges {
                let r = match &*self.obj(h) {
                    Object::Range(r) => *r,
                    _ => continue,
                };
                let mut n = r;
                if n.start == (p, i) {
                    n.start.1 += 1;
                }
                if n.end == (p, i) {
                    n.end.1 += 1;
                }
                if n.start != r.start || n.end != r.end {
                    self.set_range(h, n);
                }
            }
        }
        let original_parent = self.ec_parent(list[0])?;
        if self.ec_before(new_parent, list[0]) {
            let last_visible_child = self
                .ec_children(new_parent)
                .into_iter()
                .filter(|&c| self.ec_visible(c))
                .last();
            let first_visible = list.iter().copied().find(|&n| self.ec_visible(n));
            let last_child = self.dom().last_child(new_parent);
            if !self.ec_inline(new_parent)
                && self.ec_inline_o(last_visible_child)
                && self.ec_inline_o(first_visible)
                && !self.ec_is_o(last_child, &["br"])
            {
                let doc = self.dom().node_document(new_parent);
                let br = self.ec_create(doc, "br");
                self.ec_append(cx, new_parent, br);
            }
            for &n in &list {
                let len = self.dom().child_count(new_parent) as u32;
                self.ec_move(cx, n, new_parent, len);
            }
        } else {
            let first_visible_child = self
                .ec_children(new_parent)
                .into_iter()
                .find(|&c| self.ec_visible(c));
            let last_visible = list.iter().copied().rev().find(|&n| self.ec_visible(n));
            if !self.ec_inline(new_parent)
                && self.ec_inline_o(first_visible_child)
                && self.ec_inline_o(last_visible)
                && !self.ec_is(*list.last()?, &["br"])
            {
                let doc = self.dom().node_document(new_parent);
                let br = self.ec_create(doc, "br");
                let first = self.dom().first_child(new_parent);
                self.ec_insert(cx, new_parent, br, first);
            }
            for &n in list.iter().rev() {
                self.ec_move(cx, n, new_parent, 0);
            }
        }
        if self.ec_editable(original_parent) && !self.dom().has_child_nodes(original_parent) {
            self.ec_remove(cx, original_parent);
        }
        let next = self.dom().next_sibling(new_parent);
        if let Some(next) = next.filter(|&n| self.ec_editable(n) && sibling_criteria(self, n)) {
            let last_child = self.dom().last_child(new_parent);
            let next_first = self.dom().first_child(next);
            if !self.ec_inline(new_parent)
                && self.ec_inline_o(last_child)
                && self.ec_inline_o(next_first)
                && !self.ec_is_o(last_child, &["br"])
            {
                let doc = self.dom().node_document(new_parent);
                let br = self.ec_create(doc, "br");
                self.ec_append(cx, new_parent, br);
            }
            while let Some(c) = self.ec_first_child(next) {
                let len = self.dom().child_count(new_parent) as u32;
                self.ec_move(cx, c, new_parent, len);
            }
            self.ec_remove(cx, next);
        }
        self.ec_remove_extraneous_line_breaks_from(cx, new_parent);
        Some(new_parent)
    }

    // ----- split the parent -------------------------------------------------------------------------------

    /// "Split the parent" of a list of consecutive siblings: they move out of their parent, which
    /// is cloned for the siblings before them.
    pub(super) fn ec_split_parent(&self, cx: &mut Cx, list: &[NodeId]) {
        let Some(&first) = list.first() else {
            return;
        };
        let last = *list.last().unwrap();
        let Some(original_parent) = self.ec_parent(first) else {
            return;
        };
        if !self.ec_editable(original_parent) || self.ec_parent(original_parent).is_none() {
            return;
        }
        let in_list = |n: Option<NodeId>| n.is_some_and(|n| list.contains(&n));
        let first_in = in_list(self.dom().first_child(original_parent));
        if first_in {
            self.ec_remove_extraneous_line_breaks_before(cx, original_parent);
        }
        let follows = first_in && self.ec_follows_line_break(original_parent);
        let precedes = in_list(self.dom().last_child(original_parent))
            && self.ec_precedes_line_break(original_parent);
        let first_in = in_list(self.dom().first_child(original_parent));
        let last_in = in_list(self.dom().last_child(original_parent));
        let doc = self.dom().node_document(first);
        if !first_in && last_in {
            for &n in list.iter().rev() {
                let Some(gp) = self.ec_parent(original_parent) else {
                    return;
                };
                let i = 1 + self.ec_index(original_parent);
                self.ec_move(cx, n, gp, i);
            }
            if precedes && !self.ec_precedes_line_break(last) {
                let br = self.ec_create(doc, "br");
                self.ec_insert_after(cx, last, br);
            }
            self.ec_remove_extraneous_line_breaks_at_end(cx, original_parent);
            return;
        }
        if !first_in {
            let Ok(cloned) = self.dom_mut().clone_node(original_parent, false) else {
                return;
            };
            self.ec_remove_attribute(cx, original_parent, "id");
            let Some(gp) = self.ec_parent(original_parent) else {
                return;
            };
            self.ec_insert(cx, gp, cloned, Some(original_parent));
            while self.dom().previous_sibling(first).is_some() {
                let Some(c) = self.dom().first_child(original_parent) else {
                    break;
                };
                let n = self.dom().child_count(cloned) as u32;
                self.ec_move(cx, c, cloned, n);
            }
        }
        for &n in list {
            let Some(gp) = self.ec_parent(original_parent) else {
                return;
            };
            let i = self.ec_index(original_parent);
            self.ec_move(cx, n, gp, i);
        }
        if follows && !self.ec_follows_line_break(first) {
            let br = self.ec_create(doc, "br");
            if let Some(p) = self.ec_parent(first) {
                self.ec_insert(cx, p, br, Some(first));
            }
        }
        let op_first = self.dom().first_child(original_parent);
        if self.ec_inline(last)
            && !self.ec_is(last, &["br"])
            && self.ec_is_o(op_first, &["br"])
            && !self.ec_inline(original_parent)
        {
            if let Some(c) = op_first {
                self.ec_remove(cx, c);
            }
        }
        if !self.dom().has_child_nodes(original_parent) {
            self.ec_remove(cx, original_parent);
            if precedes && !self.ec_precedes_line_break(last) {
                let br = self.ec_create(doc, "br");
                self.ec_insert_after(cx, last, br);
            }
        } else {
            self.ec_remove_extraneous_line_breaks_before(cx, original_parent);
        }
        if self.dom().next_sibling(last).is_none() {
            if let Some(p) = self.ec_parent(last) {
                self.ec_remove_extraneous_line_breaks_at_end(cx, p);
            }
        }
    }

    /// "Remove a node while preserving its descendants".
    pub(super) fn ec_remove_preserving_descendants(&self, cx: &mut Cx, node: NodeId) {
        let children = self.ec_children(node);
        if children.is_empty() {
            self.ec_remove(cx, node);
        } else {
            self.ec_split_parent(cx, &children);
        }
    }

    // ----- fixing the tree --------------------------------------------------------------------------------

    /// "Fix disallowed ancestors" of a node.
    pub(super) fn ec_fix_disallowed_ancestors(&self, cx: &mut Cx, doc: NodeId, node: NodeId) {
        if !self.ec_editable(node) {
            return;
        }
        if self.ec_disallowed_everywhere(node) {
            if self.ec_is(node, &["dd", "dt"]) {
                self.ec_wrap(
                    cx,
                    vec![node],
                    &|e, s| e.ec_is(s, &["dl"]) && !e.dom().has_attributes(s),
                    &|e, _| Some(e.ec_create(doc, "dl")),
                );
                return;
            }
            let host = self.ec_host_of(node);
            if !host.is_some_and(|h| self.ec_allowed_child(Item::Name("p"), Item::Node(h))) {
                return;
            }
            if !self.ec_prohibited_paragraph_child(Some(node)) {
                return;
            }
            let name = self.ec_default_single_line_container(doc);
            let node = self.ec_set_tag_name(cx, node, name);
            self.ec_fix_disallowed_ancestors(cx, doc, node);
            for child in self.ec_children(node) {
                if self.ec_prohibited_paragraph_child(Some(child)) {
                    let values = self.ec_record_values(&[child]);
                    self.ec_split_parent(cx, &[child]);
                    self.ec_restore_values(cx, &values);
                }
            }
            return;
        }
        let values = self.ec_record_values(&[node]);
        while let Some(p) = self.ec_parent(node) {
            if self.ec_allowed_child(Item::Node(node), Item::Node(p)) {
                break;
            }
            let before = self.ec_parent(node);
            self.ec_split_parent(cx, &[node]);
            if self.ec_parent(node) == before {
                break;
            }
        }
        self.ec_restore_values(cx, &values);
    }

    /// "Normalize sublists" of an `li`: its `ol` and `ul` children (and what follows them) move out
    /// after it.
    pub(super) fn ec_normalize_sublists(&self, cx: &mut Cx, item: NodeId) {
        if !self.ec_is(item, &["li"])
            || !self.ec_editable(item)
            || !self.ec_editable_o(self.ec_parent(item))
        {
            return;
        }
        let doc = self.dom().node_document(item);
        let mut new_item: Option<NodeId> = None;
        while self
            .ec_children(item)
            .into_iter()
            .any(|c| self.ec_is(c, &["ol", "ul"]))
        {
            let Some(child) = self.dom().last_child(item) else {
                break;
            };
            let space_text = {
                let dom = self.dom();
                dom.is_text(child)
                    && dom
                        .character_data(child)
                        .unwrap_or("")
                        .chars()
                        .all(|c| matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r'))
            };
            let Some(parent) = self.ec_parent(item) else {
                break;
            };
            if self.ec_is(child, &["ol", "ul"]) || (new_item.is_none() && space_text) {
                new_item = None;
                let i = 1 + self.ec_index(item);
                self.ec_move(cx, child, parent, i);
            } else {
                let ni = match new_item {
                    Some(n) => n,
                    None => {
                        let n = self.ec_create(doc, "li");
                        self.ec_insert_after(cx, item, n);
                        new_item = Some(n);
                        n
                    }
                };
                self.ec_move(cx, child, ni, 0);
            }
        }
    }
}
