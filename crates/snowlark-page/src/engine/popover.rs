// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML "the popover attribute": the popover states (auto, manual, hint), showing and hiding with
//! `beforetoggle`/`toggle` events, each document's showing auto and hint popover lists (the popover
//! stacks, nesting by flat tree ancestor and invoker), light dismiss on pointer down and up, Esc
//! as a close request, the popover focusing steps and focus restoration, `popovertarget` and
//! `popovertargetaction` invokers, the top layer and `:popover-open`.
//!
//! Not implemented: pending top layer removals (`overlay` transitions: a hidden popover leaves the
//! top layer at once), close watcher grouping by user activation, implicit anchor elements.

use snowlark_bindings::generated::{ShowPopoverOptions, TogglePopoverOptionsOrBoolean};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, ns, LocalName, NodeId};
use snowlark_style::ElementState;

use super::close_watcher::CloseWatcher;
use super::events::EventExt;
use super::{exception, node_handle, Engine};
use crate::event_loop::TaskSource;

/// A popover attribute state other than "no popover".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PopoverMode {
    Auto,
    Manual,
    Hint,
}

/// What a popover keeps besides its visibility (the showing popovers are `HtmlState::popovers`).
#[derive(Default)]
pub struct PopoverData {
    /// "popover hiding".
    pub showing_or_hiding: bool,
    /// "popover invoker".
    pub invoker: Option<NodeId>,
    /// "opened in popover mode".
    pub mode: Option<PopoverMode>,
    /// "previously focused element".
    pub previously_focused: Option<NodeId>,
}

/// Each document's popover stacks and the element state of each popover.
#[derive(Default)]
pub struct PopoverState {
    pub data: rustc_hash::FxHashMap<NodeId, PopoverData>,
    /// Each document's "showing auto popover list", bottom first.
    pub auto: rustc_hash::FxHashMap<NodeId, Vec<NodeId>>,
    /// Each document's "showing hint popover list", bottom first.
    pub hint: rustc_hash::FxHashMap<NodeId, Vec<NodeId>>,
    /// Each document's "popover pointerdown target".
    pub pointerdown_target: rustc_hash::FxHashMap<NodeId, Option<NodeId>>,
    /// Each document's "hint stack parent": the auto popover the first hint popover is nested in.
    pub hint_stack_parent: rustc_hash::FxHashMap<NodeId, NodeId>,
    /// The documents whose "showing popover" is true.
    pub showing_popover: rustc_hash::FxHashSet<NodeId>,
    /// Each document's "hiding popover nesting count" (absent: 0).
    pub hiding_count: rustc_hash::FxHashMap<NodeId, u32>,
    /// Each popover's "popover toggle task tracker": the pending task's generation and old state.
    pub toggle_tracker: rustc_hash::FxHashMap<NodeId, (u64, &'static str)>,
    pub toggle_generation: u64,
    /// While a changed `popover` attribute hides its popover: the element and its old state (the
    /// hide sees the state the popover was shown in).
    pub old_state: Option<(NodeId, Option<PopoverMode>)>,
    /// While an invoker or a command button hides its popover: the popover and the button (the
    /// source of the events).
    pub hide_source: Option<(NodeId, NodeId)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stack {
    Auto,
    Hint,
}

fn mode_of(value: &str) -> PopoverMode {
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        PopoverMode::Auto
    } else if value.eq_ignore_ascii_case("hint") {
        PopoverMode::Hint
    } else {
        PopoverMode::Manual
    }
}

impl Engine {
    /// The element's popover attribute state (`None`: no popover).
    pub(crate) fn popover_state(&self, el: NodeId) -> Option<PopoverMode> {
        if let Some((e, old)) = self.popover.borrow().old_state {
            if e == el {
                return old;
            }
        }
        let dom = self.dom();
        if !dom.is_element(el) || *dom.namespace(el) != ns!(html) {
            return None;
        }
        dom.attr(el, &ns!(), &LocalName::from("popover"))
            .map(mode_of)
    }

    /// Whether the popover's visibility state is showing.
    pub(crate) fn popover_showing(&self, el: NodeId) -> bool {
        self.html.borrow().popovers.contains(&el)
    }

    fn popover_mode(&self, el: NodeId) -> Option<PopoverMode> {
        self.popover.borrow().data.get(&el).and_then(|d| d.mode)
    }

    fn popover_flag(&self, el: NodeId) -> bool {
        self.popover
            .borrow()
            .data
            .get(&el)
            .is_some_and(|d| d.showing_or_hiding)
    }

