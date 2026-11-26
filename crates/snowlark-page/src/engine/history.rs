// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §7.4.1 session history and §7.2.4 the `History` interface. Each navigable has its session
//! history entries, each with a step; a top-level traversable has a current step, and every
//! navigable under it shows its entry with the greatest step not after it (the joint session
//! history). `pushState`/`replaceState` and fragment navigations add or change an entry of the
//! document's navigable (§7.4.4 "URL and history update steps"); a new document adds one, or
//! takes the current one ("replace", a reload, a traversal); `history.go()` moves the current step,
//! and each navigable whose entry changes traverses: within its document (`popstate`, and
//! `hashchange` when only the fragment differs), or by loading the entry's URL again (no document
//! is kept for later).
//!
//! With navigation in the browser ([`Engine::set_browser_navigation`]) the top-level document's
//! entries before it are the browser's: its own `history.go()` is the browser's to resolve
//! ([`Engine::take_traversals`]), and the browser names the document's entry to go to
//! ([`Engine::go_in_document`]), as Chromium's renderer sends `GoToEntryAtOffset` and the browser's
//! `NavigationControllerImpl` answers with a same-document history navigation.

use snowlark_bindings::generated::{HistoryImpl, ScrollRestoration};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsValue};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;

use super::events::EventExt;
use super::frames::{ContextId, Frames, HistoryHandling, NavigationRequest, TOP};
use super::held::Held;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// HTML §7.4.1.1 a session history entry.
pub struct Entry {
    pub step: u64,
    pub url: String,
    /// The document the entry was last shown with: a traversal to the entry while its navigable
    /// shows another one loads `url` again.
    pub document: NodeId,
    /// "classic history API state", serialized.
    pub state: Option<Vec<u8>>,
    pub scroll_restoration: ScrollRestoration,
}

/// A navigable's session history entries (in step order) and its active one.
pub struct NavigableHistory {
    pub entries: Vec<Entry>,
    pub current: usize,
}

#[derive(Default)]
pub struct HistoryState {
    pub navigables: rustc_hash::FxHashMap<ContextId, NavigableHistory>,
    /// Each top-level traversable's current session history step.
    pub steps: rustc_hash::FxHashMap<ContextId, u64>,
    /// Each document's History object's state, deserialized: what `history.state` returns (absent:
    /// not deserialized yet, or null).
    pub states: rustc_hash::FxHashMap<NodeId, Held<v8::Value>>,
    /// The navigables loading an entry's document for a traversal.
    pub traversing: Vec<ContextId>,
    /// Traversals asked for while their traversable was traversing (its traversable, the
    /// document that asked, the delta): they run once it is done, the session history traversal
    /// queue being one at a time.
    pub deferred: Vec<(ContextId, NodeId, i64)>,
}

/// The top-level traversable of a navigable.
fn top_of(f: &Frames, mut id: ContextId) -> ContextId {
    while let Some(p) = f.list[id].parent {
        id = p;
    }
    id
}

/// HTML §7.4.4 "can have its URL rewritten".
fn can_rewrite(document: &url::Url, target: &url::Url) -> bool {
    if document.scheme() != target.scheme()
        || document.username() != target.username()
        || document.password() != target.password()
        || document.host() != target.host()
        || document.port() != target.port()
    {
        return false;
    }
    if matches!(target.scheme(), "http" | "https") {
        return true;
    }
    if target.scheme() == "file" {
        return document.path() == target.path();
    }
    document.path() == target.path() && document.query() == target.query()
}

impl Engine {
    /// HTML §7.2.5: each of History's members first throws a "SecurityError" when its relevant
    /// global object's associated Document is not fully active.
    fn history_check_fully_active(&self, history: Handle) -> rt::Result<()> {
        let window = match &*self.obj(history) {
            Object::History { window } => *window,
            _ => return Ok(()),
        };
        let active = self
            .associated_document(window)
            .filter(|&d| self.dom().is_alive(d) && self.is_fully_active(d));
        if active.is_none() {
            return Err(exception(
                ExceptionName::SecurityError,
                "The document is not fully active",
            ));
        }
        Ok(())
    }

