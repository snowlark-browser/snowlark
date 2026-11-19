// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs: the active range and the selection the commands move, "move preserving
//! ranges", contained nodes, equivalent points, block-extending a range and the line break tests
//! ("follows a line break", "precedes a line break").

use std::cmp::Ordering;

use snowlark_bindings::rt::{Cx, Handle};
use snowlark_dom::NodeId;

use crate::engine::ranges::{bp_position, BoundaryPair, Bp};
use crate::engine::selection::Direction;
use crate::engine::{Engine, Object};

impl Engine {
    // ----- the active range -------------------------------------------------------------------------------

    /// The selection's range object of `doc`.
    pub(super) fn ec_range_handle(&self, doc: NodeId) -> Option<Handle> {
        let h = self.selection_of(doc)?;
        let obj = self.obj(h);
        match &*obj {
            Object::Selection(s) => s.range,
            _ => None,
        }
    }

    /// "The active range": the range of the document's selection.
    pub(crate) fn ec_active(&self, doc: NodeId) -> Option<BoundaryPair> {
        self.ec_range_handle(doc).map(|h| self.range_bounds(h))
    }

    /// Selects from `anchor` to `focus` (Selection `collapse` then `extend`), updating the
    /// selection's range in place.
    pub(crate) fn ec_select(&self, doc: NodeId, anchor: Bp, focus: Bp) {
        let Some(range) = self.ec_range_handle(doc) else {
            self.set_doc_selection(doc, Some((anchor, focus)));
            return;
        };
        let ord = bp_position(&self.dom(), anchor, focus);
        let (r, d) = match ord {
            Ordering::Greater => (
                BoundaryPair {
                    start: focus,
                    end: anchor,
                },
                Direction::Backward,
            ),
            Ordering::Less => (
                BoundaryPair {
                    start: anchor,
                    end: focus,
                },
                Direction::Forward,
            ),
            Ordering::Equal => (
                BoundaryPair {
                    start: anchor,
                    end: focus,
                },
                Direction::None,
            ),
        };
        self.set_range(range, r);
        if let Some(h) = self.selection_of(doc) {
            if let Object::Selection(s) = &mut *self.obj_mut(h) {
                s.direction = d;
            }
        }
    }

    /// Selection `collapse(node, offset)`.
    pub(crate) fn ec_collapse(&self, doc: NodeId, bp: Bp) {
        self.ec_select(doc, bp, bp);
    }

    /// Selection `collapseToStart()` (`start` true) or `collapseToEnd()`.
    pub(super) fn ec_collapse_to(&self, doc: NodeId, start: bool) {
        if let Some(r) = self.ec_active(doc) {
            self.ec_collapse(doc, if start { r.start } else { r.end });
        }
    }

    // ----- tree helpers -----------------------------------------------------------------------------------

    pub(super) fn ec_len(&self, n: NodeId) -> u32 {
        self.dom().length(n) as u32
    }

    pub(super) fn ec_index(&self, n: NodeId) -> u32 {
        self.dom().index(n) as u32
    }

    pub(super) fn ec_parent(&self, n: NodeId) -> Option<NodeId> {
        self.dom().parent(n)
    }

    pub(super) fn ec_child(&self, n: NodeId, i: u32) -> Option<NodeId> {
        self.dom().child_at(n, i as usize)
    }

    /// The child of `n` at `offset - 1`.
    pub(super) fn ec_child_before(&self, n: NodeId, offset: u32) -> Option<NodeId> {
        offset.checked_sub(1).and_then(|i| self.ec_child(n, i))
    }

    pub(super) fn ec_first_child(&self, n: NodeId) -> Option<NodeId> {
        self.dom().first_child(n)
    }

    pub(super) fn ec_next_sibling(&self, n: NodeId) -> Option<NodeId> {
        self.dom().next_sibling(n)
    }

    pub(super) fn ec_children(&self, n: NodeId) -> Vec<NodeId> {
        self.dom().children(n).collect()
    }

    /// Whether `a` is before `b` in tree order.
    pub(super) fn ec_before(&self, a: NodeId, b: NodeId) -> bool {
        a != b && self.dom().tree_order(a, b) == Ordering::Less
    }

    pub(super) fn ec_bp_cmp(&self, a: Bp, b: Bp) -> Ordering {
        bp_position(&self.dom(), a, b)
    }

    // ----- moving nodes -----------------------------------------------------------------------------------

