// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! stylo's DOM traits over `snowlark-dom`, through two `Copy` handles:
//!
//! - [`StyleNode`] (a reference to the [`TreeView`] of one restyle plus a [`NodeId`]) implements `TNode`,
//!   `TDocument` and `TShadowRoot`;
//! - [`StyleElement`] implements `TElement` and `selectors::Element`. It is a single pointer, to the
//!   element's [`ElementCell`] in the view, because stylo's style sharing cache keeps elements in
//!   pointer-sized slots (`sharing::TypelessSharingCache`). The view builds the cells, one per element slot
//!   of the side table, in chunks of [`CELL_CHUNK`], each the first time a handle in it is asked for:
//!   a view lives for one restyle or one query, and one that touches a few elements builds a few
//!   chunks, not a cell for every element the event loop ever had (Blink's style update is in
//!   proportion to what is dirty, not to the heap).
//!
//! The traversal walks the flat tree (CSS Scoping §2.1): a shadow host's children are its shadow root's
//! children, a `slot`'s children are its assigned nodes (or its own children when it has none), and the
//! parent of a slotted node is its slot. Selector matching uses the DOM tree (`parent_element` and the
//! sibling links), as Selectors 4 requires.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::ptr::NonNull;
use std::sync::OnceLock;

use app_units::Au;
use rustc_hash::FxHashMap;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::{BloomFilter, BLOOM_HASH_MASK};
use selectors::matching::{ElementSelectorFlags, MatchingContext, VisitedHandlingMode};
use selectors::sink::Push;
use selectors::OpaqueElement;
use servo_arc::{Arc, ArcBorrow};
use snowlark_dom::{local_name, ns, Dom, LocalName, Namespace, NodeData, NodeId};
use style::animation::AnimationSetKey;
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::author_styles::AuthorStyles;
use style::bloom::each_relevant_element_hash;
use style::context::{QuirksMode, SharedStyleContext};
use style::data::{ElementDataMut, ElementDataRef};
use style::dom::{LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot};
use style::properties::PropertyDeclarationBlock;
use style::rule_tree::{CascadeLevel, CascadeOrigin};
use style::selector_parser::{AttrValue, Lang, NonTSPseudoClass, PseudoElement, SelectorImpl};
use style::shared_lock::{Locked, SharedRwLock};
use style::stylesheets::layer_rule::LayerOrder;
use style::stylesheets::DocumentStyleSheet;
use style::stylist::CascadeData;
use style::values::computed::Display;
use style::values::{AtomIdent, AtomString, GenericAtomIdent};
use style::{Atom as WeakAtom, CaseSensitivityExt};
use stylo_dom::ElementState;

use crate::table::{bits, Slot, StyleData};

/// The style data of one shadow root: its stylesheets (in tree order, then adopted) and their cascade data.
pub(crate) type ShadowStyles = AuthorStyles<DocumentStyleSheet>;

