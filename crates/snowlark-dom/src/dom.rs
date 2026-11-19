// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The node arena and its typed accessors.

use std::borrow::Cow;
use std::num::NonZeroU32;

use html5ever::{local_name, ns, LocalName, Namespace, Prefix, QualName};

use crate::node::*;
use crate::records::{MutationRecord, Reaction};

/// The node store of one event loop (one similar-origin window agent): every document that script in that
/// agent can reach, and every node of those documents, lives in one `Dom`. A document is a node in it
/// ([`NodeType::Document`]); a node's node document is a field of the node, so adopting a node into
/// another document (iframes, `template` contents, `DOMParser`, `createHTMLDocument`) changes a field and
/// never the node's [`NodeId`].
///
/// Reading takes `&Dom` and `Dom` is `Sync`, so stylo's parallel traversal can share it; every mutation
/// takes `&mut Dom`.
///
/// Accessors panic when given a stale [`NodeId`] (one whose node was freed): that is a bug in the caller,
/// never something script can cause, because the bindings keep a node alive while a wrapper refers to it.
#[derive(Debug)]
pub struct Dom {
    pub(crate) nodes: Vec<Node>,
    /// Each slot's subtree filter ([`crate::filter`]), beside `nodes` so that a node does not grow.
    pub(crate) filters: Vec<crate::filter::Filter>,
    /// The filter bits of attribute names, computed once per name.
    pub(crate) name_bits: crate::filter::NameBits,
    /// The reusable free slots, one bit each (set: free). Allocation takes the lowest one, as a
    /// bump allocator over coalesced free runs does (Oilpan's free lists after a sweep): nodes made
    /// together sit together in the arena, and the live ones gather at its start. A last-in-first-out
    /// list of single slots instead hands out the order the collector freed them in, which over many
    /// documents' lifetimes scatters a tree across the whole arena (TodoMVC-jQuery, ten Speedometer
    /// iterations: neighbours 75,000 slots apart on average, and every walk slower each iteration).
    free_bits: Vec<u64>,
    /// How many bits of `free_bits` are set.
    free_count: usize,
    /// No word of `free_bits` below this one has a bit set.
    free_low: usize,
    live: usize,
    /// Bumped on every mutation of any node.
    pub(crate) version: u64,
    /// Bumped when the shape of any tree changes: children inserted, removed or moved, a node adopted,
    /// a shadow root attached. Attribute and character data changes leave it alone.
    pub(crate) tree_version: u64,
    /// Bumped when an `id`, `name`, `class` or `href` attribute (in any namespace) is set, changed or
    /// removed.
    pub(crate) key_attr_version: u64,
    /// Number of nodes that carry a registered or transient observer flag.
    pub(crate) observed: usize,
    pub(crate) transient: Vec<(NodeId, NodeId)>,
    pub(crate) records: Vec<MutationRecord>,
    pub(crate) reactions_enabled: bool,
    pub(crate) reactions: Vec<Reaction>,
    /// The shadow-including descendants of the nodes removed since the reactions were last taken,
    /// which their [`Reaction::Removed`] ranges index.
    pub(crate) removed_descendants: Vec<NodeId>,
    /// The list of the batch last taken ([`Dom::take_removed_descendants`]).
    pub(crate) removed_taken: Vec<NodeId>,
    /// Shadow roots ever created in this store (never lowered): while zero, no node is in a shadow
    /// tree or hosts one, and walks looking for one can be skipped.
    pub(crate) shadow_roots_created: usize,
    /// Set while the fragment parser builds in its own document (no script can reach it): tree
    /// reactions are left out, attribute changes kept (event handler content attributes).
    pub(crate) quiet_tree_reactions: bool,
    /// The fragment the fragment parsing algorithm just returned to `innerHTML`, `outerHTML`,
    /// `insertAdjacentHTML` or `setHTMLUnsafe`, while it is inserted ([`Dom::insert`]): no script
    /// has seen it.
    pub(crate) unreached_fragment: Option<NodeId>,
    /// Animated values of attributes (SVG Animations §3.5: not the DOM's values, which stay).
    animated_attrs: rustc_hash::FxHashMap<NodeId, Vec<(LocalName, String)>>,
    /// What the layer above wants reactions for ([`Dom::set_reaction_filter`]).
    pub(crate) reaction_filter: Option<crate::records::ReactionFilter>,
    /// Custom elements' custom state sets (HTML §4.13.6 `CustomStateSet`), what `:state()` matches.
    pub(crate) custom_states: rustc_hash::FxHashMap<NodeId, Vec<String>>,
    /// The number of live nodes whose node document each Document is (itself included):
    /// [`Dom::documents_owned_within`] without walking the arena. Kept by [`Dom::set_document`].
    pub(crate) doc_nodes: rustc_hash::FxHashMap<NodeId, u32>,
}

impl Default for Dom {
    fn default() -> Self {
        Dom::new()
    }
}

const FIRST_GENERATION: NonZeroU32 = match NonZeroU32::new(1) {
    Some(g) => g,
    None => unreachable!(),
};

impl Dom {
    /// An empty node store.
    pub fn new() -> Dom {
        Dom {
            nodes: Vec::new(),
            filters: Vec::new(),
            name_bits: crate::filter::NameBits::new(),
            free_bits: Vec::new(),
            free_count: 0,
            free_low: 0,
            live: 0,
            version: 0,
            tree_version: 0,
            key_attr_version: 0,
            observed: 0,
            transient: Vec::new(),
            records: Vec::new(),
            reactions_enabled: false,
            reactions: Vec::new(),
            removed_descendants: Vec::new(),
            removed_taken: Vec::new(),
            shadow_roots_created: 0,
            quiet_tree_reactions: false,
            unreached_fragment: None,
            animated_attrs: Default::default(),
            reaction_filter: None,
            custom_states: Default::default(),
            doc_nodes: Default::default(),
        }
    }

