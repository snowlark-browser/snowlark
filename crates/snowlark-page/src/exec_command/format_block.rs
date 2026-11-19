// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs §7 "The formatBlock command": the action, the value and the indeterminacy.

use snowlark_bindings::rt::Cx;
use snowlark_dom::NodeId;

use super::nodes::{Item, PROHIBITED_PARAGRAPH_CHILD};
use crate::engine::Engine;

/// "A formattable block name".
const FORMATTABLE_BLOCK_NAMES: &[&str] = &[
    "address", "dd", "div", "dt", "h1", "h2", "h3", "h4", "h5", "h6", "p", "pre",
];

/// "A non-list single-line container" name.
const NON_LIST_SINGLE_LINE_CONTAINER: &[&str] = &[
    "address", "div", "h1", "h2", "h3", "h4", "h5", "h6", "listing", "p", "pre", "xmp",
];

impl Engine {
    /// Whether `n` is the ancestor of a prohibited paragraph child.
    fn ec_holds_prohibited_paragraph_child(&self, n: NodeId) -> bool {
        self.ec_some_descendant(n, |e, d| e.ec_is(d, PROHIBITED_PARAGRAPH_CHILD))
    }

    /// The formattable block a visible leaf is in: its nearest inclusive ancestor in the same
    /// editing host that is an HTML element with a formattable block name, if that one is
    /// editable and holds no prohibited paragraph child.
    fn ec_format_block_of(&self, leaf: NodeId) -> Option<String> {
        let mut node = leaf;
        while let Some(p) = self.ec_parent(node) {
            if !self.ec_editable(p)
                || !self.ec_same_host(node, Some(p))
                || self.ec_is(node, FORMATTABLE_BLOCK_NAMES)
            {
                break;
            }
            node = p;
        }
        (self.ec_editable(node)
            && self.ec_is(node, FORMATTABLE_BLOCK_NAMES)
            && !self.ec_holds_prohibited_paragraph_child(node))
        .then(|| self.ec_name(node))
        .flatten()
    }

    /// The visible editable childless nodes contained in the block-extended active range.
    fn ec_format_block_leaves(&self, doc: NodeId) -> Option<Vec<NodeId>> {
        let r = self.ec_block_extend(self.ec_active(doc)?);
        Some(self.ec_contained_nodes(r, true, |e, n| {
            e.ec_visible(n) && e.ec_editable(n) && !e.dom().has_child_nodes(n)
        }))
    }

    /// `queryCommandIndeterm("formatBlock")`.
    pub(super) fn ec_format_block_indeterm(&self, doc: NodeId) -> bool {
        let Some(leaves) = self.ec_format_block_leaves(doc) else {
            return false;
        };
        let mut kind: Option<String> = None;
        for leaf in leaves {
            let current = self.ec_format_block_of(leaf).unwrap_or_default();
            match &kind {
                None => kind = Some(current),
                Some(k) if *k != current => return true,
                Some(_) => {}
            }
        }
        false
    }

    /// `queryCommandValue("formatBlock")`.
    pub(super) fn ec_format_block_value(&self, doc: NodeId) -> String {
        self.ec_format_block_leaves(doc)
            .and_then(|l| l.first().copied())
            .and_then(|leaf| self.ec_format_block_of(leaf))
            .unwrap_or_default()
    }

    /// The `formatBlock` command's action.
    pub(super) fn ec_format_block(&self, cx: &mut Cx, doc: NodeId, value: &str) -> bool {
        let value = if value.len() >= 2 && value.starts_with('<') && value.ends_with('>') {
            &value[1..value.len() - 1]
        } else {
            value
        };
        let value = value.to_ascii_lowercase();
        let Some(&value) = FORMATTABLE_BLOCK_NAMES.iter().find(|&&n| n == value) else {
            return false;
        };
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        let r = self.ec_block_extend(r);
        let mut list = self.ec_contained_nodes(r, false, |e, n| {
            e.ec_editable(n)
                && (e.ec_is(n, NON_LIST_SINGLE_LINE_CONTAINER)
                    || e.ec_allowed_child(Item::Node(n), Item::Name("p"))
                    || e.ec_is(n, &["dd", "dt"]))
                && !e.ec_holds_prohibited_paragraph_child(n)
        });
        let values = self.ec_record_values(&list);
        for &node in &list {
            loop {
                let ancestors: Vec<NodeId> = self.dom().ancestors(node).collect();
                let inside = ancestors.into_iter().any(|a| {
                    self.ec_editable(a)
                        && self.ec_same_host(a, Some(node))
                        && self.ec_is(a, FORMATTABLE_BLOCK_NAMES)
                        && !self.ec_holds_prohibited_paragraph_child(a)
                });
                if !inside {
                    break;
                }
                let before = self.ec_parent(node);
                self.ec_split_parent(cx, &[node]);
                if self.ec_parent(node) == before {
                    break;
                }
            }
        }
        self.ec_restore_values(cx, &values);
        list.reverse();
        while let Some(first) = list.pop() {
            let sublist = if self.ec_single_line_container(first) {
                let children = self.ec_children(first);
                let values = self.ec_record_values(&children);
                self.ec_remove_preserving_descendants(cx, first);
                self.ec_restore_values(cx, &values);
                children
            } else {
                let mut sublist = vec![first];
                while let Some(&next) = list.last() {
                    let last = *sublist.last().unwrap();
                    if self.ec_next_sibling(last) == Some(next)
                        && !self.ec_single_line_container(next)
                        && !self.ec_is(last, &["br"])
                    {
                        sublist.push(next);
                        list.pop();
                    } else {
                        break;
                    }
                }
                sublist
            };
            if sublist.is_empty() {
                continue;
            }
            let plain = value == "div" || value == "p";
            let new_parent = self.ec_wrap(
                cx,
                sublist,
                &|e, n| !plain && e.ec_is(n, &[value]) && !e.dom().has_attributes(n),
                &|e, _| Some(e.ec_create(doc, value)),
            );
            if let Some(p) = new_parent {
                self.ec_fix_disallowed_ancestors(cx, doc, p);
            }
        }
        true
    }
}
