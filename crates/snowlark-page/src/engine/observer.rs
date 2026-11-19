// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mutation observers (DOM §4.3): registrations per node, record queues per observer, and the
//! "notify mutation observers" microtask, which also fires `slotchange` at signaled slots.

use snowlark_bindings::generated::{
    MutationCallback, MutationObserverImpl, MutationObserverInit, MutationRecordImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_dom::{MutationKind, NodeId};

use super::reactions::Microtask;
use super::{node_handle, Engine, Object};

/// MutationObserverInit, normalized (DOM §4.3.1 `observe()` steps 1-2).
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub child_list: bool,
    pub attributes: bool,
    pub character_data: bool,
    pub subtree: bool,
    pub attribute_old_value: bool,
    pub character_data_old_value: bool,
    pub attribute_filter: Option<Vec<String>>,
}

pub struct ObserverData {
    pub callback: super::held::Held<v8::Object>,
    pub records: Vec<Handle>,
    /// The nodes this observer is registered on.
    pub nodes: Vec<NodeId>,
}

/// A MutationRecord.
pub struct RecordData {
    pub kind: MutationKind,
    pub target: NodeId,
    pub added: Handle,
    pub removed: Handle,
    pub previous_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub attribute_name: Option<String>,
    pub attribute_namespace: Option<String>,
    pub old_value: Option<String>,
}

/// The agent's mutation observer state.
#[derive(Default)]
pub struct Observers {
    /// Every MutationObserver, in creation order (the agent's "mutation observers" set).
    pub all: Vec<Handle>,
    /// Registered observers by node.
    pub registered: rustc_hash::FxHashMap<NodeId, Vec<(Handle, Options)>>,
    pub microtask_queued: bool,
    /// Slots whose `slotchange` is pending (the agent's "signal slots").
    pub signal_slots: Vec<NodeId>,
}

impl Engine {
    fn obs<R>(&self, h: Handle, f: impl FnOnce(&mut ObserverData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::MutationObserver(o) => f(o),
            _ => panic!("not a MutationObserver"),
        }
    }

    /// "Queue a mutation observer microtask".
    pub(crate) fn queue_mutation_observer_microtask(&self, cx: &mut Cx) {
        let queue = {
            let mut o = self.observers.borrow_mut();
            !std::mem::replace(&mut o.microtask_queued, true)
        };
        if queue {
            self.queue_internal_microtask(cx, Microtask::MutationObservers);
        }
    }

    /// Hands the DOM's mutation records to the observers interested in them (DOM §4.3.3 "queue a
    /// mutation record" steps 1-4, run right after the operation that made them).
    pub(crate) fn distribute_mutation_records(&self, cx: &mut Cx) {
        let records = {
            let mut dom = self.dom_mut();
            if dom.mutation_records().is_empty() {
                return;
            }
            dom.take_mutation_records()
        };
        // (a user edit being recorded for undo keeps its own copy)
        self.undo_saw_records(&records);
        let mut any = false;
        for r in records {
            // interestedObservers: observer → whether it wants the old value.
            let mut interested: Vec<(Handle, bool)> = Vec::new();
            {
                let dom = self.dom();
                let observers = self.observers.borrow();
                let transient = dom.transient_observers();
                let mut nodes: Vec<(NodeId, Option<NodeId>)> = Vec::new();
                for n in dom.inclusive_ancestors(r.target) {
                    nodes.push((n, None));
                    for &(removed, source) in transient {
                        if removed == n {
                            nodes.push((n, Some(source)));
                        }
                    }
                }
                for (node, source) in nodes {
                    let regs = match source {
                        Some(s) => observers.registered.get(&s),
                        None => observers.registered.get(&node),
                    };
                    let Some(regs) = regs else {
                        continue;
                    };
                    for (mo, o) in regs {
                        if source.is_some() && !o.subtree {
                            continue;
                        }
                        if node != r.target && !o.subtree {
                            continue;
                        }
                        let wanted = match r.kind {
                            MutationKind::Attributes => {
                                if !o.attributes {
                                    continue;
                                }
                                if let Some(filter) = &o.attribute_filter {
                                    if r.attribute_namespace.is_some() {
                                        continue;
                                    }
                                    let name = r.attribute_name.as_deref().unwrap_or("");
                                    if !filter.iter().any(|f| f == name) {
                                        continue;
                                    }
                                }
                                o.attribute_old_value
                            }
                            MutationKind::CharacterData => {
                                if !o.character_data {
                                    continue;
                                }
                                o.character_data_old_value
                            }
                            MutationKind::ChildList => {
                                if !o.child_list {
                                    continue;
                                }
                                false
                            }
                        };
                        match interested.iter_mut().find(|(h, _)| h == mo) {
                            Some(e) => e.1 |= wanted,
                            None => interested.push((*mo, wanted)),
                        }
                    }
                }
            }
            if interested.is_empty() {
                continue;
            }
            any = true;
            for (mo, old) in interested {
                let added = self.static_node_list(r.added_nodes.clone());
                let removed = self.static_node_list(r.removed_nodes.clone());
                let record = self.alloc(Object::MutationRecord(RecordData {
                    kind: r.kind,
                    target: r.target,
                    added,
                    removed,
                    previous_sibling: r.previous_sibling,
                    next_sibling: r.next_sibling,
                    attribute_name: r.attribute_name.as_ref().map(|a| a.to_string()),
                    attribute_namespace: r.attribute_namespace.as_ref().map(|a| a.to_string()),
                    old_value: if old { r.old_value.clone() } else { None },
                }));
                self.obs(mo, |o| o.records.push(record));
            }
        }
        if any {
            self.queue_mutation_observer_microtask(cx);
        }
    }

