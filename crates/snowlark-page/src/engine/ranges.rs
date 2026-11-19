// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ranges (DOM §5): `AbstractRange`, `StaticRange`, `Range` with boundary point comparison, the
//! contents algorithms (delete, extract, clone), insert and surround, and the live range updates on
//! node insertion and removal, character data changes, `splitText` and `normalize()`.

use std::cmp::Ordering;

use snowlark_bindings::generated::{
    AbstractRangeImpl, RangeImpl, StaticRangeImpl, StaticRangeInit, TrustedHTMLOrString,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};
use snowlark_dom::{local_name, Dom, NodeData, NodeId, NodeType};

use super::{exception, node_handle, DomResultExt, Engine, Object};

/// A boundary point: (node, offset).
pub type Bp = (NodeId, u32);

/// A range's boundary points.
#[derive(Clone, Copy, Debug)]
pub struct BoundaryPair {
    pub start: Bp,
    pub end: Bp,
}

pub type RangeData = BoundaryPair;

fn is_char_data(dom: &Dom, n: NodeId) -> bool {
    dom.is_character_data(n)
}

/// DOM §5.2 "the position of a boundary point relative to another".
pub fn bp_position(dom: &Dom, a: Bp, b: Bp) -> Ordering {
    if a.0 == b.0 {
        return a.1.cmp(&b.1);
    }
    if dom.tree_order(a.0, b.0) == Ordering::Greater {
        return bp_position(dom, b, a).reverse();
    }
    if dom.is_ancestor_of(a.0, b.0) {
        let mut child = b.0;
        while dom.parent(child) != Some(a.0) {
            child = dom.parent(child).expect("an ancestor chain");
        }
        if (dom.index(child) as u32) < a.1 {
            return Ordering::Greater;
        }
    }
    Ordering::Less
}

/// A boundary point in its own tree, then where its shadow host is in each tree up from there:
/// (the tree's root, the position in it).
fn positions_up_the_trees(dom: &Dom, bp: Bp) -> Vec<(NodeId, Bp)> {
    let mut out = vec![(dom.root(bp.0), bp)];
    while let Some(&(root, _)) = out.last() {
        let Some(host) = dom.is_shadow_root(root).then(|| dom.host(root)).flatten() else {
            break;
        };
        let Some(parent) = dom.parent(host) else {
            out.push((host, (host, 0)));
            break;
        };
        out.push((dom.root(parent), (parent, dom.index(host) as u32)));
    }
    out
}

/// The position of a boundary point relative to another that may be in another tree of the
/// document: [`bp_position`] in the nearest tree both are in or under, a shadow tree counting as
/// after the position of its host (shadow-including tree order, DOM §4.8). Equal when no tree has
/// them both.
pub fn composed_position(dom: &Dom, a: Bp, b: Bp) -> Ordering {
    if dom.root(a.0) == dom.root(b.0) {
        return bp_position(dom, a, b);
    }
    let (up_a, up_b) = (
        positions_up_the_trees(dom, a),
        positions_up_the_trees(dom, b),
    );
    for (i, (root, at_a)) in up_a.iter().enumerate() {
        if let Some(j) = up_b.iter().position(|(r, _)| r == root) {
            return bp_position(dom, *at_a, up_b[j].1).then(i.cmp(&j));
        }
    }
    Ordering::Equal
}

/// Where a selection the user makes from `anchor` towards `focus` ends in `anchor`'s tree: a live
/// range has its ends in one tree (DOM §5), so `focus` in a tree under it stops at the near or far
/// edge of the shadow host it is in, and `focus` anywhere else at the edge of `anchor`'s tree.
pub fn focus_in_tree_of(dom: &Dom, anchor: Bp, focus: Bp) -> Bp {
    let root = dom.root(anchor.0);
    if dom.root(focus.0) == root {
        return focus;
    }
    let forward = composed_position(dom, anchor, focus) != Ordering::Greater;
    match positions_up_the_trees(dom, focus)
        .into_iter()
        .find(|(r, _)| *r == root)
    {
        Some((_, (node, offset))) => (node, offset + forward as u32),
        None => (root, if forward { length(dom, root) } else { 0 }),
    }
}

fn length(dom: &Dom, n: NodeId) -> u32 {
    dom.length(n) as u32
}

impl Engine {
    /// A range's boundary points.
    pub(crate) fn range_bounds(&self, h: Handle) -> BoundaryPair {
        self.range(h)
    }