    /// Whether a shadow root was ever created in this store (if not, no node is in a shadow tree).
    #[inline]
    pub fn has_shadow_roots(&self) -> bool {
        self.shadow_roots_created != 0
    }

    // ----- arena -------------------------------------------------------------------------------------------

    pub(crate) fn alloc(&mut self, document: Option<NodeId>, data: NodeData) -> NodeId {
        self.live += 1;
        if let Some(d) = document {
            *self.doc_nodes.entry(d).or_default() += 1;
        }
        let filter = crate::filter::initial_filter(&data);
        let node = |generation| Node {
            generation,
            flags: 0,
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            document,
            data,
        };
        if let Some(index) = self.take_free_slot() {
            let slot = &mut self.nodes[index as usize];
            debug_assert!(
                matches!(slot.data, NodeData::Free { .. }),
                "free bit of a live node"
            );
            let generation = slot.generation;
            *slot = node(generation);
            self.filters[index as usize] = filter;
            NodeId { index, generation }
        } else {
            let index = u32::try_from(self.nodes.len()).expect("more than 2^32 nodes");
            self.nodes.push(node(FIRST_GENERATION));
            self.filters.push(filter);
            NodeId {
                index,
                generation: FIRST_GENERATION,
            }
        }
    }

    /// The lowest reusable free slot, now taken.
    #[inline]
    fn take_free_slot(&mut self) -> Option<u32> {
        if self.free_count == 0 {
            return None;
        }
        let mut w = self.free_low;
        while self.free_bits[w] == 0 {
            w += 1;
        }
        let bit = self.free_bits[w].trailing_zeros() as usize;
        self.free_bits[w] &= !(1u64 << bit);
        self.free_count -= 1;
        self.free_low = w;
        Some((w * 64 + bit) as u32)
    }

    /// Makes slot `index` reusable.
    #[inline]
    fn put_free_slot(&mut self, index: u32) {
        let (w, bit) = (index as usize / 64, index as usize % 64);
        if w >= self.free_bits.len() {
            self.free_bits.resize(w + 1, 0);
        }
        debug_assert_eq!(self.free_bits[w] & (1u64 << bit), 0, "slot freed twice");
        self.free_bits[w] |= 1u64 << bit;
        self.free_count += 1;
        self.free_low = self.free_low.min(w);
    }

    /// Releases one slot. The generation is bumped so outstanding ids become stale; a slot whose
    /// generation would pass [`NodeId::MAX_GENERATION`] is retired instead of reused.
    fn release(&mut self, id: NodeId) {
        let slot = &mut self.nodes[id.index as usize];
        debug_assert_eq!(slot.generation, id.generation);
        if slot.flags & (flags::OBSERVED | flags::TRANSIENT_OBSERVED) != 0 {
            self.observed -= 1;
        }
        self.live -= 1;
        if let Some(d) = slot.document.take() {
            if let std::collections::hash_map::Entry::Occupied(mut e) = self.doc_nodes.entry(d) {
                *e.get_mut() -= 1;
                if *e.get() == 0 {
                    e.remove();
                }
            }
        }
        let slot = &mut self.nodes[id.index as usize];
        match slot
            .generation
            .checked_add(1)
            .filter(|g| g.get() <= NodeId::MAX_GENERATION)
        {
            Some(generation) => {
                slot.generation = generation;
                slot.data = NodeData::Free {};
                slot.flags = 0;
                slot.parent = None;
                slot.first_child = None;
                slot.last_child = None;
                slot.prev_sibling = None;
                slot.next_sibling = None;
                slot.document = None;
                self.put_free_slot(id.index);
            }
            None => {
                // Retired: its free bit is never set, so it is never reused.
                slot.data = NodeData::Free {};
                slot.generation = FIRST_GENERATION;
                slot.flags = 0;
            }
        }
    }

    /// Frees `root` and everything it owns: its descendants, template contents, shadow roots, and
    /// materialized Attr nodes; for a Document, also its template contents owner document. `root` must have
    /// no parent. This is the hook the bindings' GC uses once no wrapper keeps the subtree reachable.
    pub fn free_subtree(&mut self, root: NodeId) {
        assert!(
            self.n(root).parent.is_none(),
            "free_subtree on a node with a parent"
        );
        for id in self.owned_nodes(root) {
            self.release(id);
        }
    }

    /// [`Dom::free_subtree`] with the nodes [`Dom::owned_nodes`] returned for `root`, when the caller
    /// already has them (and changed nothing in between).
    pub fn free_owned(&mut self, root: NodeId, owned: &[NodeId]) {
        assert!(
            self.n(root).parent.is_none(),
            "free_owned on a node with a parent"
        );
        debug_assert_eq!(owned, &self.owned_nodes(root)[..]);
        for &id in owned {
            self.release(id);
        }
    }

