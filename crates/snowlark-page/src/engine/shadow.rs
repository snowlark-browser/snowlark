// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shadow trees (DOM §4.2.2, §4.8): `ShadowRoot`, slots and slottables, slot assignment (computed from
//! the tree, with the last assignment of each slot kept to signal `slotchange`), and `HTMLSlotElement`.

use snowlark_bindings::generated::{
    AssignedNodesOptions, ElementOrText, GetHTMLOptions, HTMLSlotElementImpl, ShadowRootImpl,
    ShadowRootMode, SlotAssignmentMode, TrustedHTMLOrNullEmptyString,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_dom::{local_name, ns, Dom, LocalName, NodeId, SerializeOptions};

use super::element::InsertMode;

use super::{handle_node, node_handle, Engine};

/// Slot state the DOM does not keep: manual assignments and the last computed assignment of each slot.
#[derive(Default)]
pub struct Slots {
    /// A slot's manually assigned nodes (`slot.assign()`).
    pub manual: rustc_hash::FxHashMap<NodeId, Vec<NodeId>>,
    /// The assigned nodes of each slot as last computed.
    pub assigned: rustc_hash::FxHashMap<NodeId, Vec<NodeId>>,
}

fn is_slot(dom: &Dom, n: NodeId) -> bool {
    dom.is_html_element_named(n, &local_name!("slot"))
}

fn slottable(dom: &Dom, n: NodeId) -> bool {
    dom.is_element(n) || dom.is_exclusive_text(n)
}

fn slot_name(dom: &Dom, slot: NodeId) -> &str {
    dom.attr(slot, &ns!(), &local_name!("name")).unwrap_or("")
}

fn slottable_name(dom: &Dom, n: NodeId) -> &str {
    if dom.is_element(n) {
        dom.attr(n, &ns!(), &local_name!("slot")).unwrap_or("")
    } else {
        ""
    }
}

impl Engine {
    /// DOM §4.2.2.3 "find a slot" for `node`; `open` limits it to open shadow roots.
    pub(crate) fn find_slot(&self, node: NodeId, open: bool) -> Option<NodeId> {
        let dom = self.dom();
        if !slottable(&dom, node) {
            return None;
        }
        let parent = dom.parent(node)?;
        let shadow = dom.shadow_root(parent)?;
        let data = dom.shadow_root_data(shadow)?;
        if open && data.mode() != snowlark_dom::ShadowRootMode::Open {
            return None;
        }
        if !dom.may_hold_slots(shadow) {
            return None;
        }
        if data.slot_assignment() == snowlark_dom::SlotAssignmentMode::Manual {
            let slots = self.slots.borrow();
            return dom.descendants(shadow).find(|&s| {
                is_slot(&dom, s) && slots.manual.get(&s).is_some_and(|m| m.contains(&node))
            });
        }
        let name = slottable_name(&dom, node);
        dom.descendants(shadow)
            .find(|&s| is_slot(&dom, s) && slot_name(&dom, s) == name)
    }

    /// A slottable's assigned slot (closed shadow roots included), for event paths.
    pub(crate) fn assigned_slot_of(&self, node: NodeId) -> Option<NodeId> {
        {
            let dom = self.dom();
            let parent = dom.parent(node)?;
            dom.shadow_root(parent)?;
        }
        self.find_slot(node, false)
    }

    /// DOM §4.2.2.3 "find slottables" for `slot`.
    pub(crate) fn find_slottables(&self, slot: NodeId) -> Vec<NodeId> {
        let dom = self.dom();
        let root = dom.root(slot);
        let Some(data) = dom.shadow_root_data(root) else {
            return Vec::new();
        };
        let host = data.host();
        if data.slot_assignment() == snowlark_dom::SlotAssignmentMode::Manual {
            let slots = self.slots.borrow();
            return slots
                .manual
                .get(&slot)
                .map(|m| {
                    m.iter()
                        .copied()
                        .filter(|&n| dom.is_alive(n) && dom.parent(n) == Some(host))
                        .collect()
                })
                .unwrap_or_default();
        }
        let children: Vec<NodeId> = dom.children(host).filter(|&c| slottable(&dom, c)).collect();
        drop(dom);
        children
            .into_iter()
            .filter(|&c| self.find_slot(c, false) == Some(slot))
            .collect()
    }

    /// CSS Scoping 1 `:has-slotted`: whether `slot`'s flattened assigned nodes are non-empty, an
    /// assigned slot of an outer tree counting when it has some itself (fallback content does not
    /// count).
    pub(crate) fn has_slotted(&self, slot: NodeId) -> bool {
        {
            let dom = self.dom();
            if !is_slot(&dom, slot) || !dom.is_shadow_root(dom.root(slot)) {
                return false;
            }
        }
        self.find_slottables(slot).into_iter().any(|n| {
            let nested = {
                let dom = self.dom();
                is_slot(&dom, n) && dom.is_shadow_root(dom.root(n))
            };
            !nested || self.has_slotted(n)
        })
    }

    /// DOM §4.2.2.3 "find flattened slottables".
    fn find_flattened_slottables(&self, slot: NodeId) -> Vec<NodeId> {
        {
            let dom = self.dom();
            if !dom.is_shadow_root(dom.root(slot)) {
                return Vec::new();
            }
        }
        let mut slottables = self.find_slottables(slot);
        if slottables.is_empty() {
            let dom = self.dom();
            slottables = dom.children(slot).filter(|&c| slottable(&dom, c)).collect();
        }
        let mut result = Vec::new();
        for n in slottables {
            let nested = {
                let dom = self.dom();
                is_slot(&dom, n) && dom.is_shadow_root(dom.root(n))
            };
            if nested {
                result.extend(self.find_flattened_slottables(n));
            } else {
                result.push(n);
            }
        }
        result
    }

    /// Recomputes the assignment of every slot in `shadow` and signals the slots whose assigned nodes
    /// changed (DOM §4.2.2.4 "assign slottables for a tree"). A tree without slots has none to
    /// assign (its removed slots were forgotten by their removing steps).
    fn update_slots(&self, shadow: NodeId) {
        let slots: Vec<NodeId> = {
            let dom = self.dom();
            if !dom.is_alive(shadow) || !dom.is_shadow_root(shadow) || !dom.may_hold_slots(shadow) {
                return;
            }
            dom.descendants(shadow)
                .filter(|&s| is_slot(&dom, s))
                .collect()
        };
        for slot in slots {
            let now = self.find_slottables(slot);
            let changed = {
                let mut s = self.slots.borrow_mut();
                let before = s.assigned.entry(slot).or_default();
                if *before != now {
                    *before = now;
                    true
                } else {
                    false
                }
            };
            if changed {
                self.signal_slot_change(slot);
            }
        }
    }

    /// The shadow roots whose assignment a change at `node` may affect: those that can hold slots.
    fn affected_shadows(&self, node: NodeId, parent: Option<NodeId>) -> Vec<NodeId> {
        let dom = self.dom();
        let mut out = Vec::new();
        if !dom.has_shadow_roots() {
            return out;
        }
        let has_slots = |r: NodeId| dom.is_shadow_root(r) && dom.may_hold_slots(r);
        let mut parent_root = None;
        if let Some(p) = parent.filter(|&p| dom.is_alive(p)) {
            if let Some(s) = dom.shadow_root(p).filter(|&s| dom.may_hold_slots(s)) {
                out.push(s);
            }
            let root = dom.root(p);
            if has_slots(root) && !out.contains(&root) {
                out.push(root);
            }
            parent_root = Some(root);
        }
        // (a node still in `parent`, as an inserted one is, has its root: one walk, not two)
        let root = match parent_root {
            Some(r) if dom.parent(node) == parent => r,
            _ => dom.root(node),
        };
        if has_slots(root) && !out.contains(&root) {
            out.push(root);
        }
        out
    }

    pub(crate) fn slot_insertion_steps(&self, el: NodeId) {
        let parent = self.dom().parent(el);
        for s in self.affected_shadows(el, parent) {
            self.update_slots(s);
        }
    }

    pub(crate) fn slot_removing_steps(&self, el: NodeId, old_parent: Option<NodeId>) {
        // A removed slot keeps no assignment. The removed node's reaction (the one with an old parent)
        // covers its descendants, which get reactions of their own.
        let removed_slots: Vec<NodeId> = if old_parent.is_none()
            || self.slots.borrow().assigned.is_empty()
            || !self.dom().may_hold_slots(el)
        {
            Vec::new()
        } else {
            let dom = self.dom();
            dom.inclusive_descendants(el)
                .filter(|&s| is_slot(&dom, s))
                .collect()
        };
        for slot in removed_slots {
            let had = self
                .slots
                .borrow_mut()
                .assigned
                .remove(&slot)
                .is_some_and(|a| !a.is_empty());
            if had {
                self.signal_slot_change(slot);
            }
        }
        for s in self.affected_shadows(el, old_parent) {
            self.update_slots(s);
        }
    }

    pub(crate) fn slot_attribute_changed(&self, el: NodeId, local: &LocalName) {
        let parent = self.dom().parent(el);
        if &**local == "slot" || (&**local == "name" && is_slot(&self.dom(), el)) {
            for s in self.affected_shadows(el, parent) {
                self.update_slots(s);
            }
        }
    }

    /// Children of a slot or a host changed (text slottables, fallback content).
    pub(crate) fn slot_children_changed(&self, parent: NodeId) {
        let (host_shadow, in_shadow_slot) = {
            let dom = self.dom();
            (
                dom.shadow_root(parent),
                is_slot(&dom, parent) && dom.is_shadow_root(dom.root(parent)),
            )
        };
        if let Some(s) = host_shadow {
            self.update_slots(s);
        }
        if in_shadow_slot {
            let empty = self
                .slots
                .borrow()
                .assigned
                .get(&parent)
                .is_none_or(|a| a.is_empty());
            if empty {
                self.signal_slot_change(parent);
            }
        }
    }
}

impl ShadowRootImpl for Engine {
    fn mode(&self, this: Handle) -> ShadowRootMode {
        match self.dom().shadow_root_data(self.n(this)).map(|s| s.mode()) {
            Some(snowlark_dom::ShadowRootMode::Closed) => ShadowRootMode::Closed,
            _ => ShadowRootMode::Open,
        }
    }
    fn delegates_focus(&self, this: Handle) -> bool {
        self.dom()
            .shadow_root_data(self.n(this))
            .is_some_and(|s| s.delegates_focus())
    }
    fn serializable(&self, this: Handle) -> bool {
        self.dom()
            .shadow_root_data(self.n(this))
            .is_some_and(|s| s.serializable())
    }
    fn slot_assignment(&self, this: Handle) -> SlotAssignmentMode {
        match self
            .dom()
            .shadow_root_data(self.n(this))
            .map(|s| s.slot_assignment())
        {
            Some(snowlark_dom::SlotAssignmentMode::Manual) => SlotAssignmentMode::Manual,
            _ => SlotAssignmentMode::Named,
        }
    }
    fn clonable(&self, this: Handle) -> bool {
        self.dom()
            .shadow_root_data(self.n(this))
            .is_some_and(|s| s.clonable())
    }
    fn host(&self, this: Handle) -> Handle {
        node_handle(self.dom().host(self.n(this)).expect("a shadow root's host"))
    }
    fn inner_html(&self, this: Handle) -> TrustedHTMLOrNullEmptyString {
        TrustedHTMLOrNullEmptyString::String(self.fragment_serialize(self.n(this), false))
    }
    fn set_inner_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: TrustedHTMLOrNullEmptyString,
    ) -> rt::Result<()> {
        let markup = self.tt_html(cx, value, self.n(this), "ShadowRoot innerHTML")?;
        self.set_inner_html_steps(cx, self.n(this), &markup)
    }
    fn set_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: snowlark_bindings::generated::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.set_html_steps(cx, self.n(this), html, InsertMode::safe(_o))
    }
    fn append_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: snowlark_bindings::generated::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.insert_html_steps(cx, self.n(this), html, InsertMode::safe(_o), 4)
    }
    fn prepend_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: snowlark_bindings::generated::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.insert_html_steps(cx, self.n(this), html, InsertMode::safe(_o), 3)
    }
    fn set_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: snowlark_bindings::generated::TrustedHTMLOrString,
        _o: snowlark_bindings::generated::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "ShadowRoot setHTMLUnsafe")?;
        self.set_html_steps(cx, self.n(this), &m, mode)
    }
    fn append_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: snowlark_bindings::generated::TrustedHTMLOrString,
        _o: snowlark_bindings::generated::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "ShadowRoot appendHTMLUnsafe")?;
        self.insert_html_steps(cx, self.n(this), &m, mode, 4)
    }
    fn prepend_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: snowlark_bindings::generated::TrustedHTMLOrString,
        _o: snowlark_bindings::generated::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "ShadowRoot prependHTMLUnsafe")?;
        self.insert_html_steps(cx, self.n(this), &m, mode, 3)
    }
    fn get_html(
        &self,
        _cx: &mut Cx,
        this: Handle,
        options: GetHTMLOptions,
    ) -> rt::Result<DomStr<'_>> {
        let roots: Vec<NodeId> = options
            .shadow_roots
            .iter()
            .filter_map(|h| handle_node(*h))
            .collect();
        Ok(DomStr::Owned(self.dom().get_html(
            self.n(this),
            SerializeOptions {
                serializable_shadow_roots: options.serializable_shadow_roots,
                shadow_roots: &roots,
                scripting_enabled: true,
            },
        )))
    }
}

