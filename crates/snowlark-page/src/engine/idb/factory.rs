// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IndexedDB §4.3 `IDBFactory` with the open and delete requests' tasks (§5.1, §5.3, §5.7), §4.4
//! `IDBDatabase`, §4.2 `IDBVersionChangeEvent`, and `indexedDB` on the global.

use snowlark_bindings::generated::{
    IDBDatabaseImpl, IDBDatabaseInfo, IDBFactoryImpl, IDBObjectStoreParameters,
    IDBTransactionDurability, IDBTransactionMode, IDBTransactionOptions, IDBVersionChangeEventImpl,
    IDBVersionChangeEventInit, StringOrStringSequence,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle, JsPromise};
use snowlark_bindings::InterfaceId;
use snowlark_idb::{Change, Database, Durability, KeyPath, Mode, Msg};

use super::transaction::{failure_name, inactive};
use super::{ConnData, IdbObject, RequestData, Step, StoreHandle, TxnState};
use crate::engine::events::EventExt;
use crate::engine::{exception, Engine, Object};

impl Engine {
    /// The storage key of a global (its origin), or `None` for an opaque origin ("obtain a storage
    /// key" fails).
    pub(crate) fn storage_key(&self, global: Handle) -> Option<String> {
        let doc = self.associated_document(global)?;
        let origin = self.origin_of_document(doc);
        if origin.starts_with("opaque:file:") {
            // (file: URLs' origin is opaque, an implementation-defined choice; their documents
            // share one storage key, as in Chrome)
            return Some("file://".to_owned());
        }
        (origin != "null" && !origin.starts_with("opaque:")).then_some(origin)
    }

    fn factory_global(&self, factory: Handle) -> Handle {
        self.idb(factory, |o| o.global())
    }

    fn new_open_request(&self, global: Handle) -> Handle {
        self.idb_alloc(IdbObject::Request(RequestData {
            global,
            open: true,
            source: None,
            transaction: None,
            done: false,
            result: None,
            error: None,
        }))
    }

    /// Fires an `IDBVersionChangeEvent`; returns whether a listener threw.
    pub(crate) fn fire_version_change(
        &self,
        cx: &mut Cx,
        target: Handle,
        type_: &str,
        old: u64,
        new: Option<u64>,
    ) -> bool {
        let ev = self.trusted_event(
            InterfaceId::IDBVersionChangeEvent,
            type_,
            false,
            false,
            EventExt::VersionChange { old, new },
        );
        self.dispatch_tracking(cx, ev, target).1
    }

