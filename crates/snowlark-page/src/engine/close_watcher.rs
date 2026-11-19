// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §6.10 close requests and close watchers: each window's close watcher manager (its groups,
//! the allowed number of groups, and whether the next user activation allows a new group), the
//! close watchers of open dialogs and of showing auto and hint popovers, "request to close" (with
//! the history-action activation that lets a `cancel` listener prevent the close), "close",
//! "destroy", and "process close watchers" for a close request (Esc).
//!
//! A close watcher here is named by what established it: a dialog or a popover (the `CloseWatcher`
//! interface is not exposed). Each window is keyed by its associated document.

use snowlark_bindings::rt::Cx;
use snowlark_dom::NodeId;

use super::Engine;

/// A close watcher, by the element whose close watcher it is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CloseWatcher {
    /// A dialog's close watcher ("set the dialog close watcher").
    Dialog(NodeId),
    /// An auto or hint popover's "popover close watcher".
    Popover(NodeId),
}

impl CloseWatcher {
    fn element(self) -> NodeId {
        match self {
            CloseWatcher::Dialog(e) | CloseWatcher::Popover(e) => e,
        }
    }
}

/// A window's "close watcher manager".
pub struct CloseWatcherManager {
    /// "groups": lists of close watchers, oldest first.
    pub groups: Vec<Vec<CloseWatcher>>,
    /// "allowed number of groups".
    pub allowed_groups: usize,
    /// "next user interaction allows a new group".
    pub next_allows_new_group: bool,
}

impl Default for CloseWatcherManager {
    fn default() -> Self {
        CloseWatcherManager {
            groups: Vec::new(),
            allowed_groups: 1,
            next_allows_new_group: true,
        }
    }
}

/// Every window's close watcher manager, and what the close watchers keep.
#[derive(Default)]
pub struct CloseWatcherState {
    /// Each window's manager, by its associated document.
    pub managers: rustc_hash::FxHashMap<NodeId, CloseWatcherManager>,
    /// Close watchers whose "is running cancel action" is true.
    pub running_cancel: rustc_hash::FxHashSet<CloseWatcher>,
    /// The windows (by associated document) with history-action activation: the last activation
    /// timestamp is not the last history-action activation timestamp.
    pub history_activated: rustc_hash::FxHashSet<NodeId>,
}

impl Engine {
    /// "Establish a close watcher" in the window of `doc`.
    pub(crate) fn establish_close_watcher(&self, doc: NodeId, w: CloseWatcher) {
        let mut s = self.close_watchers.borrow_mut();
        let m = s.managers.entry(doc).or_default();
        for g in &mut m.groups {
            g.retain(|&x| x != w);
        }
        m.groups.retain(|g| !g.is_empty());
        if m.groups.len() < m.allowed_groups {
            m.groups.push(vec![w]);
        } else if let Some(last) = m.groups.last_mut() {
            last.push(w);
        } else {
            m.groups.push(vec![w]);
        }
        m.next_allows_new_group = true;
    }

    /// "Destroy" a close watcher: it leaves every group of its window, and empty groups go.
    pub(crate) fn destroy_close_watcher(&self, w: CloseWatcher) {
        let mut s = self.close_watchers.borrow_mut();
        for m in s.managers.values_mut() {
            for g in &mut m.groups {
                g.retain(|&x| x != w);
            }
            m.groups.retain(|g| !g.is_empty());
        }
    }

    /// "Is active": a group of its window holds it. Returns that window's document.
    fn close_watcher_window(&self, w: CloseWatcher) -> Option<NodeId> {
        let s = self.close_watchers.borrow();
        s.managers
            .iter()
            .find(|(_, m)| m.groups.iter().any(|g| g.contains(&w)))
            .map(|(&d, _)| d)
    }

    /// The close watcher's "get enabled state".
    fn close_watcher_enabled(&self, w: CloseWatcher) -> bool {
        match w {
            CloseWatcher::Dialog(d) => self.dialog_close_watcher_enabled(d),
            CloseWatcher::Popover(_) => true,
        }
    }

    /// HTML §6.4.3 "history-action activation" of the window of `doc`: an activation since the
    /// last time it was consumed.
    fn has_history_action_activation(&self, doc: NodeId) -> bool {
        self.close_watchers
            .borrow()
            .history_activated
            .contains(&doc)
    }

