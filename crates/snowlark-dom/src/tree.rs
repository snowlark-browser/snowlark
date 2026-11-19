// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Trees (DOM §1.1, §4.2.1): navigation, tree order, iterators, `contains`, `compareDocumentPosition`.

use std::cmp::Ordering;

use smallvec::SmallVec;

use crate::dom::Dom;
use crate::node::{NodeData, NodeId};

/// `Node.DOCUMENT_POSITION_*` bits.
pub mod position {
    pub const DISCONNECTED: u16 = 0x01;
    pub const PRECEDING: u16 = 0x02;
    pub const FOLLOWING: u16 = 0x04;
    pub const CONTAINS: u16 = 0x08;
    pub const CONTAINED_BY: u16 = 0x10;
    pub const IMPLEMENTATION_SPECIFIC: u16 = 0x20;
}

/// Children of a node, first to last.
#[derive(Clone)]
pub struct Children<'a> {
    dom: &'a Dom,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        self.next = self.dom.n(id).next_sibling;
        Some(id)
    }
}

/// Children of a node, last to first.
#[derive(Clone)]
pub struct ChildrenRev<'a> {
    dom: &'a Dom,
    next: Option<NodeId>,
}

impl Iterator for ChildrenRev<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        self.next = self.dom.n(id).prev_sibling;
        Some(id)
    }
}

/// Ancestors (parent chain), nearest first.
#[derive(Clone)]
pub struct Ancestors<'a> {
    dom: &'a Dom,
    next: Option<NodeId>,
    shadow_including: bool,
}

impl Iterator for Ancestors<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        self.next = if self.shadow_including {
            self.dom.shadow_including_parent(id)
        } else {
            self.dom.n(id).parent
        };
        Some(id)
    }
}

/// Descendants of a root in tree order (preorder, depth-first), optionally shadow-including.
#[derive(Clone)]
pub struct Descendants<'a> {
    dom: &'a Dom,
    root: NodeId,
    next: Option<NodeId>,
    shadow_including: bool,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        self.next = if self.shadow_including {
            self.dom.next_in_shadow_including_order(id, self.root)
        } else {
            self.dom.next_in_tree_order(id, Some(self.root))
        };
        Some(id)
    }
}

/// Nodes preceding a node in tree order, nearest first (reverse tree order), within its root.
#[derive(Clone)]
pub struct Preceding<'a> {
    dom: &'a Dom,
    next: Option<NodeId>,
}

impl Iterator for Preceding<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        self.next = self.dom.previous_in_tree_order(id, None);
        Some(id)
    }
}