    /// A message from the backend, in a database access task.
    pub(crate) fn idb_message(&self, cx: &mut Cx, m: Msg) {
        match m {
            Msg::VersionChange { conn, old, new } => {
                let h = self.idb.borrow().connections.get(&conn).copied();
                if let Some(h) = h {
                    let (pending, global) = self.conn(h, |c| (c.close_pending, c.global));
                    if !pending {
                        self.in_global(cx, global, |cx| {
                            self.fire_version_change(cx, h, "versionchange", old, new);
                        });
                    }
                }
                let (backend, _) = self.idb_backend();
                backend.versionchange_fired(conn);
            }
            Msg::Blocked { req, old, new } => {
                let Some(h) = self.idb.borrow().open_requests.get(&req).copied() else {
                    return;
                };
                let global = self.req(h, |r| r.global);
                self.in_global(cx, global, |cx| {
                    self.fire_version_change(cx, h, "blocked", old, new);
                });
            }
            Msg::Opened {
                req,
                conn,
                schema,
                version,
            } => {
                let Some(h) = self.idb.borrow_mut().open_requests.remove(&req) else {
                    return;
                };
                let global = self.req(h, |r| r.global);
                let c = self.new_connection(global, conn, schema, version);
                self.in_global(cx, global, |cx| {
                    let v = self.handle_value(cx, c);
                    self.req(h, |r| {
                        r.result = Some(v);
                        r.done = true;
                    });
                    self.fire_event(cx, h, "success", false, false);
                });
            }
            Msg::Upgrade {
                req,
                conn,
                txn,
                schema,
                old,
                new,
            } => self.run_upgrade(cx, req, conn, txn, schema, old, new),
            Msg::OpenFailed { req, failure } => {
                let Some(h) = self.idb.borrow_mut().open_requests.remove(&req) else {
                    return;
                };
                let message = match failure {
                    snowlark_idb::Failure::Version => {
                        "The requested version is less than the existing version"
                    }
                    _ => "The database could not be opened",
                };
                self.fail_open_request(cx, h, failure_name(failure), message);
            }
            Msg::Deleted { req, result } => {
                let Some(h) = self.idb.borrow_mut().open_requests.remove(&req) else {
                    return;
                };
                match result {
                    Ok(old) => {
                        let global = self.req(h, |r| r.global);
                        self.req(h, |r| {
                            r.result = None;
                            r.done = true;
                        });
                        self.in_global(cx, global, |cx| {
                            self.fire_version_change(cx, h, "success", old, None);
                        });
                    }
                    Err(f) => self.fail_open_request(
                        cx,
                        h,
                        failure_name(f),
                        "The database could not be deleted",
                    ),
                }
            }
            Msg::Started { txn } => self.idb_started(txn),
            Msg::Committed { txn, result } => self.idb_committed(cx, txn, result),
            Msg::Answer { txn, seq, answer } => self.idb_answer(cx, txn, seq, answer),
            Msg::Estimate { id, usage, quota } => self.storage_estimated(cx, id, usage, quota),
            Msg::Databases { id, result } => {
                let Some(p) = self.idb.borrow_mut().lists.remove(&id) else {
                    return;
                };
                match result {
                    Ok(list) => {
                        let infos: Vec<IDBDatabaseInfo> = list
                            .into_iter()
                            .map(|(name, version)| IDBDatabaseInfo {
                                name: Some(name),
                                version: Some(version),
                            })
                            .collect();
                        p.resolve(cx, infos);
                    }
                    Err(f) => p.reject(
                        cx,
                        Error::dom(failure_name(f), "The databases could not be listed"),
                    ),
                }
            }
        }
    }

    /// A handle's wrapper as a value.
    pub(crate) fn handle_value(&self, cx: &mut Cx, h: Handle) -> snowlark_bindings::rt::JsValue {
        let scope = cx.scope();
        match rt::wrap(scope, h) {
            Some(o) => snowlark_bindings::rt::JsValue::new(scope, o.into()),
            None => rt::undefined_value(scope),
        }
    }

    fn fail_open_request(&self, cx: &mut Cx, h: Handle, name: ExceptionName, message: &str) {
        let global = self.req(h, |r| r.global);
        let err = self.idb_exception(name, message);
        self.req(h, |r| {
            r.result = None;
            r.error = Some(err);
            r.done = true;
        });
        self.in_global(cx, global, |cx| {
            self.fire_event(cx, h, "error", true, true);
        });
    }

    fn new_connection(&self, global: Handle, id: u64, schema: Database, version: u64) -> Handle {
        let name = schema.name.clone();
        let stores = schema
            .stores
            .values()
            .map(|s| (s.id, s.name.clone()))
            .collect();
        let h = self.idb_alloc(IdbObject::Database(ConnData {
            global,
            id,
            schema,
            name,
            version,
            stores,
            close_pending: false,
            closed: false,
            upgrade: None,
            live: Vec::new(),
        }));
        self.idb.borrow_mut().connections.insert(id, h);
        h
    }

