// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mutation algorithms (DOM §4.2.3) and the `ParentNode` / `ChildNode` convenience methods (DOM §4.2.6,
//! §4.2.8), plus "adopt" (DOM §4.5).
//!
//! Not yet here (they belong to layers that do not exist yet): live range and NodeIterator updates
//! (§5, §6.1), slot assignment (§4.2.2.3), custom element reactions (raised as [`Reaction`]s instead),
//! the state-preserving `moveBefore()`.

use smallvec::SmallVec;

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::node::{NodeData, NodeId, NodeType};
use crate::records::Reaction;

/// An argument of `ParentNode.append()` and friends: `(Node or DOMString)`.
#[derive(Clone, Debug)]
pub enum NodeOrString {
    Node(NodeId),
    String(String),
}

type NodeList = SmallVec<[NodeId; 8]>;

impl Dom {
    // ----- links -------------------------------------------------------------------------------------------

    pub(crate) fn link_before(&mut self, node: NodeId, parent: NodeId, before: Option<NodeId>) {
        debug_assert!(self.n(node).parent.is_none());
        let prev = match before {
            Some(b) => self.n(b).prev_sibling,
            None => self.n(parent).last_child,
        };
        {
            let n = self.n_mut(node);
            n.parent = Some(parent);
            n.prev_sibling = prev;
            n.next_sibling = before;
        }
        match prev {
            Some(p) => self.n_mut(p).next_sibling = Some(node),
            None => self.n_mut(parent).first_child = Some(node),
        }
        match before {
            Some(b) => self.n_mut(b).prev_sibling = Some(node),
            None => self.n_mut(parent).last_child = Some(node),
        }
        let bits = self.subtree_filter(node);
        self.add_filter_bits(parent, bits);
    }

    fn unlink(&mut self, node: NodeId) {
        let (parent, prev, next) = {
            let n = self.n_mut(node);
            let r = (
                n.parent.take(),
                n.prev_sibling.take(),
                n.next_sibling.take(),
            );
            (r.0.expect("unlink without parent"), r.1, r.2)
        };
        match prev {
            Some(p) => self.n_mut(p).next_sibling = next,
            None => self.n_mut(parent).first_child = next,
        }
        match next {
            Some(n) => self.n_mut(n).prev_sibling = prev,
            None => self.n_mut(parent).last_child = prev,
        }
    }

    /// The fragment parsing algorithm's last steps for nodes nobody can observe yet (built in the
    /// parser's own document, with tree reactions quiet): moves `from`'s children to the end of
    /// `fragment`, whose node document `document` becomes theirs, in one walk. What "insert" (with its
    /// adoption) and "remove" would leave, without their per-node bookkeeping: links, node
    /// documents, CONNECTED flags cleared, template contents adopted.
    pub(crate) fn move_parsed_children(
        &mut self,
        from: NodeId,
        fragment: NodeId,
        document: NodeId,
    ) {
        debug_assert!(self.quiet_tree_reactions);
        let mut templates: SmallVec<[NodeId; 4]> = SmallVec::new();
        while let Some(child) = self.first_child(from) {
            self.unlink(child);
            let mut cur = Some(child);
            while let Some(id) = cur {
                // (disconnected first: the count of connected bases is the old document's)
                self.set_connected(id, false);
                self.set_document(id, Some(document));
                if let NodeData::Element(e) = &self.n(id).data {
                    if e.template_contents.is_some() {
                        templates.push(id);
                    }
                    debug_assert!(e.attrs.iter().all(|a| a.node.is_none()));
                }
                cur = self.next_in_shadow_including_order(id, child);
            }
            self.link_before(child, fragment, None);
        }
        for t in templates {
            let owner = self.template_contents_owner_document(document);
            if let Some(contents) = self.template_contents(t) {
                self.adopt(contents, owner);
            }
        }
        self.bump_tree(from);
        self.bump_tree(fragment);
    }