    fn history_document(&self, history: Handle) -> Option<NodeId> {
        let window = match &*self.obj(history) {
            Object::History { window } => *window,
            _ => return None,
        };
        self.document_of_window(window)
    }

    /// The top-level traversable of a navigable.
    pub(crate) fn traversable_of(&self, id: ContextId) -> ContextId {
        top_of(&self.frames.borrow(), id)
    }

    /// Runs `f` on `id`'s session history, made first if it has none: one entry at step 0, for
    /// the document the navigable shows.
    fn with_nav<R>(&self, id: ContextId, f: impl FnOnce(&mut NavigableHistory) -> R) -> R {
        let doc = self.frames.borrow().list[id].document;
        let url = {
            let dom = self.dom();
            if dom.is_alive(doc) {
                dom.document_url(doc).to_owned()
            } else {
                "about:blank".to_owned()
            }
        };
        let mut st = self.history.borrow_mut();
        let h = st.navigables.entry(id).or_insert_with(|| NavigableHistory {
            entries: vec![Entry {
                step: 0,
                url,
                document: doc,
                state: None,
                scroll_restoration: ScrollRestoration::Auto,
            }],
            current: 0,
        });
        f(h)
    }

    /// Makes the session history of `doc`'s navigable if it has none yet. Called before the
    /// document's URL changes, so its first entry keeps the URL the document was loaded at.
    pub(crate) fn ensure_doc_history(&self, doc: NodeId) {
        if let Some(id) = self.context_of_document(doc) {
            self.with_nav(id, |_| ());
        }
    }

    /// The navigables of `trav` that have session history entries here.
    fn navigables_of(&self, trav: ContextId) -> Vec<ContextId> {
        let f = self.frames.borrow();
        let mut ids: Vec<ContextId> = self
            .history
            .borrow()
            .navigables
            .keys()
            .copied()
            .filter(|&id| !f.list[id].discarded && top_of(&f, id) == trav)
            .collect();
        // (a parent is made before its children: tree order for what matters here)
        ids.sort_unstable();
        ids
    }

    /// HTML "get all used history steps" of a traversable, in order.
    fn used_steps(&self, trav: ContextId) -> Vec<u64> {
        let ids = self.navigables_of(trav);
        let st = self.history.borrow();
        let mut steps: Vec<u64> = ids
            .iter()
            .flat_map(|id| st.navigables[id].entries.iter().map(|e| e.step))
            .collect();
        // (a traversable with no entries here yet has its initial one)
        if !st.navigables.contains_key(&trav) {
            steps.push(0);
        }
        steps.sort_unstable();
        steps.dedup();
        steps
    }

    fn current_step(&self, trav: ContextId) -> u64 {
        self.history.borrow().steps.get(&trav).copied().unwrap_or(0)
    }

    /// Adds `entry` (its step set here) to `id`'s entries at the traversable's next step, which
    /// becomes current; the entries after the current step go, every navigable's ("clear the
    /// forward session history"). Returns the new entry's index.
    fn history_push(&self, id: ContextId, mut entry: Entry) -> usize {
        let trav = self.traversable_of(id);
        self.with_nav(id, |_| ());
        let current = self.current_step(trav);
        let members = self.navigables_of(trav);
        let mut st = self.history.borrow_mut();
        for m in members {
            if let Some(h) = st.navigables.get_mut(&m) {
                // (each navigable's active entry is at or before the current step)
                h.entries.retain(|e| e.step <= current);
                h.current = h.current.min(h.entries.len().saturating_sub(1));
            }
        }
        entry.step = current + 1;
        st.steps.insert(trav, current + 1);
        let h = st.navigables.get_mut(&id).expect("made above");
        h.entries.push(entry);
        h.current = h.entries.len() - 1;
        h.current
    }

