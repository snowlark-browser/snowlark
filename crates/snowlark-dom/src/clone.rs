// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cloning (DOM §4.4 "clone a node") and node equality (`isEqualNode`).

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::node::{Attr, DocumentData, NodeData, NodeId};
use crate::records::Reaction;
use crate::shadow::ShadowRootInit;

impl Dom {
    /// `Node.cloneNode(subtree)`.
    pub fn clone_node(&mut self, node: NodeId, subtree: bool) -> DomResult<NodeId> {
        if self.is_shadow_root(node) {
            return Err(DomError::NotSupported);
        }
        let document = self.node_document(node);
        Ok(self.clone_a_node(node, document, subtree, None))
    }

    /// `Document.importNode(node, subtree)`.
    pub fn import_node(
        &mut self,
        document: NodeId,
        node: NodeId,
        subtree: bool,
    ) -> DomResult<NodeId> {
        if self.is_document(node) || self.is_shadow_root(node) {
            return Err(DomError::NotSupported);
        }
        Ok(self.clone_a_node(node, document, subtree, None))
    }

    /// DOM §4.4 "clone a node" `node` into `document`, appending the copy to `parent` when given. Iterative,
    /// so deep trees don't exhaust the stack. A Document clones into itself (the copy is its own node
    /// document).
    pub fn clone_a_node(
        &mut self,
        node: NodeId,
        document: NodeId,
        subtree: bool,
        parent: Option<NodeId>,
    ) -> NodeId {
        let mut root_copy = None;
        let mut work = vec![(node, parent)];
        while let Some((src, parent)) = work.pop() {
            let target_doc = match parent {
                Some(p) => self.node_document(p),
                None => document,
            };
            // 2. Clone a single node.
            let copy = self.clone_single_node(src, target_doc);
            root_copy.get_or_insert(copy);
            // 3. Cloning steps (HTML's template step is built in, the rest are reactions).
            self.react(|| Reaction::Cloned {
                node: src,
                copy,
                subtree,
            });
            let mut pending = Vec::new();
            if subtree {
                if let (Some(from), Some(to)) =
                    (self.template_contents(src), self.template_contents(copy))
                {
                    pending.extend(self.children(from).map(|c| (c, Some(to))));
                }
            }
            // 4.
            if let Some(p) = parent {
                self.insert(copy, p, None, false);
            }
            // 5.
            if subtree {
                pending.extend(self.children(src).map(|c| (c, Some(copy))));
            }
            // 6. Clonable shadow roots come along.
            if let Some(shadow) = self.shadow_root(src) {
                let data = self.shadow_root_data(shadow).expect("shadow root").clone();
                if data.clonable {
                    let init = ShadowRootInit {
                        mode: data.mode,
                        delegates_focus: data.delegates_focus,
                        slot_assignment: data.slot_assignment,
                        clonable: true,
                        serializable: data.serializable,
                    };
                    let copy_shadow = self.create_shadow_root(copy, init);
                    if let NodeData::ShadowRoot(s) = &mut self.n_mut(copy_shadow).data {
                        s.declarative = data.declarative;
                    }
                    pending.extend(self.children(shadow).map(|c| (c, Some(copy_shadow))));
                }
            }
            // Children are processed in order: push in reverse onto the stack.
            work.extend(pending.into_iter().rev());
        }
        root_copy.expect("at least the root was cloned")
    }

    /// DOM "clone a single node".
    fn clone_single_node(&mut self, node: NodeId, document: NodeId) -> NodeId {
        match &self.n(node).data {
            NodeData::Element(e) => {
                let name = e.name.clone();
                let is = e.is_value.clone();
                let attrs: Vec<Attr> = e
                    .attrs
                    .iter()
                    .map(|a| Attr {
                        name: a.name.clone(),
                        value: a.value.clone(),
                        node: None,
                    })
                    .collect();
                let copy = self.create_element_with_name(document, name, is);
                // "Append" each attribute: no mutation record (nothing observes the new copy) but the
                // attribute change steps, so event handler content attributes of the copy are live.
                for a in attrs {
                    self.push_parsed_attribute(copy, a);
                }
                copy
            }
            NodeData::Document(d) => {
                let DocumentData {
                    kind,
                    mode,
                    content_type,
                    url,
                    encoding,
                    allow_declarative_shadow_roots,
                    ..
                } = (**d).clone();
                let copy = self.create_document(kind);
                let data = self.doc_mut(copy);
                data.mode = mode;
                data.content_type = content_type;
                data.url = url;
                data.encoding = encoding;
                data.allow_declarative_shadow_roots = allow_declarative_shadow_roots;
                copy
            }
            NodeData::DocumentType(d) => {
                let d = d.clone();
                self.create_doctype_unchecked(document, &d.name, &d.public_id, &d.system_id)
            }
            NodeData::Attr(_) => {
                let a = self.attr_node_attr(node).clone();
                self.create_attr_unchecked(document, a.name, &a.value)
            }
            NodeData::Text(s) => {
                let s = s.clone();
                self.alloc(Some(document), NodeData::Text(s))
            }
            NodeData::CDataSection(s) => {
                let s = s.clone();
                self.alloc(Some(document), NodeData::CDataSection(s))
            }
            NodeData::Comment(s) => {
                let s = s.clone();
                self.alloc(Some(document), NodeData::Comment(s))
            }
            NodeData::ProcessingInstruction { target, data } => {
                let (t, d) = (target.clone(), data.clone());
                self.alloc(
                    Some(document),
                    NodeData::ProcessingInstruction { target: t, data: d },
                )
            }
            NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) => {
                self.create_document_fragment(document)
            }
            NodeData::Free { .. } => unreachable!(),
        }
    }

    /// `Node.isEqualNode(other)` (DOM §4.4 "equals").
    pub fn is_equal_node(&self, a: NodeId, b: NodeId) -> bool {
        let mut work = vec![(a, b)];
        while let Some((a, b)) = work.pop() {
            if !self.equal_single(a, b) {
                return false;
            }
            if self.child_count(a) != self.child_count(b) {
                return false;
            }
            work.extend(self.children(a).zip(self.children(b)));
        }
        true
    }

    fn equal_single(&self, a: NodeId, b: NodeId) -> bool {
        if self.node_type(a) != self.node_type(b) {
            return false;
        }
        match (&self.n(a).data, &self.n(b).data) {
            (NodeData::DocumentType(x), NodeData::DocumentType(y)) => {
                x.name == y.name && x.public_id == y.public_id && x.system_id == y.system_id
            }
            (NodeData::Element(x), NodeData::Element(y)) => {
                x.name == y.name
                    && x.attrs.len() == y.attrs.len()
                    && x.attrs.iter().all(|p| {
                        y.attrs.iter().any(|q| {
                            p.name.ns == q.name.ns
                                && p.name.local == q.name.local
                                && p.value == q.value
                        })
                    })
            }
            (NodeData::Attr(_), NodeData::Attr(_)) => {
                let (x, y) = (self.attr_node_attr(a), self.attr_node_attr(b));
                x.name.ns == y.name.ns && x.name.local == y.name.local && x.value == y.value
            }
            (
                NodeData::ProcessingInstruction {
                    target: t1,
                    data: d1,
                },
                NodeData::ProcessingInstruction {
                    target: t2,
                    data: d2,
                },
            ) => t1 == t2 && d1 == d2,
            (NodeData::Text(x), NodeData::Text(y))
            | (NodeData::CDataSection(x), NodeData::CDataSection(y))
            | (NodeData::Comment(x), NodeData::Comment(y)) => x == y,
            _ => true,
        }
    }
}