/// What the handles of one restyle (or one query) see: the DOM, the side table, the shadow roots' cascade
/// data, and the document's mode.
pub(crate) struct TreeView<'a> {
    pub dom: &'a Dom,
    pub data: &'a StyleData,
    pub shadows: &'a FxHashMap<NodeId, ShadowStyles>,
    pub document: NodeId,
    pub quirks_mode: QuirksMode,
    pub html_document: bool,
    /// Whether any animation or transition is running in the document (`TElement::may_have_animations`).
    pub animating: bool,
    /// The script animations in effect, when there are any (`TElement::animation_rule`).
    pub script: Option<&'a std::sync::RwLock<crate::web_animations::ScriptAnimations>>,
    /// Handles for the assigned nodes of each slot, parallel to `StyleData::slotted`, so
    /// `TElement::slotted_nodes` can return a slice.
    slotted: OnceLock<Vec<Vec<StyleNode<'a>>>>,
    /// One cell per slot of the side table (parallel to `StyleData::slots`), which [`StyleElement`]s point
    /// to, in chunks of [`CELL_CHUNK`] built when first used.
    cells: Box<[OnceLock<Box<[ElementCell<'a>]>>]>,
}

/// The number of [`ElementCell`]s a [`TreeView`] builds at once.
pub(crate) const CELL_CHUNK: usize = 512;

impl<'a> TreeView<'a> {
    pub fn new(
        dom: &'a Dom,
        data: &'a StyleData,
        shadows: &'a FxHashMap<NodeId, ShadowStyles>,
        document: NodeId,
        animating: bool,
    ) -> TreeView<'a> {
        let quirks_mode = match dom.quirks_mode(document) {
            snowlark_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
            snowlark_dom::QuirksMode::Quirks => QuirksMode::Quirks,
            snowlark_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
        };
        TreeView {
            dom,
            data,
            shadows,
            document,
            quirks_mode,
            html_document: dom.is_html_document(document),
            animating,
            script: None,
            slotted: OnceLock::new(),
            cells: (0..data.slots.len().div_ceil(CELL_CHUNK))
                .map(|_| OnceLock::new())
                .collect(),
        }
    }

    /// The handle of node `id`.
    #[inline]
    pub fn node(&'a self, id: NodeId) -> StyleNode<'a> {
        StyleNode { view: self, id }
    }

    /// The handle of element `id`, which must have a slot in the side table (every element of a styled
    /// tree is prepared before the traversal; a missing slot means a DOM change was not reported).
    /// A missing one is a bug: a panic in debug builds; in release builds it is logged and the
    /// handle of the document element stands in, so invalidation outside the traversal does extra
    /// work rather than abort the page (the restyle prepares any missing element before its
    /// traversal starts).
    #[inline]
    pub fn element(&'a self, id: NodeId) -> StyleElement<'a> {
        match self.data.dense(id) {
            Some(dense) => self.cell(dense),
            None => self.unprepared(id),
        }
    }

    /// The handle of the element in slot `dense` of the side table.
    #[inline]
    fn cell(&'a self, dense: usize) -> StyleElement<'a> {
        let chunk = self.cells[dense / CELL_CHUNK].get_or_init(|| {
            let start = dense / CELL_CHUNK * CELL_CHUNK;
            let end = (start + CELL_CHUNK).min(self.data.slots.len());
            self.data.slots[start..end]
                .iter()
                .map(|slot| ElementCell {
                    view: self,
                    id: slot.id,
                })
                .collect()
        });
        StyleElement(&chunk[dense % CELL_CHUNK])
    }

    /// The number of cell chunks built so far (for tests).
    #[cfg(test)]
    pub(crate) fn built_chunks(&self) -> usize {
        self.cells.iter().filter(|c| c.get().is_some()).count()
    }

    #[cold]
    fn unprepared(&'a self, id: NodeId) -> StyleElement<'a> {
        let message =
            format!("snowlark-style: element {id:?} was not prepared (missing node_inserted?)");
        debug_assert!(false, "{message}");
        let stand_in = self
            .dom
            .document_element(self.document)
            .and_then(|root| self.data.dense(root));
        match stand_in {
            Some(dense) => {
                eprintln!("{message}; the document element stands in");
                self.cell(dense)
            }
            None => panic!("{message}"),
        }
    }

    fn slotted(&'a self) -> &'a [Vec<StyleNode<'a>>] {
        self.slotted.get_or_init(|| {
            self.data
                .slotted
                .iter()
                .map(|list| list.iter().map(|&id| self.node(id)).collect())
                .collect()
        })
    }
}

/// The parent of `id` in the flat tree: its assigned slot, its parent element, or the host of the shadow
/// root it is a child of. The slot assignment is the one the last style update prepared.
pub fn flat_parent(dom: &Dom, data: &StyleData, id: NodeId) -> Option<NodeId> {
    if let Some(slot) = assigned_slot_of(dom, data, id) {
        return Some(slot);
    }
    let parent = dom.parent(id)?;
    match dom.data(parent) {
        NodeData::Element(_) => Some(parent),
        NodeData::ShadowRoot(s) => Some(s.host()),
        _ => None,
    }
}

/// The assigned slot of a slottable: from the table for elements, by "find a slot" for text.
pub(crate) fn assigned_slot_of(dom: &Dom, data: &StyleData, id: NodeId) -> Option<NodeId> {
    if dom.is_element(id) {
        return data.slot(id).and_then(|s| s.assigned_slot);
    }
    if dom.is_text(id) {
        return crate::slots::find_slot(dom, id);
    }
    None
}

/// A node as stylo sees it.
#[derive(Clone, Copy)]
pub struct StyleNode<'a> {
    view: &'a TreeView<'a>,
    id: NodeId,
}

/// What a [`StyleElement`] points to: the view and the element's id.
pub(crate) struct ElementCell<'a> {
    view: &'a TreeView<'a>,
    id: NodeId,
}

/// An element as stylo sees it (see the module documentation for why it is a pointer).
#[derive(Clone, Copy)]
pub struct StyleElement<'a>(&'a ElementCell<'a>);

impl<'a> StyleNode<'a> {
    #[inline]
    fn dom(self) -> &'a Dom {
        self.view.dom
    }

    #[inline]
    fn with(self, id: NodeId) -> StyleNode<'a> {
        StyleNode {
            view: self.view,
            id,
        }
    }
}

impl<'a> StyleElement<'a> {
    /// HTML §3.2.6.4: whether the element's directionality is right-to-left (a text field in the
    /// auto state by its `value` attribute: the page's dirty value is not here).
    fn directionality_is_rtl(&self) -> bool {
        let dom = self.dom();
        crate::i18n::directionality_is_rtl(dom, self.node_id(), &|n| {
            crate::i18n::default_value(dom, n)
        })
    }

    /// The element's id.
    #[inline]
    pub(crate) fn node_id(self) -> NodeId {
        self.0.id
    }

    #[inline]
    fn view(self) -> &'a TreeView<'a> {
        self.0.view
    }

    #[inline]
    fn dom(self) -> &'a Dom {
        self.0.view.dom
    }

    /// The element handle of another element.
    #[inline]
    fn with(self, id: NodeId) -> StyleElement<'a> {
        self.0.view.element(id)
    }

    /// The node handle of any node.
    #[inline]
    fn node_at(self, id: NodeId) -> StyleNode<'a> {
        self.0.view.node(id)
    }

    #[inline]
    fn slot(self) -> &'a Slot {
        self.0.view.data.el(self.0.id)
    }

    #[inline]
    fn is_html(self) -> bool {
        *self.dom().namespace(self.node_id()) == ns!(html)
    }

    #[inline]
    fn is_html_named(self, name: &LocalName) -> bool {
        self.dom().is_html_element_named(self.node_id(), name)
    }

    /// `TElement::ensure_data` without the `unsafe` contract: the traversal has exclusive access to the
    /// element it is styling.
    pub(crate) fn ensure_data_mut(self) -> ElementDataMut<'a> {
        let slot = self.slot();
        slot.set(bits::HAS_DATA);
        slot.data.borrow_mut()
    }

    pub(crate) fn set_dirty_bit(self) {
        self.slot().set(bits::DIRTY_DESCENDANTS);
    }

    pub(crate) fn clear_dirty_descendants(self) {
        self.slot().clear(bits::DIRTY_DESCENDANTS);
    }

    pub(crate) fn note_restyled(self, change: u32) {
        self.0.view.data.note_restyled(self.0.id, change);
    }

    fn selector_flags(self) -> ElementSelectorFlags {
        ElementSelectorFlags::from_bits_truncate(
            (self.slot().flags.load(std::sync::atomic::Ordering::Relaxed) & bits::SELECTOR_FLAGS)
                as usize,
        )
    }
}

