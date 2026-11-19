// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IndexedDB §2.7 transactions and §2.8 requests: placing requests (§5.6), running them in order,
//! firing `success` and `error` with the transaction active (§5.9, §5.10), committing (§5.4) and
//! aborting (§5.5, §5.8), auto-commit, the cleanup at the end of each microtask checkpoint, and
//! the `IDBTransaction` and `IDBRequest` interfaces.

use std::collections::VecDeque;

use snowlark_bindings::generated::{
    IDBObjectStoreOrIDBIndexOrIDBCursor, IDBRequestImpl, IDBRequestReadyState,
    IDBTransactionDurability, IDBTransactionImpl, IDBTransactionMode,
};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsValue};
use snowlark_bindings::InterfaceId;
use snowlark_idb::{Answer, Change, Durability, Failure, Key, Mode, Request};

use super::{
    applied, write_failed, IdbObject, Op, Pending, Sent, SentState, Step, StoreHandle, Then,
    TxnData, TxnState,
};
use crate::engine::events::EventExt;
use crate::engine::{exception, Engine};
use crate::event_loop::TaskSource;

/// How many requests one task sends at most.
const SEND_BATCH: usize = 64;

/// The DOMException name of a backend failure.
pub(crate) fn failure_name(f: Failure) -> ExceptionName {
    match f {
        Failure::Version => ExceptionName::VersionError,
        Failure::Abort => ExceptionName::AbortError,
        Failure::Quota => ExceptionName::QuotaExceededError,
        Failure::Unknown => ExceptionName::UnknownError,
    }
}

pub(crate) fn inactive() -> rt::Error {
    exception(
        ExceptionName::TransactionInactiveError,
        "The transaction is not active",
    )
}

