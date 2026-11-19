// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs §7 "Block formatting commands": "indent" and "outdent" a node list, "toggle
//! lists" and "the selection's list state", and the commands built on them: `indent`, `outdent`,
//! `insertOrderedList` and `insertUnorderedList`.

use snowlark_bindings::rt::Cx;
use snowlark_dom::{ns, NodeId};

use super::nodes::Item;
use crate::engine::ranges::BoundaryPair;
use crate::engine::Engine;

/// "The selection's list state".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ListState {
    None,
    Ol,
    Ul,
    Mixed,
    MixedOl,
    MixedUl,
}

impl Engine {
    // ----- definitions ------------------------------------------------------------------------------------

    /// "A simple indentation element": an indentation element whose only attributes are a `style`
    /// that sets nothing but `margin`, `border` and `padding` (and their longhands), and `dir`.
    pub(super) fn ec_simple_indentation_element(&self, n: NodeId) -> bool {
        if !self.ec_indentation_element(n) {
            return false;
        }
        let attrs_ok = {
            let dom = self.dom();
            dom.attrs(n)
                .iter()
                .all(|a| a.namespace() == &ns!() && matches!(&**a.local_name(), "style" | "dir"))
        };
        attrs_ok
            && self.ec_style_longhands(n).iter().all(|p| {
                p.starts_with("margin") || p.starts_with("border") || p.starts_with("padding")
            })
    }

    /// Whether a descendant of `n` satisfies `cond` (tree order, stopping at the first).
    pub(super) fn ec_some_descendant(
        &self,
        n: NodeId,
        cond: impl Fn(&Engine, NodeId) -> bool,
    ) -> bool {
        let mut cur = self.dom().next_in_tree_order(n, Some(n));
        while let Some(d) = cur {
            if cond(self, d) {
                return true;
            }
            cur = self.dom().next_in_tree_order(d, Some(n));
        }
        false
    }

    /// The `li` ancestors of the range's start and end the list commands normalize, in tree order:
    /// those of the end node below the common ancestor, then every one of the start node's
    /// inclusive ancestors.
    fn ec_list_items(&self, r: BoundaryPair) -> Vec<NodeId> {
        let common = {
            let dom = self.dom();
            dom.inclusive_ancestors(r.start.0)
                .find(|&a| dom.is_inclusive_ancestor_of(a, r.end.0))
        };
        let mut items = Vec::new();
        let end_chain: Vec<NodeId> = self.dom().inclusive_ancestors(r.end.0).collect();
        for a in end_chain {
            if Some(a) == common {
                break;
            }
            if self.ec_is(a, &["li"]) {
                items.insert(0, a);
            }
        }
        let start_chain: Vec<NodeId> = self.dom().inclusive_ancestors(r.start.0).collect();
        for a in start_chain {
            if self.ec_is(a, &["li"]) {
                items.insert(0, a);
            }
        }
        items
    }

    // ----- indenting and outdenting -----------------------------------------------------------------------

    /// "Indent" a list of consecutive siblings.
    pub(super) fn ec_indent_nodes(&self, cx: &mut Cx, doc: NodeId, list: Vec<NodeId>) {
        let Some(&first) = list.first() else {
            return;
        };
        let tag = self
            .ec_parent(first)
            .filter(|&p| self.ec_is(p, &["ol", "ul"]))
            .and_then(|p| self.ec_name(p));
        if let Some(tag) = tag {
            let tag = tag.as_str();
            self.ec_wrap(cx, list, &|e, n| e.ec_is(n, &[tag]), &|e, _| {
                Some(e.ec_create(doc, tag))
            });
            return;
        }
        let new_parent = self.ec_wrap(
            cx,
            list,
            &|e, n| e.ec_simple_indentation_element(n),
            &|e, _| Some(e.ec_create(doc, "blockquote")),
        );
        if let Some(p) = new_parent {
            self.ec_fix_disallowed_ancestors(cx, doc, p);
        }
    }