    /// `root` and everything it owns, as [`Dom::free_subtree`] would free them: its descendants,
    /// template contents, shadow roots and materialized Attr nodes (and a Document's template contents
    /// owner document).
    pub fn owned_nodes(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        // Each tree (the root's, and the shadow trees, template contents, Attr nodes and template
        // contents owner documents it owns) is walked in tree order; the owned trees wait on a
        // stack, which most subtrees never use.
        let mut trees: smallvec::SmallVec<[NodeId; 4]> = smallvec::smallvec![root];
        while let Some(tree) = trees.pop() {
            let mut cur = Some(tree);
            while let Some(id) = cur {
                let node = self.n(id);
                match &node.data {
                    NodeData::Element(e) => {
                        trees.extend(e.shadow_root);
                        // A declarative shadow root's template points at the shadow root, which it
                        // doesn't own.
                        trees.extend(e.template_contents.filter(|&c| self.host(c) == Some(id)));
                        trees.extend(e.attrs.iter().filter_map(|a| a.node));
                    }
                    NodeData::Document(d) => trees.extend(d.template_contents_owner),
                    _ => {}
                }
                out.push(id);
                cur = self.next_in_tree_order(id, Some(tree));
            }
        }
        out
    }

    /// The opaque root of a node (what keeps its wrapper reachable, ARCHITECTURE.md "Wrappers and
    /// GC"): the root of its tree, continued through a shadow root's or template contents' host and
    /// an Attr's element.
    pub fn opaque_root(&self, id: NodeId) -> NodeId {
        // A connected node's shadow-including root is its document, which ends the walk.
        if self.is_connected(id) {
            return self.node_document(id);
        }
        let mut cur = id;
        loop {
            let root = self.root(cur);
            let next = match &self.n(root).data {
                NodeData::ShadowRoot(s) => Some(s.host),
                NodeData::DocumentFragment { host } => *host,
                NodeData::Attr(a) => a.owner,
                _ => None,
            };
            match next {
                Some(n) => cur = n,
                None => return root,
            }
        }
    }

    /// Whether every live node whose node document is one of the Documents in `owned` (as
    /// [`Dom::owned_nodes`] returns them) is in `owned` too: freeing them then leaves no node with a
    /// freed node document. Costs `owned`'s length (the per-document counts, not an arena walk).
    pub fn documents_owned_within(&self, owned: &[NodeId]) -> bool {
        let docs: smallvec::SmallVec<[NodeId; 2]> = owned
            .iter()
            .copied()
            .filter(|&n| self.is_document(n))
            .collect();
        if docs.is_empty() {
            return true;
        }
        let of_docs = |n: &Node| {
            !matches!(n.data, NodeData::Free { .. })
                && n.document.is_some_and(|d| docs.contains(&d))
        };
        let inside = owned.iter().filter(|&&id| of_docs(self.n(id))).count();
        let all: usize = docs
            .iter()
            .map(|d| self.doc_nodes.get(d).copied().unwrap_or(0) as usize)
            .sum();
        all == inside
    }

