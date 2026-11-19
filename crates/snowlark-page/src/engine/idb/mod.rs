// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Indexed Database API 3.0 (<https://w3c.github.io/IndexedDB/>): the IDB* interfaces over the shared
//! backend of `snowlark-idb`.
//!
//! The backend runs the steps the spec runs in parallel (connection queues, scheduling, writing, and
//! each request's operation); it answers through this event loop's inbox, which [`Engine::poll_idb`]
//! turns into database access tasks. A started transaction sends its requests to the backend in
//! order without waiting ([`snowlark_idb::request`]); the backend runs them in that order, against
//! the database its scheduling gives the transaction alone, and each answer arrives as a task that
//! fires the request's `success` or `error` event, in request order (§5.6). The backend applies
//! each change at once and keeps its undo entry, so an abort rolls back and a commit writes the
//! changes (§5.4, §5.5). Each connection keeps a copy of the database's schema, which
//! only its own upgrade transaction changes.
//!
//! - [`keys`] converts values to keys and back, evaluates key paths, and has `IDBKeyRange`,
//!   `IDBRecord`, `DOMStringList` and `indexedDB.cmp()`.
//! - [`factory`] has `IDBFactory`, the open and delete requests, `IDBDatabase` and
//!   `IDBVersionChangeEvent`.
//! - [`transaction`] has `IDBTransaction` and `IDBRequest`: the lifecycle, firing events, commit
//!   and abort.
//! - [`store`] has `IDBObjectStore` and `IDBIndex` and their operations.
//! - [`cursor`] has `IDBCursor`.

pub(crate) mod cursor;
pub(crate) mod factory;
pub(crate) mod keys;
pub(crate) mod store;
pub(crate) mod transaction;

use std::collections::VecDeque;
use std::sync::Arc;

use snowlark_bindings::rt::{Cx, ExceptionName, Handle, JsPromise, JsValue};
use snowlark_bindings::InterfaceId;
use snowlark_idb::{
    Answer, Backend, Database, Direction, Durability, Found, Inbox, Key, KeyPath, KeyRange, Mode,
    Refused, Request, WriteError,
};

use super::{Engine, Object};
use crate::event_loop::TaskSource;

/// A transaction's state (§2.7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnState {
    Active,
    Inactive,
    Committing,
    Finished,
}

