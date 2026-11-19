// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-element style side table ([`StyleData`]): stylo's `ElementData`, the restyle bits, the element
//! state and the attribute-derived caches (id and class atoms, the parsed `style` attribute, presentational
//! hints, slot assignment), kept outside `snowlark-dom` because the DOM must not depend on style.
//!
//! The table is indexed by [`NodeId::index`] through a dense map: only elements get a slot. A slot records
//! the generation of the node it was prepared for, so a reused arena index is detected and the slot reset
//! (`StyleData::prepare_element`).
//!
//! During a traversal the table is shared read-only (`&StyleData`) between stylo's worker threads; the
//! per-element bits are atomics, and `ElementData` lives in stylo's `ElementDataWrapper`, whose exclusive
//! access per element is stylo's traversal invariant (an element is styled by exactly one thread, after its
//! parent).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use servo_arc::Arc;
use smallvec::SmallVec;
use snowlark_dom::NodeId;
use style::data::{ElementData, ElementDataWrapper};
use style::properties::{ComputedValues, PropertyDeclarationBlock};
use style::shared_lock::{Locked, SharedRwLock};
use style::Atom;
use stylo_dom::ElementState;

use crate::damage::{ElementRestyle, Restyles};

/// Bits in [`Slot::flags`] above the 16 bits that hold stylo's `ElementSelectorFlags`.
pub(crate) mod bits {
    /// The selector flags (stylo's `ElementSelectorFlags`, 12 bits used).
    pub const SELECTOR_FLAGS: u32 = 0xffff;
    /// `ElementData` has been initialized (`TElement::has_data`).
    pub const HAS_DATA: u32 = 1 << 16;
    /// A descendant in the flat tree needs a restyle.
    pub const DIRTY_DESCENDANTS: u32 = 1 << 17;
    /// The element has a snapshot in the snapshot map.
    pub const HAS_SNAPSHOT: u32 = 1 << 18;
    /// The snapshot has been processed by the invalidator.
    pub const HANDLED_SNAPSHOT: u32 = 1 << 19;
    /// The element was inserted with descendants that are not prepared yet (they are at the next
    /// restyle, see `StyleEngine::node_inserted`).
    pub const UNPREPARED_DESCENDANTS: u32 = 1 << 20;
    /// An attribute that can change the presentational hints changed since they were parsed: they
    /// are parsed again at the next restyle (see `StyleEngine::flush_dirty_hints`).
    pub const HINTS_DIRTY: u32 = 1 << 21;
}

/// What an element's attributes say, parsed once per attribute change instead of on every selector match.
#[derive(Default)]
pub(crate) struct AttrCache {
    /// The `id` attribute as an atom (null-namespace `id`).
    pub id: Option<Atom>,
    /// The `class` attribute's tokens.
    pub classes: SmallVec<[Atom; 2]>,
    /// The `part` attribute's tokens.
    pub parts: SmallVec<[Atom; 1]>,
    /// The `exportparts` attribute's mappings (CSS Shadow Parts 1 §4.2): (inner name, outer name).
    pub exportparts: SmallVec<[(Atom, Atom); 1]>,
    /// The parsed `style` attribute.
    pub style_attribute: Option<Arc<Locked<PropertyDeclarationBlock>>>,
    /// Presentational hints (HTML §15 "presentational hints"), as one declaration block.
    pub hints: Option<Arc<Locked<PropertyDeclarationBlock>>>,
}

/// One element's style state.
pub(crate) struct Slot {
    /// The node this slot was prepared for (a later node at the same arena index has another generation).
    pub id: NodeId,
    /// [`bits`] plus the selector flags.
    pub flags: AtomicU32,
    /// stylo's per-element data: computed styles, restyle hint, damage.
    pub data: ElementDataWrapper,
    /// A copy of the primary style, synced after each traversal, so `StyleData::styles` can hand out a
    /// plain reference.
    pub primary: Option<Arc<ComputedValues>>,
    /// State derived from the DOM (link, defined, form control attributes).
    pub derived_state: ElementState,
    /// State set by the page layer ([`StyleData::set_state`]): hover, focus, active, checkedness...
    pub set_state: ElementState,
    /// The state bits the page layer owns; the derived value of these bits is ignored.
    pub set_mask: ElementState,
    /// Attribute caches.
    pub attrs: AttrCache,
    /// The shadow root this element is in, if any.
    pub containing_shadow: Option<NodeId>,
    /// The slot this element is assigned to (DOM §4.2.2.3), if any.
    pub assigned_slot: Option<NodeId>,
    /// For a `slot` element with assigned nodes: index into `StyleData::slotted`.
    pub slotted_list: Option<u32>,
    /// The size of this element as a size query container, set by layout.
    pub container_size: Option<(Option<f32>, Option<f32>)>,
}