/// `NodeId` packed into the pointer-sized opaque handles stylo uses as map keys.
#[inline]
pub(crate) fn opaque_node(id: NodeId) -> OpaqueNode {
    OpaqueNode(id.to_bits() as usize)
}

/// The selector matching identity of the element `id` (`selectors::Element::opaque`).
#[inline]
pub(crate) fn opaque_element(id: NodeId) -> OpaqueElement {
    let ptr = std::ptr::without_provenance_mut::<()>(id.to_bits() as usize);
    OpaqueElement::from_non_null_ptr(NonNull::new(ptr).expect("node ids are never zero"))
}

/// Inverse of [`opaque_node`].
#[inline]
pub(crate) fn node_from_opaque(node: OpaqueNode) -> Option<NodeId> {
    NodeId::from_bits(node.0 as u64)
}

impl PartialEq for StyleNode<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for StyleNode<'_> {}

impl Hash for StyleNode<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

impl fmt::Debug for StyleNode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.dom().data(self.id) {
            NodeData::Element(e) => write!(f, "<{}> {:?}", &*e.name().local, self.id),
            _ => write!(f, "{} {:?}", self.dom().node_name(self.id), self.id),
        }
    }
}

impl PartialEq for StyleElement<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for StyleElement<'_> {}

impl Hash for StyleElement<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id.hash(state)
    }
}