    /// "Outdent" a node.
    pub(super) fn ec_outdent_node(&self, cx: &mut Cx, doc: NodeId, node: NodeId) {
        if !self.ec_editable(node) {
            return;
        }
        if self.ec_simple_indentation_element(node) {
            self.ec_remove_preserving_descendants(cx, node);
            return;
        }
        if self.ec_indentation_element(node) {
            self.ec_remove_attribute(cx, node, "dir");
            for p in ["margin", "padding", "border"] {
                if self.dom().has_attribute(node, "style") {
                    self.ec_set_style(cx, node, p, "");
                }
            }
            self.ec_set_tag_name(cx, node, "div");
            return;
        }
        let climb = |simple: bool| -> (Option<NodeId>, Vec<NodeId>) {
            let mut list = Vec::new();
            let mut cur = self.ec_parent(node);
            while let Some(c) = cur {
                let stop = !self.ec_editable(c)
                    || !self.dom().is_element(c)
                    || (if simple {
                        self.ec_simple_indentation_element(c)
                    } else {
                        self.ec_indentation_element(c)
                    })
                    || self.ec_is(c, &["ol", "ul"]);
                if stop {
                    break;
                }
                list.push(c);
                cur = self.ec_parent(c);
            }
            (cur, list)
        };
        let (mut current, mut ancestors) = climb(true);
        if !current.is_some_and(|c| self.ec_editable(c) && self.ec_simple_indentation_element(c)) {
            (current, ancestors) = climb(false);
        }
        let current_indents =
            current.is_some_and(|c| self.ec_editable(c) && self.ec_indentation_element(c));
        if self.ec_is(node, &["ol", "ul"]) && !current_indents {
            for a in ["reversed", "start", "type"] {
                self.ec_remove_attribute(cx, node, a);
            }
            let children = self.ec_children(node);
            let parent_is_list = self.ec_is_o(self.ec_parent(node), &["ol", "ul"]);
            if self.dom().has_attributes(node) && !parent_is_list {
                self.ec_set_tag_name(cx, node, "div");
            } else {
                let values = self.ec_record_values(&children);
                self.ec_remove_preserving_descendants(cx, node);
                self.ec_restore_values(cx, &values);
            }
            for c in children {
                self.ec_fix_disallowed_ancestors(cx, doc, c);
            }
            return;
        }
        if !current_indents {
            return;
        }
        let Some(original) = current else {
            return;
        };
        ancestors.push(original);
        while let Some(current) = ancestors.pop() {
            let target = if self.ec_parent(node) == Some(current) {
                node
            } else {
                match ancestors.last() {
                    Some(&t) => t,
                    None => break,
                }
            };
            let next = self.ec_next_sibling(target);
            if self.ec_inline(target) && !self.ec_is(target, &["br"]) && self.ec_is_o(next, &["br"])
            {
                if let Some(br) = next {
                    self.ec_remove(cx, br);
                }
            }
            let children = self.ec_children(current);
            let i = children.iter().position(|&c| c == target).unwrap_or(0);
            let preceding = children[..i].to_vec();
            let following = children
                .get(i + 1..)
                .map(<[NodeId]>::to_vec)
                .unwrap_or_default();
            self.ec_indent_nodes(cx, doc, preceding);
            self.ec_indent_nodes(cx, doc, following);
        }
        self.ec_outdent_node(cx, doc, original);
    }

    // ----- the indent and outdent commands ----------------------------------------------------------------

    /// The `indent` command's action.
    pub(super) fn ec_indent(&self, cx: &mut Cx, doc: NodeId) -> bool {
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        for item in self.ec_list_items(r) {
            self.ec_normalize_sublists(cx, item);
        }
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        let r = self.ec_block_extend(r);
        let mut list = self.ec_contained_nodes(r, false, |e, n| {
            e.ec_editable(n)
                && (e.ec_allowed_child(Item::Node(n), Item::Name("div"))
                    || e.ec_allowed_child(Item::Node(n), Item::Name("ol")))
        });
        let first_visible = list.iter().copied().find(|&n| self.ec_visible(n));
        if let Some(fv) = first_visible
            .filter(|&n| self.ec_is(n, &["li"]) && self.ec_is_o(self.ec_parent(n), &["ol", "ul"]))
        {
            let mut sibling = self.dom().previous_sibling(fv);
            while let Some(s) = sibling.filter(|&s| self.ec_invisible(s)) {
                sibling = self.dom().previous_sibling(s);
            }
            if let Some(s) = sibling.filter(|&s| self.ec_is(s, &["li"])) {
                self.ec_normalize_sublists(cx, s);
            }
        }
        list.reverse();
        while let Some(first) = list.pop() {
            let mut sublist = vec![first];
            while let Some(&next) = list.last() {
                if self.ec_next_sibling(*sublist.last().unwrap()) == Some(next) {
                    sublist.push(next);
                    list.pop();
                } else {
                    break;
                }
            }
            self.ec_indent_nodes(cx, doc, sublist);
        }
        true
    }