    /// Sets the CONNECTED flag on `root`'s shadow-including inclusive descendants and raises insertion
    /// reactions for them.
    fn after_insert_walk(&mut self, root: NodeId, connected: bool) {
        if !connected && !self.reactions_enabled {
            return;
        }
        let mut cur = Some(root);
        while let Some(id) = cur {
            if connected {
                self.set_connected(id, true);
            }
            if id == root || self.reports_descendant(id) {
                self.react(|| Reaction::Inserted {
                    node: id,
                    connected,
                    root: id == root,
                });
            }
            cur = self.next_in_shadow_including_order(id, root);
        }
    }

    fn after_remove_walk(
        &mut self,
        root: NodeId,
        old_parent: NodeId,
        old_siblings: (Option<NodeId>, Option<NodeId>),
        was_connected: bool,
    ) {
        if !was_connected && !self.reactions_enabled {
            return;
        }
        // (the root's reaction, if queued, is at or after this index; its descendants are collected
        // as the walk goes)
        let root_reaction = self.reactions.len();
        let collect = self.reactions_enabled;
        let start = self.removed_descendants.len() as u32;
        let mut cur = Some(root);
        while let Some(id) = cur {
            self.set_connected(id, false);
            if collect && id != root {
                self.removed_descendants.push(id);
            }
            if id == root || self.reports_descendant(id) {
                let parent = (id == root).then_some(old_parent);
                let (old_previous, old_next) = if id == root {
                    old_siblings
                } else {
                    (None, None)
                };
                self.react(|| Reaction::Removed {
                    node: id,
                    old_parent: parent,
                    old_previous,
                    old_next,
                    was_connected,
                    descendants: (start, start),
                });
            }
            cur = self.next_in_shadow_including_order(id, root);
        }
        let end = self.removed_descendants.len() as u32;
        let own = self.reactions.get_mut(root_reaction..).and_then(|queued| {
            queued.iter_mut().find_map(|r| match r {
                Reaction::Removed {
                    node, descendants, ..
                } if *node == root => Some(descendants),
                _ => None,
            })
        });
        if let Some(descendants) = own {
            *descendants = (start, end);
        }
    }

    fn child_list(&self, node: NodeId) -> NodeList {
        self.children(node).collect()
    }

    // ----- validity ----------------------------------------------------------------------------------------

    /// DOM §4.2.3 "ensure pre-insert validity" of `node` into `parent` before `child`.
    pub fn ensure_pre_insert_validity(
        &self,
        node: NodeId,
        parent: NodeId,
        child: Option<NodeId>,
    ) -> DomResult<()> {
        // 1. parent must be a Document, DocumentFragment or Element.
        if !matches!(
            self.node_type(parent),
            NodeType::Document | NodeType::DocumentFragment | NodeType::Element
        ) {
            return Err(DomError::HierarchyRequest);
        }
        // 2. node must not be a host-including inclusive ancestor of parent.
        if self.is_host_including_inclusive_ancestor_of(node, parent) {
            return Err(DomError::HierarchyRequest);
        }
        // 3. child must be a child of parent.
        if let Some(c) = child {
            if self.parent(c) != Some(parent) {
                return Err(DomError::NotFound);
            }
        }
        // 4. node must be a DocumentFragment, DocumentType, Element or CharacterData node.
        let node_type = self.node_type(node);
        if matches!(node_type, NodeType::Document | NodeType::Attribute) {
            return Err(DomError::HierarchyRequest);
        }
        // 5. No Text in a Document, no doctype outside one.
        let parent_is_document = self.is_document(parent);
        if (self.is_exclusive_text(node) || matches!(self.data(node), NodeData::CDataSection(_)))
            && parent_is_document
        {
            return Err(DomError::HierarchyRequest);
        }
        if node_type == NodeType::DocumentType && !parent_is_document {
            return Err(DomError::HierarchyRequest);
        }
        // 6. Document constraints.
        if parent_is_document {
            self.check_document_child(node, parent, child, None)?;
        }
        Ok(())
    }

