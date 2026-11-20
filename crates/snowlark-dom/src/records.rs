// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mutation records (DOM §4.3) and the reaction queue that stands in for the "steps" hooks other
//! specifications define (insertion steps, removing steps, attribute change steps, ...).
//!
//! The DOM does not know about MutationObserver objects. The observer layer marks the nodes that have
//! registered observers with [`Dom::set_observed`]; while any node is marked, every mutation whose target has
//! a marked inclusive ancestor queues a full [`MutationRecord`] (old values always captured). The observer
//! layer filters by each observer's options when it delivers. With no node marked, queueing costs one
//! branch.
//!
//! Removal adds transient registered observers (DOM §4.2.3 "remove" step 18) by marking the removed node and
//! remembering the ancestor whose observers apply, see [`Dom::transient_observers`].

use html5ever::{LocalName, Namespace};

use crate::dom::Dom;
use crate::node::{flags, Attr, ElementData, NodeId};

/// `MutationRecord.type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MutationKind {
    ChildList,
    Attributes,
    CharacterData,
}

/// One queued mutation record (DOM §4.3.3 "queue a mutation record").
#[derive(Clone, Debug, PartialEq)]
pub struct MutationRecord {
    pub kind: MutationKind,
    pub target: NodeId,
    pub added_nodes: Vec<NodeId>,
    pub removed_nodes: Vec<NodeId>,
    pub previous_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    /// For "attributes": the attribute's local name.
    pub attribute_name: Option<LocalName>,
    /// For "attributes": the attribute's namespace (`None` for null).
    pub attribute_namespace: Option<Namespace>,
    /// The old attribute value or character data.
    pub old_value: Option<String>,
}

/// A hook point reached by a DOM algorithm, for the layers above (HTML element behaviour, custom element
/// reactions, live ranges, slots). Queued only while [`Dom::set_reactions_enabled`] is on; the page layer
/// drains them with [`Dom::take_reactions`] after each DOM operation, before script continues.
///
/// A reaction can name a node that was freed before the queue was drained (the scratch fragment of an
/// `innerHTML` set, for example); check [`Dom::is_alive`].
#[derive(Clone, Debug, PartialEq)]
pub enum Reaction {
    /// "Attribute change steps" (DOM §4.9) and the custom element attributeChangedCallback trigger.
    AttributeChanged {
        element: NodeId,
        local_name: LocalName,
        namespace: Option<Namespace>,
        old_value: Option<String>,
        new_value: Option<String>,
    },
    /// "Insertion steps" for each shadow-including inclusive descendant of an inserted node, in
    /// shadow-including tree order; `connected` is the node's connectedness after insertion (the
    /// "post-connection steps" and connectedCallback apply when it is true). `root` is true for the
    /// node inserted into a parent (each child of an inserted fragment) and false for its
    /// descendants, which a [`ReactionFilter`] can leave out. `subtree`, for a root, is what its
    /// shadow-including descendants (not itself) hold, as [`subtree`] bits; 0 for a descendant.
    Inserted {
        node: NodeId,
        connected: bool,
        root: bool,
        subtree: u8,
    },
    /// "Removing steps": `old_parent` is set for the removed node itself and null for its descendants,
    /// as are its old siblings (what live ranges and NodeIterators adjust by, DOM §4.2.3 "remove" steps
    /// 4-7). `was_connected` says whether a disconnectedCallback applies. `descendants`, for the
    /// removed node itself, is its shadow-including descendants at its removal, in tree order, as a
    /// range of the list [`Dom::take_removed_descendants`] returns with the batch (empty for the
    /// descendants' own reactions): the walk that disconnects them collects them, so a layer that
    /// visits each of them does not walk the subtree again. `subtree`, for the removed node itself,
    /// is what those descendants hold, as [`subtree`] bits; 0 for a descendant.
    Removed {
        node: NodeId,
        old_parent: Option<NodeId>,
        old_previous: Option<NodeId>,
        old_next: Option<NodeId>,
        was_connected: bool,
        descendants: (u32, u32),
        subtree: u8,
    },
    /// "Children changed steps" for `parent`.
    ChildrenChanged { parent: NodeId },
    /// "Replace data" (DOM §4.10) steps 8-11, for live ranges: `count` code units of `node` from
    /// `offset` were replaced by `added` code units. Not queued for an append (no live range can be
    /// after the end of the data).
    DataReplaced {
        node: NodeId,
        offset: u32,
        count: u32,
        added: u32,
    },
    /// "Split a Text node" (DOM §4.11) steps 7.2-7.5, after `new_node` was inserted and before the
    /// data from `offset` is removed from `node`: `index` is `node`'s index in `parent`.
    TextSplit {
        node: NodeId,
        new_node: NodeId,
        offset: u32,
        parent: NodeId,
        index: u32,
    },
    /// `normalize()` (DOM §4.4) steps 6.1-6.4: the data of `from` (`index` in `parent`) was appended
    /// to `into` at `at`, before `from` is removed.
    TextMerged {
        from: NodeId,
        into: NodeId,
        at: u32,
        parent: NodeId,
        index: u32,
    },
    /// "Adopting steps" for each shadow-including inclusive descendant.
    Adopted { node: NodeId, old_document: NodeId },
    /// "Cloning steps": `copy` was cloned from `node`.
    Cloned {
        node: NodeId,
        copy: NodeId,
        subtree: bool,
    },
}

