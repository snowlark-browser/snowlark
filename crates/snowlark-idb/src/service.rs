// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What an event loop needs of the IndexedDB backend, as a trait: the seam where the backend can
//! move to another process (`docs/process-isolation.md`, "IndexedDB: where a transaction's working
//! state lives"). A page holds a [`Backend`] and reaches the databases only through it: it keeps a
//! copy of each connection's schema, and every record it reads or writes goes through a call
//! naming its transaction, which the backend checks.
//!
//! [`LocalIdb`] is the backend in this process. No call waits for an answer: what the spec does "in
//! parallel" (opens, deletes, starts, commits, and every request's operation) comes back through the
//! loop's [`Inbox`]. The id of a new transaction comes back from the call that creates it, at once:
//! the backend's here, one the caller's process names itself across a pipe.

use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::{Durability, Inbox, LocalIdb, LoopId, Mode};
use crate::db::Change;
use crate::request::Request;

/// The IndexedDB backend as an event loop sees it. See [`LocalIdb`] for what each call does.
pub trait IdbService: Send + Sync {
    fn register_loop(&self, inbox: &Arc<Inbox>) -> LoopId;
    fn loop_gone(&self, loop_: LoopId);
    fn open(&self, loop_: LoopId, req: u64, key: &str, name: &str, version: Option<u64>);
    fn delete_database(&self, loop_: LoopId, req: u64, key: &str, name: &str);
    fn databases(&self, loop_: LoopId, id: u64, key: &str);
    fn estimate(&self, loop_: LoopId, id: u64, key: &str);
    fn versionchange_fired(&self, conn: u64);
    fn connection_closed(&self, conn: u64);
    fn upgrade_finished(&self, txn: u64, committed: bool);
    fn create_transaction(&self, conn: u64, mode: Mode, scope: Vec<u64>) -> u64;
    fn commit(&self, txn: u64, durability: Durability);
    fn transaction_finished(&self, txn: u64);

    /// A schema change in an upgrade transaction, in order with its requests; nothing answers (a
    /// page that follows the spec makes none the backend refuses).
    fn change(&self, txn: u64, change: Change);
    fn revert(&self, txn: u64);
    /// A read or write in a transaction, in order with the transaction's other calls: the answer
    /// comes to `loop_` as [`crate::Msg::Answer`] with `seq` ([`crate::request`]).
    fn request(&self, loop_: LoopId, txn: u64, seq: u64, request: Request);

    /// Waits until everything queued so far is written (tests, shutdown).
    fn flush(&self);
}

impl IdbService for LocalIdb {
    fn register_loop(&self, inbox: &Arc<Inbox>) -> LoopId {
        LocalIdb::register_loop(self, inbox)
    }
    fn loop_gone(&self, loop_: LoopId) {
        LocalIdb::loop_gone(self, loop_)
    }
    fn open(&self, loop_: LoopId, req: u64, key: &str, name: &str, version: Option<u64>) {
        LocalIdb::open(self, loop_, req, key, name, version)
    }
    fn delete_database(&self, loop_: LoopId, req: u64, key: &str, name: &str) {
        LocalIdb::delete_database(self, loop_, req, key, name)
    }
    fn databases(&self, loop_: LoopId, id: u64, key: &str) {
        LocalIdb::databases(self, loop_, id, key)
    }
    fn estimate(&self, loop_: LoopId, id: u64, key: &str) {
        LocalIdb::estimate(self, loop_, id, key)
    }
    fn versionchange_fired(&self, conn: u64) {
        LocalIdb::versionchange_fired(self, conn)
    }
    fn connection_closed(&self, conn: u64) {
        LocalIdb::connection_closed(self, conn)
    }
    fn upgrade_finished(&self, txn: u64, committed: bool) {
        LocalIdb::upgrade_finished(self, txn, committed)
    }
    fn create_transaction(&self, conn: u64, mode: Mode, scope: Vec<u64>) -> u64 {
        LocalIdb::create_transaction(self, conn, mode, scope)
    }
    fn commit(&self, txn: u64, durability: Durability) {
        LocalIdb::commit(self, txn, durability)
    }
    fn transaction_finished(&self, txn: u64) {
        LocalIdb::transaction_finished(self, txn)
    }
    fn change(&self, txn: u64, change: Change) {
        let r = LocalIdb::apply(self, txn, change, false);
        debug_assert!(r.is_ok(), "the backend refused a change: {r:?}");
    }
    fn revert(&self, txn: u64) {
        LocalIdb::revert(self, txn)
    }
    fn request(&self, loop_: LoopId, txn: u64, seq: u64, request: Request) {
        LocalIdb::request(self, loop_, txn, seq, request)
    }
    fn flush(&self) {
        LocalIdb::flush(self)
    }
}

/// The handle to a profile's IndexedDB backend that pages and workers share.
#[derive(Clone)]
pub struct Backend(Arc<dyn IdbService>);

impl Backend {
    /// A backend in this process that keeps nothing on disk.
    pub fn in_memory() -> Backend {
        Backend(Arc::new(LocalIdb::in_memory()))
    }

    /// [`Backend::in_memory`], reporting `quota` from `estimate()` ([`LocalIdb::set_quota`]).
    pub fn in_memory_with_quota(quota: u64) -> Backend {
        let idb = LocalIdb::in_memory();
        idb.set_quota(quota);
        Backend(Arc::new(idb))
    }

    /// A backend in this process that keeps its databases in `dir`.
    pub fn persistent(dir: PathBuf) -> Backend {
        Backend(Arc::new(LocalIdb::persistent(dir)))
    }

    /// [`Backend::persistent`], reporting `quota` from `estimate()` ([`LocalIdb::set_quota`]).
    pub fn persistent_with_quota(dir: PathBuf, quota: u64) -> Backend {
        let idb = LocalIdb::persistent(dir);
        idb.set_quota(quota);
        Backend(Arc::new(idb))
    }

    /// A handle to another implementation (a backend in another process).
    pub fn new(service: Arc<dyn IdbService>) -> Backend {
        Backend(service)
    }

    /// The implementation behind the handle (what the browser serves a renderer process from).
    pub fn service(&self) -> &Arc<dyn IdbService> {
        &self.0
    }
}

impl Default for Backend {
    fn default() -> Self {
        Backend::in_memory()
    }
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend").finish_non_exhaustive()
    }
}

impl std::ops::Deref for Backend {
    type Target = dyn IdbService;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