impl Dom {
    /// The children of `id`.
    #[inline]
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            dom: self,
            next: self.n(id).first_child,
        }
    }

    /// The children of `id`, last first.
    #[inline]
    pub fn children_rev(&self, id: NodeId) -> ChildrenRev<'_> {
        ChildrenRev {
            dom: self,
            next: self.n(id).last_child,
        }
    }

    /// The element children of `id` (`ParentNode.children`).
    pub fn element_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id).filter(move |&c| self.is_element(c))
    }

    /// Number of children (`childNodes.length`).
    pub fn child_count(&self, id: NodeId) -> usize {
        self.children(id).count()
    }

    /// The child at `index` (`childNodes[index]`).
    pub fn child_at(&self, id: NodeId, index: usize) -> Option<NodeId> {
        self.children(id).nth(index)
    }

    /// The node's index: the number of preceding siblings (DOM §1.1).
    pub fn index(&self, id: NodeId) -> usize {
        let mut count = 0;
        let mut cur = self.n(id).prev_sibling;
        while let Some(c) = cur {
            count += 1;
            cur = self.n(c).prev_sibling;
        }
        count
    }

    /// `ParentNode.firstElementChild`.
    pub fn first_element_child(&self, id: NodeId) -> Option<NodeId> {
        self.children(id).find(|&c| self.is_element(c))
    }

    /// `ParentNode.lastElementChild`.
    pub fn last_element_child(&self, id: NodeId) -> Option<NodeId> {
        self.children_rev(id).find(|&c| self.is_element(c))
    }

    /// `ParentNode.childElementCount`.
    pub fn child_element_count(&self, id: NodeId) -> usize {
        self.element_children(id).count()
    }

    /// `NonDocumentTypeChildNode.previousElementSibling`.
    pub fn previous_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut cur = self.n(id).prev_sibling;
        while let Some(c) = cur {
            if self.is_element(c) {
                return Some(c);
            }
            cur = self.n(c).prev_sibling;
        }
        None
    }

    /// `NonDocumentTypeChildNode.nextElementSibling`.
    pub fn next_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut cur = self.n(id).next_sibling;
        while let Some(c) = cur {
            if self.is_element(c) {
                return Some(c);
            }
            cur = self.n(c).next_sibling;
        }
        None
    }

    /// Ancestors of `id`, nearest first.
    pub fn ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors {
            dom: self,
            next: self.n(id).parent,
            shadow_including: false,
        }
    }

    /// `id` followed by its ancestors.
    pub fn inclusive_ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors {
            dom: self,
            next: Some(id),
            shadow_including: false,
        }
    }

    /// Shadow-including ancestors: the parent, or for a shadow root its host (DOM §4.8).
    pub fn shadow_including_ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors {
            dom: self,
            next: self.shadow_including_parent(id),
            shadow_including: true,
        }
    }

    /// `id` followed by its shadow-including ancestors.
    pub fn shadow_including_inclusive_ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors {
            dom: self,
            next: Some(id),
            shadow_including: true,
        }
    }

    #[inline]
    pub(crate) fn shadow_including_parent(&self, id: NodeId) -> Option<NodeId> {
        let n = self.n(id);
        match (&n.data, n.parent) {
            (_, Some(p)) => Some(p),
            (NodeData::ShadowRoot(s), None) => Some(s.host),
            _ => None,
        }
    }

    /// Descendants of `root` in tree order, excluding `root`.
    pub fn descendants(&self, root: NodeId) -> Descendants<'_> {
        Descendants {
            dom: self,
            root,
            next: self.n(root).first_child,
            shadow_including: false,
        }
    }

    /// `root` and its descendants in tree order.
    pub fn inclusive_descendants(&self, root: NodeId) -> Descendants<'_> {
        Descendants {
            dom: self,
            root,
            next: Some(root),
            shadow_including: false,
        }
    }

    /// Shadow-including descendants of `root` in shadow-including tree order (DOM §4.8), excluding `root`.
    pub fn shadow_including_descendants(&self, root: NodeId) -> Descendants<'_> {
        Descendants {
            dom: self,
            root,
            next: self.next_in_shadow_including_order(root, root),
            shadow_including: true,
        }
    }

    /// `root` and its shadow-including descendants in shadow-including tree order.
    pub fn shadow_including_inclusive_descendants(&self, root: NodeId) -> Descendants<'_> {
        Descendants {
            dom: self,
            root,
            next: Some(root),
            shadow_including: true,
        }
    }

    /// Nodes following `id` in tree order, to the end of its tree.
    pub fn following(&self, id: NodeId) -> Descendants<'_> {
        let root = self.root(id);
        Descendants {
            dom: self,
            root,
            next: self.next_in_tree_order(id, Some(root)),
            shadow_including: false,
        }
    }

    /// Nodes preceding `id` in tree order, nearest first.
    pub fn preceding(&self, id: NodeId) -> Preceding<'_> {
        Preceding {
            dom: self,
            next: self.previous_in_tree_order(id, None),
        }
    }

    /// The node after `id` in tree order, staying inside `root` when given.
    #[inline]
    pub fn next_in_tree_order(&self, id: NodeId, root: Option<NodeId>) -> Option<NodeId> {
        let n = self.n(id);
        if n.first_child.is_some() {
            return n.first_child;
        }
        self.next_skipping_children(id, root)
    }

    /// The node after `id`'s subtree in tree order, staying inside `root` when given.
    #[inline]
    pub fn next_skipping_children(&self, id: NodeId, root: Option<NodeId>) -> Option<NodeId> {
        let mut cur = id;
        loop {
            if Some(cur) == root {
                return None;
            }
            let n = self.n(cur);
            if n.next_sibling.is_some() {
                return n.next_sibling;
            }
            cur = n.parent?;
        }
    }

    /// The node before `id` in tree order, staying inside `root` when given.
    pub fn previous_in_tree_order(&self, id: NodeId, root: Option<NodeId>) -> Option<NodeId> {
        if Some(id) == root {
            return None;
        }
        let n = self.n(id);
        match n.prev_sibling {
            Some(mut p) => {
                while let Some(last) = self.n(p).last_child {
                    p = last;
                }
                Some(p)
            }
            None => n.parent,
        }
    }

    pub(crate) fn next_in_shadow_including_order(
        &self,
        id: NodeId,
        root: NodeId,
    ) -> Option<NodeId> {
        let n = self.n(id);
        if let NodeData::Element(e) = &n.data {
            if e.shadow_root.is_some() {
                return e.shadow_root;
            }
        }
        if n.first_child.is_some() {
            return n.first_child;
        }
        self.next_skipping_shadow_including(id, root)
    }

    /// The node after `id` in shadow-including tree order within `root` (a host's shadow root
    /// comes before its children).
    #[inline]
    pub fn next_in_shadow_including_tree_order(&self, id: NodeId, root: NodeId) -> Option<NodeId> {
        self.next_in_shadow_including_order(id, root)
    }

    /// The node after `id`'s shadow-including subtree (its descendants and shadow tree) in
    /// shadow-including tree order within `root`.
    pub fn next_skipping_shadow_including(&self, id: NodeId, root: NodeId) -> Option<NodeId> {
        let mut cur = id;
        loop {
            if cur == root {
                return None;
            }
            let n = self.n(cur);
            if n.next_sibling.is_some() {
                return n.next_sibling;
            }
            match (n.parent, &n.data) {
                (Some(p), _) => cur = p,
                (None, NodeData::ShadowRoot(s)) => {
                    // The shadow tree is done: continue with the host's children.
                    let host = s.host;
                    if let Some(c) = self.n(host).first_child {
                        return Some(c);
                    }
                    cur = host;
                }
                (None, _) => return None,
            }
        }
    }

    /// The root of `id`'s tree (DOM §1.1).
    pub fn root(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while let Some(p) = self.n(cur).parent {
            cur = p;
        }
        cur
    }

    /// The shadow-including root (DOM §4.8).
    pub fn shadow_including_root(&self, id: NodeId) -> NodeId {
        let mut root = self.root(id);
        while let NodeData::ShadowRoot(s) = &self.n(root).data {
            root = self.root(s.host);
        }
        root
    }

    /// `Node.getRootNode({composed})`.
    pub fn get_root_node(&self, id: NodeId, composed: bool) -> NodeId {
        if composed {
            self.shadow_including_root(id)
        } else {
            self.root(id)
        }
    }

    /// Whether `node` is in a shadow tree (its root is a shadow root).
    pub fn is_in_shadow_tree(&self, id: NodeId) -> bool {
        self.is_shadow_root(self.root(id))
    }

    /// `a` is an ancestor of `b`.
    pub fn is_ancestor_of(&self, a: NodeId, b: NodeId) -> bool {
        self.ancestors(b).any(|x| x == a)
    }

    /// `a` is an inclusive ancestor of `b`.
    pub fn is_inclusive_ancestor_of(&self, a: NodeId, b: NodeId) -> bool {
        a == b || self.is_ancestor_of(a, b)
    }

    /// `Node.contains(other)`: `other` is an inclusive descendant of `id`.
    pub fn contains(&self, id: NodeId, other: Option<NodeId>) -> bool {
        other.is_some_and(|o| self.is_inclusive_ancestor_of(id, o))
    }

    /// `a` is a shadow-including inclusive ancestor of `b`.
    pub fn is_shadow_including_inclusive_ancestor_of(&self, a: NodeId, b: NodeId) -> bool {
        self.shadow_including_inclusive_ancestors(b).any(|x| x == a)
    }

    /// DOM §4.7 "host-including inclusive ancestor": inclusive ancestor, or through the root's host.
    pub fn is_host_including_inclusive_ancestor_of(&self, a: NodeId, b: NodeId) -> bool {
        let mut cur = b;
        loop {
            if self.is_inclusive_ancestor_of(a, cur) {
                return true;
            }
            match self.host(self.root(cur)) {
                Some(host) => cur = host,
                None => return false,
            }
        }
    }

    /// Orders two nodes of the same tree in tree order. Nodes of different trees have no tree order;
    /// they still get a consistent one, by their roots, so that `a` before `b` means `b` after `a`:
    /// callers that swap their arguments on `Greater`, like DOM's boundary point comparison, end.
    pub fn tree_order(&self, a: NodeId, b: NodeId) -> Ordering {
        if a == b {
            return Ordering::Equal;
        }
        let path = |mut n: NodeId| {
            let mut v: SmallVec<[NodeId; 32]> = SmallVec::new();
            v.push(n);
            while let Some(p) = self.n(n).parent {
                v.push(p);
                n = p;
            }
            v.reverse();
            v
        };
        let pa = path(a);
        let pb = path(b);
        if pa[0] != pb[0] {
            return pa[0].to_bits().cmp(&pb[0].to_bits());
        }
        let common = pa.iter().zip(pb.iter()).take_while(|(x, y)| x == y).count();
        if common == pa.len() {
            return Ordering::Less; // a is an ancestor of b
        }
        if common == pb.len() {
            return Ordering::Greater;
        }
        // Siblings pa[common] and pb[common]: which comes first?
        let (x, y) = (pa[common], pb[common]);
        let mut cur = self.n(x).next_sibling;
        while let Some(c) = cur {
            if c == y {
                return Ordering::Less;
            }
            cur = self.n(c).next_sibling;
        }
        Ordering::Greater
    }

    /// Orders two nodes of one shadow-including tree in shadow-including tree order (DOM §4.2.2: a
    /// shadow host, then its shadow tree, then its children), by walking up from each node rather
    /// than through the tree. Nodes of different trees compare equal.
    pub fn shadow_including_tree_order(&self, a: NodeId, b: NodeId) -> Ordering {
        if a == b {
            return Ordering::Equal;
        }
        let path = |n: NodeId| {
            let mut v: SmallVec<[NodeId; 32]> =
                self.shadow_including_inclusive_ancestors(n).collect();
            v.reverse();
            v
        };
        let pa = path(a);
        let pb = path(b);
        let common = pa.iter().zip(pb.iter()).take_while(|(x, y)| x == y).count();
        if common == 0 {
            return Ordering::Equal;
        }
        if common == pa.len() {
            return Ordering::Less; // a is a shadow-including ancestor of b
        }
        if common == pb.len() {
            return Ordering::Greater;
        }
        // Children of one node, x and y: its shadow root (the one with no parent) comes before its
        // children, which are in sibling order.
        let (x, y) = (pa[common], pb[common]);
        match (self.n(x).parent.is_none(), self.n(y).parent.is_none()) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let mut cur = self.n(x).next_sibling;
        while let Some(c) = cur {
            if c == y {
                return Ordering::Less;
            }
            cur = self.n(c).next_sibling;
        }
        Ordering::Greater
    }

    /// `Node.compareDocumentPosition(other)` on `id` (DOM §4.4).
    pub fn compare_document_position(&self, id: NodeId, other: NodeId) -> u16 {
        use position::*;
        if id == other {
            return 0;
        }
        let mut node1 = Some(other);
        let mut node2 = Some(id);
        let mut attr1 = None;
        let mut attr2 = None;
        if let NodeData::Attr(a) = &self.n(other).data {
            attr1 = Some(other);
            node1 = a.owner;
        }
        if let NodeData::Attr(a) = &self.n(id).data {
            attr2 = Some(id);
            node2 = a.owner;
            if let (Some(a1), Some(n1), Some(n2)) = (attr1, node1, node2) {
                if n1 == n2 {
                    for attr in &self.el(n2).attrs {
                        if attr.node == Some(a1) {
                            return IMPLEMENTATION_SPECIFIC | PRECEDING;
                        }
                        if attr.node == Some(id) {
                            return IMPLEMENTATION_SPECIFIC | FOLLOWING;
                        }
                    }
                }
            }
        }
        let (Some(n1), Some(n2)) = (node1, node2) else {
            return disconnected(other, id);
        };
        if self.root(n1) != self.root(n2) {
            return disconnected(other, id);
        }
        if (attr1.is_none() && self.is_ancestor_of(n1, n2)) || (n1 == n2 && attr2.is_some()) {
            return CONTAINS | PRECEDING;
        }
        if (attr2.is_none() && self.is_ancestor_of(n2, n1)) || (n1 == n2 && attr1.is_some()) {
            return CONTAINED_BY | FOLLOWING;
        }
        if self.tree_order(n1, n2) == Ordering::Less {
            PRECEDING
        } else {
            FOLLOWING
        }
    }
}

fn disconnected(other: NodeId, this: NodeId) -> u16 {
    use position::*;
    // "Consistent" ordering for disconnected nodes: by arena slot.
    let dir = if other.to_bits() < this.to_bits() {
        PRECEDING
    } else {
        FOLLOWING
    };
    DISCONNECTED | IMPLEMENTATION_SPECIFIC | dir
}