    /// A new document becomes active in navigable `id`: its session history entry, by the
    /// history handling of the navigation that brought it.
    pub(crate) fn history_commit(&self, id: ContextId, doc: NodeId) {
        let (handling, initial) = {
            let mut f = self.frames.borrow_mut();
            let b = &mut f.list[id];
            (
                std::mem::take(&mut b.history_handling),
                b.initial_about_blank,
            )
        };
        self.traversal_load_ended(id);
        let url = self.dom().document_url(doc).to_owned();
        let fresh = |step| Entry {
            step,
            url: url.clone(),
            document: doc,
            state: None,
            scroll_restoration: ScrollRestoration::Auto,
        };
        // (the browser holds the top-level document's entries before this one; an embedder's own
        // navigation starts a session history of its own)
        if (id == TOP && self.browser_navigates()) || handling == HistoryHandling::New {
            let mut st = self.history.borrow_mut();
            st.navigables.insert(
                id,
                NavigableHistory {
                    entries: vec![fresh(0)],
                    current: 0,
                },
            );
            st.steps.insert(id, 0);
            return;
        }
        match handling {
            HistoryHandling::Auto if !initial => {
                self.history_push(id, fresh(0));
            }
            HistoryHandling::Push => {
                self.history_push(id, fresh(0));
            }
            HistoryHandling::Auto | HistoryHandling::Replace | HistoryHandling::New => self
                .with_nav(id, |h| {
                    let step = h.entries[h.current].step;
                    h.entries[h.current] = fresh(step);
                }),
            // (the entry stays, its state too; a redirect may have changed the URL)
            HistoryHandling::Reload | HistoryHandling::Traverse => self.with_nav(id, |h| {
                let e = &mut h.entries[h.current];
                e.url = url.clone();
                e.document = doc;
            }),
        }
    }

    /// Navigables destroyed: their session history entries go.
    pub(crate) fn history_navigables_destroyed(&self, ids: &[ContextId]) {
        {
            let mut st = self.history.borrow_mut();
            for id in ids {
                st.navigables.remove(id);
                st.steps.remove(id);
            }
        }
        for &id in ids {
            self.traversal_load_ended(id);
        }
    }

    /// The document's navigable's active entry gets `url` and `state` (HTML §7.4.4 "URL and
    /// history update steps"), a new one after it with `push`. Returns the active entry's index.
    fn same_document_entry(
        &self,
        doc: NodeId,
        url: &str,
        state: Option<Vec<u8>>,
        push: bool,
    ) -> usize {
        let Some(id) = self.context_of_document(doc) else {
            return 0;
        };
        if push {
            let scroll_restoration = self.with_nav(id, |h| h.entries[h.current].scroll_restoration);
            self.history_push(
                id,
                Entry {
                    step: 0,
                    url: url.to_owned(),
                    document: doc,
                    state,
                    scroll_restoration,
                },
            )
        } else {
            self.with_nav(id, |h| {
                let e = &mut h.entries[h.current];
                e.url = url.to_owned();
                e.document = doc;
                e.state = state;
                h.current
            })
        }
    }