    /// The `outdent` command's action.
    pub(super) fn ec_outdent(&self, cx: &mut Cx, doc: NodeId) -> bool {
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        for item in self.ec_list_items(r) {
            self.ec_normalize_sublists(cx, item);
        }
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        let r = self.ec_block_extend(r);
        let mut list = self.ec_contained_nodes(r, false, |e, n| {
            e.ec_editable(n)
                && (!e.ec_some_descendant(n, |e, d| e.ec_editable(d))
                    || e.ec_is(n, &["ol", "ul"])
                    || (e.ec_is(n, &["li"]) && e.ec_is_o(e.ec_parent(n), &["ol", "ul"])))
        });
        list.reverse();
        while !list.is_empty() {
            while let Some(&first) = list.last() {
                if self.ec_is(first, &["ol", "ul"])
                    || !self.ec_is_o(self.ec_parent(first), &["ol", "ul"])
                {
                    list.pop();
                    self.ec_outdent_node(cx, doc, first);
                } else {
                    break;
                }
            }
            let Some(first) = list.pop() else {
                break;
            };
            let mut sublist = vec![first];
            while let Some(&next) = list.last() {
                if self.ec_next_sibling(*sublist.last().unwrap()) == Some(next)
                    && !self.ec_is(next, &["ol", "ul"])
                {
                    sublist.push(next);
                    list.pop();
                } else {
                    break;
                }
            }
            let values = self.ec_record_values(&sublist);
            self.ec_split_parent(cx, &sublist);
            for &n in &sublist {
                self.ec_fix_disallowed_ancestors(cx, doc, n);
            }
            self.ec_restore_values(cx, &values);
        }
        true
    }

    // ----- toggling lists ---------------------------------------------------------------------------------

    /// The nodes the list state and toggling lists work on: those contained in the block-extended
    /// range that are editable, not indentation elements, and lists, list children or allowed
    /// children of `li` (none inside another).
    fn ec_list_nodes(&self, r: BoundaryPair) -> Vec<NodeId> {
        self.ec_contained_nodes(r, false, |e, n| {
            e.ec_editable(n)
                && !e.ec_indentation_element(n)
                && (e.ec_is(n, &["ol", "ul"])
                    || e.ec_is_o(e.ec_parent(n), &["ol", "ul"])
                    || e.ec_allowed_child(Item::Node(n), Item::Name("li")))
        })
    }

    /// "The selection's list state".
    pub(super) fn ec_list_state(&self, doc: NodeId) -> ListState {
        let Some(r) = self.ec_active(doc) else {
            return ListState::None;
        };
        let list = self.ec_list_nodes(self.ec_block_extend(r));
        if list.is_empty() {
            return ListState::None;
        }
        let in_list = |n: NodeId, tag: &str| {
            let parent = self.ec_parent(n);
            self.ec_is(n, &[tag])
                || self.ec_is_o(parent, &[tag])
                || (self.ec_is_o(parent, &["li"])
                    && self.ec_is_o(parent.and_then(|p| self.ec_parent(p)), &[tag]))
        };
        let holds = |n: NodeId, tag: &str| {
            self.ec_is(n, &[tag]) || self.ec_some_descendant(n, |e, d| e.ec_is(d, &[tag]))
        };
        if list.iter().all(|&n| in_list(n, "ol")) && !list.iter().any(|&n| holds(n, "ul")) {
            return ListState::Ol;
        }
        if list.iter().all(|&n| in_list(n, "ul")) && !list.iter().any(|&n| holds(n, "ol")) {
            return ListState::Ul;
        }
        let has_ol = list.iter().any(|&n| in_list(n, "ol") || holds(n, "ol"));
        let has_ul = list.iter().any(|&n| in_list(n, "ul") || holds(n, "ul"));
        match (has_ol, has_ul) {
            (true, true) => ListState::Mixed,
            (true, false) => ListState::MixedOl,
            (false, true) => ListState::MixedUl,
            (false, false) => ListState::None,
        }
    }