/// What a node is to the walk of an inserted or removed subtree ([`ReactionFilter::descendant`]),
/// and what the subtree holds (the `subtree` of [`Reaction::Inserted`] and [`Reaction::Removed`]:
/// the OR of its descendants' bits but [`REPORT`]).
pub mod subtree {
    /// The descendant gets a reaction of its own (never in a summary).
    pub const REPORT: u8 = 1;
    /// The bits a [`super::ReactionFilter`] gives meaning to (all of them without a filter).
    pub const LAYER: u8 = 0x7E;
    /// A descendant was marked with [`crate::Dom::set_wrapped`].
    pub const WRAPPED: u8 = 0x80;
}

/// Which nodes the layer above wants per-node reactions for, besides the nodes a mutation is about
/// ([`Dom::set_reaction_filter`]). Without a filter every node gets its reactions.
#[derive(Clone, Copy, Debug)]
pub struct ReactionFilter {
    /// [`subtree`] bits for a descendant element of an inserted, removed or adopted node:
    /// [`subtree::REPORT`] when it gets an `Inserted`, `Removed` or `Adopted` reaction of its own,
    /// and any of the layer's own bits ([`subtree::LAYER`]), which the walk ORs into the `subtree`
    /// summary of the inserted or removed node's reaction. Descendants that are not elements get no
    /// reaction: the layer above walks the subtree of the reported node for what it needs of them.
    pub descendant: fn(&ElementData) -> u8,
    /// Whether an attribute that the parser or cloning gives a new element, before anything can
    /// observe it, gets an `AttributeChanged` reaction.
    pub new_attribute: fn(&ElementData, &Attr) -> bool,
}

impl Dom {
    /// Sets (or clears) the [`ReactionFilter`]: reactions for descendants and for the attributes of
    /// new elements that nobody above needs are not queued.
    pub fn set_reaction_filter(&mut self, filter: Option<ReactionFilter>) {
        self.reaction_filter = filter;
    }

    /// Whether `id`, a descendant of the node a tree mutation is about, gets its own reaction.
    #[inline]
    pub(crate) fn reports_descendant(&self, id: NodeId) -> bool {
        self.descendant_bits(id) & subtree::REPORT != 0
    }

    /// The [`subtree`] bits of `id`, a descendant of the node a tree mutation is about: whether it
    /// gets its own reaction, the reaction filter's bits for it, and [`subtree::WRAPPED`].
    #[inline]
    pub(crate) fn descendant_bits(&self, id: NodeId) -> u8 {
        let n = self.n(id);
        let wrapped = if n.flags & flags::WRAPPED != 0 {
            subtree::WRAPPED
        } else {
            0
        };
        wrapped
            | match self.reaction_filter {
                None => subtree::REPORT | subtree::LAYER,
                Some(f) => match &n.data {
                    crate::node::NodeData::Element(e) => (f.descendant)(e),
                    _ => 0,
                },
            }
    }