    /// `pushState`/`replaceState` (HTML §7.2.4 "shared history push/replace state steps").
    fn push_or_replace(
        &self,
        cx: &mut Cx,
        history: Handle,
        data: JsValue,
        url: Option<&str>,
        push: bool,
    ) -> rt::Result<()> {
        self.history_check_fully_active(history)?;
        let Some(doc) = self.history_document(history) else {
            return Ok(());
        };
        if !self.is_active_document(doc) {
            return Err(exception(
                ExceptionName::SecurityError,
                "The document is not fully active",
            ));
        }
        // 3. StructuredSerializeForStorage.
        let (bytes, _) =
            self.serializing_for_storage(|| self.serialize_with_ports(cx, &data, &[]))?;
        let current = url::Url::parse(self.dom().document_url(doc)).ok();
        // 4: a URL that is null or the empty string keeps the document's (its fragment too),
        // whatever the base URL.
        let new_url = match url.filter(|u| !u.is_empty()) {
            Some(u) => {
                let parsed = self.encoding_parse_url(doc, u).ok_or_else(|| {
                    exception(ExceptionName::SecurityError, "The URL does not parse")
                })?;
                if let Some(cur) = &current {
                    if !can_rewrite(cur, &parsed) {
                        return Err(exception(
                            ExceptionName::SecurityError,
                            "The URL cannot replace the document's",
                        ));
                    }
                }
                parsed.to_string()
            }
            None => self.dom().document_url(doc).to_owned(),
        };
        self.ensure_doc_history(doc);
        self.dom_mut().set_document_url(doc, &new_url);
        let index = self.same_document_entry(doc, &new_url, Some(bytes.clone()), push);
        // "URL and history update steps": the History object's state is the serialized data,
        // deserialized in the document's realm.
        let state = self.in_document_realm(cx, doc, |cx| self.deserialize_value(cx, &bytes).ok());
        {
            let mut st = self.history.borrow_mut();
            match state {
                Some(v) => st.states.insert(doc, Held::value(v)),
                None => st.states.remove(&doc),
            };
        }
        self.note_same_document(doc, &new_url, push, index);
        Ok(())
    }

    /// HTML §8.4.1 "document open steps" step 12: the "URL and history update steps" with `url` and
    /// no serialized data. The document and its navigable's active entry get the URL; the entry
    /// keeps its state.
    pub(crate) fn document_open_url(&self, doc: NodeId, url: &str) {
        self.ensure_doc_history(doc);
        self.dom_mut().set_document_url(doc, url);
        let Some(id) = self.context_of_document(doc) else {
            return;
        };
        let index = self.with_nav(id, |h| {
            let e = &mut h.entries[h.current];
            e.url = url.to_owned();
            e.document = doc;
            h.current
        });
        self.note_same_document(doc, url, false, index);
    }

    /// A fragment navigation's session history entry (HTML §7.4.2.3.3 "navigate to a fragment"):
    /// a new one with no state, or with `replace` the active one becomes it.
    pub(crate) fn fragment_history_entry(&self, doc: NodeId, url: &str, replace: bool) {
        let index = self.same_document_entry(doc, url, None, !replace);
        self.history.borrow_mut().states.remove(&doc);
        self.note_same_document(doc, url, !replace, index);
    }

    /// The top-level document's own session history entry `index` becomes current (the browser
    /// traversed to it): `popstate`, and `hashchange` when only the fragment differs, in a task
    /// (HTML §7.4.6.4). An index the document does not have does nothing (the browser then loads
    /// the entry's URL).
    pub fn go_in_document(&self, index: usize) {
        self.queue_task(TaskSource::HistoryTraversal, move |e, cx| {
            let step = e
                .history
                .borrow()
                .navigables
                .get(&TOP)
                .and_then(|h| h.entries.get(index))
                .map(|en| en.step);
            if let Some(step) = step {
                e.apply_history_step(cx, TOP, step, None);
            }
        });
    }

    /// `history.go(delta)` (HTML §7.2.4): a reload for 0, else a traversal of the traversable by
    /// `delta` steps, queued.
    fn traverse(&self, history: Handle, delta: i32) {
        let Some(doc) = self.history_document(history) else {
            return;
        };
        if delta == 0 {
            // A reload of the document.
            if let Ok(u) = url::Url::parse(self.dom().document_url(doc)) {
                if self.is_rendered_document(doc) {
                    if let Some(id) = self.context_of_document(doc) {
                        self.navigate_request(
                            id,
                            NavigationRequest {
                                url: u,
                                post: None,
                                source: Some(doc),
                                form: false,
                                reload: true,
                                referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                                history: HistoryHandling::Reload,
                            },
                        );
                    }
                }
            }
            return;
        }
        // With navigation in the browser, the top-level document's traversals are the browser's
        // to resolve (it holds the entries before this document).
        if self.browser_navigates() && self.document() == Some(doc) {
            self.embedder.borrow_mut().traversals.push(delta as isize);
            return;
        }
        self.queue_history_traversal(doc, delta as i64);
    }