    /// DOM §4.3.2 "notify mutation observers".
    pub(crate) fn notify_mutation_observers(&self, cx: &mut Cx) {
        // notifySet is the agent's pending mutation observers: those with records now. One that gets a
        // record from another's callback waits for the next microtask, after this one's `slotchange`
        // events (steps 2-3, 6).
        let (all, signals) = {
            let mut o = self.observers.borrow_mut();
            o.microtask_queued = false;
            (o.all.clone(), std::mem::take(&mut o.signal_slots))
        };
        let notify: Vec<Handle> = all
            .into_iter()
            .filter(|&mo| self.obs(mo, |o| !o.records.is_empty()))
            .collect();
        // Transient registered observers go now (step 6.2, for every observer at once).
        self.dom_mut().clear_transient_observers();
        for mo in notify {
            let records = self.obs(mo, |o| std::mem::take(&mut o.records));
            if records.is_empty() {
                continue;
            }
            let callback = self.obs(mo, |o| o.callback.clone());
            let Some(callback) = callback.get(cx.scope()).map(MutationCallback) else {
                continue;
            };
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let mut cx2 = Cx::new(tc);
            let r = callback.invoke(&mut cx2, Some(&mo), records, mo);
            if r.is_err() {
                if let Some(e) = tc.exception() {
                    let e = v8::Global::new(tc, e);
                    let m = tc.message().map(|m| v8::Global::new(tc, m));
                    tc.reset();
                    let mut cx3 = Cx::new(tc);
                    self.report_exception(&mut cx3, e, m);
                }
            }
        }
        for slot in signals {
            if self.dom().is_alive(slot) {
                self.fire_event(cx, node_handle(slot), "slotchange", true, false);
            }
        }
    }

    /// DOM §4.2.2.5 "signal a slot change".
    pub(crate) fn signal_slot_change(&self, slot: NodeId) {
        let mut o = self.observers.borrow_mut();
        if !o.signal_slots.contains(&slot) {
            o.signal_slots.push(slot);
        }
        // The microtask is queued by the next reaction pass (it has a scope).
        self.pending_slot_signal.set(true);
    }
}