impl Slot {
    fn new(id: NodeId) -> Slot {
        Slot {
            id,
            flags: AtomicU32::new(0),
            data: ElementDataWrapper::default(),
            primary: None,
            derived_state: ElementState::empty(),
            set_state: ElementState::empty(),
            set_mask: ElementState::empty(),
            attrs: AttrCache::default(),
            containing_shadow: None,
            assigned_slot: None,
            slotted_list: None,
            container_size: None,
        }
    }

    /// The effective element state.
    #[inline]
    pub fn state(&self) -> ElementState {
        (self.derived_state & !self.set_mask) | (self.set_state & self.set_mask)
    }

    #[inline]
    pub fn has(&self, bit: u32) -> bool {
        self.flags.load(Ordering::Relaxed) & bit != 0
    }

    #[inline]
    pub fn set(&self, bit: u32) {
        self.flags.fetch_or(bit, Ordering::Relaxed);
    }

    #[inline]
    pub fn clear(&self, bit: u32) {
        self.flags.fetch_and(!bit, Ordering::Relaxed);
    }

    /// Drops the computed styles and resets stylo's data.
    pub fn reset_data(&mut self) {
        *self.data.borrow_mut() = ElementData::default();
        self.primary = None;
        self.clear(bits::HAS_DATA | bits::DIRTY_DESCENDANTS);
    }
}

/// A sentinel in the dense index for "no slot".
const NONE: u32 = u32::MAX;

/// The style side table of one [`snowlark_dom::Dom`]: one per event loop, shared by the style engines of all
/// the documents in it. Also owns the shared lock that guards CSSOM objects (stylesheets, declaration
/// blocks) for those documents.
pub struct StyleData {
    /// Arena index → dense slot index.
    index: Vec<u32>,
    pub(crate) slots: Vec<Slot>,
    pub(crate) lock: SharedRwLock,
    /// The assigned nodes of each slot element that has some (indexed by `Slot::slotted_list`).
    pub(crate) slotted: Vec<Vec<NodeId>>,
    /// Free entries of `slotted`.
    pub(crate) slotted_free: Vec<u32>,
    /// Manual slot assignments (`HTMLSlotElement.assign()`): each slot's manually assigned nodes.
    pub(crate) manual_slots: rustc_hash::FxHashMap<NodeId, Vec<NodeId>>,
    /// Ids of elements restyled by the current traversal (`restyled_len` entries are valid).
    pub(crate) restyled: Vec<AtomicU64>,
    /// The packed [`ElementRestyle`] of each `restyled` entry.
    pub(crate) restyled_change: Vec<AtomicU32>,
    pub(crate) restyled_len: AtomicU32,
    /// Restyle records not yet taken by [`StyleData::take_restyles`].
    restyle_log: Vec<ElementRestyle>,
    restyle_log_overflowed: bool,
    /// While a reaction batch is handled ([`StyleData::begin_batch`]): an element that, with every
    /// flat-tree ancestor, got the dirty-descendants bit in this batch (`NodeId::to_bits`, 0 for
    /// none), where the next ancestor walk stops. In a batch the tree is as the operation left it
    /// and only the traversal clears the bit, so it holds until a slot is mutated (a reset clears
    /// the bit; a new slot assignment moves an element in the flat tree), which forgets it.
    marked: AtomicU64,
    in_batch: bool,
}

impl Default for StyleData {
    fn default() -> Self {
        StyleData::new()
    }
}

impl std::fmt::Debug for StyleData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StyleData")
            .field("slots", &self.slots.len())
            .finish_non_exhaustive()
    }
}

impl StyleData {
    /// An empty table.
    pub fn new() -> StyleData {
        StyleData {
            index: Vec::new(),
            slots: Vec::new(),
            lock: SharedRwLock::new(),
            slotted: Vec::new(),
            slotted_free: Vec::new(),
            manual_slots: Default::default(),
            restyled: Vec::new(),
            restyled_change: Vec::new(),
            restyled_len: AtomicU32::new(0),
            restyle_log: Vec::new(),
            restyle_log_overflowed: false,
            marked: AtomicU64::new(0),
            in_batch: false,
        }
    }