    /// Queues `doc`'s traversal by `delta`. It waits while its traversable loads documents for an
    /// earlier traversal, and does nothing if `doc` is no longer active by then (so of several
    /// traversals asked for at once, the first that moves wins).
    fn queue_history_traversal(&self, doc: NodeId, delta: i64) {
        self.queue_task(TaskSource::HistoryTraversal, move |e, cx| {
            if !e.is_active_document(doc) {
                return;
            }
            let Some(id) = e.context_of_document(doc) else {
                return;
            };
            let trav = e.traversable_of(id);
            let busy = {
                let f = e.frames.borrow();
                e.history
                    .borrow()
                    .traversing
                    .iter()
                    .any(|&t| !f.list[t].discarded && top_of(&f, t) == trav)
            };
            if busy {
                e.history.borrow_mut().deferred.push((trav, doc, delta));
                return;
            }
            e.traverse_by_delta(cx, trav, delta, doc);
        });
    }

    /// A navigable's traversal load ended (its document came, or the navigable went): the
    /// traversals that waited for its traversable run, in order, if it has no other.
    fn traversal_load_ended(&self, id: ContextId) {
        let ready = {
            let mut st = self.history.borrow_mut();
            let Some(i) = st.traversing.iter().position(|&t| t == id) else {
                return;
            };
            st.traversing.remove(i);
            let f = self.frames.borrow();
            let trav = top_of(&f, id);
            if st
                .traversing
                .iter()
                .any(|&t| !f.list[t].discarded && top_of(&f, t) == trav)
            {
                return;
            }
            let (ready, rest): (Vec<_>, Vec<_>) =
                st.deferred.drain(..).partition(|&(t, _, _)| t == trav);
            st.deferred = rest;
            ready
        };
        for (_, doc, delta) in ready {
            self.queue_history_traversal(doc, delta);
        }
    }

    /// HTML "traverse the history by a delta": the used step `delta` away from the current one,
    /// if there is one (with navigation in the browser, a traversal past the top-level
    /// document's entries is the browser's, [`Engine::take_traversals`]).
    fn traverse_by_delta(&self, cx: &mut Cx, trav: ContextId, delta: i64, source: NodeId) {
        let steps = self.used_steps(trav);
        let current = self.current_step(trav);
        let index = steps.iter().filter(|&&s| s <= current).count() as i64 - 1;
        let target = index + delta;
        if target < 0 || target >= steps.len() as i64 {
            if trav == TOP && self.browser_navigates() {
                let beyond = if target < 0 {
                    target
                } else {
                    target - (steps.len() as i64 - 1)
                };
                self.embedder.borrow_mut().traversals.push(beyond as isize);
            }
            return;
        }
        self.apply_history_step(cx, trav, steps[target as usize], Some(source));
    }