    fn range(&self, h: Handle) -> BoundaryPair {
        match &*self.obj(h) {
            Object::Range(r) | Object::StaticRange(r) => *r,
            _ => panic!("not a range"),
        }
    }

    pub(crate) fn set_range(&self, h: Handle, r: BoundaryPair) {
        if let Object::Range(x) = &mut *self.obj_mut(h) {
            *x = r;
        }
        self.range_mutated(h);
    }

    /// A new live range collapsed at (document, 0).
    pub(crate) fn new_range(&self, doc: NodeId) -> Handle {
        let h = self.alloc(Object::Range(BoundaryPair {
            start: (doc, 0),
            end: (doc, 0),
        }));
        self.ranges.borrow_mut().push(h);
        h
    }

    /// DOM §5.5 "set the start or end".
    fn set_boundary(&self, h: Handle, node: NodeId, offset: u32, start: bool) -> rt::Result<()> {
        let dom = self.dom();
        if dom.node_type(node) == NodeType::DocumentType {
            return Err(exception(
                ExceptionName::InvalidNodeTypeError,
                "The node is a doctype",
            ));
        }
        if offset > length(&dom, node) {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The offset is beyond the node's length",
            ));
        }
        let mut r = self.range(h);
        let bp = (node, offset);
        let same_root = |x: NodeId| dom.root(x) == dom.root(node);
        if start {
            if !same_root(r.start.0) || bp_position(&dom, bp, r.end) == Ordering::Greater {
                r.end = bp;
            }
            r.start = bp;
        } else {
            if !same_root(r.start.0) || bp_position(&dom, bp, r.start) == Ordering::Less {
                r.start = bp;
            }
            r.end = bp;
        }
        drop(dom);
        self.set_range(h, r);
        Ok(())
    }

    fn range_root(&self, dom: &Dom, r: &BoundaryPair) -> NodeId {
        dom.root(r.start.0)
    }

    /// DOM §5.5 "contained".
    fn contained(&self, dom: &Dom, n: NodeId, r: &BoundaryPair) -> bool {
        dom.root(n) == self.range_root(dom, r)
            && bp_position(dom, (n, 0), r.start) == Ordering::Greater
            && bp_position(dom, (n, length(dom, n)), r.end) == Ordering::Less
    }

    /// DOM §5.5 "partially contained".
    fn partially_contained(&self, dom: &Dom, n: NodeId, r: &BoundaryPair) -> bool {
        let a = dom.is_inclusive_ancestor_of(n, r.start.0);
        let b = dom.is_inclusive_ancestor_of(n, r.end.0);
        a != b
    }

    fn replace_data_quiet(&self, n: NodeId, offset: u32, count: u32, data: &str) {
        let _ = self
            .dom_mut()
            .replace_data(n, offset as usize, count as usize, data);
    }

    fn clone_char_data(
        &self,
        cx: &mut Cx,
        n: NodeId,
        offset: u32,
        count: u32,
    ) -> rt::Result<NodeId> {
        let clone = self.clone_with_steps(cx, n, None, false)?;
        let data = self
            .dom()
            .substring_data(n, offset as usize, count as usize)
            .map_err(super::dom_err)?;
        self.dom_mut().set_data(clone, &data);
        Ok(clone)
    }

    fn partial_children(
        &self,
        r: &BoundaryPair,
    ) -> (NodeId, Option<NodeId>, Option<NodeId>, Vec<NodeId>) {
        let dom = self.dom();
        let mut common = r.start.0;
        while !dom.is_inclusive_ancestor_of(common, r.end.0) {
            common = dom.parent(common).expect("a common ancestor");
        }
        let first = if dom.is_inclusive_ancestor_of(r.start.0, r.end.0) {
            None
        } else {
            dom.children(common)
                .find(|&c| self.partially_contained(&dom, c, r))
        };
        let last = if dom.is_inclusive_ancestor_of(r.end.0, r.start.0) {
            None
        } else {
            dom.children_rev(common)
                .find(|&c| self.partially_contained(&dom, c, r))
        };
        let contained: Vec<NodeId> = dom
            .children(common)
            .filter(|&c| self.contained(&dom, c, r))
            .collect();
        (common, first, last, contained)
    }

    /// DOM §5.5 "clone the contents" of a range.
    pub(crate) fn clone_contents_of(&self, cx: &mut Cx, r: BoundaryPair) -> rt::Result<NodeId> {
        let doc = self.dom().node_document(r.start.0);
        let fragment = self.dom_mut().create_document_fragment(doc);
        if r.start == r.end {
            return Ok(fragment);
        }
        let (sn, so) = r.start;
        let (en, eo) = r.end;
        if sn == en && is_char_data(&self.dom(), sn) {
            let clone = self.clone_char_data(cx, sn, so, eo - so)?;
            self.dom_mut().insert(clone, fragment, None, true);
            return Ok(fragment);
        }
        let (_, first, last, contained) = self.partial_children(&r);
        if contained
            .iter()
            .any(|&c| self.dom().node_type(c) == NodeType::DocumentType)
        {
            return Err(exception(
                ExceptionName::HierarchyRequestError,
                "The range contains a doctype",
            ));
        }
        if let Some(f) = first {
            if is_char_data(&self.dom(), f) {
                let len = length(&self.dom(), sn);
                let clone = self.clone_char_data(cx, sn, so, len - so)?;
                self.dom_mut().insert(clone, fragment, None, true);
            } else {
                let clone = self.clone_with_steps(cx, f, None, false)?;
                self.dom_mut().insert(clone, fragment, None, true);
                let len = length(&self.dom(), f);
                let sub = self.clone_contents_of(
                    cx,
                    BoundaryPair {
                        start: r.start,
                        end: (f, len),
                    },
                )?;
                self.dom_mut().insert(sub, clone, None, true);
            }
        }
        for c in contained {
            let clone = self.clone_with_steps(cx, c, None, true)?;
            self.dom_mut().insert(clone, fragment, None, true);
        }
        if let Some(l) = last {
            if is_char_data(&self.dom(), l) {
                let clone = self.clone_char_data(cx, en, 0, eo)?;
                self.dom_mut().insert(clone, fragment, None, true);
            } else {
                let clone = self.clone_with_steps(cx, l, None, false)?;
                self.dom_mut().insert(clone, fragment, None, true);
                let sub = self.clone_contents_of(
                    cx,
                    BoundaryPair {
                        start: (l, 0),
                        end: r.end,
                    },
                )?;
                self.dom_mut().insert(sub, clone, None, true);
            }
        }
        Ok(fragment)
    }

    /// The (newNode, newOffset) the delete and extract algorithms collapse to.
    fn collapse_point(&self, r: &BoundaryPair) -> Bp {
        let dom = self.dom();
        if dom.is_inclusive_ancestor_of(r.start.0, r.end.0) {
            return r.start;
        }
        let mut reference = r.start.0;
        while let Some(p) = dom.parent(reference) {
            if dom.is_inclusive_ancestor_of(p, r.end.0) {
                break;
            }
            reference = p;
        }
        let parent = dom.parent(reference).expect("a parent");
        (parent, dom.index(reference) as u32 + 1)
    }

    /// DOM §5.5 "extract" a range's contents.
    pub(crate) fn extract_contents_of(
        &self,
        cx: &mut Cx,
        h: Option<Handle>,
        r: BoundaryPair,
    ) -> rt::Result<NodeId> {
        let doc = self.dom().node_document(r.start.0);
        let fragment = self.dom_mut().create_document_fragment(doc);
        if r.start == r.end {
            return Ok(fragment);
        }
        let (sn, so) = r.start;
        let (en, eo) = r.end;
        if sn == en && is_char_data(&self.dom(), sn) {
            let clone = self.clone_char_data(cx, sn, so, eo - so)?;
            self.dom_mut().insert(clone, fragment, None, true);
            self.replace_data_quiet(sn, so, eo - so, "");
            self.after_mutation(cx);
            return Ok(fragment);
        }
        let (_, first, last, contained) = self.partial_children(&r);
        if contained
            .iter()
            .any(|&c| self.dom().node_type(c) == NodeType::DocumentType)
        {
            return Err(exception(
                ExceptionName::HierarchyRequestError,
                "The range contains a doctype",
            ));
        }
        let new_point = self.collapse_point(&r);
        if let Some(f) = first {
            if is_char_data(&self.dom(), f) {
                let len = length(&self.dom(), sn);
                let clone = self.clone_char_data(cx, sn, so, len - so)?;
                self.dom_mut().insert(clone, fragment, None, true);
                self.replace_data_quiet(sn, so, len - so, "");
            } else {
                let clone = self.clone_with_steps(cx, f, None, false)?;
                self.dom_mut().insert(clone, fragment, None, true);
                let len = length(&self.dom(), f);
                let sub = self.extract_contents_of(
                    cx,
                    None,
                    BoundaryPair {
                        start: r.start,
                        end: (f, len),
                    },
                )?;
                self.dom_mut().insert(sub, clone, None, true);
            }
        }
        for c in contained {
            self.dom_mut().insert(c, fragment, None, false);
        }
        if let Some(l) = last {
            if is_char_data(&self.dom(), l) {
                let clone = self.clone_char_data(cx, en, 0, eo)?;
                self.dom_mut().insert(clone, fragment, None, true);
                self.replace_data_quiet(en, 0, eo, "");
            } else {
                let clone = self.clone_with_steps(cx, l, None, false)?;
                self.dom_mut().insert(clone, fragment, None, true);
                let sub = self.extract_contents_of(
                    cx,
                    None,
                    BoundaryPair {
                        start: (l, 0),
                        end: r.end,
                    },
                )?;
                self.dom_mut().insert(sub, clone, None, true);
            }
        }
        if let Some(h) = h {
            self.set_range(
                h,
                BoundaryPair {
                    start: new_point,
                    end: new_point,
                },
            );
        }
        self.after_mutation(cx);
        Ok(fragment)
    }

    /// DOM §5.5 "insert" a node into a range.
    fn range_insert(&self, cx: &mut Cx, h: Handle, node: NodeId) -> rt::Result<()> {
        let r = self.range(h);
        let (sn, so) = r.start;
        let hierarchy = || exception(ExceptionName::HierarchyRequestError, "Cannot insert there");
        let reference = {
            let dom = self.dom();
            let bad = matches!(
                dom.data(sn),
                NodeData::ProcessingInstruction { .. } | NodeData::Comment(_)
            ) || (dom.is_text(sn) && dom.parent(sn).is_none())
                || sn == node;
            if bad {
                return Err(hierarchy());
            }
            if dom.is_text(sn) {
                Some(sn)
            } else {
                dom.child_at(sn, so as usize)
            }
        };
        let parent = match reference {
            None => sn,
            Some(r) => self.dom().parent(r).ok_or_else(hierarchy)?,
        };
        self.dom()
            .ensure_pre_insert_validity(node, parent, reference)
            .map_err(super::dom_err)?;
        let mut reference = reference;
        if self.dom().is_text(sn) {
            let split = self
                .dom_mut()
                .split_text(sn, so as usize)
                .map_err(super::dom_err)?;
            reference = Some(split);
        }
        if reference == Some(node) {
            reference = self.dom().next_sibling(node);
        }
        if self.dom().parent(node).is_some() {
            self.dom_mut().remove_node(node);
        }
        let new_offset = {
            let dom = self.dom();
            let base = match reference {
                None => length(&dom, parent),
                Some(r) => dom.index(r) as u32,
            };
            base + if dom.is_document_fragment(node) {
                length(&dom, node)
            } else {
                1
            }
        };
        let res = self.dom_mut().insert_before(parent, node, reference);
        self.after_mutation(cx);
        res.map_err(super::dom_err)?;
        let r = self.range(h);
        if r.start == r.end {
            self.set_range(
                h,
                BoundaryPair {
                    start: r.start,
                    end: (parent, new_offset),
                },
            );
        }
        Ok(())
    }

    /// Live range updates for an inserted node (DOM §4.2.3 "insert" step 6).
    pub(crate) fn live_ranges_inserted(&self, node: NodeId, count: u32) {
        // The node's index costs a sibling walk: only when a live range has a boundary in its
        // parent (a page that keeps a range elsewhere, or one not collected yet, pays nothing).
        if self.ranges.borrow().is_empty() {
            return;
        }
        let dom = self.dom();
        let Some(parent) = dom.parent(node) else {
            return;
        };
        let mut index = None;
        for &h in self.ranges.borrow().iter() {
            if let Object::Range(r) = &mut *self.obj_mut(h) {
                if r.start.0 != parent && r.end.0 != parent {
                    continue;
                }
                let index = *index.get_or_insert_with(|| dom.index(node) as u32);
                if r.start.0 == parent && r.start.1 > index {
                    r.start.1 += count;
                }
                if r.end.0 == parent && r.end.1 > index {
                    r.end.1 += count;
                }
            }
        }
    }

    /// Live range updates for a removed node (DOM §4.2.3 "remove" steps 4-7).
    pub(crate) fn live_ranges_removed(
        &self,
        node: NodeId,
        parent: NodeId,
        old_previous: Option<NodeId>,
    ) {
        if self.ranges.borrow().is_empty() {
            return;
        }
        let dom = self.dom();
        // (the old index costs a sibling walk: only for a range the removal moves)
        let mut index = None;
        for &h in self.ranges.borrow().iter() {
            if let Object::Range(r) = &mut *self.obj_mut(h) {
                let start_in = dom.is_inclusive_ancestor_of(node, r.start.0);
                let end_in = dom.is_inclusive_ancestor_of(node, r.end.0);
                if !start_in && !end_in && r.start.0 != parent && r.end.0 != parent {
                    continue;
                }
                let index = *index
                    .get_or_insert_with(|| old_previous.map_or(0, |p| dom.index(p) as u32 + 1));
                if start_in {
                    r.start = (parent, index);
                }
                if end_in {
                    r.end = (parent, index);
                }
                if r.start.0 == parent && r.start.1 > index {
                    r.start.1 -= 1;
                }
                if r.end.0 == parent && r.end.1 > index {
                    r.end.1 -= 1;
                }
            }
        }
    }

    /// Calls `f` on each end of each live range.
    fn each_live_range_end(&self, mut f: impl FnMut(&mut Bp)) {
        for &h in self.ranges.borrow().iter() {
            if let Object::Range(r) = &mut *self.obj_mut(h) {
                f(&mut r.start);
                f(&mut r.end);
            }
        }
    }

    /// Live range updates for "replace data" (DOM §4.10 steps 8-11): `count` code units of `node`
    /// from `offset` were replaced by `added`.
    pub(crate) fn live_ranges_data_replaced(
        &self,
        node: NodeId,
        offset: u32,
        count: u32,
        added: u32,
    ) {
        self.each_live_range_end(|bp| {
            if bp.0 != node {
                return;
            }
            if bp.1 > offset && bp.1 <= offset + count {
                bp.1 = offset;
            } else if bp.1 > offset + count {
                bp.1 = bp.1 - count + added;
            }
        });
    }

    /// Live range updates for "split a Text node" (DOM §4.11 steps 7.2-7.5): `new_node` took `node`'s
    /// data from `offset`; `node` was at `index` in `parent`.
    pub(crate) fn live_ranges_text_split(
        &self,
        node: NodeId,
        new_node: NodeId,
        offset: u32,
        parent: NodeId,
        index: u32,
    ) {
        self.each_live_range_end(|bp| {
            if bp.0 == node && bp.1 > offset {
                *bp = (new_node, bp.1 - offset);
            } else if bp.0 == parent && bp.1 == index + 1 {
                bp.1 += 1;
            }
        });
    }

    /// Live range updates for `normalize()` (DOM §4.4 steps 6.1-6.4): `from`'s data (it was at
    /// `index` in `parent`) now follows `into`'s from `at`.
    pub(crate) fn live_ranges_text_merged(
        &self,
        from: NodeId,
        into: NodeId,
        at: u32,
        parent: NodeId,
        index: u32,
    ) {
        self.each_live_range_end(|bp| {
            if bp.0 == from {
                *bp = (into, bp.1 + at);
            } else if bp.0 == parent && bp.1 == index {
                *bp = (into, at);
            }
        });
    }
}