    fn set_popover_flag(&self, el: NodeId, on: bool) {
        self.popover
            .borrow_mut()
            .data
            .entry(el)
            .or_default()
            .showing_or_hiding = on;
    }

    fn popover_list(&self, doc: NodeId, stack: Stack) -> Vec<NodeId> {
        let p = self.popover.borrow();
        let map = match stack {
            Stack::Auto => &p.auto,
            Stack::Hint => &p.hint,
        };
        map.get(&doc).cloned().unwrap_or_default()
    }

    /// "Topmost auto or hint popover": the last of the hint list, else of the auto list.
    pub(crate) fn topmost_auto_or_hint_popover(&self, doc: NodeId) -> Option<NodeId> {
        let p = self.popover.borrow();
        p.hint
            .get(&doc)
            .and_then(|l| l.last())
            .or_else(|| p.auto.get(&doc).and_then(|l| l.last()))
            .copied()
    }

    /// "Check popover validity". `Err` only when `throw` is set.
    fn check_popover_validity(
        &self,
        el: NodeId,
        expected_showing: bool,
        throw: bool,
        expected_doc: Option<NodeId>,
    ) -> rt::Result<bool> {
        if self.popover_state(el).is_none() {
            return if throw {
                Err(exception(
                    ExceptionName::NotSupportedError,
                    "The element is not a popover",
                ))
            } else {
                Ok(false)
            };
        }
        if self.popover_showing(el) != expected_showing {
            return Ok(false);
        }
        let doc = self.dom().node_document(el);
        // (a showing popover can be hidden once its document is no longer fully active)
        let invalid = !self.dom().is_connected(el)
            || (!expected_showing && !self.is_fully_active(doc))
            || expected_doc.is_some_and(|d| d != doc)
            || (self.dom().is_html_element_named(el, &local_name!("dialog"))
                && self.dialogs.borrow().modal.contains(&el))
            || self.has_fullscreen_flag(el);
        if invalid {
            return if throw {
                Err(exception(
                    ExceptionName::InvalidStateError,
                    "The popover is not in a state it can be shown or hidden in",
                ))
            } else {
                Ok(false)
            };
        }
        Ok(true)
    }

    /// Fires `beforetoggle` at a popover; returns whether it was not canceled.
    fn popover_beforetoggle(
        &self,
        cx: &mut Cx,
        el: NodeId,
        old: &str,
        new: &str,
        cancelable: bool,
        source: Option<NodeId>,
    ) -> bool {
        let e = self.trusted_event(
            InterfaceId::ToggleEvent,
            "beforetoggle",
            false,
            cancelable,
            EventExt::Toggle {
                old_state: old.to_owned(),
                new_state: new.to_owned(),
                source: source.map(node_handle),
            },
        );
        self.dispatch(cx, e, node_handle(el), false)
    }

