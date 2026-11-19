// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Traversal (DOM §6): `NodeIterator` and `TreeWalker` with their `NodeFilter` callbacks, and the
//! NodeIterator pre-removing steps (run after the removal, from the removed node's old siblings).

use snowlark_bindings::generated::{NodeFilter, NodeIteratorImpl, TreeWalkerImpl};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle};
use snowlark_dom::{Dom, NodeId};

use super::{exception, node_handle, Engine, Object};

const FILTER_ACCEPT: u16 = 1;
const FILTER_REJECT: u16 = 2;
const FILTER_SKIP: u16 = 3;

pub struct WalkerData {
    pub root: NodeId,
    pub what_to_show: u32,
    pub filter: Option<NodeFilter>,
    pub current: NodeId,
    pub active: bool,
}

pub struct IteratorData {
    pub root: NodeId,
    pub what_to_show: u32,
    pub filter: Option<NodeFilter>,
    pub reference: NodeId,
    pub pointer_before_reference: bool,
    pub active: bool,
}

impl Engine {
    pub(crate) fn new_tree_walker(
        &self,
        root: NodeId,
        what_to_show: u32,
        filter: Option<NodeFilter>,
    ) -> Handle {
        self.alloc(Object::TreeWalker(WalkerData {
            root,
            what_to_show,
            filter,
            current: root,
            active: false,
        }))
    }

    pub(crate) fn new_node_iterator(
        &self,
        root: NodeId,
        what_to_show: u32,
        filter: Option<NodeFilter>,
    ) -> Handle {
        let h = self.alloc(Object::NodeIterator(IteratorData {
            root,
            what_to_show,
            filter,
            reference: root,
            pointer_before_reference: true,
            active: false,
        }));
        self.iterators.borrow_mut().push(h);
        h
    }