    /// "Move preserving ranges": `node` moves to (`new_parent`, `new_index`); every live range's
    /// boundary points follow the editing spec's rules instead of the removal and insertion ones.
    pub(super) fn ec_move(&self, cx: &mut Cx, node: NodeId, new_parent: NodeId, new_index: u32) {
        let (old_parent, old_index, before) = {
            let dom = self.dom();
            (
                dom.parent(node),
                dom.index(node) as i64,
                dom.child_at(new_parent, new_index as usize),
            )
        };
        let new_index = new_index as i64;
        let fix = |bp: &mut Bp| {
            let (mut n, mut o) = (bp.0, bp.1 as i64);
            if n == new_parent && o > new_index {
                o += 1;
            }
            if Some(n) == old_parent && (o == old_index || o == old_index + 1) {
                n = new_parent;
                o += new_index - old_index;
            }
            if Some(n) == old_parent && o > old_index + 1 {
                o -= 1;
            }
            *bp = (n, o.max(0) as u32);
        };
        let updates: Vec<(Handle, BoundaryPair)> = {
            let ranges = self.ranges.borrow();
            ranges
                .iter()
                .filter_map(|&h| match &*self.obj(h) {
                    Object::Range(r) => {
                        let mut r = *r;
                        fix(&mut r.start);
                        fix(&mut r.end);
                        Some((h, r))
                    }
                    _ => None,
                })
                .collect()
        };
        let _ = self.dom_mut().insert_before(new_parent, node, before);
        self.after_mutation(cx);
        for (h, r) in updates {
            self.set_range(h, r);
        }
    }

    // ----- contained nodes --------------------------------------------------------------------------------

    /// DOM "contained" in a range.
    pub(super) fn ec_contained(&self, n: NodeId, r: BoundaryPair) -> bool {
        let dom = self.dom();
        dom.root(n) == dom.root(r.start.0)
            && bp_position(&dom, (n, 0), r.start) == Ordering::Greater
            && bp_position(&dom, (n, dom.length(n) as u32), r.end) == Ordering::Less
    }

    /// The nodes contained in `r` for which `cond` holds, in tree order; with `all` false, none whose
    /// ancestor is already in the list.
    pub(super) fn ec_contained_nodes(
        &self,
        r: BoundaryPair,
        all: bool,
        cond: impl Fn(&Engine, NodeId) -> bool,
    ) -> Vec<NodeId> {
        let (mut node, stop) = {
            let dom = self.dom();
            let (s, so) = r.start;
            let node = if dom.has_child_nodes(s) && (so as usize) < dom.child_count(s) {
                dom.child_at(s, so as usize)
            } else if so as usize == dom.length(s) {
                dom.next_skipping_children(s, None)
            } else {
                dom.next_in_tree_order(s, None)
            };
            let (e, eo) = r.end;
            let stop = if dom.has_child_nodes(e) && (eo as usize) < dom.child_count(e) {
                dom.child_at(e, eo as usize)
            } else {
                dom.next_skipping_children(e, None)
            };
            (node, stop)
        };
        let mut out = Vec::new();
        while let Some(n) = node {
            if let Some(s) = stop {
                if !self.ec_before(n, s) {
                    break;
                }
            }
            if self.ec_contained(n, r) && cond(self, n) {
                out.push(n);
                node = if all {
                    self.dom().next_in_tree_order(n, None)
                } else {
                    self.dom().next_skipping_children(n, None)
                };
                continue;
            }
            node = self.dom().next_in_tree_order(n, None);
        }
        out
    }

    // ----- equivalent points ------------------------------------------------------------------------------

    fn ec_next_equivalent_point(&self, (node, offset): Bp) -> Option<Bp> {
        if self.ec_len(node) == 0 {
            return None;
        }
        if offset == self.ec_len(node) {
            if let Some(p) = self.ec_parent(node) {
                if self.ec_inline(node) {
                    return Some((p, 1 + self.ec_index(node)));
                }
            }
        }
        let child = self.ec_child(node, offset)?;
        (self.ec_len(child) != 0 && self.ec_inline(child)).then_some((child, 0))
    }

    fn ec_previous_equivalent_point(&self, (node, offset): Bp) -> Option<Bp> {
        if self.ec_len(node) == 0 {
            return None;
        }
        if offset == 0 {
            if let Some(p) = self.ec_parent(node) {
                if self.ec_inline(node) {
                    return Some((p, self.ec_index(node)));
                }
            }
        }
        let child = self.ec_child_before(node, offset)?;
        let len = self.ec_len(child);
        (len != 0 && self.ec_inline(child)).then_some((child, len))
    }

    /// "The first equivalent point".
    pub(super) fn ec_first_equivalent_point(&self, mut bp: Bp) -> Bp {
        while let Some(p) = self.ec_previous_equivalent_point(bp) {
            bp = p;
        }
        bp
    }

    /// "The last equivalent point".
    pub(super) fn ec_last_equivalent_point(&self, mut bp: Bp) -> Bp {
        while let Some(p) = self.ec_next_equivalent_point(bp) {
            bp = p;
        }
        bp
    }