    /// The opaque roots of the live nodes whose node document is `doc` but that are not in its
    /// tree (subtrees removed from it, nodes it created and never inserted), each once: what
    /// [`Dom::documents_owned_within`] waits on before `doc` can be freed. Walks the arena.
    pub fn detached_roots_of(&self, doc: NodeId) -> Vec<NodeId> {
        let mut roots = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if matches!(node.data, NodeData::Free { .. }) || node.document != Some(doc) {
                continue;
            }
            let id = NodeId {
                index: index as u32,
                generation: node.generation,
            };
            let root = self.opaque_root(id);
            if root != doc && seen.insert(root) {
                roots.push(root);
            }
        }
        roots
    }

    /// Whether `id` still names a live node.
    #[inline]
    pub fn is_alive(&self, id: NodeId) -> bool {
        self.nodes.get(id.index as usize).is_some_and(|n| {
            n.generation == id.generation && !matches!(n.data, NodeData::Free { .. })
        })
    }

    /// The number of arena slots, free ones included: every [`NodeId::index`] is below it.
    #[inline]
    pub fn node_slots(&self) -> usize {
        self.nodes.len()
    }

    /// How far apart in the arena tree neighbours are: the mean index distance from each live node to
    /// its first child and to its next sibling. Nodes made in tree order into fresh slots are next to
    /// each other (about 1); reused slots scatter them. A diagnostic for harness reports.
    pub fn tree_scatter(&self) -> f64 {
        let (mut sum, mut links) = (0u64, 0u64);
        for (i, n) in self.nodes.iter().enumerate() {
            if matches!(n.data, NodeData::Free { .. }) {
                continue;
            }
            for next in [n.first_child, n.next_sibling].into_iter().flatten() {
                sum += (next.index as i64 - i as i64).unsigned_abs();
                links += 1;
            }
        }
        if links == 0 {
            0.0
        } else {
            sum as f64 / links as f64
        }
    }

    /// Number of live Documents (each is its own node document).
    pub fn document_count(&self) -> usize {
        self.doc_nodes.len()
    }

    /// Each live Document with the number of live nodes whose node document it is (itself included).
    pub fn documents_with_node_counts(&self) -> Vec<(NodeId, usize)> {
        self.doc_nodes
            .iter()
            .map(|(&d, &n)| (d, n as usize))
            .collect()
    }

    /// The live nodes that have no node document (none should).
    pub fn nodes_without_document(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| !matches!(n.data, NodeData::Free { .. }) && n.document.is_none())
            .count()
    }

    /// Number of live nodes.
    pub fn node_count(&self) -> usize {
        self.live
    }

    /// The memory the arena's slots take (live and free; not what the nodes point to).
    pub fn arena_bytes(&self) -> usize {
        std::mem::size_of_val(self.nodes.as_slice())
    }

    /// Number of arena slots (live and free): every [`NodeId::index`] is below it. Side tables indexed by
    /// node index (style data, layout boxes) size themselves by it.
    pub fn arena_len(&self) -> usize {
        self.nodes.len()
    }

    /// The arena slot for `id`.
    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        self.n(id)
    }

    #[inline]
    pub(crate) fn n(&self, id: NodeId) -> &Node {
        let node = &self.nodes[id.index as usize];
        assert!(node.generation == id.generation, "stale NodeId {id:?}");
        node
    }

    #[inline]
    pub(crate) fn n_mut(&mut self, id: NodeId) -> &mut Node {
        let node = &mut self.nodes[id.index as usize];
        assert!(node.generation == id.generation, "stale NodeId {id:?}");
        node
    }

    /// Store-wide version, bumped on every mutation.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Store-wide tree version, bumped when any tree's shape changes (children inserted, removed or
    /// moved, nodes adopted, shadow roots attached), not by attribute or character data changes: what
    /// a live collection whose membership depends on tree position and element names alone compares.
    pub fn tree_version(&self) -> u64 {
        self.tree_version
    }

    /// Changes when the tree version does or an `id`, `name`, `class` or `href` attribute changes: what a
    /// live collection whose membership reads only those attributes compares (the two counters only
    /// grow, so their sum changes whenever either does).
    pub fn key_attr_tree_version(&self) -> u64 {
        self.tree_version + self.key_attr_version
    }

    // ----- node creation (no validation; the DOM API versions are in api.rs) -------------------------------

    /// Creates a new Document (DOM §4.5): "about:blank", UTF-8, no-quirks, content type `text/html` for HTML
    /// and `application/xml` for XML documents.
    pub fn create_document(&mut self, kind: DocumentKind) -> NodeId {
        let content_type = match kind {
            DocumentKind::Html => "text/html",
            DocumentKind::Xml => "application/xml",
        };
        let id = self.alloc(
            None,
            NodeData::Document(Box::new(DocumentData {
                kind,
                mode: QuirksMode::NoQuirks,
                content_type: content_type.to_owned(),
                url: "about:blank".to_owned(),
                encoding: encoding_rs::UTF_8,
                allow_declarative_shadow_roots: false,
                template_contents_owner: None,
                is_template_contents_owner: false,
                version: 0,
                connected_bases: 0,
                pragma_language: None,
                names: Default::default(),
            })),
        );
        self.set_document(id, Some(id));
        self.set_connected(id, true);
        id
    }

    /// Sets `id`'s node document, keeping the per-document node counts.
    pub(crate) fn set_document(&mut self, id: NodeId, document: Option<NodeId>) {
        let old = std::mem::replace(&mut self.n_mut(id).document, document);
        if old == document {
            return;
        }
        if let Some(o) = old {
            if let std::collections::hash_map::Entry::Occupied(mut e) = self.doc_nodes.entry(o) {
                *e.get_mut() -= 1;
                if *e.get() == 0 {
                    e.remove();
                }
            }
        }
        if let Some(d) = document {
            *self.doc_nodes.entry(d).or_default() += 1;
        }
    }

    /// HTML §4.12.3 "appropriate template contents owner document".
    pub fn template_contents_owner_document(&mut self, document: NodeId) -> NodeId {
        let data = self.doc(document);
        if data.is_template_contents_owner {
            return document;
        }
        if let Some(owner) = data.template_contents_owner {
            return owner;
        }
        let kind = data.kind;
        let allow = data.allow_declarative_shadow_roots;
        let owner = self.create_document(kind);
        self.doc_mut(owner).is_template_contents_owner = true;
        // (declarative shadow roots in template contents attach as in their document)
        self.doc_mut(owner).allow_declarative_shadow_roots = allow;
        self.doc_mut(document).template_contents_owner = Some(owner);
        owner
    }

    /// "Create an element" (DOM §4.9) without the custom element registry lookup: the element starts
    /// "undefined" when it could become a custom element and "uncustomized" otherwise, and the page layer
    /// upgrades it later. An HTML `template` gets its template contents (HTML §4.12.3).
    pub fn create_element_with_name(
        &mut self,
        document: NodeId,
        name: QualName,
        is: Option<LocalName>,
    ) -> NodeId {
        let custom_state = if name.ns == ns!(html)
            && (is.is_some() || crate::names::is_valid_custom_element_name(&name.local))
        {
            CustomElementState::Undefined
        } else {
            CustomElementState::Uncustomized
        };
        let is_template = name.ns == ns!(html) && name.local == local_name!("template");
        let id = self.alloc(
            Some(document),
            NodeData::Element(ElementData {
                name,
                attrs: AttrList::new(),
                shadow_root: None,
                template_contents: None,
                custom_state,
                is_value: is,
                flags: 0,
            }),
        );
        if is_template {
            let owner = self.template_contents_owner_document(document);
            let contents = self.alloc(Some(owner), NodeData::DocumentFragment { host: Some(id) });
            self.element_mut(id).template_contents = Some(contents);
        }
        id
    }

    /// A new HTML element (`ns` = HTML, no prefix) named `local`.
    pub fn create_html_element(&mut self, document: NodeId, local: LocalName) -> NodeId {
        self.create_element_with_name(document, QualName::new(None, ns!(html), local), None)
    }

    /// A new Text node.
    pub fn create_text_node(&mut self, document: NodeId, data: &str) -> NodeId {
        self.alloc(Some(document), NodeData::Text(data.into()))
    }

    /// A new Comment node.
    pub fn create_comment(&mut self, document: NodeId, data: &str) -> NodeId {
        self.alloc(Some(document), NodeData::Comment(data.into()))
    }

    /// A new DocumentFragment (`host` null).
    pub fn create_document_fragment(&mut self, document: NodeId) -> NodeId {
        self.alloc(Some(document), NodeData::DocumentFragment { host: None })
    }

    pub(crate) fn create_cdata_unchecked(&mut self, document: NodeId, data: &str) -> NodeId {
        self.alloc(Some(document), NodeData::CDataSection(data.into()))
    }

    pub(crate) fn create_pi_unchecked(
        &mut self,
        document: NodeId,
        target: &str,
        data: &str,
    ) -> NodeId {
        self.alloc(
            Some(document),
            NodeData::ProcessingInstruction {
                target: target.to_owned(),
                data: data.to_owned(),
            },
        )
    }

    pub(crate) fn create_doctype_unchecked(
        &mut self,
        document: NodeId,
        name: &str,
        public_id: &str,
        system_id: &str,
    ) -> NodeId {
        self.alloc(
            Some(document),
            NodeData::DocumentType(Box::new(DoctypeData {
                name: name.to_owned(),
                public_id: public_id.to_owned(),
                system_id: system_id.to_owned(),
            })),
        )
    }

    pub(crate) fn create_attr_unchecked(
        &mut self,
        document: NodeId,
        name: QualName,
        value: &str,
    ) -> NodeId {
        self.alloc(
            Some(document),
            NodeData::Attr(Box::new(AttrNodeData {
                owner: None,
                detached: Attr::new(name, value),
            })),
        )
    }

    // ----- typed accessors ---------------------------------------------------------------------------------

    /// `Node.nodeType` (DOM §4.4).
    #[inline]
    pub fn node_type(&self, id: NodeId) -> NodeType {
        self.n(id).node_type()
    }

    /// The node's data.
    #[inline]
    pub fn data(&self, id: NodeId) -> &NodeData {
        &self.n(id).data
    }

    /// `Node.parentNode`.
    #[inline]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.n(id).parent
    }

    /// `Node.parentElement`.
    #[inline]
    pub fn parent_element(&self, id: NodeId) -> Option<NodeId> {
        self.n(id).parent.filter(|&p| self.is_element(p))
    }

    /// `Node.firstChild`.
    #[inline]
    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.n(id).first_child
    }

    /// `Node.lastChild`.
    #[inline]
    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.n(id).last_child
    }

    /// `Node.previousSibling`.
    #[inline]
    pub fn previous_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.n(id).prev_sibling
    }

    /// `Node.nextSibling`.
    #[inline]
    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.n(id).next_sibling
    }

    /// `Node.hasChildNodes()`.
    #[inline]
    pub fn has_child_nodes(&self, id: NodeId) -> bool {
        self.n(id).first_child.is_some()
    }

    /// The node document (DOM §4.4). For a Document, itself.
    #[inline]
    pub fn node_document(&self, id: NodeId) -> NodeId {
        self.n(id).document.expect("node without a node document")
    }

    /// `Node.ownerDocument`: null for a Document.
    #[inline]
    pub fn owner_document(&self, id: NodeId) -> Option<NodeId> {
        let node = self.n(id);
        if matches!(node.data, NodeData::Document(_)) {
            None
        } else {
            node.document
        }
    }

    /// `Node.isConnected`: the shadow-including root is a document (DOM §4.4). Cached as a flag.
    #[inline]
    pub fn is_connected(&self, id: NodeId) -> bool {
        self.n(id).flags & flags::CONNECTED != 0
    }

    #[inline]
    pub fn is_element(&self, id: NodeId) -> bool {
        matches!(self.n(id).data, NodeData::Element(_))
    }

    #[inline]
    pub fn is_document(&self, id: NodeId) -> bool {
        matches!(self.n(id).data, NodeData::Document(_))
    }

    /// A Text or CDATASection node.
    #[inline]
    pub fn is_text(&self, id: NodeId) -> bool {
        matches!(
            self.n(id).data,
            NodeData::Text(_) | NodeData::CDataSection(_)
        )
    }

    /// An "exclusive Text node": a Text node that is not a CDATASection (DOM §4.11).
    #[inline]
    pub fn is_exclusive_text(&self, id: NodeId) -> bool {
        matches!(self.n(id).data, NodeData::Text(_))
    }

    /// Text, CDATASection, Comment or ProcessingInstruction.
    #[inline]
    pub fn is_character_data(&self, id: NodeId) -> bool {
        matches!(
            self.n(id).data,
            NodeData::Text(_)
                | NodeData::CDataSection(_)
                | NodeData::Comment(_)
                | NodeData::ProcessingInstruction { .. }
        )
    }

    /// A DocumentFragment, including a ShadowRoot.
    #[inline]
    pub fn is_document_fragment(&self, id: NodeId) -> bool {
        matches!(
            self.n(id).data,
            NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_)
        )
    }

    #[inline]
    pub fn is_shadow_root(&self, id: NodeId) -> bool {
        matches!(self.n(id).data, NodeData::ShadowRoot(_))
    }

    /// Element data, if `id` is an element.
    #[inline]
    pub fn element(&self, id: NodeId) -> Option<&ElementData> {
        match &self.n(id).data {
            NodeData::Element(e) => Some(e),
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn el(&self, id: NodeId) -> &ElementData {
        self.element(id).expect("not an element")
    }

    #[inline]
    pub(crate) fn element_mut(&mut self, id: NodeId) -> &mut ElementData {
        match &mut self.n_mut(id).data {
            NodeData::Element(e) => e,
            _ => panic!("not an element"),
        }
    }

    /// The element's local name. Panics on non-elements.
    #[inline]
    pub fn local_name(&self, id: NodeId) -> &LocalName {
        &self.el(id).name.local
    }

    /// The element's namespace (empty for null). Panics on non-elements.
    #[inline]
    pub fn namespace(&self, id: NodeId) -> &Namespace {
        &self.el(id).name.ns
    }

    /// The element's namespace prefix. Panics on non-elements.
    #[inline]
    pub fn prefix(&self, id: NodeId) -> Option<&Prefix> {
        self.el(id).name.prefix.as_ref()
    }

    /// The element's full name. Panics on non-elements.
    #[inline]
    pub fn element_name(&self, id: NodeId) -> &QualName {
        &self.el(id).name
    }

    /// Whether `id` is an element in the HTML namespace with local name `local`.
    #[inline]
    /// Sets or clears the CONNECTED flag, counting the document's connected `base` elements.
    pub(crate) fn set_connected(&mut self, id: NodeId, connected: bool) {
        let n = self.n_mut(id);
        if (n.flags & flags::CONNECTED != 0) == connected {
            return;
        }
        if connected {
            n.flags |= flags::CONNECTED;
        } else {
            n.flags &= !flags::CONNECTED;
        }
        let (is_base, has_attrs) = match &n.data {
            NodeData::Element(e) => (
                e.name.ns == ns!(html) && e.name.local == local_name!("base"),
                !e.attrs.is_empty(),
            ),
            _ => (false, false),
        };
        if has_attrs {
            self.index_element(id, connected);
        }
        let n = self.n(id);
        if let (true, Some(doc)) = (is_base, n.document) {
            if let NodeData::Document(d) = &mut self.n_mut(doc).data {
                if connected {
                    d.connected_bases += 1;
                } else {
                    d.connected_bases -= 1;
                }
            }
        }
    }

    /// Whether an HTML `base` element is connected in `doc` (in its tree or a shadow tree): without
    /// one, the document base URL is the fallback base URL and no walk is needed to find that out.
    pub fn has_connected_base(&self, doc: NodeId) -> bool {
        match &self.n(doc).data {
            NodeData::Document(d) => d.connected_bases > 0,
            _ => false,
        }
    }

    /// HTML §3.2.6.2 the document's "pragma-set default language", the language of its nodes
    /// with no `lang` of their own or of an ancestor.
    pub fn pragma_language(&self, doc: NodeId) -> Option<&str> {
        match &self.n(doc).data {
            NodeData::Document(d) => d.pragma_language.as_deref(),
            _ => None,
        }
    }

    /// Sets the document's pragma-set default language (HTML §4.2.5.3, the content language state).
    pub fn set_pragma_language(&mut self, doc: NodeId, language: &str) {
        if let NodeData::Document(d) = &mut self.n_mut(doc).data {
            d.pragma_language = Some(language.into());
        }
    }

    pub fn is_html_element_named(&self, id: NodeId, local: &LocalName) -> bool {
        self.element(id)
            .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == *local)
    }

    /// Whether the element is a `script` element, of HTML or of SVG (SVG 2 §15.6: scripts run as
    /// HTML's do).
    pub fn is_script_element(&self, id: NodeId) -> bool {
        self.element(id).is_some_and(|e| {
            e.name.local == local_name!("script")
                && (e.name.ns == ns!(html) || e.name.ns == ns!(svg))
        })
    }

    /// Whether the element is a `style` element, of HTML or of SVG (SVG 2 §6.4: its content is a
    /// style sheet as in HTML).
    pub fn is_style_element(&self, id: NodeId) -> bool {
        self.element(id).is_some_and(|e| {
            e.name.local == local_name!("style")
                && (e.name.ns == ns!(html) || e.name.ns == ns!(svg))
        })
    }

    /// The element's qualified name (DOM §4.9).
    pub fn qualified_name(&self, id: NodeId) -> Cow<'_, str> {
        qualified_name(&self.el(id).name)
    }

    /// `Element.tagName`: the qualified name, ASCII-uppercased for an HTML element in an HTML document.
    pub fn tag_name(&self, id: NodeId) -> Cow<'_, str> {
        let q = self.qualified_name(id);
        if self.is_html_element_in_html_document(id) && q.bytes().any(|b| b.is_ascii_lowercase()) {
            Cow::Owned(q.to_ascii_uppercase())
        } else {
            q
        }
    }

    /// An element in the HTML namespace whose node document is an HTML document.
    #[inline]
    pub fn is_html_element_in_html_document(&self, id: NodeId) -> bool {
        self.element(id).is_some_and(|e| e.name.ns == ns!(html))
            && self.is_html_document(self.node_document(id))
    }

    /// `Node.nodeName` (DOM §4.4).
    pub fn node_name(&self, id: NodeId) -> Cow<'_, str> {
        match &self.n(id).data {
            NodeData::Element(_) => self.tag_name(id),
            NodeData::Attr(_) => Cow::Owned(self.attr_node_attr(id).qualified_name().into_owned()),
            NodeData::Text(_) => Cow::Borrowed("#text"),
            NodeData::CDataSection(_) => Cow::Borrowed("#cdata-section"),
            NodeData::ProcessingInstruction { target, .. } => Cow::Borrowed(target),
            NodeData::Comment(_) => Cow::Borrowed("#comment"),
            NodeData::Document(_) => Cow::Borrowed("#document"),
            NodeData::DocumentType(d) => Cow::Borrowed(&d.name),
            NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) => {
                Cow::Borrowed("#document-fragment")
            }
            NodeData::Free { .. } => unreachable!(),
        }
    }

    /// The element's custom element state.
    pub fn custom_element_state(&self, id: NodeId) -> CustomElementState {
        self.el(id).custom_state
    }

    /// The element's custom states (`ElementInternals.states`), in insertion order.
    pub fn custom_states(&self, id: NodeId) -> &[String] {
        self.custom_states.get(&id).map_or(&[], Vec::as_slice)
    }

    /// Replaces the element's custom states.
    pub fn set_custom_states(&mut self, id: NodeId, states: Vec<String>) {
        if states.is_empty() {
            self.custom_states.remove(&id);
        } else {
            self.custom_states.insert(id, states);
        }
    }

    /// Sets the custom element state (the custom elements layer drives this).
    pub fn set_custom_element_state(&mut self, id: NodeId, state: CustomElementState) {
        self.element_mut(id).custom_state = state;
    }

    /// Sets the element's namespace prefix: DOM §4.9 "create an element" step 7.1.5 gives an element
    /// a custom element constructor returned the prefix of the name it was created with.
    pub fn set_element_prefix(&mut self, id: NodeId, prefix: Option<Prefix>) {
        self.element_mut(id).name.prefix = prefix;
    }

    /// The element's `is` value.
    pub fn is_value(&self, id: NodeId) -> Option<&LocalName> {
        self.el(id).is_value.as_ref()
    }

    /// The element's flag bits ([`element_flags`]).
    pub fn element_flags(&self, id: NodeId) -> u8 {
        self.el(id).flags
    }

    /// Sets or clears element flag bits.
    /// Marks a form-associated custom element (it becomes a listed element: the collections of
    /// its form and fieldsets change).
    pub fn set_form_associated_custom(&mut self, id: NodeId, on: bool) {
        let was = self.el(id).flags & crate::node::element_flags::FORM_ASSOCIATED_CUSTOM != 0;
        if was != on {
            self.set_element_flag(id, crate::node::element_flags::FORM_ASSOCIATED_CUSTOM, on);
            self.bump(id);
        }
    }

    pub fn set_element_flag(&mut self, id: NodeId, flag: u8, on: bool) {
        let e = self.element_mut(id);
        if on {
            e.flags |= flag;
        } else {
            e.flags &= !flag;
        }
    }

    /// The template contents of an HTML `template` element (HTML §4.12.3).
    pub fn template_contents(&self, id: NodeId) -> Option<NodeId> {
        self.element(id).and_then(|e| e.template_contents)
    }

    /// The element's shadow root, if it is a shadow host (regardless of mode).
    pub fn shadow_root(&self, id: NodeId) -> Option<NodeId> {
        self.element(id).and_then(|e| e.shadow_root)
    }

    /// Shadow root data, if `id` is a shadow root.
    pub fn shadow_root_data(&self, id: NodeId) -> Option<&ShadowRootData> {
        match &self.n(id).data {
            NodeData::ShadowRoot(s) => Some(s),
            _ => None,
        }
    }

    /// A DocumentFragment's host (DOM §4.7): a shadow root's host, or a template contents' template.
    pub fn host(&self, id: NodeId) -> Option<NodeId> {
        match &self.n(id).data {
            NodeData::ShadowRoot(s) => Some(s.host),
            NodeData::DocumentFragment { host } => *host,
            _ => None,
        }
    }

    /// `DocumentType.name`, `publicId`, `systemId`.
    pub fn doctype(&self, id: NodeId) -> Option<(&str, &str, &str)> {
        match &self.n(id).data {
            NodeData::DocumentType(d) => Some((&d.name, &d.public_id, &d.system_id)),
            _ => None,
        }
    }

    /// `ProcessingInstruction.target`.
    pub fn pi_target(&self, id: NodeId) -> Option<&str> {
        match &self.n(id).data {
            NodeData::ProcessingInstruction { target, .. } => Some(target),
            _ => None,
        }
    }

    // ----- documents ---------------------------------------------------------------------------------------

    #[inline]
    pub(crate) fn doc(&self, id: NodeId) -> &DocumentData {
        match &self.n(id).data {
            NodeData::Document(d) => d,
            _ => panic!("not a document"),
        }
    }

    #[inline]
    pub(crate) fn doc_mut(&mut self, id: NodeId) -> &mut DocumentData {
        match &mut self.n_mut(id).data {
            NodeData::Document(d) => d,
            _ => panic!("not a document"),
        }
    }

    /// Document data, if `id` is a document.
    pub fn document_data(&self, id: NodeId) -> Option<&DocumentData> {
        match &self.n(id).data {
            NodeData::Document(d) => Some(d),
            _ => None,
        }
    }

    /// An HTML document (DOM §4.5 "type" is "html").
    #[inline]
    pub fn is_html_document(&self, document: NodeId) -> bool {
        self.doc(document).kind == DocumentKind::Html
    }

    /// The document's type.
    pub fn document_kind(&self, document: NodeId) -> DocumentKind {
        self.doc(document).kind
    }

    /// The document's mode.
    pub fn quirks_mode(&self, document: NodeId) -> QuirksMode {
        self.doc(document).mode
    }

    /// Sets the document's mode.
    pub fn set_quirks_mode(&mut self, document: NodeId, mode: QuirksMode) {
        self.doc_mut(document).mode = mode;
    }

    /// `Document.contentType`.
    pub fn content_type(&self, document: NodeId) -> &str {
        &self.doc(document).content_type
    }

    /// Sets the content type.
    pub fn set_content_type(&mut self, document: NodeId, content_type: &str) {
        self.doc_mut(document).content_type = content_type.to_owned();
    }

    /// The document's URL, serialized.
    pub fn document_url(&self, document: NodeId) -> &str {
        &self.doc(document).url
    }

    /// Sets the document's URL.
    pub fn set_document_url(&mut self, document: NodeId, url: &str) {
        self.doc_mut(document).url = url.to_owned();
    }

    /// The document's encoding.
    pub fn encoding(&self, document: NodeId) -> &'static encoding_rs::Encoding {
        self.doc(document).encoding
    }

    /// Sets the document's encoding.
    pub fn set_encoding(&mut self, document: NodeId, encoding: &'static encoding_rs::Encoding) {
        self.doc_mut(document).encoding = encoding;
    }

    /// The document's "allow declarative shadow roots" flag.
    pub fn allow_declarative_shadow_roots(&self, document: NodeId) -> bool {
        self.doc(document).allow_declarative_shadow_roots
    }

    /// Sets "allow declarative shadow roots".
    pub fn set_allow_declarative_shadow_roots(&mut self, document: NodeId, allow: bool) {
        self.doc_mut(document).allow_declarative_shadow_roots = allow;
        if let Some(owner) = self.doc(document).template_contents_owner {
            self.doc_mut(owner).allow_declarative_shadow_roots = allow;
        }
    }

    /// The document's tree version: bumped on every mutation of a node whose node document it is. Live
    /// collections and style invalidation compare against it.
    pub fn document_version(&self, document: NodeId) -> u64 {
        self.doc(document).version
    }

    /// `Document.doctype`: the first DocumentType child.
    pub fn document_doctype(&self, document: NodeId) -> Option<NodeId> {
        self.children(document)
            .find(|&c| matches!(self.n(c).data, NodeData::DocumentType(_)))
    }

    /// `Document.documentElement`: the first element child.
    pub fn document_element(&self, document: NodeId) -> Option<NodeId> {
        self.children(document).find(|&c| self.is_element(c))
    }

    /// The `body` element (HTML §3.1.3): the first `body` or `frameset` child of the `html` document element.
    pub fn body(&self, document: NodeId) -> Option<NodeId> {
        let html = self.document_element(document)?;
        if !self.is_html_element_named(html, &local_name!("html")) {
            return None;
        }
        self.children(html).find(|&c| {
            self.is_html_element_named(c, &local_name!("body"))
                || self.is_html_element_named(c, &local_name!("frameset"))
        })
    }

    /// The `head` element: the first `head` child of the `html` document element.
    pub fn head(&self, document: NodeId) -> Option<NodeId> {
        let html = self.document_element(document)?;
        if !self.is_html_element_named(html, &local_name!("html")) {
            return None;
        }
        self.children(html)
            .find(|&c| self.is_html_element_named(c, &local_name!("head")))
    }

    /// Records a mutation of a node whose node document is `doc` (bumps both version counters).
    #[inline]
    /// The attribute's value as rendering sees it: an animated value, else the DOM's.
    pub fn presented_attr(&self, element: NodeId, local: &LocalName) -> Option<&str> {
        self.animated_attr(element, local)
            .or_else(|| self.attr(element, &ns!(), local))
    }

    /// The animated value of an attribute in no namespace.
    pub fn animated_attr(&self, element: NodeId, local: &str) -> Option<&str> {
        if self.animated_attrs.is_empty() {
            return None;
        }
        let list = self.animated_attrs.get(&element)?;
        list.iter().find(|a| &*a.0 == local).map(|a| a.1.as_str())
    }

    /// The animated attributes of `element`.
    pub fn animated_attrs(&self, element: NodeId) -> &[(LocalName, String)] {
        self.animated_attrs
            .get(&element)
            .map_or(&[], |list| list.as_slice())
    }

    /// Sets or clears an attribute's animated value. What depends on attributes hears of it as of an
    /// attribute change; script does not (no mutation record).
    pub fn set_animated_attr(&mut self, element: NodeId, local: LocalName, value: Option<String>) {
        let old_value = self.presented_attr(element, &local).map(str::to_owned);
        let list = self.animated_attrs.entry(element).or_default();
        list.retain(|a| a.0 != local);
        if let Some(v) = &value {
            list.push((local.clone(), v.clone()));
        }
        if list.is_empty() {
            self.animated_attrs.remove(&element);
        }
        let new_value = self.presented_attr(element, &local).map(str::to_owned);
        self.react(|| Reaction::AttributeChanged {
            element,
            local_name: local,
            namespace: None,
            old_value,
            new_value,
        });
        self.bump(element);
    }

    /// Forgets the animated values of nodes that go.
    pub fn forget_animated_attrs(&mut self, gone: impl Fn(NodeId) -> bool) {
        self.animated_attrs.retain(|n, _| !gone(*n));
    }

    /// Records a change to the shape of the tree `node` is in (also a mutation: [`Self::bump`]).
    pub(crate) fn bump_tree(&mut self, node: NodeId) {
        self.tree_version += 1;
        self.bump(node);
    }

    pub(crate) fn bump(&mut self, node: NodeId) {
        self.version += 1;
        let doc = self.n(node).document;
        if let Some(doc) = doc {
            if let NodeData::Document(d) = &mut self.n_mut(doc).data {
                d.version += 1;
            }
        }
    }
}