    /// Marks `node` as having (had) a script wrapper, for [`subtree::WRAPPED`]. The mark stays for
    /// the node's lifetime.
    #[inline]
    pub fn set_wrapped(&mut self, node: NodeId) {
        self.n_mut(node).flags |= flags::WRAPPED;
    }

    /// Whether [`Dom::set_wrapped`] marked `node`.
    #[inline]
    pub fn is_wrapped(&self, node: NodeId) -> bool {
        self.n(node).flags & flags::WRAPPED != 0
    }

    /// False only for a live node [`Dom::set_wrapped`] never marked; true for a marked one and for
    /// an id that is no longer alive (whose mark says nothing).
    #[inline]
    pub fn may_be_wrapped(&self, node: NodeId) -> bool {
        match self.nodes.get(node.index as usize) {
            Some(n)
                if n.generation == node.generation
                    && !matches!(n.data, crate::node::NodeData::Free { .. }) =>
            {
                n.flags & flags::WRAPPED != 0
            }
            _ => true,
        }
    }

    /// Marks `node` as having (or no longer having) registered MutationObservers.
    pub fn set_observed(&mut self, node: NodeId, observed: bool) {
        let n = self.n_mut(node);
        let before = n.flags & (flags::OBSERVED | flags::TRANSIENT_OBSERVED) != 0;
        if observed {
            n.flags |= flags::OBSERVED;
        } else {
            n.flags &= !flags::OBSERVED;
        }
        let after = n.flags & (flags::OBSERVED | flags::TRANSIENT_OBSERVED) != 0;
        match (before, after) {
            (false, true) => self.observed += 1,
            (true, false) => self.observed -= 1,
            _ => {}
        }
    }

    /// Whether `node` is marked with [`Dom::set_observed`].
    pub fn is_observed(&self, node: NodeId) -> bool {
        self.n(node).flags & flags::OBSERVED != 0
    }

    /// The transient registered observers added since the last [`Dom::clear_transient_observers`]: each pair
    /// is (removed node, inclusive ancestor of its old parent whose subtree observers now also observe the
    /// removed node).
    pub fn transient_observers(&self) -> &[(NodeId, NodeId)] {
        &self.transient
    }

    /// Removes all transient registered observers (DOM §4.3.2 "notify mutation observers" step 6.2).
    pub fn clear_transient_observers(&mut self) {
        for (node, _) in std::mem::take(&mut self.transient) {
            if !self.is_alive(node) {
                continue;
            }
            let n = self.n_mut(node);
            if n.flags & flags::TRANSIENT_OBSERVED != 0 {
                n.flags &= !flags::TRANSIENT_OBSERVED;
                if n.flags & flags::OBSERVED == 0 {
                    self.observed -= 1;
                }
            }
        }
    }

    /// Takes the queued mutation records, oldest first.
    pub fn take_mutation_records(&mut self) -> Vec<MutationRecord> {
        std::mem::take(&mut self.records)
    }

    /// The queued mutation records.
    pub fn mutation_records(&self) -> &[MutationRecord] {
        &self.records
    }

    /// Turns the reaction queue on or off.
    pub fn set_reactions_enabled(&mut self, enabled: bool) {
        self.reactions_enabled = enabled;
    }

    /// Takes the queued reactions, oldest first. The removed nodes' descendants their
    /// [`Reaction::Removed`] ranges index are kept for [`Dom::take_removed_descendants`] (until the
    /// next take).
    pub fn take_reactions(&mut self) -> Vec<Reaction> {
        self.removed_taken.clear();
        std::mem::swap(&mut self.removed_taken, &mut self.removed_descendants);
        std::mem::take(&mut self.reactions)
    }

