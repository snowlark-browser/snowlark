// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Selection API (https://w3c.github.io/selection-api/): each document with a browsing context has
//! one `Selection` holding at most one live `Range` (shared with script: `getRangeAt(0)` returns it)
//! and a direction. Every change queues one `selectionchange` at the document.

use std::cmp::Ordering;

use snowlark_bindings::generated::{RangeImpl, SelectionImpl};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};
use snowlark_dom::{NodeId, NodeType};

use super::ranges::{bp_position, composed_position, focus_in_tree_of, BoundaryPair, Bp};
use super::{exception, node_handle, Engine, Object};
use crate::event_loop::TaskSource;

/// A selection's direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
    None,
}

/// A `Selection` object's state.
#[derive(Clone, Debug)]
pub struct SelectionData {
    pub document: NodeId,
    /// The live range (a `Range` object).
    pub range: Option<Handle>,
    pub direction: Direction,
}

impl Engine {
    /// The `Selection` of a document with a browsing context (made on first use).
    pub(crate) fn selection_of(&self, doc: NodeId) -> Option<Handle> {
        if !self.doc_state(doc, |s| s.has_browsing_context) {
            return None;
        }
        if let Some(&h) = self.selections.borrow().get(&doc) {
            return Some(h);
        }
        let h = self.alloc(Object::Selection(Box::new(SelectionData {
            document: doc,
            range: None,
            direction: Direction::None,
        })));
        self.selections.borrow_mut().insert(doc, h);
        Some(h)
    }

    fn sel_data(&self, h: Handle) -> SelectionData {
        match &*self.obj(h) {
            Object::Selection(s) => (**s).clone(),
            _ => panic!("not a Selection"),
        }
    }

    fn set_sel_data(&self, h: Handle, f: impl FnOnce(&mut SelectionData)) {
        if let Object::Selection(s) = &mut *self.obj_mut(h) {
            f(s);
        }
    }

    /// The boundary points of a document's selection, with its direction.
    pub(crate) fn doc_selection(&self, doc: NodeId) -> Option<(BoundaryPair, Direction)> {
        let h = *self.selections.borrow().get(&doc)?;
        let s = self.sel_data(h);
        let r = s.range?;
        Some((self.range_bounds(r), s.direction))
    }

    /// The anchor and focus of a document's selection.
    pub(crate) fn doc_anchor_focus(&self, doc: NodeId) -> Option<(Bp, Bp)> {
        let (r, d) = self.doc_selection(doc)?;
        Some(if d == Direction::Backward {
            (r.end, r.start)
        } else {
            (r.start, r.end)
        })
    }

    /// Selects from `anchor` to `focus` in `doc` (a new range; the direction from their order),
    /// or clears the selection. A range has its ends in one tree: a `focus` in another stops at the
    /// edge of `anchor`'s ([`focus_in_tree_of`]).
    pub(crate) fn set_doc_selection(&self, doc: NodeId, ends: Option<(Bp, Bp)>) {
        let Some(h) = self.selection_of(doc) else {
            return;
        };
        match ends {
            None => self.sel_set(h, None, Direction::None),
            Some((anchor, focus)) => {
                let focus = focus_in_tree_of(&self.dom(), anchor, focus);
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
                self.sel_set(h, Some(r), d);
            }
        }
    }

    /// Gives a selection a new range (a new `Range` object) and direction.
    fn sel_set(&self, h: Handle, r: Option<BoundaryPair>, direction: Direction) {
        let doc = self.sel_data(h).document;
        let range = r.map(|b| {
            let rh = self.new_range(doc);
            self.set_range(rh, b);
            rh
        });
        self.set_sel_data(h, |s| {
            s.range = range;
            s.direction = direction;
        });
        self.selection_changed(doc);
    }

    /// The selection of `doc` changed: one `selectionchange` is queued at it (Selection API §5.3),
    /// and its rendering follows.
    pub(crate) fn selection_changed(&self, doc: NodeId) {
        self.render.borrow_mut().dirty = true;
        self.schedule_selectionchange(doc);
    }

    /// Selection API §5.3 "schedule a selectionchange event" on a document, or on an `input` or
    /// `textarea` whose text selection changed: one queued at a time per target, bubbling from an
    /// element.
    pub(crate) fn schedule_selectionchange(&self, target: NodeId) {
        if !self.selectionchange_pending.borrow_mut().insert(target) {
            return;
        }
        self.queue_task(TaskSource::UserInteraction, move |e, cx| {
            e.selectionchange_pending.borrow_mut().remove(&target);
            let (alive, element) = {
                let dom = e.dom();
                (dom.is_alive(target), dom.is_element(target))
            };
            if alive {
                e.fire_event(cx, node_handle(target), "selectionchange", element, false);
            }
        });
    }