    /// The step-6 checks shared by pre-insert (`replacing` = None) and replace (`replacing` = the child).
    fn check_document_child(
        &self,
        node: NodeId,
        parent: NodeId,
        child: Option<NodeId>,
        replacing: Option<NodeId>,
    ) -> DomResult<()> {
        let is_doctype = |d: &Dom, n: NodeId| d.node_type(n) == NodeType::DocumentType;
        let has_element_child = self
            .children(parent)
            .any(|c| self.is_element(c) && Some(c) != replacing);
        let doctype_following = |d: &Dom, c: NodeId| {
            let mut cur = d.next_sibling(c);
            while let Some(s) = cur {
                if is_doctype(d, s) {
                    return true;
                }
                cur = d.next_sibling(s);
            }
            false
        };
        let element_preceding = |d: &Dom, c: NodeId| {
            let mut cur = d.previous_sibling(c);
            while let Some(s) = cur {
                if d.is_element(s) {
                    return true;
                }
                cur = d.previous_sibling(s);
            }
            false
        };
        let child_is_doctype = child.is_some_and(|c| is_doctype(self, c));
        let element_check = |d: &Dom| -> DomResult<()> {
            if has_element_child {
                return Err(DomError::HierarchyRequest);
            }
            if replacing.is_none() && child_is_doctype {
                return Err(DomError::HierarchyRequest);
            }
            if child.is_some_and(|c| doctype_following(d, c)) {
                return Err(DomError::HierarchyRequest);
            }
            Ok(())
        };
        match self.node_type(node) {
            NodeType::DocumentFragment => {
                let mut elements = 0;
                for c in self.children(node) {
                    if self.is_element(c) {
                        elements += 1;
                    } else if self.is_text(c) {
                        return Err(DomError::HierarchyRequest);
                    }
                }
                if elements > 1 {
                    return Err(DomError::HierarchyRequest);
                }
                if elements == 1 {
                    element_check(self)?;
                }
            }
            NodeType::Element => element_check(self)?,
            NodeType::DocumentType => {
                if self
                    .children(parent)
                    .any(|c| is_doctype(self, c) && Some(c) != replacing)
                {
                    return Err(DomError::HierarchyRequest);
                }
                match child {
                    Some(c) => {
                        if element_preceding(self, c) {
                            return Err(DomError::HierarchyRequest);
                        }
                    }
                    None => {
                        if has_element_child {
                            return Err(DomError::HierarchyRequest);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ----- the algorithms ----------------------------------------------------------------------------------

    /// DOM §4.2.3 "pre-insert" `node` into `parent` before `child`; `Node.insertBefore(node, child)`.
    pub fn insert_before(
        &mut self,
        parent: NodeId,
        node: NodeId,
        child: Option<NodeId>,
    ) -> DomResult<NodeId> {
        self.ensure_pre_insert_validity(node, parent, child)?;
        let mut reference = child;
        if reference == Some(node) {
            reference = self.next_sibling(node);
        }
        self.insert(node, parent, reference, false);
        Ok(node)
    }

    /// DOM §4.2.3 "append"; `Node.appendChild(node)`.
    pub fn append_child(&mut self, parent: NodeId, node: NodeId) -> DomResult<NodeId> {
        self.insert_before(parent, node, None)
    }

    /// DOM §4.2.3 "insert" `node` into `parent` before `child`, without validity checks. Callers (the parser,
    /// the other algorithms) guarantee validity.
    pub fn insert(
        &mut self,
        node: NodeId,
        parent: NodeId,
        child: Option<NodeId>,
        suppress_observers: bool,
    ) {
        // 1-2. The nodes to insert.
        let is_fragment = self.is_document_fragment(node);
        let nodes: NodeList = if is_fragment {
            self.child_list(node)
        } else {
            SmallVec::from_slice(&[node])
        };
        // 3.
        if nodes.is_empty() {
            return;
        }
        // 4. A fragment's children are removed from it first.
        if is_fragment && self.unreached_fragment == Some(node) {
            // The fragment parsing algorithm's own fragment, which no script has seen: no observer,
            // live range, NodeIterator or custom element can be on it, and none of its nodes is
            // connected, so their removing steps concern nobody (as the parser's own moves,
            // `move_parsed_children`). The insertion below reports the nodes.
            for &c in &nodes {
                self.unlink(c);
            }
        } else if is_fragment {
            for &c in &nodes {
                self.remove(c, true);
            }
            self.queue_tree_record(node, &[], &nodes, None, None);
        }
        // 5. (live ranges: not yet)
        // 6.
        let previous_sibling = match child {
            Some(c) => self.previous_sibling(c),
            None => self.last_child(parent),
        };
        let parent_document = self.node_document(parent);
        let parent_connected = self.is_connected(parent);
        // 7.
        for &n in &nodes {
            self.adopt(n, parent_document);
            self.link_before(n, parent, child);
            self.after_insert_walk(n, parent_connected);
        }
        // 8.
        if !suppress_observers {
            self.queue_tree_record(parent, &nodes, &[], previous_sibling, child);
        }
        // 9.
        self.react(|| Reaction::ChildrenChanged { parent });
        self.bump_tree(parent);
    }

    /// DOM §4.2.3 "replace" `child` with `node` within `parent`; `Node.replaceChild(node, child)`.
    pub fn replace_child(
        &mut self,
        parent: NodeId,
        node: NodeId,
        child: NodeId,
    ) -> DomResult<NodeId> {
        // 1.
        if !matches!(
            self.node_type(parent),
            NodeType::Document | NodeType::DocumentFragment | NodeType::Element
        ) {
            return Err(DomError::HierarchyRequest);
        }
        // 2.
        if self.is_host_including_inclusive_ancestor_of(node, parent) {
            return Err(DomError::HierarchyRequest);
        }
        // 3.
        if self.parent(child) != Some(parent) {
            return Err(DomError::NotFound);
        }
        // 4.
        let node_type = self.node_type(node);
        if matches!(node_type, NodeType::Document | NodeType::Attribute) {
            return Err(DomError::HierarchyRequest);
        }
        // 5.
        let parent_is_document = self.is_document(parent);
        if self.is_text(node) && parent_is_document {
            return Err(DomError::HierarchyRequest);
        }
        if node_type == NodeType::DocumentType && !parent_is_document {
            return Err(DomError::HierarchyRequest);
        }
        // 6.
        if parent_is_document {
            self.check_document_child(node, parent, Some(child), Some(child))?;
        }
        // 7-8.
        let mut reference = self.next_sibling(child);
        if reference == Some(node) {
            reference = self.next_sibling(node);
        }
        // 9-11.
        let previous_sibling = self.previous_sibling(child);
        let mut removed = NodeList::new();
        if self.parent(child).is_some() {
            removed.push(child);
            self.remove(child, true);
        }
        // 12.
        let nodes: NodeList = if self.is_document_fragment(node) {
            self.child_list(node)
        } else {
            SmallVec::from_slice(&[node])
        };
        // 13.
        self.insert(node, parent, reference, true);
        // 14.
        if !nodes.is_empty() || !removed.is_empty() {
            self.queue_tree_record(parent, &nodes, &removed, previous_sibling, reference);
        }
        Ok(child)
    }

    /// DOM §4.2.3 "replace all" with `node` within `parent`.
    pub fn replace_all(&mut self, node: Option<NodeId>, parent: NodeId) {
        // The node lists are only kept for an observer's record.
        let record = self.wants_records(parent);
        let mut removed = Vec::new();
        let mut added = Vec::new();
        if record {
            removed.extend(self.children(parent));
            match node {
                Some(n) if self.is_document_fragment(n) => added.extend(self.children(n)),
                Some(n) => added.push(n),
                None => {}
            }
        }
        let had_children = self.first_child(parent).is_some();
        while let Some(c) = self.first_child(parent) {
            self.remove_inner(c, true, false);
        }
        // (insert runs the children changed steps; they run once if it inserts nothing)
        let inserts =
            node.is_some_and(|n| !self.is_document_fragment(n) || self.has_child_nodes(n));
        if let Some(n) = node {
            self.insert(n, parent, None, true);
        }
        if had_children && !inserts {
            self.react(|| Reaction::ChildrenChanged { parent });
        }
        if !added.is_empty() || !removed.is_empty() {
            self.queue_tree_record(parent, &added, &removed, None, None);
        }
    }

    /// DOM §4.2.3 "pre-remove" `child` from `parent`; `Node.removeChild(child)`.
    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> DomResult<NodeId> {
        if self.parent(child) != Some(parent) {
            return Err(DomError::NotFound);
        }
        self.remove(child, false);
        Ok(child)
    }

    /// DOM §4.2.3 "remove" `node` from its parent. Panics if it has none.
    pub fn remove(&mut self, node: NodeId, suppress_observers: bool) {
        self.remove_inner(node, suppress_observers, true);
    }

    /// "Remove", with the children changed steps left to the caller when `children_changed` is
    /// false ("replace all" runs them once for all the children it removes and inserts: their
    /// subjects look at the parent's children as they end up).
    fn remove_inner(&mut self, node: NodeId, suppress_observers: bool, children_changed: bool) {
        // 1.
        let parent = self.parent(node).expect("remove: node has no parent");
        // 3-7. (live ranges and NodeIterators: not yet)
        // 8-9.
        let old_previous = self.previous_sibling(node);
        let old_next = self.next_sibling(node);
        let was_connected = self.is_connected(node);
        // 10.
        self.unlink(node);
        // 11-13. (slots: not yet)
        // 14-17. Removing steps, disconnected reactions, CONNECTED flags.
        self.after_remove_walk(node, parent, (old_previous, old_next), was_connected);
        // 18.
        self.add_transient_observers(node, parent);
        // 19.
        if !suppress_observers {
            self.queue_tree_record(parent, &[], &[node], old_previous, old_next);
        }
        // 20-21. (child text content change steps are a ChildrenChanged reaction)
        if children_changed {
            self.react(|| Reaction::ChildrenChanged { parent });
        }
        self.bump_tree(parent);
    }

    /// `ChildNode.remove()`: removes `node` from its parent, if any.
    pub fn remove_node(&mut self, node: NodeId) {
        if self.parent(node).is_some() {
            self.remove(node, false);
        }
    }

    /// DOM §4.5 "adopt" `node` into `document`.
    pub fn adopt(&mut self, node: NodeId, document: NodeId) {
        // 1.
        let old_document = self.node_document(node);
        // 2.
        if self.parent(node).is_some() {
            self.remove(node, false);
        }
        // 3.
        if document == old_document {
            return;
        }
        let mut cur = Some(node);
        let mut templates: SmallVec<[NodeId; 4]> = SmallVec::new();
        while let Some(id) = cur {
            self.set_document(id, Some(document));
            if let NodeData::Element(e) = &self.n(id).data {
                let attr_nodes: SmallVec<[NodeId; 4]> =
                    e.attrs.iter().filter_map(|a| a.node).collect();
                if e.template_contents.is_some() {
                    templates.push(id);
                }
                for a in attr_nodes {
                    self.set_document(a, Some(document));
                }
            }
            if id == node || self.reports_descendant(id) {
                self.react(|| Reaction::Adopted {
                    node: id,
                    old_document,
                });
            }
            cur = self.next_in_shadow_including_order(id, node);
        }
        // HTML §4.12.3 the template element's adopting steps: its contents move to the new document's
        // template contents owner document.
        for t in templates {
            let owner = self.template_contents_owner_document(document);
            if let Some(contents) = self.template_contents(t) {
                self.adopt(contents, owner);
            }
        }
        self.bump_tree(node);
    }

    /// `Document.adoptNode(node)` (DOM §4.5).
    pub fn adopt_node(&mut self, document: NodeId, node: NodeId) -> DomResult<NodeId> {
        if self.is_document(node) {
            return Err(DomError::NotSupported);
        }
        if self.is_shadow_root(node) {
            return Err(DomError::HierarchyRequest);
        }
        if self.is_document_fragment(node) && self.host(node).is_some() {
            return Ok(node);
        }
        self.adopt(node, document);
        Ok(node)
    }

    // ----- ParentNode / ChildNode -----------------------------------------------------------------------

    /// DOM §4.2.6 "converting nodes into a node".
    pub fn convert_nodes_into_node(
        &mut self,
        nodes: Vec<NodeOrString>,
        document: NodeId,
    ) -> DomResult<NodeId> {
        let mut ids = Vec::with_capacity(nodes.len());
        for n in nodes {
            ids.push(match n {
                NodeOrString::Node(id) => id,
                NodeOrString::String(s) => self.create_text_node(document, &s),
            });
        }
        if ids.len() == 1 {
            return Ok(ids[0]);
        }
        let fragment = self.create_document_fragment(document);
        for id in ids {
            self.append_child(fragment, id)?;
        }
        Ok(fragment)
    }

    fn in_list(nodes: &[NodeOrString], id: NodeId) -> bool {
        nodes
            .iter()
            .any(|n| matches!(n, NodeOrString::Node(x) if *x == id))
    }

    /// `ParentNode.prepend(...nodes)`.
    pub fn prepend(&mut self, parent: NodeId, nodes: Vec<NodeOrString>) -> DomResult<()> {
        let doc = self.node_document(parent);
        let node = self.convert_nodes_into_node(nodes, doc)?;
        let first = self.first_child(parent);
        self.insert_before(parent, node, first)?;
        Ok(())
    }

    /// `ParentNode.append(...nodes)`.
    pub fn append(&mut self, parent: NodeId, nodes: Vec<NodeOrString>) -> DomResult<()> {
        let doc = self.node_document(parent);
        let node = self.convert_nodes_into_node(nodes, doc)?;
        self.append_child(parent, node)?;
        Ok(())
    }

    /// `ParentNode.replaceChildren(...nodes)`.
    pub fn replace_children(&mut self, parent: NodeId, nodes: Vec<NodeOrString>) -> DomResult<()> {
        let doc = self.node_document(parent);
        let node = self.convert_nodes_into_node(nodes, doc)?;
        self.ensure_pre_insert_validity(node, parent, None)?;
        self.replace_all(Some(node), parent);
        Ok(())
    }

    /// `ChildNode.before(...nodes)`.
    pub fn before(&mut self, node: NodeId, nodes: Vec<NodeOrString>) -> DomResult<()> {
        let Some(parent) = self.parent(node) else {
            return Ok(());
        };
        let mut viable_prev = self.previous_sibling(node);
        while let Some(p) = viable_prev {
            if !Self::in_list(&nodes, p) {
                break;
            }
            viable_prev = self.previous_sibling(p);
        }
        let doc = self.node_document(node);
        let new = self.convert_nodes_into_node(nodes, doc)?;
        let reference = match viable_prev {
            None => self.first_child(parent),
            Some(p) => self.next_sibling(p),
        };
        self.insert_before(parent, new, reference)?;
        Ok(())
    }

    fn viable_next_sibling(&self, node: NodeId, nodes: &[NodeOrString]) -> Option<NodeId> {
        let mut next = self.next_sibling(node);
        while let Some(n) = next {
            if !Self::in_list(nodes, n) {
                break;
            }
            next = self.next_sibling(n);
        }
        next
    }

    /// `ChildNode.after(...nodes)`.
    pub fn after(&mut self, node: NodeId, nodes: Vec<NodeOrString>) -> DomResult<()> {
        let Some(parent) = self.parent(node) else {
            return Ok(());
        };
        let viable_next = self.viable_next_sibling(node, &nodes);
        let doc = self.node_document(node);
        let new = self.convert_nodes_into_node(nodes, doc)?;
        self.insert_before(parent, new, viable_next)?;
        Ok(())
    }

    /// `ChildNode.replaceWith(...nodes)`.
    pub fn replace_with(&mut self, node: NodeId, nodes: Vec<NodeOrString>) -> DomResult<()> {
        let Some(parent) = self.parent(node) else {
            return Ok(());
        };
        let viable_next = self.viable_next_sibling(node, &nodes);
        let doc = self.node_document(node);
        let new = self.convert_nodes_into_node(nodes, doc)?;
        if self.parent(node) == Some(parent) {
            self.replace_child(parent, new, node)?;
        } else {
            self.insert_before(parent, new, viable_next)?;
        }
        Ok(())
    }
}