impl MutationObserverImpl for Engine {
    fn observe(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: Handle,
        options: MutationObserverInit,
    ) -> rt::Result<()> {
        let target = self.n(target);
        // 1-6.
        let mut attributes = options.attributes;
        if attributes.is_none()
            && (options.attribute_old_value.is_some() || options.attribute_filter.is_some())
        {
            attributes = Some(true);
        }
        let mut character_data = options.character_data;
        if character_data.is_none() && options.character_data_old_value.is_some() {
            character_data = Some(true);
        }
        let attributes = attributes.unwrap_or(false);
        let character_data = character_data.unwrap_or(false);
        if !options.child_list && !attributes && !character_data {
            return Err(rt::Error::Type(
                "One of childList, attributes or characterData must be true".into(),
            ));
        }
        if options.attribute_old_value == Some(true) && !attributes {
            return Err(rt::Error::Type(
                "attributeOldValue requires attributes".into(),
            ));
        }
        if options.attribute_filter.is_some() && !attributes {
            return Err(rt::Error::Type(
                "attributeFilter requires attributes".into(),
            ));
        }
        if options.character_data_old_value == Some(true) && !character_data {
            return Err(rt::Error::Type(
                "characterDataOldValue requires characterData".into(),
            ));
        }
        let opts = Options {
            child_list: options.child_list,
            attributes,
            character_data,
            subtree: options.subtree,
            attribute_old_value: options.attribute_old_value.unwrap_or(false),
            character_data_old_value: options.character_data_old_value.unwrap_or(false),
            attribute_filter: options.attribute_filter,
        };
        // 7-8.
        let replaced = {
            let mut o = self.observers.borrow_mut();
            let regs = o.registered.entry(target).or_default();
            match regs.iter_mut().find(|(h, _)| *h == this) {
                Some(r) => {
                    r.1 = opts;
                    true
                }
                None => {
                    regs.push((this, opts));
                    false
                }
            }
        };
        if !replaced {
            self.obs(this, |o| o.nodes.push(target));
        }
        self.dom_mut().set_observed(target, true);
        Ok(())
    }

    fn disconnect(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let nodes = self.obs(this, |o| {
            o.records.clear();
            std::mem::take(&mut o.nodes)
        });
        let mut emptied = Vec::new();
        {
            let mut o = self.observers.borrow_mut();
            for n in nodes {
                if let Some(regs) = o.registered.get_mut(&n) {
                    regs.retain(|(h, _)| *h != this);
                    if regs.is_empty() {
                        o.registered.remove(&n);
                        emptied.push(n);
                    }
                }
            }
        }
        let mut dom = self.dom_mut();
        for n in emptied {
            if dom.is_alive(n) {
                dom.set_observed(n, false);
            }
        }
        Ok(())
    }

    fn take_records(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        Ok(self.obs(this, |o| std::mem::take(&mut o.records)))
    }

    fn constructor(&self, _cx: &mut Cx, callback: MutationCallback) -> rt::Result<Handle> {
        let h = self.alloc(Object::MutationObserver(ObserverData {
            callback: super::held::Held::object(callback.0),
            records: Vec::new(),
            nodes: Vec::new(),
        }));
        self.observers.borrow_mut().all.push(h);
        Ok(h)
    }
}

impl Engine {
    fn rec<R>(&self, h: Handle, f: impl FnOnce(&RecordData) -> R) -> R {
        match &*self.obj(h) {
            Object::MutationRecord(r) => f(r),
            _ => panic!("not a MutationRecord"),
        }
    }
}

impl MutationRecordImpl for Engine {
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.rec(this, |r| match r.kind {
            MutationKind::Attributes => "attributes",
            MutationKind::CharacterData => "characterData",
            MutationKind::ChildList => "childList",
        }))
    }
    fn target(&self, this: Handle) -> Handle {
        node_handle(self.rec(this, |r| r.target))
    }
    fn added_nodes(&self, this: Handle) -> Handle {
        self.rec(this, |r| r.added)
    }
    fn removed_nodes(&self, this: Handle) -> Handle {
        self.rec(this, |r| r.removed)
    }
    fn previous_sibling(&self, this: Handle) -> Option<Handle> {
        self.rec(this, |r| r.previous_sibling).map(node_handle)
    }
    fn next_sibling(&self, this: Handle) -> Option<Handle> {
        self.rec(this, |r| r.next_sibling).map(node_handle)
    }
    fn attribute_name(&self, this: Handle) -> Option<DomStr<'_>> {
        self.rec(this, |r| r.attribute_name.clone())
            .map(DomStr::Owned)
    }
    fn attribute_namespace(&self, this: Handle) -> Option<DomStr<'_>> {
        self.rec(this, |r| r.attribute_namespace.clone())
            .map(DomStr::Owned)
    }
    fn old_value(&self, this: Handle) -> Option<DomStr<'_>> {
        self.rec(this, |r| r.old_value.clone()).map(DomStr::Owned)
    }
}