impl fmt::Debug for StyleElement<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_node().fmt(f)
    }
}

impl NodeInfo for StyleNode<'_> {
    #[inline]
    fn is_element(&self) -> bool {
        self.dom().is_element(self.id)
    }

    #[inline]
    fn is_text_node(&self) -> bool {
        self.dom().is_text(self.id)
    }
}

impl<'a> TDocument for StyleNode<'a> {
    type ConcreteNode = StyleNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn is_html_document(&self) -> bool {
        self.view.html_document
    }

    fn quirks_mode(&self) -> QuirksMode {
        self.view.quirks_mode
    }

    fn shared_lock(&self) -> &SharedRwLock {
        &self.view.data.lock
    }
}

impl<'a> TShadowRoot for StyleNode<'a> {
    type ConcreteNode = StyleNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn host(&self) -> StyleElement<'a> {
        self.view
            .element(self.dom().host(self.id).expect("shadow root without host"))
    }

    fn style_data<'b>(&self) -> Option<&'b CascadeData>
    where
        Self: 'b,
    {
        self.view.shadows.get(&self.id).map(|s| &*s.data)
    }
}

impl<'a> TNode for StyleNode<'a> {
    type ConcreteElement = StyleElement<'a>;
    type ConcreteDocument = StyleNode<'a>;
    type ConcreteShadowRoot = StyleNode<'a>;

    #[inline]
    fn parent_node(&self) -> Option<Self> {
        self.dom().parent(self.id).map(|p| self.with(p))
    }

    #[inline]
    fn first_child(&self) -> Option<Self> {
        self.dom().first_child(self.id).map(|c| self.with(c))
    }

    #[inline]
    fn last_child(&self) -> Option<Self> {
        self.dom().last_child(self.id).map(|c| self.with(c))
    }

    #[inline]
    fn prev_sibling(&self) -> Option<Self> {
        self.dom().previous_sibling(self.id).map(|c| self.with(c))
    }

    #[inline]
    fn next_sibling(&self) -> Option<Self> {
        self.dom().next_sibling(self.id).map(|c| self.with(c))
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        self.with(self.view.document)
    }

    fn is_in_document(&self) -> bool {
        self.dom().is_connected(self.id)
    }

    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        flat_parent(self.dom(), self.view.data, self.id).map(|p| self.view.element(p))
    }

    #[inline]
    fn opaque(&self) -> OpaqueNode {
        opaque_node(self.id)
    }

    fn debug_id(self) -> usize {
        self.id.to_bits() as usize
    }

    #[inline]
    fn as_element(&self) -> Option<Self::ConcreteElement> {
        self.dom()
            .is_element(self.id)
            .then(|| self.view.element(self.id))
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        self.dom().is_document(self.id).then_some(*self)
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        self.dom().is_shadow_root(self.id).then_some(*self)
    }
}

/// The flat-tree children of an element.
pub enum FlatChildren<'a> {
    /// DOM children, from `next`.
    Dom {
        view: &'a TreeView<'a>,
        next: Option<NodeId>,
    },
    /// A slot's assigned nodes.
    Slotted {
        slot: NodeId,
        nodes: std::slice::Iter<'a, StyleNode<'a>>,
    },
}

impl<'a> Iterator for FlatChildren<'a> {
    type Item = StyleNode<'a>;

    #[inline]
    fn next(&mut self) -> Option<StyleNode<'a>> {
        match self {
            FlatChildren::Dom { view, next } => {
                let id = (*next)?;
                *next = view.dom.next_sibling(id);
                Some(view.node(id))
            }
            FlatChildren::Slotted { slot, nodes } => loop {
                let node = *nodes.next()?;
                // The flat tree's children and parents must agree: the traversal gives a child the
                // depth of its parent's plus one, and the bloom filter climbs the child's parents
                // from there. An element the list holds whose assigned slot is another (a stale
                // list) is left out rather than met at the wrong depth.
                if node.dom().is_element(node.id)
                    && node.view.data.slot(node.id).and_then(|s| s.assigned_slot) != Some(*slot)
                {
                    debug_assert!(
                        false,
                        "slot {slot:?} lists {:?}, assigned elsewhere",
                        node.id
                    );
                    continue;
                }
                return Some(node);
            },
        }
    }
}