    /// Starts a reaction batch: see [`StyleData::marked_ancestors`].
    pub(crate) fn begin_batch(&mut self) {
        self.in_batch = true;
        *self.marked.get_mut() = 0;
    }

    /// Forgets [`StyleData::marked_ancestors`].
    pub(crate) fn forget_marked_ancestors(&mut self) {
        *self.marked.get_mut() = 0;
    }

    /// Ends a reaction batch.
    pub(crate) fn end_batch(&mut self) {
        self.in_batch = false;
        *self.marked.get_mut() = 0;
    }

    /// The element where an ancestor walk that sets the dirty-descendants bit can stop: it and its
    /// flat-tree ancestors have the bit (set in this reaction batch, nothing cleared since).
    #[inline]
    pub(crate) fn marked_ancestors(&self) -> Option<NodeId> {
        NodeId::from_bits(self.marked.load(Ordering::Relaxed))
    }

    /// Records, in a reaction batch, that `id` and its flat-tree ancestors have the bit.
    #[inline]
    pub(crate) fn set_marked_ancestors(&self, id: NodeId) {
        if self.in_batch {
            self.marked.store(id.to_bits(), Ordering::Relaxed);
        }
    }

    /// The lock guarding the stylesheets and declaration blocks of every document in this table's `Dom`.
    pub fn shared_lock(&self) -> &SharedRwLock {
        &self.lock
    }

    /// Grows the dense index to cover `arena_len` arena slots.
    pub(crate) fn reserve(&mut self, arena_len: usize) {
        if self.index.len() < arena_len {
            self.index.resize(arena_len, NONE);
        }
        if self.restyled.len() < self.slots.len() {
            self.restyled
                .resize_with(self.slots.len(), || AtomicU64::new(0));
            self.restyled_change
                .resize_with(self.slots.len(), || AtomicU32::new(0));
        }
    }

    /// The slot for `id`, if it was prepared for this generation.
    #[inline]
    pub(crate) fn slot(&self, id: NodeId) -> Option<&Slot> {
        let dense = *self.index.get(id.index() as usize)?;
        if dense == NONE {
            return None;
        }
        let slot = &self.slots[dense as usize];
        (slot.id == id).then_some(slot)
    }

    #[inline]
    pub(crate) fn slot_mut(&mut self, id: NodeId) -> Option<&mut Slot> {
        // (a mutated slot can lose the dirty bit or move in the flat tree)
        *self.marked.get_mut() = 0;
        let dense = *self.index.get(id.index() as usize)?;
        if dense == NONE {
            return None;
        }
        let slot = &mut self.slots[dense as usize];
        (slot.id == id).then_some(slot)
    }

    /// The index of `id`'s slot in `slots`, if it was prepared for this generation.
    #[inline]
    pub(crate) fn dense(&self, id: NodeId) -> Option<usize> {
        let dense = *self.index.get(id.index() as usize)?;
        (dense != NONE && self.slots[dense as usize].id == id).then_some(dense as usize)
    }

    /// The slot for an element the traversal reaches. Every element in a styled tree is prepared before
    /// the traversal, so a missing slot is a bug in the caller (a DOM change not reported).
    #[inline]
    pub(crate) fn el(&self, id: NodeId) -> &Slot {
        match self.slot(id) {
            Some(s) => s,
            None => {
                panic!("snowlark-style: element {id:?} was not prepared (missing node_inserted?)")
            }
        }
    }

    /// Makes sure `id` has a slot for its current generation, resetting a slot left by an earlier node
    /// at the same arena index. Returns whether the slot was (re)created.
    pub(crate) fn prepare_slot(&mut self, id: NodeId) -> bool {
        *self.marked.get_mut() = 0;
        let i = id.index() as usize;
        if self.index.len() <= i {
            self.index.resize(i + 1, NONE);
        }
        let dense = self.index[i];
        if dense == NONE {
            self.index[i] = self.slots.len() as u32;
            self.slots.push(Slot::new(id));
            self.restyled.push(AtomicU64::new(0));
            self.restyled_change.push(AtomicU32::new(0));
            return true;
        }
        let slot = &mut self.slots[dense as usize];
        if slot.id != id {
            if let Some(list) = slot.slotted_list.take() {
                self.slotted[list as usize].clear();
                self.slotted_free.push(list);
            }
            *slot = Slot::new(id);
            return true;
        }
        false
    }