impl Engine {
    /// A new transaction object, registered as live.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_transaction(
        &self,
        global: Handle,
        conn: Handle,
        id: u64,
        mode: Mode,
        durability: Durability,
        scope: Vec<(u64, String)>,
        state: TxnState,
        started: bool,
    ) -> Handle {
        let h = self.idb_alloc(IdbObject::Transaction(TxnData {
            global,
            id,
            conn,
            mode,
            durability,
            scope,
            state,
            started,
            cleanup: false,
            requests: VecDeque::new(),
            sent: VecDeque::new(),
            seq: 0,
            held: false,
            sending: false,
            processing: false,
            writing: false,
            error: None,
            handles: Vec::new(),
            aborting: Vec::new(),
            schema_at_start: None,
            open_request: None,
            old_version: 0,
        }));
        self.idb.borrow_mut().transactions.insert(id, h);
        self.conn(conn, |c| c.live.push(h));
        h
    }

    pub(crate) fn txn_state(&self, t: Handle) -> TxnState {
        self.txn(t, |t| t.state)
    }

    /// Applies a change in a transaction: to the connection's schema copy now, and to the database
    /// in order with the transaction's requests, with its undo entry kept.
    pub(crate) fn txn_apply(&self, t: Handle, change: Change) {
        let (id, conn) = self.txn(t, |t| (t.id, t.conn));
        self.conn(conn, |c| c.schema.apply_schema(&change));
        let (backend, _) = self.idb_backend();
        backend.change(id, change);
    }

    /// §6.1 "store a record into an object store", its last steps: stores `change` (a `Put`) in
    /// a transaction; `add` fails over a record with the same key. The result is `key`.
    pub(crate) fn txn_put(&self, change: Change, add: bool, key: Key) -> Step {
        Step::finish(Request::Apply { change, add }, move |e, cx, a| {
            applied(a).map_err(write_failed)?;
            Ok(e.key_to_js(cx, &key))
        })
    }

    /// §5.6 "asynchronously execute a request": adds the request (a new one with `source`, or
    /// `request`) to the transaction's list, to run when it is first.
    pub(crate) fn place_request(
        &self,
        source: Handle,
        t: Handle,
        op: Op,
        request: Option<Handle>,
    ) -> Handle {
        let global = self.txn(t, |t| t.global);
        let request = request.unwrap_or_else(|| {
            self.idb_alloc(IdbObject::Request(super::RequestData {
                global,
                open: false,
                source: Some(source),
                transaction: None,
                done: false,
                result: None,
                error: None,
            }))
        });
        self.req(request, |r| r.transaction = Some(t));
        self.txn(t, |t| {
            t.requests.push_back(Pending {
                request: Some(request),
                op,
            })
        });
        self.schedule_processing(t);
        request
    }

    /// Adds an operation with no request (an index's population).
    pub(crate) fn place_operation(&self, t: Handle, op: Op) {
        self.txn(t, |t| t.requests.push_back(Pending { request: None, op }));
        self.schedule_processing(t);
    }

    /// Queues the task that sends the transaction's next items, if they may go, and the one that
    /// fires the next item's event, if it has its result.
    pub(crate) fn schedule_processing(&self, t: Handle) {
        let go = self.txn(t, |t| {
            let go = t.started
                && !t.sending
                && !t.held
                && t.state != TxnState::Finished
                && !t.requests.is_empty();
            if go {
                t.sending = true;
            }
            go
        });
        if go {
            self.queue_task(TaskSource::DatabaseAccess, move |e, cx| {
                e.send_requests(cx, t)
            });
        }
        self.schedule_delivery(t);
    }

    /// Queues the task that fires the next item's event, if it has its result.
    fn schedule_delivery(&self, t: Handle) {
        let go = self.txn(t, |t| {
            let go = !t.processing
                && t.state != TxnState::Finished
                && t.sent.front().is_some_and(|s| s.state.ready());
            if go {
                t.processing = true;
            }
            go
        });
        if go {
            self.queue_task(TaskSource::DatabaseAccess, move |e, cx| {
                e.txn(t, |t| t.processing = false);
                e.process_next(cx, t)
            });
        }
    }

    /// Runs the operations of the items not sent, in order, and sends what they ask the backend,
    /// up to one that must have its answer first, and at most [`SEND_BATCH`] in one task (a backend
    /// in this process runs each as it is sent, and other tasks should not wait for them all).
    fn send_requests(&self, cx: &mut Cx, t: Handle) {
        let global = self.txn(t, |t| {
            t.sending = false;
            t.global
        });
        let mut sent = 0;
        self.in_global(cx, global, |cx| loop {
            if sent == SEND_BATCH {
                break;
            }
            sent += 1;
            let Some(Pending { request, op }) = self.txn(t, |t| {
                if t.state == TxnState::Finished || !t.started || t.held {
                    return None;
                }
                t.requests.pop_front()
            }) else {
                break;
            };
            let step = op(self, cx);
            self.txn(t, |t| {
                t.sent.push_back(Sent {
                    request,
                    seq: 0,
                    state: SentState::Ready(Err((ExceptionName::AbortError, ""))),
                })
            });
            self.take_step(t, step);
        });
        self.schedule_processing(t);
    }

    /// Puts the last sent item's next step in place: a call sent, or its result.
    fn take_step(&self, t: Handle, step: Step) {
        let (state, call) = match step {
            Step::Done(r) => (SentState::Ready(r), None),
            Step::Call(request, then) => {
                let hold = matches!(then, Then::Continue(_));
                (SentState::Waiting(then), Some((request, hold)))
            }
        };
        let (id, seq) = self.txn(t, |t| {
            let item = t.sent.back_mut().expect("an item");
            item.state = state;
            if let Some((_, hold)) = &call {
                t.seq += 1;
                item.seq = t.seq;
                t.held = *hold;
            } else {
                t.held = false;
            }
            (t.id, t.seq)
        });
        if let Some((request, _)) = call {
            let (backend, loop_) = self.idb_backend();
            backend.request(loop_, id, seq, request);
        }
    }

    /// The backend's answer to a transaction's call `seq`: the item has its result, or (one that
    /// held the others back) goes on. Fires the first item's event if no task is queued to.
    pub(crate) fn idb_answer(&self, cx: &mut Cx, id: u64, seq: u64, answer: Answer) {
        let Some(t) = self.idb.borrow().transactions.get(&id).copied() else {
            return;
        };
        let then = self.txn(t, |t| {
            if t.state == TxnState::Finished {
                return None;
            }
            let at = t.sent.iter().position(|s| s.seq == seq)?;
            let item = &mut t.sent[at];
            if item.state.ready() {
                return None;
            }
            let old = std::mem::replace(
                &mut item.state,
                SentState::Ready(Err((ExceptionName::AbortError, ""))),
            );
            let SentState::Waiting(then) = old else {
                return None;
            };
            Some((at, then))
        });
        let Some((at, then)) = then else {
            return;
        };
        match then {
            Then::Finish(f) => self.txn(t, |t| t.sent[at].state = SentState::Answered(f, answer)),
            Then::Continue(c) => {
                // (only the last item sent goes on: nothing was sent after it)
                debug_assert_eq!(at + 1, self.txn(t, |t| t.sent.len()));
                let global = self.txn(t, |t| t.global);
                let step = self.in_global(cx, global, |cx| c(self, cx, answer));
                if self.txn_state(t) == TxnState::Finished {
                    return;
                }
                self.take_step(t, step);
                self.schedule_processing(t);
            }
        }
        if !self.txn(t, |t| t.processing) {
            self.process_next(cx, t);
        }
    }

    /// Fires the event of the first item, if it has its result (§5.6's last steps).
    fn process_next(&self, cx: &mut Cx, t: Handle) {
        let Some((item, global)) = self.txn(t, |t| {
            if t.state == TxnState::Finished || !t.sent.front()?.state.ready() {
                return None;
            }
            t.sent.pop_front().map(|p| (p, t.global))
        }) else {
            return;
        };
        let Sent { request, state, .. } = item;
        self.in_global(cx, global, |cx| {
            let result = match state {
                SentState::Ready(r) => r,
                SentState::Answered(f, answer) => f(self, cx, answer),
                SentState::Waiting(_) => unreachable!("an item without its result"),
            };
            match request {
                None => {
                    if let Err((name, message)) = result {
                        let err = self.idb_exception(name, message);
                        self.abort_transaction(t, Some(err));
                        return;
                    }
                }
                Some(req) => {
                    if let Err((name, message)) = &result {
                        if self.txn_state(t) == TxnState::Committing {
                            // (the request is still in the list: the abort fires its error)
                            self.txn(t, |t| {
                                t.sent.push_front(Sent {
                                    request: Some(req),
                                    seq: 0,
                                    state: SentState::Ready(Err((ExceptionName::AbortError, ""))),
                                })
                            });
                            let err = self.idb_exception(*name, message);
                            self.abort_transaction(t, Some(err));
                            return;
                        }
                    }
                    match result {
                        Ok(v) => {
                            self.req(req, |r| {
                                r.done = true;
                                r.result = Some(v);
                                r.error = None;
                            });
                            self.fire_success(cx, req, t);
                        }
                        Err((name, message)) => {
                            let err = self.idb_exception(name, message);
                            self.req(req, |r| {
                                r.done = true;
                                r.result = None;
                                r.error = Some(err);
                            });
                            self.fire_error(cx, req, t);
                        }
                    }
                }
            }
            self.after_request(t);
        });
    }

    fn after_request(&self, t: Handle) {
        match self.txn_state(t) {
            TxnState::Committing => self.try_write(t),
            // (an operation with no request, an index's population, may have been the last)
            TxnState::Inactive => self.maybe_autocommit(t),
            _ => {}
        }
        self.schedule_processing(t);
    }

    /// Dispatches `event` at `target`; returns (not canceled, whether a listener threw).
    pub(crate) fn dispatch_tracking(
        &self,
        cx: &mut Cx,
        event: Handle,
        target: Handle,
    ) -> (bool, bool) {
        self.dispatch_flag(cx, event, target, false)
    }

    /// §5.9 "fire a success event".
    pub(crate) fn fire_success(&self, cx: &mut Cx, req: Handle, t: Handle) {
        let ev = self.trusted_event(InterfaceId::Event, "success", false, false, EventExt::None);
        self.txn(t, |t| {
            if t.state == TxnState::Inactive {
                t.state = TxnState::Active;
            }
        });
        let (_, threw) = self.dispatch_tracking(cx, ev, req);
        if self.txn_state(t) == TxnState::Active {
            self.txn(t, |t| t.state = TxnState::Inactive);
            if threw {
                let err = self.idb_exception(ExceptionName::AbortError, "An event listener threw");
                self.abort_transaction(t, Some(err));
            } else if self.txn(t, |t| t.idle()) {
                self.commit_transaction(t);
            }
        }
    }

    /// §5.10 "fire an error event".
    pub(crate) fn fire_error(&self, cx: &mut Cx, req: Handle, t: Handle) {
        let ev = self.trusted_event(InterfaceId::Event, "error", true, true, EventExt::None);
        self.txn(t, |t| {
            if t.state == TxnState::Inactive {
                t.state = TxnState::Active;
            }
        });
        let (not_canceled, threw) = self.dispatch_tracking(cx, ev, req);
        if self.txn_state(t) == TxnState::Active {
            self.txn(t, |t| t.state = TxnState::Inactive);
            if threw {
                let err = self.idb_exception(ExceptionName::AbortError, "An event listener threw");
                self.abort_transaction(t, Some(err));
                return;
            }
            if not_canceled {
                let err = self.req(req, |r| r.error);
                self.abort_transaction(t, err);
                return;
            }
            if self.txn(t, |t| t.idle()) {
                self.commit_transaction(t);
            }
        }
    }

    /// The end of a microtask checkpoint: HTML's "cleanup Indexed Database transactions" (§2.7.1):
    /// the transactions `transaction()` made in this task become inactive, and those with no
    /// request commit.
    pub(crate) fn idb_cleanup(&self) -> bool {
        let txns: Vec<Handle> = {
            let Ok(st) = self.idb.try_borrow() else {
                return false;
            };
            if st.transactions.is_empty() {
                return false;
            }
            st.transactions.values().copied().collect()
        };
        let mut any = false;
        for t in txns {
            let cleaned = self.txn(t, |t| {
                if !t.cleanup {
                    return false;
                }
                t.cleanup = false;
                if t.state == TxnState::Active {
                    t.state = TxnState::Inactive;
                }
                true
            });
            if cleaned {
                any = true;
                self.maybe_autocommit(t);
            }
        }
        any
    }

    /// Commits an inactive transaction with no request left (§2.7.1 "attempt to commit").
    pub(crate) fn maybe_autocommit(&self, t: Handle) {
        let go = self.txn(t, |t| t.state == TxnState::Inactive && t.idle());
        if go {
            self.commit_transaction(t);
        }
    }

    /// §5.4 "commit a transaction": committing; the changes are written once every request ran.
    pub(crate) fn commit_transaction(&self, t: Handle) {
        self.txn(t, |t| t.state = TxnState::Committing);
        self.try_write(t);
    }

    /// §5.4 step 2: writes the changes when the transaction is committing, started, and has no
    /// request left.
    pub(crate) fn try_write(&self, t: Handle) {
        let go = self.txn(t, |t| {
            let go = t.state == TxnState::Committing && t.started && t.idle() && !t.writing;
            if go {
                t.writing = true;
            }
            go
        });
        if !go {
            return;
        }
        let (id, durability) = self.txn(t, |t| (t.id, t.durability));
        let (backend, _) = self.idb_backend();
        backend.commit(id, durability);
    }

    /// The backend wrote (or failed to write) a transaction's changes: §5.4 step 2.5's task.
    pub(crate) fn idb_committed(&self, cx: &mut Cx, id: u64, result: Result<(), Failure>) {
        let Some(t) = self.idb.borrow().transactions.get(&id).copied() else {
            return;
        };
        if self.txn_state(t) != TxnState::Committing {
            return;
        }
        if let Err(f) = result {
            let err = self.idb_exception(failure_name(f), "The changes could not be written");
            self.abort_transaction(t, Some(err));
            return;
        }
        let (conn, mode, global, open_request) = self.txn(t, |t| {
            t.state = TxnState::Finished;
            (t.conn, t.mode, t.global, t.open_request)
        });
        self.idb.borrow_mut().transactions.remove(&id);
        self.conn(conn, |c| c.live.retain(|&x| x != t));
        let upgrade = mode == Mode::VersionChange;
        if upgrade {
            self.conn(conn, |c| c.upgrade = None);
        } else {
            let (backend, _) = self.idb_backend();
            backend.transaction_finished(id);
        }
        self.in_global(cx, global, |cx| {
            self.fire_event(cx, t, "complete", false, false);
        });
        if upgrade {
            if let Some(req) = open_request {
                self.req(req, |r| r.transaction = None);
                self.queue_task(TaskSource::DatabaseAccess, move |e, cx| {
                    e.upgrade_finished(cx, req, conn, id, true)
                });
            }
        }
        self.check_closed(conn);
    }

    /// Rolls back what a transaction changed.
    pub(crate) fn revert_transaction(&self, t: Handle) {
        let id = self.txn(t, |t| t.id);
        let (backend, _) = self.idb_backend();
        backend.revert(id);
    }

    /// Ends a transaction without events (its event loop or global is going away).
    pub(crate) fn finish_silently(&self, t: Handle) {
        let (id, conn, mode) = self.txn(t, |t| {
            t.state = TxnState::Finished;
            t.requests.clear();
            t.sent.clear();
            (t.id, t.conn, t.mode)
        });
        self.idb.borrow_mut().transactions.remove(&id);
        self.conn(conn, |c| {
            c.live.retain(|&x| x != t);
            if c.upgrade == Some(t) {
                c.upgrade = None;
            }
        });
        let (backend, _) = self.idb_backend();
        if mode == Mode::VersionChange {
            backend.upgrade_finished(id, false);
        } else {
            backend.transaction_finished(id);
        }
    }

    /// §5.5 "abort a transaction" with `error` (a DOMException, or none).
    pub(crate) fn abort_transaction(&self, t: Handle, error: Option<Handle>) {
        if self.txn_state(t) == TxnState::Finished {
            return;
        }
        self.revert_transaction(t);
        let (id, conn, mode, global) = self.txn(t, |t| (t.id, t.conn, t.mode, t.global));
        let upgrade = mode == Mode::VersionChange;
        if upgrade {
            self.abort_upgrade(t);
        }
        let pending: Vec<Handle> = self.txn(t, |t| {
            t.state = TxnState::Finished;
            t.error = error;
            t.held = false;
            let sent = std::mem::take(&mut t.sent).into_iter().map(|s| s.request);
            let unsent = std::mem::take(&mut t.requests)
                .into_iter()
                .map(|p| p.request);
            sent.chain(unsent).flatten().collect()
        });
        self.idb.borrow_mut().transactions.remove(&id);
        self.conn(conn, |c| c.live.retain(|&x| x != t));
        if !upgrade {
            let (backend, _) = self.idb_backend();
            backend.transaction_finished(id);
        }
        for req in pending {
            self.txn(t, |t| t.aborting.push(req));
            self.queue_task(TaskSource::DatabaseAccess, move |e, cx| {
                e.txn(t, |t| t.aborting.retain(|&r| r != req));
                let err = e.idb_exception(ExceptionName::AbortError, "The transaction was aborted");
                e.req(req, |r| {
                    r.done = true;
                    r.result = None;
                    r.error = Some(err);
                });
                e.in_global(cx, global, |cx| {
                    e.fire_event(cx, req, "error", true, true);
                });
            });
        }
        self.queue_task(TaskSource::DatabaseAccess, move |e, cx| {
            if upgrade {
                e.conn(conn, |c| c.upgrade = None);
            }
            e.in_global(cx, global, |cx| {
                e.fire_event(cx, t, "abort", true, false);
            });
            if upgrade {
                if let Some(req) = e.txn(t, |t| t.open_request) {
                    e.req(req, |r| {
                        r.transaction = None;
                        r.result = None;
                        r.done = false;
                    });
                    e.queue_task(TaskSource::DatabaseAccess, move |e, cx| {
                        e.upgrade_finished(cx, req, conn, id, false)
                    });
                }
            }
            e.check_closed(conn);
        });
    }

    /// §5.8 "abort an upgrade transaction": the connection's version and object store set, and the
    /// handles' names and index sets, go back to the database's (already reverted) state.
    fn abort_upgrade(&self, t: Handle) {
        let (conn, old, handles, at_start) = self.txn(t, |t| {
            (
                t.conn,
                t.old_version,
                t.handles.clone(),
                t.schema_at_start.take(),
            )
        });
        let Some(db) = at_start else {
            return;
        };
        let stores: Vec<(u64, String)> =
            db.stores.values().map(|s| (s.id, s.name.clone())).collect();
        self.conn(conn, |c| {
            c.version = old;
            c.stores = stores;
            c.schema = db.clone();
        });
        for (store_id, h) in handles {
            let index_handles = self.store_h(h, |s| {
                match db.store(store_id) {
                    Some(st) => {
                        s.deleted = false;
                        if !s.created {
                            s.name = st.name.clone();
                        }
                        s.indexes = st
                            .indexes
                            .values()
                            .map(|i| (i.id, i.name.clone()))
                            .collect();
                    }
                    None => s.indexes.clear(),
                }
                s.index_handles.clone()
            });
            for (index_id, ih) in index_handles {
                self.index_h(ih, |i| {
                    if !i.created {
                        if let Some(ix) = db.index(store_id, index_id) {
                            i.name = ix.name.clone();
                        }
                    }
                });
            }
        }
    }

    /// A connection with its close pending flag set and no live transaction is closed (§5.2).
    pub(crate) fn check_closed(&self, conn: Handle) {
        let close = self.conn(conn, |c| {
            let go = c.close_pending && !c.closed && c.live.is_empty() && c.upgrade.is_none();
            if go {
                c.closed = true;
            }
            go.then_some(c.id)
        });
        if let Some(id) = close {
            self.idb.borrow_mut().connections.remove(&id);
            let (backend, _) = self.idb_backend();
            backend.connection_closed(id);
        }
    }

    /// The backend says a transaction may start.
    pub(crate) fn idb_started(&self, id: u64) {
        let Some(t) = self.idb.borrow().transactions.get(&id).copied() else {
            return;
        };
        self.txn(t, |t| t.started = true);
        self.schedule_processing(t);
        self.try_write(t);
    }

    /// The object store handle of a transaction for a store (one per store and transaction).
    pub(crate) fn store_handle(&self, t: Handle, store_id: u64) -> Option<Handle> {
        if let Some(h) = self.txn(t, |t| {
            t.handles
                .iter()
                .find(|(id, _)| *id == store_id)
                .map(|(_, h)| *h)
        }) {
            return Some(h);
        }
        let (conn, global) = self.txn(t, |t| (t.conn, t.global));
        let handle = self.conn(conn, |c| {
            let s = c.schema.store(store_id)?;
            Some(StoreHandle {
                global,
                txn: t,
                id: store_id,
                name: s.name.clone(),
                key_path: s.key_path.clone(),
                auto_increment: s.auto_increment,
                indexes: s.indexes.values().map(|i| (i.id, i.name.clone())).collect(),
                index_handles: Vec::new(),
                key_path_js: None,
                created: false,
                deleted: false,
            })
        })?;
        let h = self.idb_alloc(IdbObject::Store(handle));
        self.txn(t, |t| t.handles.push((store_id, h)));
        Some(h)
    }

    /// The transaction's scope: (id, name).
    pub(crate) fn txn_scope(&self, t: Handle) -> Vec<(u64, String)> {
        let (mode, conn, scope) = self.txn(t, |t| (t.mode, t.conn, t.scope.clone()));
        if mode == Mode::VersionChange {
            self.conn(conn, |c| c.stores.clone())
        } else {
            scope
        }
    }

    /// DOM's "get the parent" of IndexedDB targets: a request's is its transaction, a
    /// transaction's its connection.
    pub(crate) fn idb_parent(&self, target: Handle) -> Option<Handle> {
        let objects = self.objects.try_borrow().ok()?;
        match objects.get(target.get() as usize - 1)? {
            crate::engine::Object::Idb(o) => match &**o {
                IdbObject::Request(r) => r.transaction,
                IdbObject::Transaction(t) => Some(t.conn),
                _ => None,
            },
            _ => None,
        }
    }
}