    /// "Queue a popover toggle event task": a later task replaces a pending one and keeps its old
    /// state.
    fn queue_popover_toggle(
        &self,
        el: NodeId,
        old: &'static str,
        new: &'static str,
        source: Option<NodeId>,
    ) {
        let (generation, old) = {
            let p = &mut *self.popover.borrow_mut();
            let old = p.toggle_tracker.get(&el).map_or(old, |t| t.1);
            p.toggle_generation += 1;
            let g = p.toggle_generation;
            p.toggle_tracker.insert(el, (g, old));
            (g, old)
        };
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let current = e.popover.borrow().toggle_tracker.get(&el).map(|t| t.0);
            if current != Some(generation) {
                return;
            }
            e.popover.borrow_mut().toggle_tracker.remove(&el);
            if !e.dom().is_alive(el) {
                return;
            }
            let source = source.filter(|&s| e.dom().is_alive(s));
            let ev = e.trusted_event(
                InterfaceId::ToggleEvent,
                "toggle",
                false,
                false,
                EventExt::Toggle {
                    old_state: old.to_owned(),
                    new_state: new.to_owned(),
                    source: source.map(node_handle),
                },
            );
            e.dispatch(cx, ev, node_handle(el), false);
        });
    }

    /// "Show popover" (`invoker`: the source).
    pub(crate) fn show_popover_steps(
        &self,
        cx: &mut Cx,
        el: NodeId,
        throw: bool,
        invoker: Option<NodeId>,
    ) -> rt::Result<()> {
        let doc = self.dom().node_document(el);
        // (no popover is shown while one is being shown or hidden in the document)
        let busy = {
            let p = self.popover.borrow();
            p.showing_popover.contains(&doc) || p.hiding_count.contains_key(&doc)
        };
        if busy {
            return if throw {
                Err(exception(
                    ExceptionName::InvalidStateError,
                    "A popover is being shown or hidden",
                ))
            } else {
                Ok(())
            };
        }
        if !self.check_popover_validity(el, false, throw, None)? {
            return Ok(());
        }
        self.popover.borrow_mut().showing_popover.insert(doc);
        let r = self.show_popover_inner(cx, el, doc, throw, invoker);
        self.popover.borrow_mut().showing_popover.remove(&doc);
        r
    }

    fn show_popover_inner(
        &self,
        cx: &mut Cx,
        el: NodeId,
        doc: NodeId,
        throw: bool,
        invoker: Option<NodeId>,
    ) -> rt::Result<()> {
        if !self.popover_beforetoggle(cx, el, "closed", "open", true, invoker) {
            return Ok(());
        }
        if !self.check_popover_validity(el, false, throw, Some(doc))? {
            return Ok(());
        }
        let mut should_restore_focus = false;
        let original = self.popover_state(el);
        let mut effective = original;
        let mut ancestor = None;
        if matches!(original, Some(PopoverMode::Auto | PopoverMode::Hint)) {
            ancestor = self.topmost_popover_ancestor(el, invoker, true);
            // (an auto popover cannot have a hint parent: it is shown as a hint)
            if effective == Some(PopoverMode::Auto)
                && ancestor.is_some_and(|a| self.popover_mode(a) == Some(PopoverMode::Hint))
            {
                effective = Some(PopoverMode::Hint);
            }
            self.hide_popover_stack_until(cx, doc, ancestor, Stack::Hint, false, true);
            if effective == Some(PopoverMode::Auto) {
                self.hide_popover_stack_until(cx, doc, ancestor, Stack::Auto, false, true);
            }
            // (the events above could have changed the popover)
            if original != self.popover_state(el) {
                return if throw {
                    Err(exception(
                        ExceptionName::InvalidStateError,
                        "The popover attribute changed while the popover was being shown",
                    ))
                } else {
                    Ok(())
                };
            }
            if !self.check_popover_validity(el, false, throw, Some(doc))? {
                return Ok(());
            }
            if self.topmost_auto_or_hint_popover(doc).is_none() {
                should_restore_focus = true;
            }
            {
                let p = &mut *self.popover.borrow_mut();
                p.data.entry(el).or_default().mode = effective;
                if effective == Some(PopoverMode::Auto) {
                    p.auto.entry(doc).or_default().push(el);
                } else {
                    p.hint.entry(doc).or_default().push(el);
                }
            }
            // (its "popover close watcher": Esc hides it)
            self.establish_close_watcher(doc, CloseWatcher::Popover(el));
        }
        {
            let p = &mut *self.popover.borrow_mut();
            let d = p.data.entry(el).or_default();
            d.previously_focused = None;
            d.invoker = invoker;
        }
        // (the invoker is the popover's implicit anchor element: CSS Anchor Positioning 1 §2.4.1)
        self.with_doc_renderer(doc, |r, _| r.set_implicit_anchor(el, invoker));
        let originally_focused = self.focused_element(doc);
        self.add_to_top_layer(el);
        if effective == Some(PopoverMode::Hint) {
            if let Some(a) = ancestor.filter(|&a| self.popover_mode(a) == Some(PopoverMode::Auto)) {
                self.popover.borrow_mut().hint_stack_parent.insert(doc, a);
            }
        }
        self.html.borrow_mut().popovers.push(el);
        self.set_element_state(el, ElementState::POPOVER_OPEN, true);
        self.popover_focusing_steps(cx, el);
        if should_restore_focus && self.popover_state(el).is_some() {
            if let Some(d) = self.popover.borrow_mut().data.get_mut(&el) {
                d.previously_focused = originally_focused;
            }
        }
        self.queue_popover_toggle(el, "closed", "open", invoker);
        Ok(())
    }
    /// "Popover focusing steps".
    fn popover_focusing_steps(&self, cx: &mut Cx, el: NodeId) {
        if self.dom().is_html_element_named(el, &local_name!("dialog")) {
            self.dialog_focusing_steps(cx, el);
            return;
        }
        let autofocus =
            |e: &Engine, n: NodeId| e.dom().attr(n, &ns!(), &local_name!("autofocus")).is_some();
        let control = if autofocus(self, el) {
            Some(el)
        } else {
            // (the autofocus delegate: the first focusable descendant with `autofocus`)
            let descendants: Vec<NodeId> = {
                let dom = self.dom();
                dom.descendants(el).filter(|&n| dom.is_element(n)).collect()
            };
            descendants
                .into_iter()
                .find(|&n| autofocus(self, n) && self.is_focusable(n))
        };
        if let Some(c) = control {
            self.focus_element(cx, c);
            self.autofocus_preempted(c);
        }
    }

    /// "Hide a popover" (`expected_doc`: the document it must still be in after events, its own
    /// when `None`).
    pub(crate) fn hide_popover_steps(
        &self,
        cx: &mut Cx,
        el: NodeId,
        focus_previous: bool,
        fire_events: bool,
        throw: bool,
        expected_doc: Option<NodeId>,
    ) -> rt::Result<()> {
        if !self.check_popover_validity(el, true, throw, None)? {
            return Ok(());
        }
        let doc = self.dom().node_document(el);
        // ("popover hiding": a nested hide fires no events)
        let nested = self.popover_flag(el);
        self.set_popover_flag(el, true);
        let fire_events = fire_events && !nested;
        *self
            .popover
            .borrow_mut()
            .hiding_count
            .entry(doc)
            .or_default() += 1;
        let r = self.hide_popover_inner(
            cx,
            el,
            doc,
            focus_previous,
            fire_events,
            throw,
            expected_doc.unwrap_or(doc),
        );
        if !nested && self.dom().is_alive(el) {
            self.set_popover_flag(el, false);
        }
        {
            let mut p = self.popover.borrow_mut();
            if let Some(n) = p.hiding_count.get_mut(&doc) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    p.hiding_count.remove(&doc);
                }
            }
        }
        r
    }

    /// "Hide popover" for an invoker or a command button: focus restored, events fired with the
    /// button as their source.
    pub(crate) fn hide_popover_from(&self, cx: &mut Cx, el: NodeId, source: NodeId) {
        let saved = self.popover.borrow_mut().hide_source.replace((el, source));
        let _ = self.hide_popover_steps(cx, el, true, true, false, None);
        self.popover.borrow_mut().hide_source = saved;
    }

    #[allow(clippy::too_many_arguments)]
    fn hide_popover_inner(
        &self,
        cx: &mut Cx,
        el: NodeId,
        doc: NodeId,
        focus_previous: bool,
        fire_events: bool,
        throw: bool,
        expected_doc: NodeId,
    ) -> rt::Result<()> {
        if matches!(
            self.popover_mode(el),
            Some(PopoverMode::Auto | PopoverMode::Hint)
        ) {
            // (the hint popovers above it; every hint popover when it is their hint stack
            // parent; the auto popovers above it)
            if self.popover_list(doc, Stack::Hint).contains(&el) {
                self.hide_popover_stack_until(
                    cx,
                    doc,
                    Some(el),
                    Stack::Hint,
                    focus_previous,
                    fire_events,
                );
            }
            if self.hint_stack_parent(doc) == Some(el) {
                self.hide_popover_stack_until(
                    cx,
                    doc,
                    None,
                    Stack::Hint,
                    focus_previous,
                    fire_events,
                );
            }
            if self.popover_list(doc, Stack::Auto).contains(&el) {
                self.hide_popover_stack_until(
                    cx,
                    doc,
                    Some(el),
                    Stack::Auto,
                    focus_previous,
                    fire_events,
                );
            }
            if !self.check_popover_validity(el, true, throw, Some(expected_doc))? {
                return Ok(());
            }
        }
        let source = self
            .popover
            .borrow()
            .hide_source
            .filter(|&(e, _)| e == el)
            .map(|(_, s)| s);
        if fire_events {
            self.popover_beforetoggle(cx, el, "open", "closed", false, source);
            if !self.check_popover_validity(el, true, throw, Some(expected_doc))? {
                return Ok(());
            }
        }
        self.popover_hidden(el, doc);
        if fire_events {
            self.queue_popover_toggle(el, "open", "closed", source);
        }
        let previous = self
            .popover
            .borrow_mut()
            .data
            .get_mut(&el)
            .and_then(|d| d.previously_focused.take());
        if let Some(prev) = previous {
            let focus_inside = self
                .focused_element(doc)
                .is_some_and(|f| self.dom().is_shadow_including_inclusive_ancestor_of(el, f));
            // (the focusing steps do nothing for an element that is no focusable area, such as one
            // the hidden popover now hides)
            if focus_previous
                && focus_inside
                && self.dom().is_alive(prev)
                && self.is_focusable_area(prev)
            {
                self.focus_element(cx, prev);
            }
        }
        Ok(())
    }

    /// The state changes of hiding: its close watcher destroyed, out of the top layer and the
    /// popover lists, no longer the hint stack parent, hidden.
    fn popover_hidden(&self, el: NodeId, doc: NodeId) {
        self.destroy_close_watcher(CloseWatcher::Popover(el));
        self.remove_from_top_layer(el);
        {
            let p = &mut *self.popover.borrow_mut();
            for map in [&mut p.auto, &mut p.hint] {
                if let Some(l) = map.get_mut(&doc) {
                    l.retain(|&e| e != el);
                }
            }
            if let Some(d) = p.data.get_mut(&el) {
                d.mode = None;
                d.invoker = None;
            }
            let no_hints = p.hint.get(&doc).is_none_or(|l| l.is_empty());
            if no_hints || p.hint_stack_parent.get(&doc) == Some(&el) {
                p.hint_stack_parent.remove(&doc);
            }
        }
        self.html.borrow_mut().popovers.retain(|&p| p != el);
        self.with_doc_renderer(doc, |r, _| r.set_implicit_anchor(el, None));
        self.set_element_state(el, ElementState::POPOVER_OPEN, false);
    }

    /// "Hide popovers until" (`endpoint` `None`: the document): the hint popovers above it, then
    /// the auto popovers above it (above its hint stack parent for a hint popover).
    pub(crate) fn hide_all_popovers_until(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        endpoint: Option<NodeId>,
        focus_previous: bool,
        fire_events: bool,
    ) {
        let endpoint_is_hint =
            endpoint.is_some_and(|e| self.popover_list(doc, Stack::Hint).contains(&e));
        self.hide_popover_stack_until(cx, doc, endpoint, Stack::Hint, focus_previous, fire_events);
        let auto_endpoint = if endpoint_is_hint {
            self.hint_stack_parent(doc)
        } else {
            endpoint
        };
        self.hide_popover_stack_until(
            cx,
            doc,
            auto_endpoint,
            Stack::Auto,
            focus_previous,
            fire_events,
        );
    }

    /// "Hide popover stack until": hides the popovers above `endpoint` in the list, topmost first
    /// (all of them when `endpoint` is not in it), then, without events, any shown meanwhile.
    fn hide_popover_stack_until(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        endpoint: Option<NodeId>,
        stack: Stack,
        focus_previous: bool,
        fire_events: bool,
    ) {
        let list = self.popover_list(doc, stack);
        let last_hide_index = endpoint
            .and_then(|e| list.iter().position(|&p| p == e))
            .map_or(0, |i| i + 1);
        let to_remain = &list[..last_hide_index];
        for &p in list[last_hide_index..].iter().rev() {
            let _ = self.hide_popover_steps(cx, p, focus_previous, fire_events, false, None);
        }
        let new_list = self.popover_list(doc, stack);
        for &p in new_list.iter().rev() {
            if to_remain.contains(&p) {
                continue;
            }
            let _ = self.hide_popover_steps(cx, p, focus_previous, false, false, None);
        }
    }

    /// Hides a popover without events or focus changes (one removed from its document, or one
    /// whose attribute no longer makes it a popover).
    fn force_hide_popover(&self, el: NodeId) {
        let doc = {
            let p = self.popover.borrow();
            p.auto
                .iter()
                .chain(p.hint.iter())
                .find(|(_, l)| l.contains(&el))
                .map(|(d, _)| *d)
        };
        let doc = doc.unwrap_or_else(|| self.dom().node_document(el));
        self.popover_hidden(el, doc);
        if let Some(d) = self.popover.borrow_mut().data.get_mut(&el) {
            d.previously_focused = None;
        }
    }
    /// "Nearest inclusive open popover".
    fn nearest_inclusive_open_popover(&self, node: NodeId) -> Option<NodeId> {
        let mut cur = Some(node);
        while let Some(n) = cur {
            if matches!(
                self.popover_mode(n),
                Some(PopoverMode::Auto | PopoverMode::Hint)
            ) && self.popover_showing(n)
            {
                return Some(n);
            }
            cur = self.flat_parent(n);
        }
        None
    }

    /// "Nearest inclusive target popover for invoker".
    fn nearest_inclusive_target_popover(&self, node: NodeId) -> Option<NodeId> {
        let mut cur = Some(node);
        while let Some(n) = cur {
            // (a popovertarget invoker, or a button whose command acts on a popover)
            let target = self
                .popover_target_element(n)
                .or_else(|| self.command_target_popover(n));
            if let Some(t) = target {
                if matches!(
                    self.popover_state(t),
                    Some(PopoverMode::Auto | PopoverMode::Hint)
                ) && self.popover_showing(t)
                {
                    return Some(t);
                }
            }
            cur = self.flat_parent(n);
        }
        None
    }

    /// "Topmost popover ancestor": of the showing auto popovers then the showing hint popovers,
    /// the last one that is a flat tree ancestor of `el` or an inclusive flat tree ancestor of its
    /// `source` (the invoker). `is_popover`: `el` is a popover being shown (else a dialog or a
    /// fullscreen element entering the top layer, with no source).
    pub(crate) fn topmost_popover_ancestor(
        &self,
        el: NodeId,
        source: Option<NodeId>,
        is_popover: bool,
    ) -> Option<NodeId> {
        debug_assert!(is_popover || source.is_none());
        let doc = self.dom().node_document(el);
        let mut combined = self.popover_list(doc, Stack::Auto);
        combined.extend(self.popover_list(doc, Stack::Hint));
        let last_ancestor_index = |start: Option<NodeId>| {
            let mut best: Option<usize> = None;
            let mut cur = start;
            while let Some(n) = cur {
                if let Some(i) = combined.iter().position(|&p| p == n) {
                    best = Some(best.map_or(i, |b| b.max(i)));
                }
                cur = self.flat_parent(n);
            }
            best
        };
        let popover_index = last_ancestor_index(self.flat_parent(el));
        let source_index = source.and_then(|s| last_ancestor_index(Some(s)));
        let index = match (popover_index, source_index) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        index.map(|i| combined[i])
    }

    /// A dialog or a fullscreen element entering the top layer hides the popovers it is not in
    /// (`show()` and "show a modal dialog": "hide popovers until" its topmost popover ancestor).
    pub(crate) fn hide_popovers_for_top_layer(&self, cx: &mut Cx, el: NodeId) {
        let doc = self.dom().node_document(el);
        let until = self.topmost_popover_ancestor(el, None, false);
        self.hide_all_popovers_until(cx, doc, until, false, true);
    }

    /// The document's "hint stack parent".
    fn hint_stack_parent(&self, doc: NodeId) -> Option<NodeId> {
        self.popover.borrow().hint_stack_parent.get(&doc).copied()
    }
    /// "Popover stack position".
    fn popover_stack_position(&self, el: Option<NodeId>, doc: NodeId) -> usize {
        let Some(el) = el else { return 0 };
        let autos = self.popover_list(doc, Stack::Auto);
        let hints = self.popover_list(doc, Stack::Hint);
        if let Some(i) = hints.iter().position(|&p| p == el) {
            return i + autos.len() + 1;
        }
        autos.iter().position(|&p| p == el).map_or(0, |i| i + 1)
    }

    /// "Topmost clicked popover".
    fn topmost_clicked_popover(&self, node: NodeId) -> Option<NodeId> {
        let doc = self.dom().node_document(node);
        let clicked = self.nearest_inclusive_open_popover(node);
        let target = self.nearest_inclusive_target_popover(node);
        if self.popover_stack_position(clicked, doc) > self.popover_stack_position(target, doc) {
            clicked
        } else {
            target
        }
    }

    /// "Light dismiss open popovers", for a trusted `pointerdown` or `pointerup` at `target`.
    pub(crate) fn popover_light_dismiss(&self, cx: &mut Cx, target: NodeId, pointerdown: bool) {
        let doc = self.dom().node_document(target);
        if self.topmost_auto_or_hint_popover(doc).is_none() {
            return;
        }
        if pointerdown {
            let t = self.topmost_clicked_popover(target);
            self.popover.borrow_mut().pointerdown_target.insert(doc, t);
            return;
        }
        let ancestor = self.topmost_clicked_popover(target);
        let down = self
            .popover
            .borrow_mut()
            .pointerdown_target
            .remove(&doc)
            .flatten();
        if ancestor == down {
            self.hide_all_popovers_until(cx, doc, ancestor, false, true);
        }
    }

    /// The popover removing steps: showing popovers that left their document are hidden, without
    /// events or focus changes, and with them what hiding them hides (the popovers above them in
    /// their list, every hint popover for the hint stack parent).
    pub(crate) fn popovers_removed(&self, cx: &mut Cx) {
        // (removed, or moved to another document: no longer in its document's top layer)
        let gone: Vec<NodeId> = {
            let showing = self.html.borrow().popovers.clone();
            showing
                .into_iter()
                .filter(|&e| {
                    let doc = {
                        let dom = self.dom();
                        if !dom.is_alive(e) || !dom.is_connected(e) {
                            return true;
                        }
                        dom.node_document(e)
                    };
                    !self
                        .dialogs
                        .borrow()
                        .top_layers
                        .get(&doc)
                        .is_some_and(|l| l.contains(&e))
                })
                .collect()
        };
        for el in gone {
            if !self.popover_showing(el) {
                continue;
            }
            let doc = {
                let p = self.popover.borrow();
                p.auto
                    .iter()
                    .chain(p.hint.iter())
                    .find(|(_, l)| l.contains(&el))
                    .map(|(d, _)| *d)
            };
            if let Some(doc) = doc {
                // (as "hide a popover" step 10 does for one that can still be hidden)
                if self.popover_list(doc, Stack::Hint).contains(&el) {
                    self.hide_popover_stack_until(cx, doc, Some(el), Stack::Hint, false, false);
                }
                if self.hint_stack_parent(doc) == Some(el) {
                    self.hide_popover_stack_until(cx, doc, None, Stack::Hint, false, false);
                }
                if self.popover_list(doc, Stack::Auto).contains(&el) {
                    self.hide_popover_stack_until(cx, doc, Some(el), Stack::Auto, false, false);
                }
            }
            if self.dom().is_alive(el) {
                self.force_hide_popover(el);
            } else {
                self.html.borrow_mut().popovers.retain(|&p| p != el);
                self.destroy_close_watcher(CloseWatcher::Popover(el));
                let p = &mut *self.popover.borrow_mut();
                for map in [&mut p.auto, &mut p.hint] {
                    for l in map.values_mut() {
                        l.retain(|&e| e != el);
                    }
                }
                p.hint_stack_parent.retain(|_, &mut e| e != el);
                p.data.remove(&el);
            }
        }
    }
    /// The `popover` attribute changed: a showing popover whose state changed is hidden (seeing
    /// its old state).
    pub(crate) fn popover_attribute_changed(
        &self,
        cx: &mut Cx,
        el: NodeId,
        old: Option<&str>,
        new: Option<&str>,
    ) {
        let (old, new) = (old.map(mode_of), new.map(mode_of));
        if old == new || !self.popover_showing(el) {
            return;
        }
        let saved = self.popover.borrow_mut().old_state.replace((el, old));
        let _ = self.hide_popover_steps(cx, el, true, true, false, None);
        self.popover.borrow_mut().old_state = saved;
        // (a popover whose attribute is gone cannot stay showing)
        if self.popover_showing(el) && self.popover_state(el).is_none() {
            self.force_hide_popover(el);
        }
    }

    /// Sequential focus navigation order: a showing popover with an invoker is a focus navigation
    /// scope placed right after its invoker, so its focusable elements follow the invoker (popovers
    /// in the order they were shown, so a nested one lands inside its parent's).
    pub(crate) fn popover_focus_order(&self, order: &mut Vec<(i64, NodeId)>) {
        let showing: Vec<(NodeId, NodeId)> = {
            let p = self.popover.borrow();
            self.html
                .borrow()
                .popovers
                .iter()
                .filter_map(|&e| p.data.get(&e).and_then(|d| d.invoker).map(|i| (e, i)))
                .collect()
        };
        for (popover, invoker) in showing {
            let inside = |n: NodeId| {
                self.dom()
                    .is_shadow_including_inclusive_ancestor_of(popover, n)
            };
            if inside(invoker) {
                continue;
            }
            // (an invoker that is not in the order itself leaves the popover where it is)
            if !order.iter().any(|&(_, n)| n == invoker) {
                continue;
            }
            let moved: Vec<(i64, NodeId)> =
                order.iter().copied().filter(|&(_, n)| inside(n)).collect();
            if moved.is_empty() {
                continue;
            }
            order.retain(|&(_, n)| !inside(n));
            if let Some(at) = order.iter().position(|&(_, n)| n == invoker) {
                order.splice(at + 1..at + 1, moved);
            }
        }
    }

    /// Whether `el` is a button for the popover target attributes: a `button`, or an `input` in the
    /// Submit Button, Reset Button, Image Button or Button state.
    fn is_popover_button(&self, el: NodeId) -> bool {
        let dom = self.dom();
        if dom.is_html_element_named(el, &local_name!("button")) {
            return true;
        }
        dom.is_html_element_named(el, &local_name!("input"))
            && dom.attr(el, &ns!(), &local_name!("type")).is_some_and(|t| {
                ["submit", "reset", "image", "button"]
                    .iter()
                    .any(|k| t.eq_ignore_ascii_case(k))
            })
    }

    fn is_submit_button(&self, el: NodeId) -> bool {
        let dom = self.dom();
        if dom.is_html_element_named(el, &local_name!("button")) {
            return super::forms::button_type_state(&dom, el) == "submit";
        }
        dom.attr(el, &ns!(), &local_name!("type"))
            .is_some_and(|t| t.eq_ignore_ascii_case("submit") || t.eq_ignore_ascii_case("image"))
    }

    fn is_reset_button(&self, el: NodeId) -> bool {
        let dom = self.dom();
        if dom.is_html_element_named(el, &local_name!("button")) {
            return super::forms::button_type_state(&dom, el) == "reset";
        }
        dom.attr(el, &ns!(), &local_name!("type"))
            .is_some_and(|t| t.eq_ignore_ascii_case("reset"))
    }

    /// The popover a button's `commandfor` and `command` (a popover command) act on.
    fn command_target_popover(&self, node: NodeId) -> Option<NodeId> {
        use snowlark_bindings::generated::HTMLButtonElementImpl;
        if !self
            .dom()
            .is_html_element_named(node, &local_name!("button"))
        {
            return None;
        }
        // ("get the target popover": a disabled button has none)
        if super::html::is_disabled(&self.dom(), node) {
            return None;
        }
        // (a button in a form acts on the form unless its type is button)
        if self.form_owner_of(node).is_some()
            && super::forms::button_type_state(&self.dom(), node) != "button"
        {
            return None;
        }
        let h = node_handle(node);
        let command = HTMLButtonElementImpl::command(self, h).to_string();
        if !matches!(
            command.as_str(),
            "toggle-popover" | "show-popover" | "hide-popover"
        ) {
            return None;
        }
        let t = self.n(HTMLButtonElementImpl::command_for_element(self, h)?);
        self.popover_state(t).is_some().then_some(t)
    }

    /// A node's "popover target element".
    pub(crate) fn popover_target_element(&self, node: NodeId) -> Option<NodeId> {
        if !self.dom().is_element(node) || !self.is_popover_button(node) {
            return None;
        }
        if super::html::is_disabled(&self.dom(), node) {
            return None;
        }
        // (a submit button submits its form; a reset button in a form resets it)
        if (self.is_submit_button(node) || self.is_reset_button(node))
            && self.form_owner_of(node).is_some()
        {
            return None;
        }
        let target = self.element_ref(node, "popovertarget")?;
        let t = self.n(target);
        self.popover_state(t).is_some().then_some(t)
    }

    /// "Popover target attribute activation behavior".
    pub(crate) fn popover_target_activation(
        &self,
        cx: &mut Cx,
        node: NodeId,
        event_target: Option<NodeId>,
    ) {
        let Some(popover) = self.popover_target_element(node) else {
            return;
        };
        {
            let dom = self.dom();
            if event_target
                .is_some_and(|t| dom.is_shadow_including_inclusive_ancestor_of(popover, t))
                && dom.is_shadow_including_inclusive_ancestor_of(node, popover)
                && node != popover
            {
                return;
            }
        }
        let action = self
            .attr_str(node, "popovertargetaction")
            .map(|v| v.to_ascii_lowercase());
        let showing = self.popover_showing(popover);
        match action.as_deref() {
            Some("show") if showing => return,
            Some("hide") if !showing => return,
            _ => {}
        }
        if showing {
            self.hide_popover_from(cx, popover, node);
        } else if self
            .check_popover_validity(popover, false, false, None)
            .unwrap_or(false)
        {
            let _ = self.show_popover_steps(cx, popover, false, Some(node));
        }
    }

    /// `showPopover(options)`.
    pub(crate) fn show_popover_method(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ShowPopoverOptions,
    ) -> rt::Result<()> {
        let invoker = options.source.map(|s| self.n(s));
        self.show_popover_steps(cx, self.n(this), true, invoker)
    }

    /// `togglePopover(options)`.
    pub(crate) fn toggle_popover_method(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: TogglePopoverOptionsOrBoolean,
    ) -> rt::Result<bool> {
        let el = self.n(this);
        let (force, invoker) = match options {
            TogglePopoverOptionsOrBoolean::Boolean(b) => (Some(b), None),
            TogglePopoverOptionsOrBoolean::TogglePopoverOptions(o) => {
                (o.force, o.source.map(|s| self.n(s)))
            }
        };
        if self.popover_showing(el) && force != Some(true) {
            self.hide_popover_steps(cx, el, true, true, true, None)?;
        } else if force != Some(false) {
            self.show_popover_steps(cx, el, true, invoker)?;
        } else {
            let showing = self.popover_showing(el);
            self.check_popover_validity(el, showing, true, None)?;
        }
        Ok(self.popover_showing(el))
    }
}