    /// DOM §6 "filter" a node with a traverser's whatToShow and filter.
    fn filter_node(&self, cx: &mut Cx, h: Handle, node: NodeId) -> rt::Result<u16> {
        let (active, what, filter) = match &*self.obj(h) {
            Object::TreeWalker(w) => (w.active, w.what_to_show, w.filter.clone()),
            Object::NodeIterator(i) => (i.active, i.what_to_show, i.filter.clone()),
            _ => unreachable!(),
        };
        if active {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The filter is already running",
            ));
        }
        let n = self.dom().node_type(node) as u32 - 1;
        if what & (1 << n) == 0 {
            return Ok(FILTER_SKIP);
        }
        let Some(filter) = filter else {
            return Ok(FILTER_ACCEPT);
        };
        self.set_traverser_active(h, true);
        let r = filter.accept_node(cx, None, node_handle(node));
        self.set_traverser_active(h, false);
        r
    }

    fn set_traverser_active(&self, h: Handle, v: bool) {
        match &mut *self.obj_mut(h) {
            Object::TreeWalker(w) => w.active = v,
            Object::NodeIterator(i) => i.active = v,
            _ => {}
        }
    }

    fn walker(&self, h: Handle) -> (NodeId, NodeId) {
        match &*self.obj(h) {
            Object::TreeWalker(w) => (w.root, w.current),
            _ => panic!("not a TreeWalker"),
        }
    }

    fn set_current(&self, h: Handle, n: NodeId) {
        if let Object::TreeWalker(w) = &mut *self.obj_mut(h) {
            w.current = n;
        }
    }

    /// DOM §6.2 "traverse children".
    fn traverse_children(&self, cx: &mut Cx, h: Handle, first: bool) -> rt::Result<Option<Handle>> {
        let (root, current) = self.walker(h);
        let child = |dom: &Dom, n: NodeId| {
            if first {
                dom.first_child(n)
            } else {
                dom.last_child(n)
            }
        };
        let sibling = |dom: &Dom, n: NodeId| {
            if first {
                dom.next_sibling(n)
            } else {
                dom.previous_sibling(n)
            }
        };
        let mut node = child(&self.dom(), current);
        while let Some(n) = node {
            let result = self.filter_node(cx, h, n)?;
            if result == FILTER_ACCEPT {
                self.set_current(h, n);
                return Ok(Some(node_handle(n)));
            }
            if result == FILTER_SKIP {
                if let Some(c) = child(&self.dom(), n) {
                    node = Some(c);
                    continue;
                }
            }
            let mut cur = n;
            loop {
                let s = sibling(&self.dom(), cur);
                if let Some(s) = s {
                    node = Some(s);
                    break;
                }
                let parent = self.dom().parent(cur);
                match parent {
                    None => return Ok(None),
                    Some(p) if p == root || p == current => return Ok(None),
                    Some(p) => cur = p,
                }
            }
        }
        Ok(None)
    }

    /// DOM §6.2 "traverse siblings".
    fn traverse_siblings(&self, cx: &mut Cx, h: Handle, next: bool) -> rt::Result<Option<Handle>> {
        let (root, current) = self.walker(h);
        let mut node = current;
        if node == root {
            return Ok(None);
        }
        let sib = |dom: &Dom, n: NodeId| {
            if next {
                dom.next_sibling(n)
            } else {
                dom.previous_sibling(n)
            }
        };
        let child = |dom: &Dom, n: NodeId| {
            if next {
                dom.first_child(n)
            } else {
                dom.last_child(n)
            }
        };
        loop {
            let mut sibling = sib(&self.dom(), node);
            while let Some(s) = sibling {
                node = s;
                let result = self.filter_node(cx, h, node)?;
                if result == FILTER_ACCEPT {
                    self.set_current(h, node);
                    return Ok(Some(node_handle(node)));
                }
                sibling = child(&self.dom(), node);
                if result == FILTER_REJECT || sibling.is_none() {
                    sibling = sib(&self.dom(), node);
                }
            }
            let parent = self.dom().parent(node);
            match parent {
                None => return Ok(None),
                Some(p) if p == root => return Ok(None),
                Some(p) => {
                    node = p;
                    if self.filter_node(cx, h, node)? == FILTER_ACCEPT {
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn iterator(&self, h: Handle) -> (NodeId, NodeId, bool) {
        match &*self.obj(h) {
            Object::NodeIterator(i) => (i.root, i.reference, i.pointer_before_reference),
            _ => panic!("not a NodeIterator"),
        }
    }

    /// DOM §6.1 "traverse" for a NodeIterator.
    fn iterator_traverse(&self, cx: &mut Cx, h: Handle, next: bool) -> rt::Result<Option<Handle>> {
        let (root, mut node, mut before) = self.iterator(h);
        loop {
            if next {
                if !before {
                    match self.dom().next_in_tree_order(node, Some(root)) {
                        Some(n) => node = n,
                        None => return Ok(None),
                    }
                } else {
                    before = false;
                }
            } else if before {
                match self.dom().previous_in_tree_order(node, Some(root)) {
                    Some(n) => node = n,
                    None => return Ok(None),
                }
            } else {
                before = true;
            }
            if self.filter_node(cx, h, node)? == FILTER_ACCEPT {
                break;
            }
        }
        if let Object::NodeIterator(i) = &mut *self.obj_mut(h) {
            i.reference = node;
            i.pointer_before_reference = before;
        }
        Ok(Some(node_handle(node)))
    }

    /// DOM §6.1 "NodeIterator pre-removing steps", from the removed node's old position.
    pub(crate) fn node_iterators_removed(
        &self,
        removed: NodeId,
        old_parent: NodeId,
        old_previous: Option<NodeId>,
        old_next: Option<NodeId>,
    ) {
        if self.iterators.borrow().is_empty() {
            return;
        }
        let iterators = self.iterators.borrow().clone();
        for h in iterators {
            let (root, reference, before) = self.iterator(h);
            let dom = self.dom();
            if !dom.is_inclusive_ancestor_of(removed, reference) || removed == root {
                continue;
            }
            if !dom.is_inclusive_ancestor_of(root, old_parent) {
                continue;
            }
            let mut new_ref = None;
            let mut new_before = before;
            if before {
                // The first following node that is in root and not in the removed subtree.
                let following = old_next.or_else(|| {
                    let mut cur = old_parent;
                    loop {
                        if cur == root {
                            break None;
                        }
                        if let Some(s) = dom.next_sibling(cur) {
                            break Some(s);
                        }
                        cur = dom.parent(cur)?;
                    }
                });
                if let Some(f) = following.filter(|&f| dom.is_inclusive_ancestor_of(root, f)) {
                    new_ref = Some(f);
                } else {
                    new_before = false;
                }
            }
            let new_ref = new_ref.unwrap_or_else(|| match old_previous {
                None => old_parent,
                Some(p) => {
                    let mut last = p;
                    while let Some(c) = dom.last_child(last) {
                        last = c;
                    }
                    last
                }
            });
            drop(dom);
            if let Object::NodeIterator(i) = &mut *self.obj_mut(h) {
                i.reference = new_ref;
                i.pointer_before_reference = new_before;
            }
        }
    }
}

impl TreeWalkerImpl for Engine {
    fn root(&self, this: Handle) -> Handle {
        node_handle(self.walker(this).0)
    }
    fn what_to_show(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::TreeWalker(w) => w.what_to_show,
            _ => 0,
        }
    }
    fn filter(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<NodeFilter>> {
        Ok(match &*self.obj(this) {
            Object::TreeWalker(w) => w.filter.clone(),
            _ => None,
        })
    }
    fn current_node(&self, this: Handle) -> Handle {
        node_handle(self.walker(this).1)
    }
    fn set_current_node(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.set_current(this, self.n(value));
        Ok(())
    }
    fn parent_node(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        let (root, mut node) = self.walker(this);
        while node != root {
            let Some(p) = self.dom().parent(node) else {
                break;
            };
            node = p;
            if self.filter_node(cx, this, node)? == FILTER_ACCEPT {
                self.set_current(this, node);
                return Ok(Some(node_handle(node)));
            }
        }
        Ok(None)
    }
    fn first_child(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        self.traverse_children(cx, this, true)
    }
    fn last_child(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        self.traverse_children(cx, this, false)
    }
    fn previous_sibling(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        self.traverse_siblings(cx, this, false)
    }
    fn next_sibling(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        self.traverse_siblings(cx, this, true)
    }
    /// DOM §6.2 `previousNode()`.
    fn previous_node(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        let (root, mut node) = self.walker(this);
        while node != root {
            let mut sibling = self.dom().previous_sibling(node);
            while let Some(s) = sibling {
                node = s;
                let mut result = self.filter_node(cx, this, node)?;
                while result != FILTER_REJECT {
                    let Some(last) = self.dom().last_child(node) else {
                        break;
                    };
                    node = last;
                    result = self.filter_node(cx, this, node)?;
                }
                if result == FILTER_ACCEPT {
                    self.set_current(this, node);
                    return Ok(Some(node_handle(node)));
                }
                sibling = self.dom().previous_sibling(node);
            }
            if node == root {
                return Ok(None);
            }
            let Some(p) = self.dom().parent(node) else {
                return Ok(None);
            };
            node = p;
            if self.filter_node(cx, this, node)? == FILTER_ACCEPT {
                self.set_current(this, node);
                return Ok(Some(node_handle(node)));
            }
        }
        Ok(None)
    }
    /// DOM §6.2 `nextNode()`.
    fn next_node(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        let (root, mut node) = self.walker(this);
        let mut result = FILTER_ACCEPT;
        loop {
            while result != FILTER_REJECT {
                let Some(c) = self.dom().first_child(node) else {
                    break;
                };
                node = c;
                result = self.filter_node(cx, this, node)?;
                if result == FILTER_ACCEPT {
                    self.set_current(this, node);
                    return Ok(Some(node_handle(node)));
                }
            }
            let mut temporary = node;
            let sibling = loop {
                if temporary == root {
                    return Ok(None);
                }
                if let Some(s) = self.dom().next_sibling(temporary) {
                    break s;
                }
                match self.dom().parent(temporary) {
                    Some(p) => temporary = p,
                    None => return Ok(None),
                }
            };
            node = sibling;
            result = self.filter_node(cx, this, node)?;
            if result == FILTER_ACCEPT {
                self.set_current(this, node);
                return Ok(Some(node_handle(node)));
            }
        }
    }
}

impl NodeIteratorImpl for Engine {
    fn root(&self, this: Handle) -> Handle {
        node_handle(self.iterator(this).0)
    }
    fn reference_node(&self, this: Handle) -> Handle {
        node_handle(self.iterator(this).1)
    }
    fn pointer_before_reference_node(&self, this: Handle) -> bool {
        self.iterator(this).2
    }
    fn what_to_show(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::NodeIterator(i) => i.what_to_show,
            _ => 0,
        }
    }
    fn filter(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<NodeFilter>> {
        Ok(match &*self.obj(this) {
            Object::NodeIterator(i) => i.filter.clone(),
            _ => None,
        })
    }
    fn next_node(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        self.iterator_traverse(cx, this, true)
    }
    fn previous_node(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        self.iterator_traverse(cx, this, false)
    }
    fn detach(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
}