impl<'a> selectors::Element for StyleElement<'a> {
    type Impl = SelectorImpl;

    #[inline]
    fn opaque(&self) -> OpaqueElement {
        opaque_element(self.node_id())
    }

    #[inline]
    fn parent_element(&self) -> Option<Self> {
        self.dom()
            .parent_element(self.node_id())
            .map(|p| self.with(p))
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        self.dom()
            .parent(self.node_id())
            .is_some_and(|p| self.dom().is_shadow_root(p))
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        let shadow = self.slot().containing_shadow?;
        self.dom().host(shadow).map(|h| self.with(h))
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    #[inline]
    fn prev_sibling_element(&self) -> Option<Self> {
        self.dom()
            .previous_element_sibling(self.node_id())
            .map(|e| self.with(e))
    }

    #[inline]
    fn next_sibling_element(&self) -> Option<Self> {
        self.dom()
            .next_element_sibling(self.node_id())
            .map(|e| self.with(e))
    }

    #[inline]
    fn first_element_child(&self) -> Option<Self> {
        self.dom()
            .first_element_child(self.node_id())
            .map(|e| self.with(e))
    }

    #[inline]
    fn is_html_element_in_html_document(&self) -> bool {
        self.view().html_document && self.is_html()
    }

    #[inline]
    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.dom().local_name(self.node_id()) == local_name
    }