impl IDBTransactionImpl for Engine {
    fn object_store_names(&self, this: Handle) -> Handle {
        let names = self.txn_scope(this).into_iter().map(|(_, n)| n).collect();
        self.sorted_name_list(names)
    }
    fn mode(&self, this: Handle) -> IDBTransactionMode {
        match self.txn(this, |t| t.mode) {
            Mode::ReadOnly => IDBTransactionMode::Readonly,
            Mode::ReadWrite => IDBTransactionMode::Readwrite,
            Mode::VersionChange => IDBTransactionMode::Versionchange,
        }
    }
    fn durability(&self, this: Handle) -> IDBTransactionDurability {
        match self.txn(this, |t| t.durability) {
            Durability::Default => IDBTransactionDurability::Default,
            Durability::Strict => IDBTransactionDurability::Strict,
            Durability::Relaxed => IDBTransactionDurability::Relaxed,
        }
    }
    fn db(&self, this: Handle) -> Handle {
        self.txn(this, |t| t.conn)
    }
    fn error(&self, this: Handle) -> Option<Handle> {
        self.txn(this, |t| t.error)
    }
    /// §4.10 `objectStore(name)`.
    fn object_store(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Handle> {
        if self.txn_state(this) == TxnState::Finished {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The transaction has finished",
            ));
        }
        let Some((id, _)) = self.txn_scope(this).into_iter().find(|(_, n)| n == name) else {
            return Err(exception(
                ExceptionName::NotFoundError,
                "No object store with that name is in the transaction's scope",
            ));
        };
        self.store_handle(this, id).ok_or_else(|| {
            exception(
                ExceptionName::NotFoundError,
                "No object store with that name is in the transaction's scope",
            )
        })
    }
    /// §4.10 `commit()`.
    fn commit(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if self.txn_state(this) != TxnState::Active {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The transaction is not active",
            ));
        }
        self.commit_transaction(this);
        Ok(())
    }
    /// §4.10 `abort()`.
    fn abort(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if matches!(
            self.txn_state(this),
            TxnState::Committing | TxnState::Finished
        ) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The transaction is committing or finished",
            ));
        }
        self.abort_transaction(this, None);
        Ok(())
    }
}