    /// A range changed: when it is a selection's, `selectionchange` follows.
    pub(crate) fn range_mutated(&self, h: Handle) {
        let doc = {
            let sels = self.selections.borrow();
            sels.iter()
                .find(|(_, &s)| matches!(&*self.obj(s), Object::Selection(d) if d.range == Some(h)))
                .map(|(&d, _)| d)
        };
        if let Some(doc) = doc {
            self.selection_changed(doc);
        }
    }

    fn sel_root_ok(&self, h: Handle, node: NodeId) -> bool {
        let doc = self.sel_data(h).document;
        self.dom().root(node) == doc
    }

    /// Whether the selection's document is a shadow-including inclusive ancestor of `node`: `collapse`,
    /// `extend` and `setBaseAndExtent` reach into shadow trees, where the others stop at the document
    /// tree.
    fn sel_includes(&self, h: Handle, node: NodeId) -> bool {
        let doc = self.sel_data(h).document;
        self.dom().shadow_including_root(node) == doc
    }

    /// The range and direction from `anchor` to `focus`, set by script: ends in different trees
    /// collapse it at `focus`, as "set the start or end" of a range does, and only the direction
    /// keeps their order.
    fn sel_ends(&self, anchor: Bp, focus: Bp) -> (BoundaryPair, Direction) {
        let dom = self.dom();
        let backward = composed_position(&dom, focus, anchor) == Ordering::Less;
        let direction = if backward {
            Direction::Backward
        } else {
            Direction::Forward
        };
        let (start, end) = if dom.root(anchor.0) != dom.root(focus.0) {
            (focus, focus)
        } else if backward {
            (focus, anchor)
        } else {
            (anchor, focus)
        };
        (BoundaryPair { start, end }, direction)
    }

    fn check_offset(&self, node: NodeId, offset: u32) -> rt::Result<()> {
        let dom = self.dom();
        if dom.node_type(node) == NodeType::DocumentType {
            return Err(exception(
                ExceptionName::InvalidNodeTypeError,
                "The node is a doctype",
            ));
        }
        if offset as usize > dom.length(node) {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The offset is beyond the node's length",
            ));
        }
        Ok(())
    }

    fn sel_anchor_focus(&self, h: Handle) -> Option<(Bp, Bp)> {
        let s = self.sel_data(h);
        let r = self.range_bounds(s.range?);
        Some(if s.direction == Direction::Backward {
            (r.end, r.start)
        } else {
            (r.start, r.end)
        })
    }
}