    /// HTML "apply the history step": every navigable of `trav` takes its entry with the greatest
    /// step not after `step`. One whose entry's document is the one it shows traverses within it;
    /// any other loads the entry's URL (its children go with its document).
    fn apply_history_step(
        &self,
        cx: &mut Cx,
        trav: ContextId,
        step: u64,
        initiator: Option<NodeId>,
    ) {
        // ("initiator-disallowed": a document not allowed by sandboxing to navigate every
        // navigable whose entry would change traverses nothing)
        if let Some(source) = initiator {
            for id in self.navigables_of(trav) {
                let changes = self.history.borrow().navigables.get(&id).is_some_and(|h| {
                    h.entries
                        .iter()
                        .rposition(|e| e.step <= step)
                        .is_some_and(|t| t != h.current)
                });
                if changes && !self.allowed_by_sandboxing_to_navigate(source, id) {
                    return;
                }
            }
        }
        self.history.borrow_mut().steps.insert(trav, step);
        let mut loading: Vec<ContextId> = Vec::new();
        let mut within: Vec<(NodeId, String, String)> = Vec::new();
        for id in self.navigables_of(trav) {
            let under_loading = {
                let f = self.frames.borrow();
                let mut p = f.list[id].parent;
                let mut hit = false;
                while let Some(c) = p {
                    if loading.contains(&c) {
                        hit = true;
                        break;
                    }
                    p = f.list[c].parent;
                }
                hit
            };
            if under_loading {
                continue;
            }
            let change = self.with_nav(id, |h| {
                let target = h.entries.iter().rposition(|e| e.step <= step)?;
                if target == h.current {
                    return None;
                }
                let from = h.entries[h.current].url.clone();
                h.current = target;
                let e = &h.entries[target];
                Some((from, e.url.clone(), e.document))
            });
            let Some((from, to, entry_doc)) = change else {
                continue;
            };
            let active = self.frames.borrow().list[id].document;
            if entry_doc == active && self.dom().is_alive(active) {
                within.push((active, from, to));
                continue;
            }
            loading.push(id);
            if let Ok(url) = url::Url::parse(&to) {
                self.history.borrow_mut().traversing.push(id);
                self.navigate_request(
                    id,
                    NavigationRequest {
                        url,
                        post: None,
                        source: None,
                        form: false,
                        reload: false,
                        referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                        history: HistoryHandling::Traverse,
                    },
                );
            }
        }
        for (doc, from, to) in within {
            self.traverse_within_document(cx, doc, &from, &to);
        }
    }

    /// HTML "update document for history step application" for a document that stays: its URL
    /// and History state become the entry's, `popstate` fires, and `hashchange` is queued when
    /// only the fragment changed.
    fn traverse_within_document(&self, cx: &mut Cx, doc: NodeId, from: &str, to: &str) {
        self.dom_mut().set_document_url(doc, to);
        self.history.borrow_mut().states.remove(&doc);
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        let state = self.history_state_value(cx, doc);
        let ev = self.trusted_event(
            InterfaceId::PopStateEvent,
            "popstate",
            false,
            false,
            EventExt::PopState(state.map(Held::value)),
        );
        self.dispatch(cx, ev, window, false);
        let (Ok(a), Ok(b)) = (url::Url::parse(from), url::Url::parse(to)) else {
            return;
        };
        let (mut a2, mut b2) = (a.clone(), b.clone());
        a2.set_fragment(None);
        b2.set_fragment(None);
        if a2 == b2 && a.fragment() != b.fragment() {
            let (old_url, new_url) = (from.to_owned(), to.to_owned());
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                if !e.is_active_document(doc) {
                    return;
                }
                let ev = e.trusted_event(
                    InterfaceId::HashChangeEvent,
                    "hashchange",
                    false,
                    false,
                    EventExt::HashChange { old_url, new_url },
                );
                e.dispatch(cx, ev, window, false);
            });
        }
    }

    /// The History object's state of `doc`: deserialized from its navigable's active entry in the
    /// document's realm the first time it is asked for, the same value after that.
    fn history_state_value(&self, cx: &mut Cx, doc: NodeId) -> Option<JsValue> {
        let held = self.history.borrow().states.get(&doc).cloned();
        if let Some(h) = held {
            return h.get(cx.scope());
        }
        let id = self.context_of_document(doc)?;
        let bytes = self.history.borrow().navigables.get(&id).and_then(|h| {
            let e = &h.entries[h.current];
            (e.document == doc).then(|| e.state.clone()).flatten()
        })?;
        let value =
            self.in_document_realm(cx, doc, |cx| self.deserialize_value(cx, &bytes).ok())?;
        self.history
            .borrow_mut()
            .states
            .insert(doc, Held::value(value.clone()));
        Some(value)
    }

    /// The embedder's part of a traversal past this document's entries (steps, negative is back).
    pub fn take_history_traversal(&self) -> Option<isize> {
        let mut t = self.take_traversals();
        (!t.is_empty()).then(|| t.remove(0))
    }

    /// `window.history`.
    pub(crate) fn history_handle(&self, window: Handle) -> Handle {
        self.window_singleton(window, "history", || Object::History { window })
    }

    /// The active entry of `doc`'s navigable, if it is `doc`'s.
    fn with_active_entry<R>(&self, doc: NodeId, f: impl FnOnce(&mut Entry) -> R) -> Option<R> {
        let id = self.context_of_document(doc)?;
        Some(self.with_nav(id, |h| f(&mut h.entries[h.current])))
    }
}