impl IDBRequestImpl for Engine {
    /// §4.1 `result`.
    fn result(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let (done, result) = self.req(this, |r| (r.done, r.result.clone()));
        if !done {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The request has not finished",
            ));
        }
        Ok(result.unwrap_or_else(|| rt::undefined_value(cx.scope())))
    }
    /// §4.1 `error`.
    fn error(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        let (done, error) = self.req(this, |r| (r.done, r.error));
        if !done {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The request has not finished",
            ));
        }
        Ok(error)
    }
    fn source(&self, this: Handle) -> Option<IDBObjectStoreOrIDBIndexOrIDBCursor> {
        let source = self.req(this, |r| r.source)?;
        self.idb(source, |o| match o {
            IdbObject::Store(_) => {
                Some(IDBObjectStoreOrIDBIndexOrIDBCursor::IDBObjectStore(source))
            }
            IdbObject::Index(_) => Some(IDBObjectStoreOrIDBIndexOrIDBCursor::IDBIndex(source)),
            IdbObject::Cursor(_) => Some(IDBObjectStoreOrIDBIndexOrIDBCursor::IDBCursor(source)),
            _ => None,
        })
    }
    fn transaction(&self, this: Handle) -> Option<Handle> {
        self.req(this, |r| r.transaction)
    }
    fn ready_state(&self, this: Handle) -> IDBRequestReadyState {
        if self.req(this, |r| r.done) {
            IDBRequestReadyState::Done
        } else {
            IDBRequestReadyState::Pending
        }
    }
}