impl SelectionImpl for Engine {
    fn anchor_node(&self, this: Handle) -> Option<Handle> {
        self.sel_anchor_focus(this).map(|(a, _)| node_handle(a.0))
    }
    fn anchor_offset(&self, this: Handle) -> u32 {
        self.sel_anchor_focus(this).map_or(0, |(a, _)| a.1)
    }
    fn focus_node(&self, this: Handle) -> Option<Handle> {
        self.sel_anchor_focus(this).map(|(_, f)| node_handle(f.0))
    }
    fn focus_offset(&self, this: Handle) -> u32 {
        self.sel_anchor_focus(this).map_or(0, |(_, f)| f.1)
    }
    fn is_collapsed(&self, this: Handle) -> bool {
        self.sel_anchor_focus(this).is_none_or(|(a, f)| a == f)
    }
    fn range_count(&self, this: Handle) -> u32 {
        u32::from(self.sel_data(this).range.is_some())
    }
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(match self.sel_anchor_focus(this) {
            None => "None",
            Some((a, f)) if a == f => "Caret",
            Some(_) => "Range",
        })
    }
    fn direction(&self, this: Handle) -> DomStr<'_> {
        let s = self.sel_data(this);
        DomStr::Borrowed(match (s.range, s.direction) {
            (Some(_), Direction::Forward) => "forward",
            (Some(_), Direction::Backward) => "backward",
            _ => "none",
        })
    }
    /// The selected text as rendered (`innerText` rules), as browsers give it.
    fn stringify(&self, this: Handle) -> DomStr<'_> {
        match self.sel_data(this).range {
            Some(r) => {
                let b = self.range_bounds(r);
                let rendered = self.is_rendered_document(self.dom().node_document(b.start.0));
                if rendered {
                    DomStr::Owned(self.range_inner_text(b))
                } else {
                    DomStr::Owned(RangeImpl::stringify(self, r).to_string())
                }
            }
            None => DomStr::Borrowed(""),
        }
    }
    fn get_range_at(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<Handle> {
        match self.sel_data(this).range {
            Some(r) if index == 0 => Ok(r),
            _ => Err(exception(
                ExceptionName::IndexSizeError,
                "No range at that index",
            )),
        }
    }
    fn add_range(&self, _cx: &mut Cx, this: Handle, range: Handle) -> rt::Result<()> {
        let r = self.range_bounds(range);
        if !self.sel_root_ok(this, r.start.0) || self.sel_data(this).range.is_some() {
            return Ok(());
        }
        self.set_sel_data(this, |s| {
            s.range = Some(range);
            s.direction = Direction::Forward;
        });
        let doc = self.sel_data(this).document;
        self.selection_changed(doc);
        Ok(())
    }
    fn remove_range(&self, _cx: &mut Cx, this: Handle, range: Handle) -> rt::Result<()> {
        if self.sel_data(this).range != Some(range) {
            return Err(exception(
                ExceptionName::NotFoundError,
                "The range is not the selection's",
            ));
        }
        self.sel_set(this, None, Direction::None);
        Ok(())
    }
    fn remove_all_ranges(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if self.sel_data(this).range.is_some() {
            self.sel_set(this, None, Direction::None);
        }
        Ok(())
    }
    fn empty(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        SelectionImpl::remove_all_ranges(self, cx, this)
    }
    fn get_composed_ranges(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: snowlark_bindings::generated::GetComposedRangesOptions,
    ) -> rt::Result<Vec<Handle>> {
        let _ = cx;
        let Some(r) = self.sel_data(this).range else {
            return Ok(Vec::new());
        };
        let b = self.range_bounds(r);
        let roots: Vec<NodeId> = options.shadow_roots.iter().map(|&s| self.n(s)).collect();
        let dom = self.dom();
        // Up to the host from a shadow tree that is not one of `roots` or an ancestor of one.
        let rescoped = |(mut node, mut offset): Bp, after: u32| loop {
            let root = dom.root(node);
            if !dom.is_shadow_root(root)
                || roots
                    .iter()
                    .any(|&s| dom.is_shadow_including_inclusive_ancestor_of(root, s))
            {
                return (node, offset);
            }
            let Some(host) = dom.host(root) else {
                return (node, offset);
            };
            let Some(parent) = dom.parent(host) else {
                return (node, offset);
            };
            offset = dom.index(host) as u32 + after;
            node = parent;
        };
        let b = BoundaryPair {
            start: rescoped(b.start, 0),
            end: rescoped(b.end, 1),
        };
        drop(dom);
        Ok(vec![self.alloc(Object::StaticRange(b))])
    }
    fn collapse(
        &self,
        cx: &mut Cx,
        this: Handle,
        node: Option<Handle>,
        offset: u32,
    ) -> rt::Result<()> {
        let Some(node) = node else {
            return SelectionImpl::remove_all_ranges(self, cx, this);
        };
        let n = self.n(node);
        self.check_offset(n, offset)?;
        if !self.sel_includes(this, n) {
            return Ok(());
        }
        let bp = (n, offset);
        self.sel_set(
            this,
            Some(BoundaryPair { start: bp, end: bp }),
            Direction::None,
        );
        Ok(())
    }
    fn set_position(
        &self,
        cx: &mut Cx,
        this: Handle,
        node: Option<Handle>,
        offset: u32,
    ) -> rt::Result<()> {
        SelectionImpl::collapse(self, cx, this, node, offset)
    }
    fn collapse_to_start(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let Some(r) = self.sel_data(this).range else {
            return Err(exception(ExceptionName::InvalidStateError, "No selection"));
        };
        let b = self.range_bounds(r);
        self.sel_set(
            this,
            Some(BoundaryPair {
                start: b.start,
                end: b.start,
            }),
            Direction::None,
        );
        Ok(())
    }
    fn collapse_to_end(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let Some(r) = self.sel_data(this).range else {
            return Err(exception(ExceptionName::InvalidStateError, "No selection"));
        };
        let b = self.range_bounds(r);
        self.sel_set(
            this,
            Some(BoundaryPair {
                start: b.end,
                end: b.end,
            }),
            Direction::None,
        );
        Ok(())
    }
    fn extend(&self, _cx: &mut Cx, this: Handle, node: Handle, offset: u32) -> rt::Result<()> {
        let n = self.n(node);
        if !self.sel_includes(this, n) {
            return Ok(());
        }
        let Some((anchor, _)) = self.sel_anchor_focus(this) else {
            return Err(exception(ExceptionName::InvalidStateError, "No selection"));
        };
        self.check_offset(n, offset)?;
        // (a new range, as the specification says)
        let (r, d) = self.sel_ends(anchor, (n, offset));
        self.sel_set(this, Some(r), d);
        Ok(())
    }
    fn set_base_and_extent(
        &self,
        _cx: &mut Cx,
        this: Handle,
        anchor_node: Handle,
        anchor_offset: u32,
        focus_node: Handle,
        focus_offset: u32,
    ) -> rt::Result<()> {
        let (a, f) = (self.n(anchor_node), self.n(focus_node));
        self.check_offset(a, anchor_offset)?;
        self.check_offset(f, focus_offset)?;
        if !self.sel_includes(this, a) || !self.sel_includes(this, f) {
            return Ok(());
        }
        let (r, d) = self.sel_ends((a, anchor_offset), (f, focus_offset));
        self.sel_set(this, Some(r), d);
        Ok(())
    }
    fn select_all_children(&self, _cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<()> {
        let n = self.n(node);
        if self.dom().node_type(n) == NodeType::DocumentType {
            return Err(exception(
                ExceptionName::InvalidNodeTypeError,
                "The node is a doctype",
            ));
        }
        if !self.sel_root_ok(this, n) {
            return Ok(());
        }
        let count = self.dom().child_count(n) as u32;
        self.sel_set(
            this,
            Some(BoundaryPair {
                start: (n, 0),
                end: (n, count),
            }),
            Direction::Forward,
        );
        Ok(())
    }
    fn modify(
        &self,
        _cx: &mut Cx,
        this: Handle,
        alter: Option<&str>,
        direction: Option<&str>,
        granularity: Option<&str>,
    ) -> rt::Result<()> {
        let Some((anchor, focus)) = self.sel_anchor_focus(this) else {
            return Ok(());
        };
        let alter = alter.unwrap_or("").to_ascii_lowercase();
        let direction = direction.unwrap_or("").to_ascii_lowercase();
        let granularity = granularity.unwrap_or("").to_ascii_lowercase();
        if !matches!(alter.as_str(), "move" | "extend") {
            return Ok(());
        }
        let forward = match direction.as_str() {
            "forward" | "right" => true,
            "backward" | "left" => false,
            _ => return Ok(()),
        };
        let doc = self.sel_data(this).document;
        let moving_range = alter == "move" && anchor != focus;
        // A range moved: from its end going forward, its start going backward.
        let from = if moving_range {
            let dom = self.dom();
            let after = bp_position(&dom, anchor, focus) == Ordering::Greater;
            if after == forward {
                anchor
            } else {
                focus
            }
        } else {
            focus
        };
        let to = match granularity.as_str() {
            // (a range moved by a character collapses to that end)
            "character" if moving_range => Some(from),
            "character" => self.step_position(doc, focus, forward, false),
            "word" => self.step_position(doc, focus, forward, true),
            // (by lines, at the focus's column)
            "line" | "paragraph" => self.line_step_at(doc, from, Some(focus), forward),
            "lineboundary" | "paragraphboundary" => self.line_boundary(doc, focus, forward),
            _ => return Ok(()),
        };
        let Some(to) = to else {
            return Ok(());
        };
        let anchor = if alter == "move" { to } else { anchor };
        self.set_doc_selection(doc, Some((anchor, to)));
        Ok(())
    }
    fn delete_from_document(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if let Some(r) = self.sel_data(this).range {
            RangeImpl::delete_contents(self, cx, r)?;
            let doc = self.sel_data(this).document;
            self.selection_changed(doc);
        }
        Ok(())
    }
    fn contains_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        node: Handle,
        allow_partial_containment: bool,
    ) -> rt::Result<bool> {
        let n = self.n(node);
        let Some(r) = self.sel_data(this).range else {
            return Ok(false);
        };
        // ("false ... if node's root is not the document": the selection can be in a shadow tree,
        // this method never sees one)
        if !self.sel_root_ok(this, n) {
            return Ok(false);
        }
        let b = self.range_bounds(r);
        let dom = self.dom();
        if dom.root(n) != dom.root(b.start.0) {
            return Ok(false);
        }
        let len = dom.length(n) as u32;
        let start_ok = bp_position(&dom, b.start, (n, 0)) != Ordering::Greater;
        let end_ok = bp_position(&dom, b.end, (n, len)) != Ordering::Less;
        if !allow_partial_containment {
            return Ok(start_ok && end_ok);
        }
        let s_before_end = bp_position(&dom, b.start, (n, len)) != Ordering::Greater;
        let e_after_start = bp_position(&dom, b.end, (n, 0)) != Ordering::Less;
        Ok(s_before_end && e_after_start)
    }
}