    /// HTML §6.4.3 "consume history-action user activation": no window of the top-level
    /// traversable's navigables has history-action activation any more.
    fn consume_history_action_activation(&self, doc: NodeId) {
        let top = self.top_level_document_of(doc);
        let mut docs = vec![top];
        docs.extend(self.descendant_documents(top));
        let mut s = self.close_watchers.borrow_mut();
        for d in docs {
            s.history_activated.remove(&d);
        }
    }

    /// An activation notification in the window of `doc`: it has history-action activation, and
    /// its close watcher manager is notified ("notify the close watcher manager about user
    /// activation").
    pub(crate) fn close_watchers_note_activation(&self, doc: NodeId) {
        let mut s = self.close_watchers.borrow_mut();
        s.history_activated.insert(doc);
        let m = s.managers.entry(doc).or_default();
        if m.next_allows_new_group {
            m.allowed_groups += 1;
        }
        m.next_allows_new_group = false;
    }
    /// "Request to close" a close watcher. Returns false when its cancel action prevented it.
    pub(crate) fn request_close_watcher(
        &self,
        cx: &mut Cx,
        w: CloseWatcher,
        require_history_action_activation: bool,
    ) -> bool {
        let Some(doc) = self.close_watcher_window(w) else {
            return true;
        };
        if !self.close_watcher_enabled(w) {
            return true;
        }
        if self.close_watchers.borrow().running_cancel.contains(&w) {
            return true;
        }
        if !self.is_fully_active(doc) {
            return true;
        }
        let can_prevent_close = !require_history_action_activation || {
            let room = {
                let s = self.close_watchers.borrow();
                s.managers
                    .get(&doc)
                    .is_some_and(|m| m.groups.len() < m.allowed_groups)
            };
            room && self.has_history_action_activation(doc)
        };
        self.close_watchers.borrow_mut().running_cancel.insert(w);
        let should_continue = match w {
            CloseWatcher::Dialog(d) => self.dialog_cancel_action(cx, d, can_prevent_close),
            CloseWatcher::Popover(_) => true,
        };
        self.close_watchers.borrow_mut().running_cancel.remove(&w);
        if !should_continue {
            // (a listener that cancels `cancel` without the right to only got a non-cancelable one)
            if can_prevent_close {
                self.consume_history_action_activation(doc);
            }
            return false;
        }
        self.close_close_watcher(cx, w);
        true
    }

    /// "Close" a close watcher: destroyed, then its close action runs.
    pub(crate) fn close_close_watcher(&self, cx: &mut Cx, w: CloseWatcher) {
        let Some(doc) = self.close_watcher_window(w) else {
            return;
        };
        if !self.close_watcher_enabled(w) || !self.is_fully_active(doc) {
            return;
        }
        self.destroy_close_watcher(w);
        if !self.dom().is_alive(w.element()) {
            return;
        }
        match w {
            CloseWatcher::Dialog(d) => self.dialog_close_action(cx, d),
            CloseWatcher::Popover(p) => {
                let _ = self.hide_popover_steps(cx, p, true, true, false, None);
            }
        }
    }

    /// "Process close watchers" for a close request in the window of `doc`: the last group's
    /// close watchers, newest first, until one's cancel action prevents the close. Returns whether
    /// an enabled close watcher was processed.
    pub(crate) fn process_close_watchers(&self, cx: &mut Cx, doc: NodeId) -> bool {
        let mut processed = false;
        let group = {
            let s = self.close_watchers.borrow();
            s.managers.get(&doc).and_then(|m| m.groups.last().cloned())
        };
        if let Some(group) = group {
            for w in group.into_iter().rev() {
                if self.close_watcher_enabled(w) {
                    processed = true;
                }
                if !self.request_close_watcher(cx, w, true) {
                    break;
                }
            }
        }
        let mut s = self.close_watchers.borrow_mut();
        if let Some(m) = s.managers.get_mut(&doc) {
            if m.allowed_groups > 1 {
                m.allowed_groups -= 1;
            }
        }
        processed
    }

    /// The window of `doc` is gone: its close watcher manager too.
    pub(crate) fn close_watchers_forget(&self, docs: &[NodeId]) {
        let mut s = self.close_watchers.borrow_mut();
        s.managers.retain(|d, _| !docs.contains(d));
        s.history_activated.retain(|d| !docs.contains(d));
    }
}