impl HistoryImpl for Engine {
    fn length(&self, _cx: &mut Cx, this: Handle) -> rt::Result<u32> {
        self.history_check_fully_active(this)?;
        Ok(
            match self
                .history_document(this)
                .and_then(|doc| self.context_of_document(doc))
            {
                Some(id) => self.used_steps(self.traversable_of(id)).len() as u32,
                None => 0,
            },
        )
    }
    fn scroll_restoration(&self, _cx: &mut Cx, this: Handle) -> rt::Result<ScrollRestoration> {
        self.history_check_fully_active(this)?;
        Ok(self
            .history_document(this)
            .and_then(|doc| self.with_active_entry(doc, |e| e.scroll_restoration))
            .unwrap_or(ScrollRestoration::Auto))
    }
    fn set_scroll_restoration(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: ScrollRestoration,
    ) -> rt::Result<()> {
        self.history_check_fully_active(this)?;
        if let Some(doc) = self.history_document(this) {
            self.with_active_entry(doc, |e| e.scroll_restoration = value);
        }
        Ok(())
    }
    fn state(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        self.history_check_fully_active(this)?;
        let state = self
            .history_document(this)
            .and_then(|doc| self.history_state_value(cx, doc));
        Ok(state.unwrap_or_else(|| rt::null_value(cx.scope())))
    }
    fn go(&self, _cx: &mut Cx, this: Handle, delta: i32) -> rt::Result<()> {
        self.history_check_fully_active(this)?;
        self.traverse(this, delta);
        Ok(())
    }
    fn back(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.history_check_fully_active(this)?;
        self.traverse(this, -1);
        Ok(())
    }
    fn forward(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.history_check_fully_active(this)?;
        self.traverse(this, 1);
        Ok(())
    }
    fn push_state(
        &self,
        cx: &mut Cx,
        this: Handle,
        data: JsValue,
        _unused: &str,
        url: Option<&str>,
    ) -> rt::Result<()> {
        self.push_or_replace(cx, this, data, url, true)
    }
    fn replace_state(
        &self,
        cx: &mut Cx,
        this: Handle,
        data: JsValue,
        _unused: &str,
        url: Option<&str>,
    ) -> rt::Result<()> {
        self.push_or_replace(cx, this, data, url, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_rewriting() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert!(can_rewrite(
            &u("https://a.test/x"),
            &u("https://a.test/y?z#w")
        ));
        assert!(!can_rewrite(&u("https://a.test/x"), &u("https://b.test/x")));
        assert!(!can_rewrite(&u("https://a.test/x"), &u("http://a.test/x")));
        assert!(can_rewrite(
            &u("data:text/html,x"),
            &u("data:text/html,x#f")
        ));
        assert!(!can_rewrite(&u("data:text/html,x"), &u("data:text/html,y")));
    }
}