    /// "Toggle lists" with `tag` (`ol` or `ul`).
    pub(super) fn ec_toggle_lists(&self, cx: &mut Cx, doc: NodeId, tag: &'static str) {
        let enable = self.ec_list_state(doc)
            != if tag == "ol" {
                ListState::Ol
            } else {
                ListState::Ul
            };
        let other: &'static str = if tag == "ol" { "ul" } else { "ol" };
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        for item in self.ec_list_items(r) {
            self.ec_normalize_sublists(cx, item);
        }
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let r = self.ec_block_extend(r);
        // (the block-extended range is live while the other lists are converted)
        let live = self.new_range(doc);
        self.set_range(live, r);
        if enable {
            let lists =
                self.ec_contained_nodes(r, true, |e, n| e.ec_editable(n) && e.ec_is(n, &[other]));
            for list in lists {
                let is_tag = |n: Option<NodeId>| {
                    n.is_some_and(|n| self.ec_editable(n) && self.ec_is(n, &[tag]))
                };
                if is_tag(self.dom().previous_sibling(list))
                    || is_tag(self.dom().next_sibling(list))
                {
                    let children = self.ec_children(list);
                    let values = self.ec_record_values(&children);
                    self.ec_split_parent(cx, &children);
                    self.ec_wrap(cx, children, &|e, n| e.ec_is(n, &[tag]), &|_, _| None);
                    self.ec_restore_values(cx, &values);
                } else {
                    self.ec_set_tag_name(cx, list, tag);
                }
            }
        }
        let r = self.range_bounds(live);
        self.ranges.borrow_mut().retain(|&h| h != live);
        let mut list = self.ec_list_nodes(r);
        if enable {
            list.retain(|&n| {
                !self.ec_is(n, &["ol", "ul"]) || self.ec_is_o(self.ec_parent(n), &["ol", "ul"])
            });
        }
        list.reverse();
        if !enable {
            while let Some(first) = list.pop() {
                if self.ec_is(first, &[tag]) {
                    self.ec_outdent_node(cx, doc, first);
                    continue;
                }
                let mut sublist = vec![first];
                while let Some(&next) = list.last() {
                    if self.ec_next_sibling(*sublist.last().unwrap()) == Some(next)
                        && !self.ec_is(next, &[tag])
                    {
                        sublist.push(next);
                        list.pop();
                    } else {
                        break;
                    }
                }
                let values = self.ec_record_values(&sublist);
                self.ec_split_parent(cx, &sublist);
                for &n in &sublist {
                    self.ec_fix_disallowed_ancestors(cx, doc, n);
                }
                self.ec_restore_values(cx, &values);
            }
            return;
        }
        while !list.is_empty() {
            let mut sublist: Vec<NodeId> = Vec::new();
            while let Some(&next) = list.last() {
                if let Some(&last) = sublist.last() {
                    if self.ec_next_sibling(last) != Some(next) {
                        break;
                    }
                }
                if self.ec_is(next, &["p", "div"]) {
                    list.pop();
                    sublist.push(self.ec_set_tag_name(cx, next, "li"));
                } else if self.ec_is(next, &["li", "ol", "ul"]) {
                    list.pop();
                    sublist.push(next);
                } else {
                    let mut to_wrap = vec![next];
                    list.pop();
                    while let Some(&n) = list.last() {
                        let last = *to_wrap.last().unwrap();
                        if self.ec_next_sibling(last) == Some(n)
                            && self.ec_inline(n)
                            && self.ec_inline(last)
                            && !self.ec_is(last, &["br"])
                        {
                            to_wrap.push(n);
                            list.pop();
                        } else {
                            break;
                        }
                    }
                    match self.ec_wrap(cx, to_wrap, &|_, _| false, &|e, _| {
                        Some(e.ec_create(doc, "li"))
                    }) {
                        Some(li) => sublist.push(li),
                        // (nothing visible to wrap: the run is dropped from this sublist)
                        None => break,
                    }
                }
            }
            let Some(&first) = sublist.first() else {
                continue;
            };
            let parent = self.ec_parent(first);
            if self.ec_is_o(parent, &[tag]) || sublist.iter().all(|&n| self.ec_is(n, &["ol", "ul"]))
            {
                continue;
            }
            if self.ec_is_o(parent, &[other]) {
                let values = self.ec_record_values(&sublist);
                self.ec_split_parent(cx, &sublist);
                self.ec_wrap(cx, sublist, &|e, n| e.ec_is(n, &[tag]), &|e, _| {
                    Some(e.ec_create(doc, tag))
                });
                self.ec_restore_values(cx, &values);
                continue;
            }
            let new_parent = self.ec_wrap(cx, sublist, &|e, n| e.ec_is(n, &[tag]), &|e, cx| {
                let parent = e.ec_parent(first);
                let prev = parent.and_then(|p| e.dom().previous_sibling(p));
                let reuse = parent
                    .is_some_and(|p| e.ec_editable(p) && e.ec_simple_indentation_element(p))
                    && prev.is_some_and(|s| e.ec_editable(s) && e.ec_is(s, &[tag]));
                let Some(list) = prev.filter(|_| reuse) else {
                    return Some(e.ec_create(doc, tag));
                };
                if let Some(last) = e.dom().last_child(list) {
                    e.ec_normalize_sublists(cx, last);
                }
                let last = e.dom().last_child(list);
                if !last.is_some_and(|l| e.ec_editable(l) && e.ec_is(l, &[tag])) {
                    let new = e.ec_create(doc, tag);
                    e.ec_append(cx, list, new);
                }
                e.dom().last_child(list)
            });
            if let Some(p) = new_parent {
                self.ec_fix_disallowed_ancestors(cx, doc, p);
            }
        }
    }
}