    /// The list the [`Reaction::Removed`] `descendants` ranges of the batch [`Dom::take_reactions`]
    /// last returned index. Hand it back with [`Dom::recycle_removed_descendants`].
    pub fn take_removed_descendants(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.removed_taken)
    }

    /// Hands back a list [`Dom::take_removed_descendants`] returned, to reuse its buffer.
    pub fn recycle_removed_descendants(&mut self, mut list: Vec<NodeId>) {
        if self.removed_taken.capacity() == 0 && list.capacity() <= 1 << 16 {
            list.clear();
            self.removed_taken = list;
        }
    }

    /// How many reactions are queued.
    #[inline]
    pub fn pending_reactions(&self) -> usize {
        self.reactions.len()
    }

    /// Hands back a list [`Dom::take_reactions`] returned, once handled, so the queue reuses its
    /// buffer instead of allocating one per operation.
    pub fn recycle_reactions(&mut self, mut list: Vec<Reaction>) {
        if self.reactions.capacity() == 0 && list.capacity() <= 4096 {
            list.clear();
            self.reactions = list;
        }
    }

    #[inline]
    pub(crate) fn react(&mut self, reaction: impl FnOnce() -> Reaction) {
        if self.reactions_enabled {
            let r = reaction();
            if self.quiet_tree_reactions && !self.reported_while_quiet(&r) {
                return;
            }
            self.reactions.push(r);
        }
    }

    /// The reactions kept while tree reactions are quiet: attribute changes, and the insertion of an
    /// element that may become a custom element (it is upgraded when its insertion is handled).
    fn reported_while_quiet(&self, r: &Reaction) -> bool {
        match r {
            Reaction::AttributeChanged { .. } => true,
            Reaction::Inserted { node, .. } => matches!(
                &self.n(*node).data,
                crate::node::NodeData::Element(e) if e.custom_state == crate::CustomElementState::Undefined
            ),
            _ => false,
        }
    }

    /// Whether a record whose target is `target` has any interested observer.
    #[inline]
    pub(crate) fn wants_records(&self, target: NodeId) -> bool {
        if self.observed == 0 {
            return false;
        }
        let mut cur = Some(target);
        while let Some(id) = cur {
            let n = self.n(id);
            if n.flags & (flags::OBSERVED | flags::TRANSIENT_OBSERVED) != 0 {
                return true;
            }
            cur = n.parent;
        }
        false
    }

    /// DOM §4.3.3 "queue a tree mutation record". The node lists are copied only when an observer
    /// wants the record.
    pub(crate) fn queue_tree_record(
        &mut self,
        target: NodeId,
        added: &[NodeId],
        removed: &[NodeId],
        previous_sibling: Option<NodeId>,
        next_sibling: Option<NodeId>,
    ) {
        debug_assert!(!added.is_empty() || !removed.is_empty());
        if !self.wants_records(target) {
            return;
        }
        self.records.push(MutationRecord {
            kind: MutationKind::ChildList,
            target,
            added_nodes: added.to_vec(),
            removed_nodes: removed.to_vec(),
            previous_sibling,
            next_sibling,
            attribute_name: None,
            attribute_namespace: None,
            old_value: None,
        });
    }

    /// Queues an "attributes" or "characterData" record.
    pub(crate) fn queue_value_record(
        &mut self,
        kind: MutationKind,
        target: NodeId,
        name: Option<(&LocalName, &Namespace)>,
        old_value: impl FnOnce(&Dom) -> Option<String>,
    ) {
        if !self.wants_records(target) {
            return;
        }
        let old_value = old_value(self);
        self.records.push(MutationRecord {
            kind,
            target,
            added_nodes: Vec::new(),
            removed_nodes: Vec::new(),
            previous_sibling: None,
            next_sibling: None,
            attribute_name: name.map(|(l, _)| l.clone()),
            attribute_namespace: name.and_then(|(_, ns)| (!ns.is_empty()).then(|| ns.clone())),
            old_value,
        });
    }

    /// DOM §4.2.3 "remove" step 18: the removed node gets transient observers from its old ancestors.
    pub(crate) fn add_transient_observers(&mut self, removed: NodeId, old_parent: NodeId) {
        if self.observed == 0 {
            return;
        }
        let mut cur = Some(old_parent);
        let mut any = false;
        while let Some(id) = cur {
            let n = self.n(id);
            if n.flags & (flags::OBSERVED | flags::TRANSIENT_OBSERVED) != 0 {
                self.transient.push((removed, id));
                any = true;
            }
            cur = self.n(id).parent;
        }
        if any {
            let n = self.n_mut(removed);
            let was_unobserved = n.flags & (flags::OBSERVED | flags::TRANSIENT_OBSERVED) == 0;
            n.flags |= flags::TRANSIENT_OBSERVED;
            if was_unobserved {
                self.observed += 1;
            }
        }
    }
}