impl AbstractRangeImpl for Engine {
    fn start_container(&self, this: Handle) -> Handle {
        node_handle(self.range(this).start.0)
    }
    fn start_offset(&self, this: Handle) -> u32 {
        self.range(this).start.1
    }
    fn end_container(&self, this: Handle) -> Handle {
        node_handle(self.range(this).end.0)
    }
    fn end_offset(&self, this: Handle) -> u32 {
        self.range(this).end.1
    }
    fn collapsed(&self, this: Handle) -> bool {
        let r = self.range(this);
        r.start == r.end
    }
}

impl StaticRangeImpl for Engine {
    fn constructor(&self, _cx: &mut Cx, init: StaticRangeInit) -> rt::Result<Handle> {
        let sc = self.n(init.start_container);
        let ec = self.n(init.end_container);
        {
            let dom = self.dom();
            let bad = |n: NodeId| {
                matches!(
                    dom.node_type(n),
                    NodeType::DocumentType | NodeType::Attribute
                )
            };
            if bad(sc) || bad(ec) {
                return Err(exception(
                    ExceptionName::InvalidNodeTypeError,
                    "A static range cannot have a doctype or Attr container",
                ));
            }
        }
        Ok(self.alloc(Object::StaticRange(BoundaryPair {
            start: (sc, init.start_offset),
            end: (ec, init.end_offset),
        })))
    }
}