/// What an operation fails with: a DOMException's name and message.
pub type OpError = (ExceptionName, &'static str);

/// What a call the backend refused fails with (a page that follows the spec never makes one).
pub(crate) fn refused(_: Refused) -> OpError {
    (
        ExceptionName::UnknownError,
        "The storage refused the operation",
    )
}

/// An operation of a request (§6). It runs on this thread when its turn to be sent comes (in the
/// order the requests were placed, once the transaction has started), and says what to ask the
/// backend, or the result.
pub type Op = Box<dyn FnOnce(&Engine, &mut Cx) -> Step>;

/// What an operation does next.
// (a step lives only until it is taken: boxing the request would cost an allocation per call)
#[allow(clippy::large_enum_variant)]
pub enum Step {
    /// Ask the backend; `Then` takes the answer.
    Call(Request, Then),
    /// The result is known.
    Done(Result<JsValue, OpError>),
}

/// Makes a request's result from the backend's answer, when the request's event is fired.
pub type Finish = Box<dyn FnOnce(&Engine, &mut Cx, Answer) -> Result<JsValue, OpError>>;

/// Takes the backend's answer as it arrives, to ask the backend again (an index's population, a
/// generated key). The transaction sends nothing after it until then.
pub type Continue = Box<dyn FnOnce(&Engine, &mut Cx, Answer) -> Step>;

pub enum Then {
    Finish(Finish),
    Continue(Continue),
}

impl Step {
    pub(crate) fn finish(
        request: Request,
        f: impl FnOnce(&Engine, &mut Cx, Answer) -> Result<JsValue, OpError> + 'static,
    ) -> Step {
        Step::Call(request, Then::Finish(Box::new(f)))
    }

    pub(crate) fn then(
        request: Request,
        f: impl FnOnce(&Engine, &mut Cx, Answer) -> Step + 'static,
    ) -> Step {
        Step::Call(request, Then::Continue(Box::new(f)))
    }
}

/// An item of a transaction's request list that has not been sent: a request and its operation,
/// or an operation with no request (an index's population, an index or store deletion).
pub struct Pending {
    pub request: Option<Handle>,
    pub op: Op,
}

/// An item whose operation was sent (or that has its result), until its event is fired.
pub struct Sent {
    pub request: Option<Handle>,
    /// Its call's number in the transaction (0: none).
    pub seq: u64,
    pub state: SentState,
}

pub enum SentState {
    Waiting(Then),
    Answered(Finish, Answer),
    Ready(Result<JsValue, OpError>),
}

impl SentState {
    /// Whether the item's event may be fired.
    pub fn ready(&self) -> bool {
        !matches!(self, SentState::Waiting(_))
    }
}

/// A record found, from an answer.
pub(crate) fn found(a: Answer) -> Result<Option<Found>, OpError> {
    match a {
        Answer::Found(r) => r.map(|f| f.map(Found::from)).map_err(refused),
        _ => Err(refused(Refused)),
    }
}

/// The records found, from an answer.
pub(crate) fn found_all(a: Answer) -> Result<Vec<Found>, OpError> {
    match a {
        Answer::All(r) => r
            .map(|v| v.into_iter().map(Found::from).collect())
            .map_err(refused),
        _ => Err(refused(Refused)),
    }
}

/// A count, from an answer.
pub(crate) fn counted(a: Answer) -> Result<u64, OpError> {
    match a {
        Answer::Count(r) => r.map_err(refused),
        _ => Err(refused(Refused)),
    }
}

/// What a write that failed fails with.
pub(crate) fn write_failed(e: WriteError) -> OpError {
    match e {
        WriteError::KeyExists => (
            ExceptionName::ConstraintError,
            "A record with the key exists in the object store",
        ),
        WriteError::NotUnique => (
            ExceptionName::ConstraintError,
            "A unique index already has a record with the key",
        ),
        WriteError::NoKeyLeft => (
            ExceptionName::ConstraintError,
            "The key generator has reached its maximum",
        ),
        WriteError::Refused => refused(Refused),
    }
}

/// The key the backend generated, from an answer.
pub(crate) fn generated(a: Answer) -> Result<u64, OpError> {
    match a {
        Answer::Generated(r) => r.map_err(write_failed),
        _ => Err(refused(Refused)),
    }
}

/// A write's outcome, from an answer.
pub(crate) fn applied(a: Answer) -> Result<(), WriteError> {
    match a {
        Answer::Applied(r) => r,
        _ => Err(WriteError::Refused),
    }
}

/// `IDBRequest` / `IDBOpenDBRequest` (§2.8).
pub struct RequestData {
    pub global: Handle,
    pub open: bool,
    pub source: Option<Handle>,
    pub transaction: Option<Handle>,
    pub done: bool,
    /// `None`: undefined.
    pub result: Option<JsValue>,
    /// A DOMException.
    pub error: Option<Handle>,
}

/// `IDBDatabase`: a connection (§2.1.1).
pub struct ConnData {
    pub global: Handle,
    /// The backend's id of the connection.
    pub id: u64,
    /// The copy of the database's schema (only this connection's upgrade transaction changes it).
    pub schema: Database,
    pub name: String,
    pub version: u64,
    /// The object store set: (id, name).
    pub stores: Vec<(u64, String)>,
    pub close_pending: bool,
    pub closed: bool,
    /// The live upgrade transaction.
    pub upgrade: Option<Handle>,
    /// The transactions created with this connection that are not finished.
    pub live: Vec<Handle>,
}

/// `IDBTransaction` (§2.7).
pub struct TxnData {
    pub global: Handle,
    pub id: u64,
    pub conn: Handle,
    pub mode: Mode,
    pub durability: Durability,
    /// The scope (id, name) of a transaction that is not an upgrade transaction (an upgrade
    /// transaction's is its connection's object store set).
    pub scope: Vec<(u64, String)>,
    pub state: TxnState,
    pub started: bool,
    /// Its cleanup event loop is this one (it came from `transaction()` in the current task).
    pub cleanup: bool,
    /// The items not sent yet.
    pub requests: VecDeque<Pending>,
    /// The items sent (or with their result), whose events are not fired yet, in order.
    pub sent: VecDeque<Sent>,
    /// The last call's number.
    pub seq: u64,
    /// A sent item waits for its answer before the next may be sent ([`Then::Continue`]).
    pub held: bool,
    /// A task to send the next items is queued.
    pub sending: bool,
    /// A task to fire the next item's event is queued.
    pub processing: bool,
    /// The changes were sent to the backend to write.
    pub writing: bool,
    pub error: Option<Handle>,
    /// The object store handles: (store id, handle).
    pub handles: Vec<(u64, Handle)>,
    /// The requests whose `error` events an abort queued (kept until they fire).
    pub aborting: Vec<Handle>,
    /// An upgrade transaction's connection schema when it started: an abort goes back to it (§5.8).
    pub schema_at_start: Option<Database>,
    /// An upgrade transaction's open request and the database's version before it.
    pub open_request: Option<Handle>,
    pub old_version: u64,
}

impl TxnData {
    /// No request is left: none to send, none whose event is not fired.
    pub fn idle(&self) -> bool {
        self.requests.is_empty() && self.sent.is_empty()
    }
}

/// `IDBObjectStore`: an object store handle (§2.2.1).
pub struct StoreHandle {
    pub global: Handle,
    pub txn: Handle,
    pub id: u64,
    pub name: String,
    pub key_path: Option<KeyPath>,
    pub auto_increment: bool,
    /// The index set: (id, name).
    pub indexes: Vec<(u64, String)>,
    pub index_handles: Vec<(u64, Handle)>,
    pub key_path_js: Option<JsValue>,
    /// The store was created in this (upgrade) transaction.
    pub created: bool,
    /// `deleteObjectStore()` deleted the store (its destruction in the database is a request that
    /// may not have run yet).
    pub deleted: bool,
}

/// `IDBIndex`: an index handle (§2.6.1).
pub struct IndexHandle {
    pub global: Handle,
    pub store: Handle,
    pub store_id: u64,
    pub id: u64,
    pub name: String,
    pub key_path: KeyPath,
    pub unique: bool,
    pub multi_entry: bool,
    pub key_path_js: Option<JsValue>,
    pub created: bool,
}

/// `IDBCursor` / `IDBCursorWithValue` (§2.10).
pub struct CursorData {
    pub global: Handle,
    pub request: Handle,
    /// The source handle (an `IDBObjectStore` or `IDBIndex`).
    pub source: Handle,
    pub txn: Handle,
    pub store_id: u64,
    pub index_id: Option<u64>,
    pub direction: Direction,
    pub range: KeyRange,
    pub position: Option<Key>,
    pub object_store_position: Option<Key>,
    pub key: Option<Key>,
    pub key_js: Option<JsValue>,
    pub primary_key_js: Option<JsValue>,
    pub value: Option<JsValue>,
    pub got_value: bool,
    pub key_only: bool,
}

/// `IDBRecord`: a record snapshot (§2.12).
pub struct RecordData {
    pub global: Handle,
    pub key: Key,
    pub primary_key: Key,
    pub value: JsValue,
    pub key_js: Option<JsValue>,
    pub primary_key_js: Option<JsValue>,
}

/// An IndexedDB platform object.
pub enum IdbObject {
    Factory { global: Handle },
    Request(RequestData),
    Database(ConnData),
    Transaction(TxnData),
    Store(StoreHandle),
    Index(IndexHandle),
    Cursor(CursorData),
    KeyRange { global: Handle, range: KeyRange },
    Record(RecordData),
}

impl IdbObject {
    pub(crate) fn interface(&self) -> InterfaceId {
        match self {
            IdbObject::Factory { .. } => InterfaceId::IDBFactory,
            IdbObject::Request(r) if r.open => InterfaceId::IDBOpenDBRequest,
            IdbObject::Request(_) => InterfaceId::IDBRequest,
            IdbObject::Database(_) => InterfaceId::IDBDatabase,
            IdbObject::Transaction(_) => InterfaceId::IDBTransaction,
            IdbObject::Store(_) => InterfaceId::IDBObjectStore,
            IdbObject::Index(_) => InterfaceId::IDBIndex,
            IdbObject::Cursor(c) if c.key_only => InterfaceId::IDBCursor,
            IdbObject::Cursor(_) => InterfaceId::IDBCursorWithValue,
            IdbObject::KeyRange { .. } => InterfaceId::IDBKeyRange,
            IdbObject::Record(_) => InterfaceId::IDBRecord,
        }
    }

    /// Whether the object is collectable: a request other than an open request, a key range, a
    /// record.
    pub(crate) fn weak(&self) -> bool {
        match self {
            IdbObject::Request(r) => !r.open,
            IdbObject::KeyRange { .. } | IdbObject::Record(_) => true,
            _ => false,
        }
    }

    /// The object's relevant global.
    pub(crate) fn global(&self) -> Handle {
        match self {
            IdbObject::Factory { global } | IdbObject::KeyRange { global, .. } => *global,
            IdbObject::Request(r) => r.global,
            IdbObject::Database(c) => c.global,
            IdbObject::Transaction(t) => t.global,
            IdbObject::Store(s) => s.global,
            IdbObject::Index(i) => i.global,
            IdbObject::Cursor(c) => c.global,
            IdbObject::Record(r) => r.global,
        }
    }

    /// The handles this object keeps alive.
    pub(crate) fn refs(&self, out: &mut Vec<Handle>) {
        match self {
            IdbObject::Request(r) => {
                out.extend(r.source);
                out.extend(r.transaction);
                out.extend(r.error);
            }
            IdbObject::Database(c) => out.extend(&c.live),
            IdbObject::Transaction(t) => {
                out.push(t.conn);
                out.extend(t.error);
                out.extend(t.requests.iter().filter_map(|p| p.request));
                out.extend(t.sent.iter().filter_map(|s| s.request));
                out.extend(&t.aborting);
                out.extend(t.handles.iter().map(|(_, h)| *h));
                out.extend(t.open_request);
            }
            IdbObject::Store(s) => {
                out.push(s.txn);
                out.extend(s.index_handles.iter().map(|(_, h)| *h));
            }
            IdbObject::Index(i) => out.push(i.store),
            IdbObject::Cursor(c) => {
                out.push(c.request);
                out.push(c.source);
                out.push(c.txn);
            }
            _ => {}
        }
    }
}

/// This event loop's IndexedDB state.
#[derive(Default)]
pub struct IdbState {
    /// The backend (the embedder's; an in-memory one of its own when it gave none).
    pub backend: Option<Backend>,
    pub inbox: Option<Arc<Inbox>>,
    pub loop_id: u64,
    /// Backend connection id → `IDBDatabase`.
    pub connections: rustc_hash::FxHashMap<u64, Handle>,
    /// Backend transaction id → `IDBTransaction` (live transactions).
    pub transactions: rustc_hash::FxHashMap<u64, Handle>,
    /// Request id → pending `IDBOpenDBRequest`.
    pub open_requests: rustc_hash::FxHashMap<u64, Handle>,
    /// `databases()` promises by id.
    pub lists: rustc_hash::FxHashMap<u64, JsPromise>,
    /// `navigator.storage.estimate()` promises by id.
    pub estimates: rustc_hash::FxHashMap<u64, JsPromise>,
    pub next_id: u64,
}

impl Engine {
    // ----- objects ---------------------------------------------------------------------------------------

    pub(crate) fn idb<R>(&self, h: Handle, f: impl FnOnce(&IdbObject) -> R) -> R {
        match &*self.obj(h) {
            Object::Idb(o) => f(o),
            _ => panic!("not an IndexedDB object"),
        }
    }

    pub(crate) fn idb_mut<R>(&self, h: Handle, f: impl FnOnce(&mut IdbObject) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::Idb(o) => f(o),
            _ => panic!("not an IndexedDB object"),
        }
    }

    pub(crate) fn idb_alloc(&self, o: IdbObject) -> Handle {
        self.alloc(Object::Idb(Box::new(o)))
    }

    pub(crate) fn req<R>(&self, h: Handle, f: impl FnOnce(&mut RequestData) -> R) -> R {
        self.idb_mut(h, |o| match o {
            IdbObject::Request(r) => f(r),
            _ => panic!("not an IDBRequest"),
        })
    }

    pub(crate) fn conn<R>(&self, h: Handle, f: impl FnOnce(&mut ConnData) -> R) -> R {
        self.idb_mut(h, |o| match o {
            IdbObject::Database(c) => f(c),
            _ => panic!("not an IDBDatabase"),
        })
    }

    pub(crate) fn txn<R>(&self, h: Handle, f: impl FnOnce(&mut TxnData) -> R) -> R {
        self.idb_mut(h, |o| match o {
            IdbObject::Transaction(t) => f(t),
            _ => panic!("not an IDBTransaction"),
        })
    }

    pub(crate) fn store_h<R>(&self, h: Handle, f: impl FnOnce(&mut StoreHandle) -> R) -> R {
        self.idb_mut(h, |o| match o {
            IdbObject::Store(s) => f(s),
            _ => panic!("not an IDBObjectStore"),
        })
    }

    pub(crate) fn index_h<R>(&self, h: Handle, f: impl FnOnce(&mut IndexHandle) -> R) -> R {
        self.idb_mut(h, |o| match o {
            IdbObject::Index(i) => f(i),
            _ => panic!("not an IDBIndex"),
        })
    }

    pub(crate) fn cursor_d<R>(&self, h: Handle, f: impl FnOnce(&mut CursorData) -> R) -> R {
        self.idb_mut(h, |o| match o {
            IdbObject::Cursor(c) => f(c),
            _ => panic!("not an IDBCursor"),
        })
    }

    /// A DOMException object.
    pub(crate) fn idb_exception(&self, name: ExceptionName, message: &str) -> Handle {
        self.alloc(Object::DomException {
            name: name.as_str().to_owned(),
            message: message.to_owned(),
        })
    }

    // ----- the backend --------------------------------------------------------------------------------

    /// Uses `backend` for this event loop's databases (the embedder's, shared by its pages; a
    /// worker shares its parent's). Must come before the first use.
    pub fn set_idb_backend(&self, backend: Backend) {
        self.idb.borrow_mut().backend = Some(backend);
    }

    /// The backend, and this loop's id in it (registered on first use).
    pub(crate) fn idb_backend(&self) -> (Backend, u64) {
        let mut st = self.idb.borrow_mut();
        let backend = st.backend.get_or_insert_with(Backend::in_memory).clone();
        if st.inbox.is_none() {
            let waker = self.waker();
            let inbox = Arc::new(Inbox::new(Box::new(move || waker.wake())));
            st.loop_id = backend.register_loop(&inbox);
            st.inbox = Some(inbox);
        }
        (backend, st.loop_id)
    }

    /// Reads the schema copy of a transaction's connection.
    pub(crate) fn txn_schema<R>(&self, t: Handle, f: impl FnOnce(&Database) -> R) -> R {
        let conn = self.txn(t, |t| t.conn);
        self.conn(conn, |c| f(&c.schema))
    }

    /// The backend of this event loop, for a worker it starts.
    pub(crate) fn idb_backend_for_worker(&self) -> Backend {
        self.idb
            .borrow_mut()
            .backend
            .get_or_insert_with(Backend::in_memory)
            .clone()
    }

    pub(crate) fn idb_next_id(&self) -> u64 {
        let mut st = self.idb.borrow_mut();
        st.next_id += 1;
        st.next_id
    }

    /// Whether IndexedDB work waits on the backend (open requests, `databases()`, transactions).
    pub(crate) fn idb_pending(&self) -> bool {
        let st = self.idb.borrow();
        !st.open_requests.is_empty()
            || !st.lists.is_empty()
            || !st.estimates.is_empty()
            || !st.transactions.is_empty()
            || st.inbox.as_ref().is_some_and(|i| i.has_messages())
    }

    /// Turns the backend's messages into database access tasks. Returns whether there were any.
    pub(crate) fn poll_idb(&self, _cx: &mut Cx) -> bool {
        let inbox = self.idb.borrow().inbox.clone();
        let Some(inbox) = inbox else {
            return false;
        };
        let msgs = inbox.drain();
        if msgs.is_empty() {
            return false;
        }
        for m in msgs {
            self.queue_task(TaskSource::DatabaseAccess, move |e, cx| {
                e.idb_message(cx, m)
            });
        }
        true
    }

    /// Runs `f` in the realm of `global`.
    pub(crate) fn in_global<R>(
        &self,
        cx: &mut Cx,
        global: Handle,
        f: impl FnOnce(&mut Cx) -> R,
    ) -> R {
        let realm = self
            .context_of_window(global)
            .and_then(|id| self.realm_of_context(id));
        self.in_realm(cx, realm, f)
    }

    /// The event loop is going away (its page closes): live transactions roll back and the
    /// backend forgets its connections.
    pub fn idb_shutdown(&self) {
        let (backend, loop_id, txns) = {
            let st = self.idb.borrow();
            let Some(b) = st.backend.clone() else {
                return;
            };
            if st.inbox.is_none() {
                return;
            }
            (
                b,
                st.loop_id,
                st.transactions.values().copied().collect::<Vec<_>>(),
            )
        };
        for t in txns {
            // (a transaction whose changes are being written keeps them: the disk will have them)
            if !self.txn(t, |t| t.writing) {
                self.revert_transaction(t);
            }
        }
        backend.loop_gone(loop_id);
        let mut st = self.idb.borrow_mut();
        st.inbox = None;
        st.transactions.clear();
        st.connections.clear();
        st.open_requests.clear();
    }

    /// A global's document stopped being active: its connections close (§2.1.1, "if the execution
    /// context where the connection was created is destroyed") after its transactions abort.
    pub(crate) fn idb_global_destroyed(&self, global: Handle) {
        let conns: Vec<Handle> = self.idb.borrow().connections.values().copied().collect();
        for c in conns {
            if self.conn(c, |d| d.global) != global {
                continue;
            }
            let live = self.conn(c, |d| d.live.clone());
            for t in live {
                if !self.txn(t, |t| t.writing) {
                    self.revert_transaction(t);
                }
                self.finish_silently(t);
            }
            self.conn(c, |d| d.close_pending = true);
            self.check_closed(c);
        }
    }
}