impl HTMLSlotElementImpl for Engine {
    fn assigned_nodes(
        &self,
        _cx: &mut Cx,
        this: Handle,
        options: AssignedNodesOptions,
    ) -> rt::Result<Vec<Handle>> {
        let slot = self.n(this);
        let nodes = if options.flatten {
            self.find_flattened_slottables(slot)
        } else {
            self.find_slottables(slot)
        };
        Ok(nodes.into_iter().map(node_handle).collect())
    }

    fn assigned_elements(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: AssignedNodesOptions,
    ) -> rt::Result<Vec<Handle>> {
        let nodes = self.assigned_nodes(cx, this, options)?;
        let dom = self.dom();
        Ok(nodes
            .into_iter()
            .filter(|h| handle_node(*h).is_some_and(|n| dom.is_element(n)))
            .collect())
    }

    /// `assign(...nodes)` (HTML §4.12.4).
    fn assign(&self, cx: &mut Cx, this: Handle, nodes: Vec<ElementOrText>) -> rt::Result<()> {
        let slot = self.n(this);
        let nodes: Vec<NodeId> = nodes
            .into_iter()
            .map(|n| match n {
                ElementOrText::Element(h) | ElementOrText::Text(h) => self.n(h),
            })
            .collect();
        let mut unique = Vec::new();
        for n in nodes {
            if !unique.contains(&n) {
                unique.push(n);
            }
        }
        // Steps 1-4: a node assigned to another slot moves here; that slot changed too, wherever it
        // is (another shadow tree, or none), and is signalled even when its tree's assignment is not
        // recomputed (step 5 covers only this slot's root).
        let mut previous: Vec<NodeId> = Vec::new();
        let updated = {
            let mut s = self.slots.borrow_mut();
            let current = s.manual.get(&slot).cloned().unwrap_or_default();
            if unique.is_empty() && current.is_empty() {
                return Ok(());
            }
            for &n in &unique {
                let prev = s
                    .manual
                    .iter()
                    .find(|(&k, v)| k != slot && v.contains(&n))
                    .map(|(&k, _)| k);
                if let Some(p) = prev {
                    if let Some(list) = s.manual.get_mut(&p) {
                        list.retain(|&x| x != n);
                    }
                    if !previous.contains(&p) {
                        previous.push(p);
                    }
                }
            }
            let updated = !previous.is_empty() || unique != current;
            if updated {
                s.manual.insert(slot, unique.clone());
            }
            updated
        };
        if !updated {
            return Ok(());
        }
        self.render_slot_assigned(slot, unique);
        for &p in &previous {
            let list = self
                .slots
                .borrow()
                .manual
                .get(&p)
                .cloned()
                .unwrap_or_default();
            self.render_slot_assigned(p, list);
        }
        let (root, mut changed) = {
            let dom = self.dom();
            let in_shadow = |s: NodeId| dom.is_alive(s) && dom.is_shadow_root(dom.root(s));
            let mut changed: Vec<NodeId> = previous.into_iter().filter(|&p| in_shadow(p)).collect();
            if in_shadow(slot) {
                changed.push(slot);
            }
            (dom.root(slot), changed)
        };
        // Signal the changed slots of this slot's tree in tree order, then the others.
        if self.dom().is_shadow_root(root) {
            let in_tree: Vec<NodeId> = {
                let dom = self.dom();
                dom.descendants(root)
                    .filter(|s| changed.contains(s))
                    .collect()
            };
            changed.retain(|s| !in_tree.contains(s));
            for s in in_tree {
                self.signal_slot_change(s);
            }
        }
        let other_roots: Vec<NodeId> = {
            let dom = self.dom();
            let mut roots = Vec::new();
            for &s in &changed {
                let r = dom.root(s);
                if r != root && !roots.contains(&r) {
                    roots.push(r);
                }
            }
            roots
        };
        for s in changed {
            self.signal_slot_change(s);
        }
        // Step 5, and the cached assignment of the other trees whose slots lost nodes.
        if self.dom().is_shadow_root(root) {
            self.update_slots(root);
        }
        for r in other_roots {
            self.update_slots(r);
        }
        self.after_mutation(cx);
        Ok(())
    }
}