impl RangeImpl for Engine {
    fn common_ancestor_container(&self, this: Handle) -> Handle {
        let r = self.range(this);
        let dom = self.dom();
        let mut c = r.start.0;
        while !dom.is_inclusive_ancestor_of(c, r.end.0) {
            c = dom.parent(c).expect("a common ancestor");
        }
        node_handle(c)
    }
    /// DOM §5.5 stringifier: the Text data in the range.
    fn stringify(&self, this: Handle) -> DomStr<'_> {
        let r = self.range(this);
        let dom = self.dom();
        let mut s = String::new();
        let (sn, so) = r.start;
        let (en, eo) = r.end;
        if sn == en && dom.is_text(sn) {
            return DomStr::Owned(
                dom.substring_data(sn, so as usize, (eo - so) as usize)
                    .unwrap_or_default(),
            );
        }
        if dom.is_text(sn) {
            let len = length(&dom, sn);
            s.push_str(
                &dom.substring_data(sn, so as usize, (len - so) as usize)
                    .unwrap_or_default(),
            );
        }
        let root = dom.root(sn);
        let mut cur = Some(root);
        while let Some(n) = cur {
            if dom.is_text(n) && self.contained(&dom, n, &r) {
                s.push_str(dom.character_data(n).unwrap_or(""));
            }
            cur = dom.next_in_tree_order(n, Some(root));
        }
        if dom.is_text(en) {
            s.push_str(&dom.substring_data(en, 0, eo as usize).unwrap_or_default());
        }
        DomStr::Owned(s)
    }
    fn set_start(&self, _cx: &mut Cx, this: Handle, node: Handle, offset: u32) -> rt::Result<()> {
        self.set_boundary(this, self.n(node), offset, true)
    }
    fn set_end(&self, _cx: &mut Cx, this: Handle, node: Handle, offset: u32) -> rt::Result<()> {
        self.set_boundary(this, self.n(node), offset, false)
    }
    fn set_start_before(&self, _cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<()> {
        let n = self.n(node);
        let (p, i) = self.parent_index(n)?;
        self.set_boundary(this, p, i, true)
    }
    fn set_start_after(&self, _cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<()> {
        let n = self.n(node);
        let (p, i) = self.parent_index(n)?;
        self.set_boundary(this, p, i + 1, true)
    }
    fn set_end_before(&self, _cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<()> {
        let n = self.n(node);
        let (p, i) = self.parent_index(n)?;
        self.set_boundary(this, p, i, false)
    }
    fn set_end_after(&self, _cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<()> {
        let n = self.n(node);
        let (p, i) = self.parent_index(n)?;
        self.set_boundary(this, p, i + 1, false)
    }
    fn collapse(&self, _cx: &mut Cx, this: Handle, to_start: bool) -> rt::Result<()> {
        let mut r = self.range(this);
        if to_start {
            r.end = r.start;
        } else {
            r.start = r.end;
        }
        self.set_range(this, r);
        Ok(())
    }
    fn select_node(&self, _cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<()> {
        let n = self.n(node);
        let (p, i) = self.parent_index(n)?;
        self.set_range(
            this,
            BoundaryPair {
                start: (p, i),
                end: (p, i + 1),
            },
        );
        Ok(())
    }
    fn select_node_contents(&self, _cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<()> {
        let n = self.n(node);
        let dom = self.dom();
        if dom.node_type(n) == NodeType::DocumentType {
            return Err(exception(
                ExceptionName::InvalidNodeTypeError,
                "The node is a doctype",
            ));
        }
        let len = length(&dom, n);
        drop(dom);
        self.set_range(
            this,
            BoundaryPair {
                start: (n, 0),
                end: (n, len),
            },
        );
        Ok(())
    }
    fn compare_boundary_points(
        &self,
        _cx: &mut Cx,
        this: Handle,
        how: u16,
        source_range: Handle,
    ) -> rt::Result<i16> {
        if how > 3 {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "Unknown comparison",
            ));
        }
        let a = self.range(this);
        let b = self.range(source_range);
        let dom = self.dom();
        if dom.root(a.start.0) != dom.root(b.start.0) {
            return Err(exception(
                ExceptionName::WrongDocumentError,
                "The ranges are in different trees",
            ));
        }
        let (x, y) = match how {
            0 => (a.start, b.start),
            1 => (a.end, b.start),
            2 => (a.end, b.end),
            _ => (a.start, b.end),
        };
        Ok(match bp_position(&dom, x, y) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        })
    }
    /// DOM §5.5 `deleteContents()`.
    fn delete_contents(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let r = self.range(this);
        if r.start == r.end {
            return Ok(());
        }
        let (sn, so) = r.start;
        let (en, eo) = r.end;
        if sn == en && is_char_data(&self.dom(), sn) {
            self.replace_data_quiet(sn, so, eo - so, "");
            self.after_mutation(cx);
            return Ok(());
        }
        let to_remove: Vec<NodeId> = {
            let dom = self.dom();
            let root = dom.root(sn);
            let mut out = Vec::new();
            let mut cur = Some(root);
            while let Some(n) = cur {
                if self.contained(&dom, n, &r)
                    && !dom.parent(n).is_some_and(|p| self.contained(&dom, p, &r))
                {
                    out.push(n);
                }
                cur = dom.next_in_tree_order(n, Some(root));
            }
            out
        };
        let new_point = self.collapse_point(&r);
        if is_char_data(&self.dom(), sn) {
            let len = length(&self.dom(), sn);
            self.replace_data_quiet(sn, so, len - so, "");
        }
        for n in to_remove {
            self.dom_mut().remove_node(n);
        }
        if is_char_data(&self.dom(), en) {
            self.replace_data_quiet(en, 0, eo, "");
        }
        self.after_mutation(cx);
        self.set_range(
            this,
            BoundaryPair {
                start: new_point,
                end: new_point,
            },
        );
        Ok(())
    }
    fn extract_contents(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let r = self.range(this);
        self.extract_contents_of(cx, Some(this), r).map(node_handle)
    }
    fn clone_contents(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let r = self.range(this);
        self.clone_contents_of(cx, r).map(node_handle)
    }
    fn insert_node(&self, cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<()> {
        self.range_insert(cx, this, self.n(node))
    }
    /// DOM §5.5 `surroundContents(newParent)`.
    fn surround_contents(&self, cx: &mut Cx, this: Handle, new_parent: Handle) -> rt::Result<()> {
        let np = self.n(new_parent);
        let r = self.range(this);
        {
            let dom = self.dom();
            let root = dom.root(r.start.0);
            let mut cur = Some(root);
            while let Some(n) = cur {
                if !dom.is_text(n) && self.partially_contained(&dom, n, &r) {
                    return Err(exception(
                        ExceptionName::InvalidStateError,
                        "The range partially contains a non-Text node",
                    ));
                }
                cur = dom.next_in_tree_order(n, Some(root));
            }
            if matches!(
                dom.node_type(np),
                NodeType::Document | NodeType::DocumentType | NodeType::DocumentFragment
            ) {
                return Err(exception(
                    ExceptionName::InvalidNodeTypeError,
                    "The new parent cannot be a document, doctype or fragment",
                ));
            }
        }
        let fragment = self.extract_contents_of(cx, Some(this), r)?;
        if self.dom().has_child_nodes(np) {
            self.dom_mut().replace_all(None, np);
        }
        self.range_insert(cx, this, np)?;
        self.dom_mut()
            .append_child(np, fragment)
            .map_err(super::dom_err)?;
        self.after_mutation(cx);
        let (p, i) = self.parent_index(np)?;
        self.set_range(
            this,
            BoundaryPair {
                start: (p, i),
                end: (p, i + 1),
            },
        );
        Ok(())
    }
    fn clone_range(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let r = self.range(this);
        let h = self.alloc(Object::Range(r));
        self.ranges.borrow_mut().push(h);
        Ok(h)
    }
    fn detach(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn is_point_in_range(
        &self,
        _cx: &mut Cx,
        this: Handle,
        node: Handle,
        offset: u32,
    ) -> rt::Result<bool> {
        let n = self.n(node);
        let r = self.range(this);
        let dom = self.dom();
        if dom.root(n) != dom.root(r.start.0) {
            return Ok(false);
        }
        if dom.node_type(n) == NodeType::DocumentType {
            return Err(exception(
                ExceptionName::InvalidNodeTypeError,
                "The node is a doctype",
            ));
        }
        if offset > length(&dom, n) {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The offset is beyond the node's length",
            ));
        }
        let bp = (n, offset);
        Ok(bp_position(&dom, bp, r.start) != Ordering::Less
            && bp_position(&dom, bp, r.end) != Ordering::Greater)
    }
    fn compare_point(
        &self,
        _cx: &mut Cx,
        this: Handle,
        node: Handle,
        offset: u32,
    ) -> rt::Result<i16> {
        let n = self.n(node);
        let r = self.range(this);
        let dom = self.dom();
        if dom.root(n) != dom.root(r.start.0) {
            return Err(exception(
                ExceptionName::WrongDocumentError,
                "The node is in another tree",
            ));
        }
        if dom.node_type(n) == NodeType::DocumentType {
            return Err(exception(
                ExceptionName::InvalidNodeTypeError,
                "The node is a doctype",
            ));
        }
        if offset > length(&dom, n) {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The offset is beyond the node's length",
            ));
        }
        let bp = (n, offset);
        if bp_position(&dom, bp, r.start) == Ordering::Less {
            return Ok(-1);
        }
        if bp_position(&dom, bp, r.end) == Ordering::Greater {
            return Ok(1);
        }
        Ok(0)
    }
    fn intersects_node(&self, _cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<bool> {
        let n = self.n(node);
        let r = self.range(this);
        let dom = self.dom();
        if dom.root(n) != dom.root(r.start.0) {
            return Ok(false);
        }
        let Some(p) = dom.parent(n) else {
            return Ok(true);
        };
        let i = dom.index(n) as u32;
        Ok(bp_position(&dom, (p, i), r.end) == Ordering::Less
            && bp_position(&dom, (p, i + 1), r.start) == Ordering::Greater)
    }
    /// CSSOM View §6.2 `getClientRects()`.
    fn get_client_rects(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let r = self.range(this);
        let rects = self
            .view_range_client_rects(r.start, r.end)
            .into_iter()
            .map(|r| self.alloc(Object::DomRect(r)))
            .collect();
        Ok(self.alloc(Object::DomRectList(rects)))
    }
    /// CSSOM View §6.2 `getBoundingClientRect()`.
    fn get_bounding_client_rect(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let r = self.range(this);
        let rect = self.view_range_bounding_rect(r.start, r.end);
        Ok(self.alloc(Object::DomRect(rect)))
    }
    /// DOM Parsing §6 `createContextualFragment(fragment)`.
    fn create_contextual_fragment(
        &self,
        cx: &mut Cx,
        this: Handle,
        string: TrustedHTMLOrString,
    ) -> rt::Result<Handle> {
        let start = self.range(this).start.0;
        let markup = self.tt_html(cx, string, start, "Range createContextualFragment")?;
        let r = self.range(this);
        let start = r.start.0;
        let context = {
            let dom = self.dom();
            match dom.data(start) {
                NodeData::Document(_)
                | NodeData::DocumentFragment { .. }
                | NodeData::ShadowRoot(_) => None,
                NodeData::Element(_) => Some(start),
                _ => dom.parent_element(start),
            }
        };
        let context = match context {
            Some(c)
                if !(self.dom().is_html_document(self.dom().node_document(c))
                    && self.dom().is_html_element_named(c, &local_name!("html"))) =>
            {
                c
            }
            _ => {
                let doc = self.dom().node_document(start);
                self.dom_mut().create_html_element(doc, local_name!("body"))
            }
        };
        let fragment = self.dom_mut().fragment_parse(context, &markup).js()?;
        // Scripts in the fragment are not "already started", so they run when inserted.
        let scripts: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(fragment)
                .filter(|&d| dom.is_script_element(d))
                .collect()
        };
        for s in scripts {
            self.dom_mut().set_element_flag(
                s,
                snowlark_dom::element_flags::SCRIPT_ALREADY_STARTED,
                false,
            );
        }
        self.process_reactions(cx);
        Ok(node_handle(fragment))
    }
    fn constructor(&self, cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.new_range(self.current_document(cx)?))
    }
}

impl Engine {
    fn parent_index(&self, n: NodeId) -> rt::Result<(NodeId, u32)> {
        let dom = self.dom();
        let p = dom.parent(n).ok_or_else(|| {
            exception(
                ExceptionName::InvalidNodeTypeError,
                "The node has no parent",
            )
        })?;
        Ok((p, dom.index(n) as u32))
    }
}