    #[inline]
    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.dom().namespace(self.node_id()) == ns
    }

    #[inline]
    fn is_same_type(&self, other: &Self) -> bool {
        self.dom().element_name(self.node_id()).local
            == other.dom().element_name(other.node_id()).local
            && self.dom().namespace(self.node_id()) == other.dom().namespace(other.node_id())
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&style::Namespace>,
        local_name: &style::LocalName,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        self.dom().attrs(self.node_id()).iter().any(|a| {
            *a.local_name() == local_name.0
                && match ns {
                    NamespaceConstraint::Any => true,
                    NamespaceConstraint::Specific(ns) => *a.namespace() == ns.0,
                }
                && operation.eval_str(a.value())
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &NonTSPseudoClass,
        _context: &mut MatchingContext<SelectorImpl>,
    ) -> bool {
        match pc {
            NonTSPseudoClass::Lang(lang) => self.match_element_lang(None, lang),
            // Selectors 4 §10.1: values other than these two are valid and match nothing.
            NonTSPseudoClass::Dir(dir) => {
                if dir.eq_ignore_ascii_case("rtl") {
                    self.directionality_is_rtl()
                } else {
                    dir.eq_ignore_ascii_case("ltr") && !self.directionality_is_rtl()
                }
            }
            NonTSPseudoClass::CustomState(state) => self.has_custom_state(&state.0),
            // HTML §15.3.8: a `table` whose `border` attribute is a non-zero integer.
            NonTSPseudoClass::ServoNonZeroBorder => {
                self.is_html_named(&local_name!("table"))
                    && self
                        .dom()
                        .attr(self.node_id(), &ns!(), &local_name!("border"))
                        .is_some_and(|v| crate::hints::parse_integer(v).is_some_and(|n| n != 0))
            }
            pc => {
                let flag = pc.state_flag();
                !flag.is_empty() && self.slot().state().intersects(flag)
            }
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &PseudoElement,
        _context: &mut MatchingContext<SelectorImpl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        let own = flags.for_self();
        if !own.is_empty() {
            self.slot().set(own.bits() as u32);
        }
        let parent = flags.for_parent();
        if !parent.is_empty() {
            if let Some(p) = self.dom().parent_element(self.node_id()) {
                if let Some(slot) = self.view().data.slot(p) {
                    slot.set(parent.bits() as u32);
                }
            }
        }
    }

    fn is_link(&self) -> bool {
        (self.is_html_named(&local_name!("a")) || self.is_html_named(&local_name!("area")))
            && self
                .dom()
                .attr(self.node_id(), &ns!(), &local_name!("href"))
                .is_some()
    }

    fn is_html_slot_element(&self) -> bool {
        self.is_html_named(&local_name!("slot"))
    }

    fn assigned_slot(&self) -> Option<Self> {
        self.slot().assigned_slot.map(|s| self.with(s))
    }

    #[inline]
    fn has_id(&self, id: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.slot()
            .attrs
            .id
            .as_ref()
            .is_some_and(|own| case_sensitivity.eq_atom(own, &id.0))
    }

    #[inline]
    fn has_class(&self, name: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.slot()
            .attrs
            .classes
            .iter()
            .any(|c| case_sensitivity.eq_atom(c, &name.0))
    }

    fn has_custom_state(&self, name: &AtomIdent) -> bool {
        self.dom()
            .custom_states(self.node_id())
            .iter()
            .any(|s| *s == *name.0)
    }

    /// CSS Shadow Parts 1 §4.2: the inner part name this host exports as `name`.
    fn imported_part(&self, name: &AtomIdent) -> Option<AtomIdent> {
        self.slot()
            .attrs
            .exportparts
            .iter()
            .find(|(_, outer)| *outer == name.0)
            .map(|(inner, _)| GenericAtomIdent(inner.clone()))
    }

    fn is_part(&self, name: &AtomIdent) -> bool {
        self.slot().attrs.parts.contains(&name.0)
    }

    fn is_empty(&self) -> bool {
        // Selectors 4 §14.2: no children other than comments and processing instructions (and empty text).
        self.dom()
            .children(self.node_id())
            .all(|c| match self.dom().data(c) {
                NodeData::Element(_) => false,
                NodeData::Text(t) | NodeData::CDataSection(t) => t.is_empty(),
                _ => true,
            })
    }

    fn is_root(&self) -> bool {
        self.dom()
            .parent(self.node_id())
            .is_some_and(|p| self.dom().is_document(p))
    }

    fn add_element_unique_hashes(&self, filter: &mut BloomFilter) -> bool {
        each_relevant_element_hash(*self, |hash| filter.insert_hash(hash & BLOOM_HASH_MASK));
        true
    }
}

impl<'a> TElement for StyleElement<'a> {
    type ConcreteNode = StyleNode<'a>;
    type TraversalChildrenIterator = FlatChildren<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self.node_at(self.node_id())
    }

    fn traversal_children(&self) -> LayoutIterator<FlatChildren<'a>> {
        let dom = self.dom();
        if let Some(shadow) = dom.shadow_root(self.node_id()) {
            return LayoutIterator(FlatChildren::Dom {
                view: self.view(),
                next: dom.first_child(shadow),
            });
        }
        if let Some(list) = self.slot().slotted_list {
            let lists = self.view().slotted();
            return LayoutIterator(FlatChildren::Slotted {
                slot: self.node_id(),
                nodes: lists[list as usize].iter(),
            });
        }
        LayoutIterator(FlatChildren::Dom {
            view: self.view(),
            next: dom.first_child(self.node_id()),
        })
    }

    fn inheritance_parent(&self) -> Option<Self> {
        self.as_node().traversal_parent()
    }

    fn is_html_element(&self) -> bool {
        self.is_html()
    }

    fn is_mathml_element(&self) -> bool {
        *self.dom().namespace(self.node_id()) == ns!(mathml)
    }

    fn is_svg_element(&self) -> bool {
        *self.dom().namespace(self.node_id()) == ns!(svg)
    }

    fn slotted_nodes(&self) -> &[Self::ConcreteNode] {
        match self.slot().slotted_list {
            Some(i) => &self.view().slotted()[i as usize],
            None => &[],
        }
    }

    fn style_attribute(&self) -> Option<ArcBorrow<'_, Locked<PropertyDeclarationBlock>>> {
        self.slot()
            .attrs
            .style_attribute
            .as_ref()
            .map(|a| a.borrow_arc())
    }

    fn animation_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        let key = AnimationSetKey::new_for_non_pseudo(opaque_node(self.node_id()));
        match self.view().script {
            // Web Animations 1 §5.4: script animations composite over the CSS animations.
            Some(script) => {
                let css = context.animations.sets.read().get(&key).and_then(|set| {
                    set.get_value_map_for_active_animations(context.current_time_for_animations)
                });
                crate::web_animations::effects::animation_block(
                    script,
                    self.node_id(),
                    css,
                    &self.view().data.lock,
                )
            }
            None => context.animations.get_animation_declarations(
                &key,
                context.current_time_for_animations,
                &self.view().data.lock,
            ),
        }
    }

    fn animated_by_script(&self, property: style::properties::PropertyDeclarationId) -> bool {
        self.view()
            .script
            .is_some_and(|s| s.read().is_ok_and(|s| s.animates(self.node_id(), property)))
    }

    fn process_script_animations(
        &self,
        context: &mut style::context::StyleContext<Self>,
        new_style: &Arc<style::properties::ComputedValues>,
    ) -> bool {
        let Some(script) = self.view().script else {
            return false;
        };
        if !script.read().is_ok_and(|s| s.has_target(self.node_id())) {
            return false;
        }
        crate::web_animations::effects::resolve_for_element(
            script,
            self.node_id(),
            *self,
            context,
            new_style,
        )
    }

    fn transition_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        context.animations.get_transition_declarations(
            &AnimationSetKey::new_for_non_pseudo(opaque_node(self.node_id())),
            context.current_time_for_animations,
            &self.view().data.lock,
        )
    }

    #[inline]
    fn state(&self) -> ElementState {
        self.slot().state()
    }

    fn has_part_attr(&self) -> bool {
        !self.slot().attrs.parts.is_empty()
    }

    fn exports_any_part(&self) -> bool {
        !self.slot().attrs.exportparts.is_empty()
    }

    /// CSS Shadow Parts 1 §4.2: the outer names under which this host exports its inner part
    /// `name`.
    fn each_exported_part<F>(&self, name: &AtomIdent, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        for (inner, outer) in &self.slot().attrs.exportparts {
            if *inner == name.0 {
                callback(AtomIdent::cast(outer));
            }
        }
    }

    #[inline]
    fn id(&self) -> Option<&WeakAtom> {
        self.slot().attrs.id.as_ref()
    }

    #[inline]
    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        for class in &self.slot().attrs.classes {
            callback(AtomIdent::cast(class));
        }
    }

    fn each_custom_state<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        for s in self.dom().custom_states(self.node_id()) {
            callback(&AtomIdent::from(s.as_str()));
        }
    }

    fn each_part<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        for part in &self.slot().attrs.parts {
            callback(AtomIdent::cast(part));
        }
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        for attr in self.dom().attrs(self.node_id()) {
            callback(GenericAtomIdent::cast(attr.local_name()));
        }
    }

    #[inline]
    fn has_dirty_descendants(&self) -> bool {
        self.slot().has(bits::DIRTY_DESCENDANTS)
    }

    #[inline]
    fn has_snapshot(&self) -> bool {
        self.slot().has(bits::HAS_SNAPSHOT)
    }

    #[inline]
    fn handled_snapshot(&self) -> bool {
        self.slot().has(bits::HANDLED_SNAPSHOT)
    }

    unsafe fn set_handled_snapshot(&self) {
        self.slot().set(bits::HANDLED_SNAPSHOT);
    }

    unsafe fn set_dirty_descendants(&self) {
        self.slot().set(bits::DIRTY_DESCENDANTS);
    }

    unsafe fn unset_dirty_descendants(&self) {
        self.slot().clear(bits::DIRTY_DESCENDANTS);
    }

    fn store_children_to_process(&self, _n: isize) {
        // Only used by post-order traversals, which ours is not.
    }

    fn did_process_child(&self) -> isize {
        0
    }

    unsafe fn ensure_data(&self) -> ElementDataMut<'_> {
        self.ensure_data_mut()
    }

    unsafe fn clear_data(&self) {
        let slot = self.slot();
        *slot.data.borrow_mut() = Default::default();
        slot.clear(bits::HAS_DATA | bits::DIRTY_DESCENDANTS);
    }

    #[inline]
    fn has_data(&self) -> bool {
        self.view()
            .data
            .slot(self.node_id())
            .is_some_and(|s| s.has(bits::HAS_DATA))
    }

    fn borrow_data(&self) -> Option<ElementDataRef<'_>> {
        let slot = self.view().data.slot(self.node_id())?;
        if !slot.has(bits::HAS_DATA) {
            return None;
        }
        // stylo reads "has data" as "has been styled" (the invalidator unwraps the primary style of
        // every element with data), so data without styles yet counts as none.
        Some(slot.data.borrow()).filter(|d| d.styles.primary.is_some())
    }

    fn mutate_data(&self) -> Option<ElementDataMut<'_>> {
        let slot = self.view().data.slot(self.node_id())?;
        slot.has(bits::HAS_DATA).then(|| slot.data.borrow_mut())
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        self.view().animating
    }

    fn has_animations(&self, context: &SharedStyleContext) -> bool {
        self.view().animating && {
            let key = AnimationSetKey::new_for_non_pseudo(opaque_node(self.node_id()));
            context.animations.has_active_animations(&key)
                || context.animations.has_active_transitions(&key)
                || self
                    .view()
                    .script
                    .is_some_and(|s| s.read().is_ok_and(|s| s.has_target(self.node_id())))
        }
    }

    fn has_css_animations(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<PseudoElement>,
    ) -> bool {
        self.view().animating
            && context
                .animations
                .has_active_animations(&AnimationSetKey::new(
                    opaque_node(self.node_id()),
                    pseudo_element,
                ))
    }

    fn has_css_transitions(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<PseudoElement>,
    ) -> bool {
        self.view().animating
            && context
                .animations
                .has_active_transitions(&AnimationSetKey::new(
                    opaque_node(self.node_id()),
                    pseudo_element,
                ))
    }

    fn shadow_root(&self) -> Option<StyleNode<'a>> {
        self.dom()
            .shadow_root(self.node_id())
            .map(|s| self.node_at(s))
    }

    fn containing_shadow(&self) -> Option<StyleNode<'a>> {
        self.slot().containing_shadow.map(|s| self.node_at(s))
    }

    fn lang_attr(&self) -> Option<AttrValue> {
        let dom = self.dom();
        dom.attr(self.node_id(), &ns!(xml), &local_name!("lang"))
            .or_else(|| dom.attr(self.node_id(), &ns!(), &local_name!("lang")))
            .map(AtomString::from)
    }

    fn match_element_lang(&self, override_lang: Option<Option<AttrValue>>, value: &Lang) -> bool {
        // HTML §3.2.6.2: the language of a node is that of its nearest inclusive ancestor (in the flat
        // tree) with a lang attribute.
        let lang = match override_lang {
            Some(Some(lang)) => lang,
            Some(None) => AtomString::from(""),
            None => AtomString::from(crate::i18n::language_of(self.dom(), self.node_id())),
        };
        value
            .split(',')
            .any(|range| crate::i18n::lang_matches(lang.as_ref(), range))
    }

    fn is_html_document_body_element(&self) -> bool {
        self.dom().body(self.view().document) == Some(self.node_id())
    }

    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        hints: &mut V,
    ) where
        V: Push<ApplicableDeclarationBlock>,
    {
        if let Some(block) = &self.slot().attrs.hints {
            hints.push(ApplicableDeclarationBlock::from_declarations(
                block.clone(),
                CascadeLevel::new(CascadeOrigin::PresHints),
                LayerOrder::root(),
            ));
        }
    }

    #[inline]
    fn local_name(&self) -> &LocalName {
        self.dom().local_name(self.node_id())
    }

    #[inline]
    fn namespace(&self) -> &Namespace {
        self.dom().namespace(self.node_id())
    }

    fn query_container_size(&self, _display: &Display) -> euclid::default::Size2D<Option<Au>> {
        let (w, h) = self.slot().container_size.unwrap_or((None, None));
        euclid::default::Size2D::new(w.map(Au::from_f32_px), h.map(Au::from_f32_px))
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.selector_flags().contains(flags)
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        self.selector_flags()
            .intersection(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING)
    }

    fn get_attr(&self, attr: &style::LocalName, namespace: &style::Namespace) -> Option<String> {
        self.dom()
            .attr(self.node_id(), &namespace.0, &attr.0)
            .map(str::to_owned)
    }
}