    /// §5.7 "upgrade a database", the task: the connection and its upgrade transaction, then
    /// `upgradeneeded`.
    #[allow(clippy::too_many_arguments)]
    fn run_upgrade(
        &self,
        cx: &mut Cx,
        req: u64,
        conn: u64,
        txn: u64,
        schema: Database,
        old: u64,
        new: u64,
    ) {
        let Some(h) = self.idb.borrow().open_requests.get(&req).copied() else {
            // (the request's global went away: the upgrade ends)
            let (backend, _) = self.idb_backend();
            backend.upgrade_finished(txn, false);
            backend.connection_closed(conn);
            return;
        };
        let global = self.req(h, |r| r.global);
        let at_start = schema.clone();
        let c = self.new_connection(global, conn, schema, new);
        let t = self.new_transaction(
            global,
            c,
            txn,
            Mode::VersionChange,
            Durability::Default,
            Vec::new(),
            TxnState::Inactive,
            true,
        );
        self.txn(t, |t| {
            t.open_request = Some(h);
            t.old_version = old;
            t.schema_at_start = Some(at_start);
        });
        self.conn(c, |c| c.upgrade = Some(t));
        self.txn_apply(t, Change::Version(new));
        self.in_global(cx, global, |cx| {
            let v = self.handle_value(cx, c);
            self.req(h, |r| {
                r.result = Some(v);
                r.transaction = Some(t);
                r.done = true;
            });
            self.txn(t, |t| t.state = TxnState::Active);
            let threw = self.fire_version_change(cx, h, "upgradeneeded", old, Some(new));
            if self.txn_state(t) == TxnState::Active {
                self.txn(t, |t| t.state = TxnState::Inactive);
                if threw {
                    let err =
                        self.idb_exception(ExceptionName::AbortError, "An event listener threw");
                    self.abort_transaction(t, Some(err));
                } else {
                    self.maybe_autocommit(t);
                }
            }
        });
    }

    /// The open request's last steps once its upgrade transaction finished (§5.1 steps 10.6-11):
    /// the connection, or an "AbortError" when the upgrade aborted or the connection closed.
    pub(crate) fn upgrade_finished(
        &self,
        cx: &mut Cx,
        req: Handle,
        conn: Handle,
        txn: u64,
        committed: bool,
    ) {
        self.idb
            .borrow_mut()
            .open_requests
            .retain(|_, &mut r| r != req);
        let (backend, _) = self.idb_backend();
        backend.upgrade_finished(txn, committed);
        let closed = self.conn(conn, |c| c.close_pending);
        if !committed || closed {
            if !closed {
                self.conn(conn, |c| c.close_pending = true);
            }
            self.check_closed(conn);
            self.fail_open_request(
                cx,
                req,
                ExceptionName::AbortError,
                "The upgrade was aborted or the connection closed",
            );
        } else {
            let global = self.req(req, |r| r.global);
            self.in_global(cx, global, |cx| {
                let v = self.handle_value(cx, conn);
                self.req(req, |r| {
                    r.result = Some(v);
                    r.error = None;
                    r.done = true;
                });
                self.fire_event(cx, req, "success", false, false);
            });
        }
    }

    /// The upgrade transaction of a connection, or an "InvalidStateError".
    fn upgrade_of(&self, conn: Handle) -> rt::Result<Handle> {
        self.conn(conn, |c| c.upgrade).ok_or_else(|| {
            exception(
                ExceptionName::InvalidStateError,
                "Not in a version change transaction",
            )
        })
    }
}

impl Engine {
    /// A global's `IDBFactory` (`indexedDB`).
    pub(crate) fn idb_factory(&self, global: Handle) -> Handle {
        self.window_singleton(global, "indexedDB", || {
            Object::Idb(Box::new(IdbObject::Factory { global }))
        })
    }
}

impl IDBFactoryImpl for Engine {
    /// §4.3 `open(name, version)`.
    fn open(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        version: Option<u64>,
    ) -> rt::Result<Handle> {
        if version == Some(0) {
            return Err(Error::Type("The version must not be 0".into()));
        }
        let global = self.factory_global(this);
        let Some(key) = self.storage_key(global) else {
            return Err(exception(
                ExceptionName::SecurityError,
                "IndexedDB is not available in an opaque origin",
            ));
        };
        let req = self.new_open_request(global);
        let id = self.idb_next_id();
        let (backend, loop_id) = self.idb_backend();
        self.idb.borrow_mut().open_requests.insert(id, req);
        backend.open(loop_id, id, &key, name, version);
        Ok(req)
    }