    // ----- block boundary points --------------------------------------------------------------------------

    /// "A block start point".
    pub(super) fn ec_block_start_point(&self, (node, offset): Bp) -> bool {
        if self.ec_parent(node).is_none() && offset == 0 {
            return true;
        }
        self.ec_child_before(node, offset)
            .is_some_and(|c| self.ec_visible(c) && (self.ec_block(c) || self.ec_is(c, &["br"])))
    }

    /// "A block end point".
    pub(super) fn ec_block_end_point(&self, (node, offset): Bp) -> bool {
        if self.ec_parent(node).is_none() && offset == self.ec_len(node) {
            return true;
        }
        self.ec_child(node, offset)
            .is_some_and(|c| self.ec_visible(c) && self.ec_block(c))
    }

    /// "A block boundary point".
    pub(super) fn ec_block_boundary_point(&self, bp: Bp) -> bool {
        self.ec_block_start_point(bp) || self.ec_block_end_point(bp)
    }

    /// "Block-extend" a range.
    pub(super) fn ec_block_extend(&self, r: BoundaryPair) -> BoundaryPair {
        let (mut start_node, mut start_offset) = r.start;
        let (mut end_node, mut end_offset) = r.end;
        let last_li = |n: NodeId| -> Option<NodeId> {
            let chain: Vec<NodeId> = self.dom().inclusive_ancestors(n).collect();
            chain.into_iter().find(|&a| self.ec_is(a, &["li"]))
        };
        if let Some(li) = last_li(start_node) {
            if let Some(p) = self.ec_parent(li) {
                start_offset = self.ec_index(li);
                start_node = p;
            }
        }
        if !self.ec_block_start_point((start_node, start_offset)) {
            loop {
                if start_offset == 0 {
                    let Some(p) = self.ec_parent(start_node) else {
                        break;
                    };
                    start_offset = self.ec_index(start_node);
                    start_node = p;
                } else {
                    start_offset -= 1;
                }
                if self.ec_block_boundary_point((start_node, start_offset)) {
                    break;
                }
            }
        }
        while start_offset == 0 {
            let Some(p) = self.ec_parent(start_node) else {
                break;
            };
            start_offset = self.ec_index(start_node);
            start_node = p;
        }
        if let Some(li) = last_li(end_node) {
            if let Some(p) = self.ec_parent(li) {
                end_offset = 1 + self.ec_index(li);
                end_node = p;
            }
        }
        if !self.ec_block_end_point((end_node, end_offset)) {
            loop {
                if end_offset == self.ec_len(end_node) {
                    let Some(p) = self.ec_parent(end_node) else {
                        break;
                    };
                    end_offset = 1 + self.ec_index(end_node);
                    end_node = p;
                } else {
                    end_offset += 1;
                }
                if self.ec_block_boundary_point((end_node, end_offset)) {
                    break;
                }
            }
        }
        while end_offset == self.ec_len(end_node) {
            let Some(p) = self.ec_parent(end_node) else {
                break;
            };
            end_offset = 1 + self.ec_index(end_node);
            end_node = p;
        }
        BoundaryPair {
            start: (start_node, start_offset),
            end: (end_node, end_offset),
        }
    }

    /// "Follows a line break".
    pub(super) fn ec_follows_line_break(&self, node: NodeId) -> bool {
        let (mut node, mut offset) = (node, 0u32);
        while !self.ec_block_boundary_point((node, offset)) {
            if self
                .ec_child_before(node, offset)
                .is_some_and(|c| self.ec_visible(c))
            {
                return false;
            }
            if offset == 0 || !self.dom().has_child_nodes(node) {
                let Some(p) = self.ec_parent(node) else {
                    return true;
                };
                offset = self.ec_index(node);
                node = p;
            } else {
                let Some(c) = self.ec_child_before(node, offset) else {
                    return true;
                };
                node = c;
                offset = self.ec_len(node);
            }
        }
        true
    }

    /// "Precedes a line break".
    pub(super) fn ec_precedes_line_break(&self, node: NodeId) -> bool {
        let mut node = node;
        let mut offset = self.ec_len(node);
        while !self.ec_block_boundary_point((node, offset)) {
            if self
                .ec_child(node, offset)
                .is_some_and(|c| self.ec_visible(c))
            {
                return false;
            }
            if offset == self.ec_len(node) || !self.dom().has_child_nodes(node) {
                let Some(p) = self.ec_parent(node) else {
                    return true;
                };
                offset = 1 + self.ec_index(node);
                node = p;
            } else {
                let Some(c) = self.ec_child(node, offset) else {
                    return true;
                };
                node = c;
                offset = 0;
            }
        }
        true
    }
}