    /// Records that the traversal restyled `id` (lock-free; every element is styled once per traversal).
    #[inline]
    pub(crate) fn note_restyled(&self, id: NodeId, change: u32) {
        let i = self.restyled_len.fetch_add(1, Ordering::Relaxed) as usize;
        if let Some(cell) = self.restyled.get(i) {
            cell.store(id.to_bits(), Ordering::Relaxed);
            self.restyled_change[i].store(change, Ordering::Relaxed);
        }
    }

    /// The elements whose styles changed since the last call, with what each change requires of
    /// layout (see [`crate::StyleChange`]): what a box tree updates itself from after a restyle.
    pub fn take_restyles(&mut self) -> Restyles {
        let overflowed = std::mem::take(&mut self.restyle_log_overflowed);
        Restyles {
            elements: std::mem::take(&mut self.restyle_log),
            overflowed,
        }
    }

    /// The restyle records since the last [`StyleData::take_restyles`], and whether more were made
    /// than the log keeps (then every element counts as restyled).
    pub fn restyles(&self) -> (&[ElementRestyle], bool) {
        (&self.restyle_log, self.restyle_log_overflowed)
    }

    /// Appends a traversal's records to the log (bounded: past the limit, only "overflowed" is kept).
    pub(crate) fn log_restyle(&mut self, record: ElementRestyle) {
        if self.restyle_log_overflowed {
            return;
        }
        if self.restyle_log.len() >= 4 * self.slots.len() + 1024 {
            self.restyle_log = Vec::new();
            self.restyle_log_overflowed = true;
            return;
        }
        self.restyle_log.push(record);
    }

    /// The assigned nodes of a `slot` element (DOM §4.2.2.2), when it has some.
    pub fn slotted_nodes(&self, slot: NodeId) -> Option<&[NodeId]> {
        let i = self.slot(slot)?.slotted_list?;
        self.slotted.get(i as usize).map(Vec::as_slice)
    }

    /// Replaces the assigned nodes of `slot`.
    pub(crate) fn set_slotted_nodes(&mut self, slot: NodeId, nodes: Vec<NodeId>) {
        let Some(dense) = self
            .index
            .get(slot.index() as usize)
            .copied()
            .filter(|&d| d != NONE)
        else {
            return;
        };
        let current = self.slots[dense as usize].slotted_list;
        match (current, nodes.is_empty()) {
            (Some(i), true) => {
                self.slotted[i as usize].clear();
                self.slotted_free.push(i);
                self.slots[dense as usize].slotted_list = None;
            }
            (Some(i), false) => self.slotted[i as usize] = nodes,
            (None, true) => {}
            (None, false) => {
                let i = match self.slotted_free.pop() {
                    Some(i) => {
                        self.slotted[i as usize] = nodes;
                        i
                    }
                    None => {
                        self.slotted.push(nodes);
                        (self.slotted.len() - 1) as u32
                    }
                };
                self.slots[dense as usize].slotted_list = Some(i);
            }
        }
    }

    /// The primary computed style of `element`, if it has been styled and is not in a `display: none`
    /// subtree (use `StyleEngine::computed_style` for those).
    #[inline]
    pub fn styles(&self, element: NodeId) -> Option<&ComputedValues> {
        let slot = self.slot(element)?;
        if !slot.has(bits::HAS_DATA) {
            return None;
        }
        slot.primary.as_deref()
    }

    /// Like [`StyleData::styles`], as a shared pointer.
    pub fn styles_arc(&self, element: NodeId) -> Option<Arc<ComputedValues>> {
        let slot = self.slot(element)?;
        if !slot.has(bits::HAS_DATA) {
            return None;
        }
        slot.primary.clone()
    }

    /// The element's current state (what `:hover`, `:checked`, ... match against).
    pub fn state(&self, element: NodeId) -> ElementState {
        self.slot(element)
            .map(Slot::state)
            .unwrap_or(ElementState::empty())
    }

    /// The size last recorded for `element` as a size query container.
    pub fn query_container_size(&self, element: NodeId) -> Option<(Option<f32>, Option<f32>)> {
        self.slot(element).and_then(|s| s.container_size)
    }

    /// Records the size of `element` as a size query container, for `@container` size queries. Layout
    /// sets this; a change needs a restyle of the container's subtree, which the caller requests with
    /// `StyleEngine::invalidate_subtree`.
    pub fn set_query_container_size(
        &mut self,
        element: NodeId,
        width: Option<f32>,
        height: Option<f32>,
    ) {
        if let Some(slot) = self.slot_mut(element) {
            slot.container_size = Some((width, height));
        }
    }
}