    /// §4.3 `deleteDatabase(name)`.
    fn delete_database(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Handle> {
        let global = self.factory_global(this);
        let Some(key) = self.storage_key(global) else {
            return Err(exception(
                ExceptionName::SecurityError,
                "IndexedDB is not available in an opaque origin",
            ));
        };
        let req = self.new_open_request(global);
        let id = self.idb_next_id();
        let (backend, loop_id) = self.idb_backend();
        self.idb.borrow_mut().open_requests.insert(id, req);
        backend.delete_database(loop_id, id, &key, name);
        Ok(req)
    }

    /// §4.3 `databases()`.
    fn databases(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let global = self.factory_global(this);
        let Some(key) = self.storage_key(global) else {
            p.reject(
                cx,
                exception(
                    ExceptionName::SecurityError,
                    "IndexedDB is not available in an opaque origin",
                ),
            );
            return Ok(p);
        };
        let id = self.idb_next_id();
        let (backend, loop_id) = self.idb_backend();
        self.idb.borrow_mut().lists.insert(id, p.clone());
        backend.databases(loop_id, id, &key);
        Ok(p)
    }

    /// §4.3 `cmp(first, second)`.
    fn cmp(
        &self,
        cx: &mut Cx,
        _this: Handle,
        first: rt::JsValue,
        second: rt::JsValue,
    ) -> rt::Result<i16> {
        let a = self.js_to_key(cx, &first)?;
        let b = self.js_to_key(cx, &second)?;
        Ok(match snowlark_idb::compare(&a, &b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })
    }
}

/// A key path from its IDL value.
pub(crate) fn key_path_of(v: StringOrStringSequence) -> KeyPath {
    match v {
        StringOrStringSequence::String(s) => KeyPath::String(s),
        StringOrStringSequence::StringSequence(v) => KeyPath::Sequence(v),
    }
}

impl IDBDatabaseImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.conn(this, |c| c.name.clone()))
    }
    fn version(&self, this: Handle) -> u64 {
        self.conn(this, |c| c.version)
    }
    fn object_store_names(&self, this: Handle) -> Handle {
        let names = self.conn(this, |c| c.stores.iter().map(|(_, n)| n.clone()).collect());
        self.sorted_name_list(names)
    }

    /// §4.4 `transaction(storeNames, mode, options)`.
    fn transaction(
        &self,
        _cx: &mut Cx,
        this: Handle,
        store_names: StringOrStringSequence,
        mode: IDBTransactionMode,
        options: IDBTransactionOptions,
    ) -> rt::Result<Handle> {
        let (upgrade, close_pending, stores, id, global) = self.conn(this, |c| {
            (c.upgrade, c.close_pending, c.stores.clone(), c.id, c.global)
        });
        if upgrade.is_some() {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "A version change transaction is running",
            ));
        }
        if close_pending {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The connection is closing",
            ));
        }
        let mut names = match store_names {
            StringOrStringSequence::String(s) => vec![s],
            StringOrStringSequence::StringSequence(v) => v,
        };
        names.sort();
        names.dedup();
        let mut scope = Vec::new();
        for n in &names {
            match stores.iter().find(|(_, s)| s == n) {
                Some((sid, _)) => scope.push((*sid, n.clone())),
                None => {
                    return Err(exception(
                        ExceptionName::NotFoundError,
                        "One of the object stores was not found",
                    ))
                }
            }
        }
        if scope.is_empty() {
            return Err(exception(
                ExceptionName::InvalidAccessError,
                "The scope is empty",
            ));
        }
        let mode = match mode {
            IDBTransactionMode::Readonly => Mode::ReadOnly,
            IDBTransactionMode::Readwrite => Mode::ReadWrite,
            IDBTransactionMode::Versionchange => {
                return Err(Error::Type(
                    "A transaction's mode must be readonly or readwrite".into(),
                ))
            }
        };
        let durability = match options.durability {
            IDBTransactionDurability::Default => Durability::Default,
            IDBTransactionDurability::Strict => Durability::Strict,
            IDBTransactionDurability::Relaxed => Durability::Relaxed,
        };
        let (backend, _) = self.idb_backend();
        let tid = backend.create_transaction(id, mode, scope.iter().map(|(s, _)| *s).collect());
        let t = self.new_transaction(
            global,
            this,
            tid,
            mode,
            durability,
            scope,
            TxnState::Active,
            false,
        );
        self.txn(t, |t| t.cleanup = true);
        Ok(t)
    }

    /// §4.4 `close()`.
    fn close(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.conn(this, |c| c.close_pending = true);
        self.check_closed(this);
        Ok(())
    }

    /// §4.4 `createObjectStore(name, options)`.
    fn create_object_store(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        options: IDBObjectStoreParameters,
    ) -> rt::Result<Handle> {
        let t = self.upgrade_of(this)?;
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        let key_path = options.key_path.map(key_path_of);
        if key_path.as_ref().is_some_and(|p| !p.is_valid()) {
            return Err(exception(
                ExceptionName::SyntaxError,
                "The key path is not valid",
            ));
        }
        if self.conn(this, |c| c.stores.iter().any(|(_, n)| n == name)) {
            return Err(exception(
                ExceptionName::ConstraintError,
                "An object store with that name exists",
            ));
        }
        let auto_increment = options.auto_increment;
        if auto_increment && matches!(&key_path, Some(KeyPath::Sequence(_)))
            || auto_increment && matches!(&key_path, Some(KeyPath::String(s)) if s.is_empty())
        {
            return Err(exception(
                ExceptionName::InvalidAccessError,
                "A key generator cannot be used with an empty or array key path",
            ));
        }
        let id = self.conn(this, |c| c.schema.take_id());
        self.txn_apply(
            t,
            Change::CreateStore {
                id,
                name: name.to_owned(),
                key_path: key_path.clone(),
                auto_increment,
            },
        );
        self.conn(this, |c| c.stores.push((id, name.to_owned())));
        let global = self.conn(this, |c| c.global);
        let h = self.idb_alloc(IdbObject::Store(StoreHandle {
            global,
            txn: t,
            id,
            name: name.to_owned(),
            key_path,
            auto_increment,
            indexes: Vec::new(),
            index_handles: Vec::new(),
            key_path_js: None,
            created: true,
            deleted: false,
        }));
        self.txn(t, |t| t.handles.push((id, h)));
        Ok(h)
    }

    /// §4.4 `deleteObjectStore(name)`.
    fn delete_object_store(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<()> {
        let t = self.upgrade_of(this)?;
        if self.txn_state(t) != TxnState::Active {
            return Err(inactive());
        }
        let Some(id) = self.conn(this, |c| {
            c.stores.iter().find(|(_, n)| n == name).map(|(id, _)| *id)
        }) else {
            return Err(exception(
                ExceptionName::NotFoundError,
                "No object store has that name",
            ));
        };
        self.conn(this, |c| c.stores.retain(|(s, _)| *s != id));
        if let Some(h) = self.txn(t, |t| {
            t.handles.iter().find(|(s, _)| *s == id).map(|(_, h)| *h)
        }) {
            self.store_h(h, |s| {
                s.indexes.clear();
                s.deleted = true;
            });
        }
        // (the store's destruction is processed as a request: the requests placed before it run
        // against the store)
        self.place_operation(
            t,
            Box::new(move |e: &Engine, cx: &mut Cx| {
                e.txn_apply(t, Change::DeleteStore(id));
                Step::Done(Ok(rt::undefined_value(cx.scope())))
            }),
        );
        Ok(())
    }
}

/// `IDBOpenDBRequest` adds only event handlers (`onblocked`, `onupgradeneeded`).
impl snowlark_bindings::generated::IDBOpenDBRequestImpl for Engine {}

impl IDBVersionChangeEventImpl for Engine {
    fn old_version(&self, this: Handle) -> u64 {
        self.ev(this, |e| match &e.ext {
            EventExt::VersionChange { old, .. } => *old,
            _ => 0,
        })
    }
    fn new_version(&self, this: Handle) -> Option<u64> {
        self.ev(this, |e| match &e.ext {
            EventExt::VersionChange { new, .. } => *new,
            _ => None,
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: IDBVersionChangeEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::IDBVersionChangeEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::VersionChange {
                old: init.old_version,
                new: init.new_version,
            },
        ))
    }
}
