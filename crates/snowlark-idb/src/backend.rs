// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The backend every event loop of a profile shares: the databases of each storage key, the
//! connection queues (IndexedDB §2.8.2) that run opens and deletes in order with their
//! `versionchange` / `blocked` steps, the open connections, and transaction scheduling (§2.7.2).
//!
//! Event loops talk to it by calling its methods; it answers through each loop's [`Inbox`], which the
//! loop drains into tasks. The steps the spec runs "in parallel" live here; the steps it queues as
//! database tasks run in the event loops. File I/O runs on the disk thread ([`crate::disk`]), whose
//! results come back through the same methods.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use serde::{Deserialize, Serialize};

use crate::db::{encode_change, Change, Database, Direction, Found, Record, Seek, Source, Undo};
use crate::disk::Disk;
use crate::key::{Key, KeyRange};
use crate::request::Answer;

/// A database shared by its connections. Only the transactions scheduling lets run touch it.
pub type DbRef = Arc<Mutex<Database>>;

/// Locks a database (a panic while it was locked leaves it usable: every change is applied whole).
pub fn lock(db: &DbRef) -> MutexGuard<'_, Database> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// An event loop's id in the backend.
pub type LoopId = u64;

/// The DOMException a backend step fails with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    Version,
    Abort,
    Unknown,
    Quota,
}

/// A transaction's mode (§2.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    ReadOnly,
    ReadWrite,
    VersionChange,
}

/// A transaction's durability hint (§2.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Durability {
    Default,
    Strict,
    Relaxed,
}

/// The storage quota `estimate()` reports for a storage key when the backend was given none
/// ([`LocalIdb::set_quota`]): the Storage Standard leaves the quota to the user agent. Nothing
/// enforces it yet.
pub const QUOTA: u64 = 10 << 30;

/// The quota of each storage key on a profile whose volume is `volume_size` bytes: of the volume, a
/// pool of 80% for all sites, and 75% of the pool for any one site (the policy the engine family
/// follows; the space that must stay free plays no part in a site's quota).
pub fn quota_for_volume(volume_size: u64) -> u64 {
    let pool = (volume_size as f64 * 0.8) as u64;
    (pool as f64 * 0.75) as u64
}

/// The quota of each storage key on a profile kept in memory (an Incognito window's), on a machine
/// with `physical_memory` bytes: a pool of 15-20% of the memory, the share drawn from `unit` (a
/// random number in [0, 1), drawn once per profile, so the quota does not name the machine's memory
/// exactly), and a third of the pool for any one site (the policy the engine family follows).
pub fn quota_in_memory(physical_memory: u64, unit: f64) -> u64 {
    const LOWER: f64 = 0.15;
    const UPPER: f64 = 0.2;
    let ratio = LOWER + unit.clamp(0.0, 1.0) * (UPPER - LOWER);
    let pool = (physical_memory as f64 * ratio) as u64;
    pool / 3
}

/// What the backend tells an event loop.
#[derive(Debug, Serialize, Deserialize)]
pub enum Msg {
    /// Fire `versionchange` at a connection, then call [`LocalIdb::versionchange_fired`].
    VersionChange {
        conn: u64,
        old: u64,
        new: Option<u64>,
    },
    /// Fire `blocked` at an open (or delete) request.
    Blocked {
        req: u64,
        old: u64,
        new: Option<u64>,
    },
    /// Run the upgrade of an open request (§5.7): the connection and its upgrade transaction.
    Upgrade {
        req: u64,
        conn: u64,
        txn: u64,
        /// The database's schema; the page keeps it as its copy for the connection.
        schema: Database,
        old: u64,
        new: u64,
    },
    /// An open request succeeded without an upgrade.
    Opened {
        req: u64,
        conn: u64,
        schema: Database,
        version: u64,
    },
    OpenFailed {
        req: u64,
        failure: Failure,
    },
    /// A delete request finished: the database's old version (0 when there was none).
    Deleted {
        req: u64,
        result: Result<u64, Failure>,
    },
    /// A transaction may start.
    Started {
        txn: u64,
    },
    /// A transaction's changes are written (or failed to be).
    Committed {
        txn: u64,
        result: Result<(), Failure>,
    },
    /// The answer to `databases()`: names and versions.
    Databases {
        id: u64,
        result: Result<Vec<(String, u64)>, Failure>,
    },
    /// The answer to `estimate()`: the bytes the storage key's databases take on disk, and the
    /// quota the backend reports for the key ([`LocalIdb::estimate`]).
    Estimate {
        id: u64,
        usage: u64,
        quota: u64,
    },
    /// The answer to request `seq` of a transaction ([`crate::request`]).
    Answer {
        txn: u64,
        seq: u64,
        answer: Answer,
    },
}

/// An event loop's inbox, and how to wake its thread.
pub struct Inbox {
    queue: Mutex<VecDeque<Msg>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Inbox {
    pub fn new(wake: Box<dyn Fn() + Send + Sync>) -> Inbox {
        Inbox {
            queue: Mutex::new(VecDeque::new()),
            wake,
        }
    }

    /// Whether messages are waiting.
    pub fn has_messages(&self) -> bool {
        !self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// The messages that arrived, in order.
    pub fn drain(&self) -> Vec<Msg> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// Adds a message and wakes the loop's thread: the backend's, or the client of a backend in
    /// another process delivering what that backend posted.
    pub fn post(&self, m: Msg) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(m);
        (self.wake)();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Open(Option<u64>),
    Delete,
}

/// An open or delete request in a connection queue.
#[derive(Clone, Copy, Debug)]
struct Item {
    loop_: LoopId,
    req: u64,
    kind: ItemKind,
}

/// Where the request at the head of a connection queue stands.
enum Stage {
    Idle,
    /// Waiting for `versionchange` events to be fired, then for the other connections to close.
    Waiting {
        item: Item,
        /// The new connection of an open.
        conn: Option<u64>,
        old: u64,
        new: Option<u64>,
        acks: Vec<u64>,
        blocked: bool,
        created: bool,
    },
    /// The upgrade transaction of an open runs (from version `old`).
    Upgrading {
        txn: u64,
        created: bool,
        old: u64,
    },
    /// A delete waits for the disk.
    Deleting {
        item: Item,
        old: u64,
    },
}

struct Conn {
    id: u64,
    loop_: LoopId,
}

struct Txn {
    id: u64,
    loop_: LoopId,
    mode: Mode,
    scope: Vec<u64>,
    started: bool,
}

/// One database name in one storage key.
struct Slot {
    db: Option<DbRef>,
    queue: VecDeque<Item>,
    stage: Stage,
    conns: Vec<Conn>,
    /// Live transactions, in creation order.
    txns: Vec<Txn>,
}

impl Slot {
    fn new(db: Option<DbRef>) -> Slot {
        Slot {
            db,
            queue: VecDeque::new(),
            stage: Stage::Idle,
            conns: Vec::new(),
            txns: Vec::new(),
        }
    }
}

type SlotKey = (String, String);

#[derive(Default)]
struct KeyState {
    loaded: bool,
    loading: bool,
    /// `databases()` calls waiting for the load: (loop, id).
    lists: Vec<(LoopId, u64)>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    loops: HashMap<LoopId, Weak<Inbox>>,
    keys: HashMap<String, KeyState>,
    slots: HashMap<SlotKey, Slot>,
    /// Connection id → its slot; transaction id → its slot.
    conn_slot: HashMap<u64, SlotKey>,
    txn_slot: HashMap<u64, SlotKey>,
}

impl State {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn post(&self, loop_: LoopId, m: Msg) {
        if let Some(inbox) = self.loops.get(&loop_).and_then(Weak::upgrade) {
            inbox.post(m);
        }
    }
}

struct Shared {
    state: Mutex<State>,
    /// What each transaction did to its database. Locked after a database, never before one, and
    /// never with `state` taken after it.
    logs: Mutex<HashMap<u64, TxnLog>>,
    disk: Option<Disk>,
    /// The quota `estimate()` reports ([`LocalIdb::set_quota`]).
    quota: std::sync::atomic::AtomicU64,
}

/// A transaction's working state: the changes to write when it commits, and what reverts them.
#[derive(Default)]
struct TxnLog {
    changes: Vec<Change>,
    undo: Vec<Undo>,
    /// Its changes went to be written: a page that goes away no longer reverts them.
    committing: bool,
}

/// The backend refused a call that does not fit its transaction (unknown, not started, outside
/// its scope or its mode). A page that follows the spec never sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused;

/// Why a write failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteError {
    Refused,
    /// `add()` over a record with the same key (a "ConstraintError").
    KeyExists,
    /// A unique index has the record's key for another record (a "ConstraintError").
    NotUnique,
    /// The store's key generator is past 2^53 (a "ConstraintError").
    NoKeyLeft,
}

impl From<Refused> for WriteError {
    fn from(_: Refused) -> WriteError {
        WriteError::Refused
    }
}

/// What a call does with a transaction's database.
enum Access {
    Read(u64),
    Write(u64),
    Schema,
}

fn store_of(source: Source) -> u64 {
    match source {
        Source::Store(s) | Source::Index(s, _) => s,
    }
}

/// The backend (cheap to clone: every clone is the same backend).
#[derive(Clone)]
pub struct LocalIdb(Arc<Shared>);

impl std::fmt::Debug for LocalIdb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalIdb").finish_non_exhaustive()
    }
}

impl Default for LocalIdb {
    fn default() -> Self {
        LocalIdb::in_memory()
    }
}

impl LocalIdb {
    /// A backend whose databases live in memory only.
    pub fn in_memory() -> LocalIdb {
        LocalIdb(Arc::new(Shared {
            state: Mutex::new(State::default()),
            logs: Mutex::default(),
            disk: None,
            quota: QUOTA.into(),
        }))
    }

    /// A backend whose databases persist under `dir` (one folder per storage key, one log file
    /// per database).
    pub fn persistent(dir: PathBuf) -> LocalIdb {
        LocalIdb(Arc::new(Shared {
            state: Mutex::new(State::default()),
            logs: Mutex::default(),
            disk: Some(Disk::start(dir)),
            quota: QUOTA.into(),
        }))
    }

    /// Sets the quota `estimate()` reports for every storage key (the embedder's, from the
    /// profile's volume: [`quota_for_volume`]).
    pub fn set_quota(&self, quota: u64) {
        self.0
            .quota
            .store(quota, std::sync::atomic::Ordering::Relaxed);
    }

    fn quota(&self) -> u64 {
        self.0.quota.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Posts `m` to an event loop's inbox (none if the loop is gone).
    pub(crate) fn post(&self, loop_: LoopId, m: Msg) {
        self.state().post(loop_, m);
    }

    fn logs(&self) -> MutexGuard<'_, HashMap<u64, TxnLog>> {
        self.0.logs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The database of a started transaction, if the transaction may do `access` there.
    fn txn_db(&self, txn: u64, access: Access) -> Result<DbRef, Refused> {
        let st = self.state();
        let sk = st.txn_slot.get(&txn).ok_or(Refused)?;
        let slot = st.slots.get(sk).ok_or(Refused)?;
        let t = slot.txns.iter().find(|t| t.id == txn).ok_or(Refused)?;
        let upgrade = t.mode == Mode::VersionChange;
        let allowed = match access {
            Access::Read(s) => upgrade || t.scope.contains(&s),
            Access::Write(s) => upgrade || (t.mode == Mode::ReadWrite && t.scope.contains(&s)),
            Access::Schema => upgrade,
        };
        if !t.started || !allowed {
            return Err(Refused);
        }
        slot.db.clone().ok_or(Refused)
    }

    // ----- a transaction's reads and writes ----------------------------------------------------------

    /// Applies a change in a transaction, keeping what reverts it. A `Put` fails if `add` and the
    /// key has a record, or if a unique index has one of its keys for another record; in a store
    /// with a key generator, it also updates the generator (§2.11 "possibly update the key generator").
    pub fn apply(&self, txn: u64, change: Change, add: bool) -> Result<(), WriteError> {
        let access = match &change {
            Change::Put { store, .. }
            | Change::DeleteRange(store, _)
            | Change::Clear(store)
            | Change::Generator(store, _) => Access::Write(*store),
            _ => Access::Schema,
        };
        let db = self.txn_db(txn, access)?;
        let mut d = lock(&db);
        let mut logs = self.logs();
        let log = logs.entry(txn).or_default();
        if log.committing {
            return Err(WriteError::Refused);
        }
        if let Some(id) = Database::created_id(&change) {
            // (ids are never reused, so a stale handle never names a new store or index)
            if id < d.next_id {
                return Err(WriteError::Refused);
            }
            d.next_id = id + 1;
        }
        if let Change::Put { store, key, record } = &change {
            let s = d.store(*store).ok_or(Refused)?;
            if add && s.records.contains_key(key) {
                return Err(WriteError::KeyExists);
            }
            for (id, keys) in &record.index_keys {
                if let Some(ix) = s.indexes.get(id).filter(|ix| ix.unique) {
                    if keys.iter().any(|k| ix.has_other(k, key)) {
                        return Err(WriteError::NotUnique);
                    }
                }
            }
            if let Some(g) = s.auto_increment.then(|| s.updated_generator(key)).flatten() {
                let g = Change::Generator(*store, g);
                log.undo.push(d.apply(g.clone()));
                log.changes.push(g);
            }
        }
        log.undo.push(d.apply(change.clone()));
        log.changes.push(change);
        Ok(())
    }

    /// Reverts what a transaction changed (§5.5 "abort a transaction").
    pub fn revert(&self, txn: u64) {
        let db = {
            let st = self.state();
            st.txn_slot
                .get(&txn)
                .and_then(|sk| st.slots.get(sk))
                .and_then(|s| s.db.clone())
        };
        let Some(db) = db else {
            self.logs().remove(&txn);
            return;
        };
        let mut d = lock(&db);
        let undo = match self.logs().get_mut(&txn) {
            Some(log) => {
                log.changes.clear();
                std::mem::take(&mut log.undo)
            }
            None => return,
        };
        for u in undo.into_iter().rev() {
            d.revert(u);
        }
    }

    /// The first record of a store or index in `range`.
    pub fn first(
        &self,
        txn: u64,
        source: Source,
        range: &KeyRange,
    ) -> Result<Option<Found>, Refused> {
        let db = self.txn_db(txn, Access::Read(store_of(source)))?;
        let found = lock(&db).first(source, range);
        Ok(found)
    }

    /// The records of a store or index in `range` in `direction`, at most `count` (0: all).
    pub fn get_all(
        &self,
        txn: u64,
        source: Source,
        range: &KeyRange,
        direction: Direction,
        count: u32,
    ) -> Result<Vec<Found>, Refused> {
        let db = self.txn_db(txn, Access::Read(store_of(source)))?;
        let found = lock(&db).get_all(source, range, direction, count);
        Ok(found)
    }

    /// How many records a store or index has in `range`.
    pub fn count(&self, txn: u64, source: Source, range: &KeyRange) -> Result<u64, Refused> {
        let db = self.txn_db(txn, Access::Read(store_of(source)))?;
        let n = lock(&db).count(source, range);
        Ok(n)
    }

    /// `count` steps of a cursor from `seek` ([`Database::cursor_advance`]).
    pub fn cursor_advance(
        &self,
        txn: u64,
        source: Source,
        direction: Direction,
        range: &KeyRange,
        seek: Seek,
        count: u32,
    ) -> Result<Option<Found>, Refused> {
        let db = self.txn_db(txn, Access::Read(store_of(source)))?;
        let found = lock(&db).cursor_advance(source, direction, range, seek, count);
        Ok(found)
    }

    /// The key a store's key generator gives next, or `None` past 2^53 (§2.11 "generate a key").
    pub fn next_key(&self, txn: u64, store: u64) -> Result<Option<u64>, Refused> {
        let db = self.txn_db(txn, Access::Write(store))?;
        let d = lock(&db);
        Ok(d.store(store).ok_or(Refused)?.generate_key())
    }

    /// §6.1 "store a record into an object store" for a store with a key generator and no key
    /// given: generates the key (§2.11), completes the index keys that depend on it (`fill`) and
    /// stores the record ([`LocalIdb::apply`]). The record's value is stored as the page gave it:
    /// an in-line key is injected into it when it is read. The key generated.
    pub fn put_generated(
        &self,
        txn: u64,
        store: u64,
        mut record: Record,
        fill: Vec<(u64, crate::request::Fill)>,
        add: bool,
    ) -> Result<u64, WriteError> {
        let generated = self.next_key(txn, store)?.ok_or(WriteError::NoKeyLeft)?;
        let key = Key::Number(generated as f64);
        for (index, f) in fill {
            record.index_keys.push((index, vec![f.with(&key)]));
        }
        self.apply(txn, Change::Put { store, key, record }, add)?;
        Ok(generated)
    }

    /// Registers an event loop; the backend posts to `inbox` until the loop is gone.
    pub fn register_loop(&self, inbox: &Arc<Inbox>) -> LoopId {
        let mut st = self.state();
        let id = st.id();
        st.loops.insert(id, Arc::downgrade(inbox));
        id
    }

    /// An event loop is gone: its connections close, its transactions end, its queued requests go.
    /// (The loop rolled back what its live transactions changed.)
    pub fn loop_gone(&self, loop_: LoopId) {
        let mut st = self.state();
        st.loops.remove(&loop_);
        let keys: Vec<SlotKey> = st.slots.keys().cloned().collect();
        for key in keys {
            let mut upgrade_done = None;
            {
                let slot = st.slots.get_mut(&key).expect("a slot");
                slot.queue.retain(|i| i.loop_ != loop_);
                let gone_conns: Vec<u64> = slot
                    .conns
                    .iter()
                    .filter(|c| c.loop_ == loop_)
                    .map(|c| c.id)
                    .collect();
                slot.conns.retain(|c| c.loop_ != loop_);
                let gone_txns: Vec<u64> = slot
                    .txns
                    .iter()
                    .filter(|t| t.loop_ == loop_)
                    .map(|t| t.id)
                    .collect();
                slot.txns.retain(|t| t.loop_ != loop_);
                // (what the page had not committed is reverted: it can no longer finish it)
                if let Some(db) = slot.db.clone() {
                    let mut d = lock(&db);
                    let mut logs = self.logs();
                    for t in &gone_txns {
                        if let Some(log) = logs.remove(t).filter(|l| !l.committing) {
                            for u in log.undo.into_iter().rev() {
                                d.revert(u);
                            }
                        }
                    }
                }
                match &mut slot.stage {
                    Stage::Waiting { item, acks, .. } => {
                        if item.loop_ == loop_ {
                            slot.stage = Stage::Idle;
                        } else {
                            acks.retain(|a| !gone_conns.contains(a));
                        }
                    }
                    Stage::Upgrading { txn, .. } if gone_txns.contains(txn) => {
                        upgrade_done = Some(*txn);
                    }
                    _ => {}
                }
                for c in gone_conns {
                    st.conn_slot.remove(&c);
                }
                for t in gone_txns {
                    st.txn_slot.remove(&t);
                }
            }
            if let Some(txn) = upgrade_done {
                Self::finish_upgrade(&mut st, &key, txn, false);
            }
            self.schedule(&mut st, &key);
            self.pump(&mut st, &key);
        }
    }

    // ----- opening and deleting --------------------------------------------------------------------

    /// §4.3 `open()`'s in-parallel steps: queues the request in its connection queue.
    pub fn open(&self, loop_: LoopId, req: u64, key: &str, name: &str, version: Option<u64>) {
        self.enqueue(
            loop_,
            key,
            name,
            Item {
                loop_,
                req,
                kind: ItemKind::Open(version),
            },
        );
    }

    /// §4.3 `deleteDatabase()`'s in-parallel steps.
    pub fn delete_database(&self, loop_: LoopId, req: u64, key: &str, name: &str) {
        self.enqueue(
            loop_,
            key,
            name,
            Item {
                loop_,
                req,
                kind: ItemKind::Delete,
            },
        );
    }

    /// §4.3 `databases()`'s in-parallel steps: answers with a [`Msg::Databases`] carrying `id`.
    pub fn databases(&self, loop_: LoopId, id: u64, key: &str) {
        let mut st = self.state();
        if self.ensure_loaded(&mut st, key) {
            Self::answer_list(&st, loop_, id, key);
        } else if let Some(k) = st.keys.get_mut(key) {
            k.lists.push((loop_, id));
        }
    }

    /// The Storage Standard's `estimate()`: the storage usage of a storage key's databases, what
    /// their files take once the writes queued so far are done (0 without a disk), and its storage
    /// quota ([`LocalIdb::set_quota`]). Answers with a [`Msg::Estimate`] carrying `id`.
    pub fn estimate(&self, loop_: LoopId, id: u64, key: &str) {
        let quota = self.quota();
        let Some(disk) = self.0.disk.as_ref() else {
            let m = Msg::Estimate {
                id,
                usage: 0,
                quota,
            };
            self.state().post(loop_, m);
            return;
        };
        let weak = Arc::downgrade(&self.0);
        disk.usage(
            key,
            Box::new(move |usage| {
                if let Some(shared) = weak.upgrade() {
                    let m = Msg::Estimate { id, usage, quota };
                    LocalIdb(shared).state().post(loop_, m);
                }
            }),
        );
    }

    fn answer_list(st: &State, loop_: LoopId, id: u64, key: &str) {
        let mut list: Vec<(String, u64)> = st
            .slots
            .iter()
            .filter(|((k, _), _)| k == key)
            .filter_map(|((_, name), slot)| {
                // (a database being upgraded is listed with its version before the upgrade)
                let v = match slot.stage {
                    Stage::Upgrading { old, .. } => old,
                    _ => lock(slot.db.as_ref()?).version,
                };
                (v > 0).then(|| (name.clone(), v))
            })
            .collect();
        list.sort();
        st.post(
            loop_,
            Msg::Databases {
                id,
                result: Ok(list),
            },
        );
    }

    /// Whether the storage key's databases are in memory; starts loading them if not.
    fn ensure_loaded(&self, st: &mut State, key: &str) -> bool {
        let disk = self.0.disk.as_ref();
        let k = st.keys.entry(key.to_owned()).or_default();
        if k.loaded {
            return true;
        }
        let Some(disk) = disk else {
            k.loaded = true;
            return true;
        };
        if !k.loading {
            k.loading = true;
            let weak = Arc::downgrade(&self.0);
            let key_owned = key.to_owned();
            disk.load_key(
                key,
                Box::new(move |dbs| {
                    if let Some(shared) = weak.upgrade() {
                        LocalIdb(shared).key_loaded(&key_owned, dbs);
                    }
                }),
            );
        }
        false
    }

    /// The disk thread read a storage key's databases.
    fn key_loaded(&self, key: &str, dbs: Vec<Database>) {
        let mut st = self.state();
        for db in dbs {
            let sk = (key.to_owned(), db.name.clone());
            let slot = st.slots.entry(sk).or_insert_with(|| Slot::new(None));
            if slot.db.is_none() {
                slot.db = Some(Arc::new(Mutex::new(db)));
            }
        }
        let lists = {
            let k = st.keys.entry(key.to_owned()).or_default();
            k.loaded = true;
            k.loading = false;
            std::mem::take(&mut k.lists)
        };
        for (loop_, id) in lists {
            Self::answer_list(&st, loop_, id, key);
        }
        let slots: Vec<SlotKey> = st.slots.keys().filter(|(k, _)| k == key).cloned().collect();
        for sk in slots {
            self.pump(&mut st, &sk);
        }
    }

    fn enqueue(&self, loop_: LoopId, key: &str, name: &str, item: Item) {
        let mut st = self.state();
        let sk = (key.to_owned(), name.to_owned());
        st.slots
            .entry(sk.clone())
            .or_insert_with(|| Slot::new(None))
            .queue
            .push_back(item);
        let _ = loop_;
        if self.ensure_loaded(&mut st, key) {
            self.pump(&mut st, &sk);
        }
    }

    /// Runs the connection queue of a slot as far as it can go (§5.1, §5.3).
    fn pump(&self, st: &mut State, sk: &SlotKey) {
        if !st.keys.get(&sk.0).is_some_and(|k| k.loaded) {
            return;
        }
        loop {
            let Some(slot) = st.slots.get(sk) else {
                return;
            };
            match &slot.stage {
                Stage::Idle => {
                    let Some(item) = st.slots.get_mut(sk).and_then(|s| s.queue.pop_front()) else {
                        return;
                    };
                    self.start_item(st, sk, item);
                }
                Stage::Waiting { acks, .. } if !acks.is_empty() => return,
                Stage::Waiting { .. } => {
                    if !self.after_versionchange(st, sk) {
                        return;
                    }
                }
                Stage::Upgrading { .. } | Stage::Deleting { .. } => return,
            }
        }
    }

    /// The first steps of an open or delete at the head of the queue.
    fn start_item(&self, st: &mut State, sk: &SlotKey, item: Item) {
        let new_conn = st.id();
        let slot = st.slots.get_mut(sk).expect("a slot");
        match item.kind {
            ItemKind::Open(version) => {
                let created = slot.db.is_none();
                let db = slot
                    .db
                    .get_or_insert_with(|| Arc::new(Mutex::new(Database::new(&sk.1))))
                    .clone();
                let current = lock(&db).version;
                let version = version.unwrap_or(if created { 1 } else { current });
                if current > version {
                    if created {
                        slot.db = None;
                    }
                    st.post(
                        item.loop_,
                        Msg::OpenFailed {
                            req: item.req,
                            failure: Failure::Version,
                        },
                    );
                    return;
                }
                if current == version {
                    slot.conns.push(Conn {
                        id: new_conn,
                        loop_: item.loop_,
                    });
                    st.conn_slot.insert(new_conn, sk.clone());
                    st.post(
                        item.loop_,
                        Msg::Opened {
                            req: item.req,
                            conn: new_conn,
                            schema: lock(&db).schema(),
                            version,
                        },
                    );
                    return;
                }
                let acks: Vec<u64> = slot.conns.iter().map(|c| c.id).collect();
                slot.stage = Stage::Waiting {
                    item,
                    conn: Some(new_conn),
                    old: current,
                    new: Some(version),
                    acks: acks.clone(),
                    blocked: false,
                    created,
                };
                let posts: Vec<(LoopId, u64)> =
                    slot.conns.iter().map(|c| (c.loop_, c.id)).collect();
                for (l, c) in posts {
                    st.post(
                        l,
                        Msg::VersionChange {
                            conn: c,
                            old: current,
                            new: Some(version),
                        },
                    );
                }
            }
            ItemKind::Delete => {
                let Some(db) = slot.db.clone() else {
                    st.post(
                        item.loop_,
                        Msg::Deleted {
                            req: item.req,
                            result: Ok(0),
                        },
                    );
                    return;
                };
                let current = lock(&db).version;
                let acks: Vec<u64> = slot.conns.iter().map(|c| c.id).collect();
                slot.stage = Stage::Waiting {
                    item,
                    conn: None,
                    old: current,
                    new: None,
                    acks,
                    blocked: false,
                    created: false,
                };
                let posts: Vec<(LoopId, u64)> =
                    slot.conns.iter().map(|c| (c.loop_, c.id)).collect();
                for (l, c) in posts {
                    st.post(
                        l,
                        Msg::VersionChange {
                            conn: c,
                            old: current,
                            new: None,
                        },
                    );
                }
            }
        }
    }

    /// Every `versionchange` was fired: `blocked` while connections stay open, then the upgrade or
    /// the deletion. Returns whether the queue can move on.
    fn after_versionchange(&self, st: &mut State, sk: &SlotKey) -> bool {
        let txn_id = st.id();
        let slot = st.slots.get_mut(sk).expect("a slot");
        let Stage::Waiting {
            item,
            conn,
            old,
            new,
            blocked,
            created,
            ..
        } = &mut slot.stage
        else {
            return true;
        };
        let (item, conn, old, new, created) = (*item, *conn, *old, *new, *created);
        if !slot.conns.is_empty() {
            if !*blocked {
                *blocked = true;
                st.post(
                    item.loop_,
                    Msg::Blocked {
                        req: item.req,
                        old,
                        new,
                    },
                );
            }
            return false;
        }
        let db = slot.db.clone().expect("a database");
        match (item.kind, conn, new) {
            (ItemKind::Open(_), Some(conn), Some(new)) => {
                let scope = lock(&db).stores.keys().copied().collect();
                slot.conns.push(Conn {
                    id: conn,
                    loop_: item.loop_,
                });
                slot.txns.push(Txn {
                    id: txn_id,
                    loop_: item.loop_,
                    mode: Mode::VersionChange,
                    scope,
                    started: true,
                });
                slot.stage = Stage::Upgrading {
                    txn: txn_id,
                    created,
                    old,
                };
                st.conn_slot.insert(conn, sk.clone());
                st.txn_slot.insert(txn_id, sk.clone());
                st.post(
                    item.loop_,
                    Msg::Upgrade {
                        req: item.req,
                        conn,
                        txn: txn_id,
                        schema: lock(&db).schema(),
                        old,
                        new,
                    },
                );
                false
            }
            _ => {
                // §5.3 steps 9-10: delete the database.
                match &self.0.disk {
                    Some(disk) => {
                        slot.stage = Stage::Deleting { item, old };
                        let weak = Arc::downgrade(&self.0);
                        let sk2 = sk.clone();
                        disk.delete(
                            &sk.0,
                            &sk.1,
                            Box::new(move |result| {
                                if let Some(shared) = weak.upgrade() {
                                    LocalIdb(shared).deleted(&sk2, result);
                                }
                            }),
                        );
                        false
                    }
                    None => {
                        slot.db = None;
                        slot.stage = Stage::Idle;
                        st.post(
                            item.loop_,
                            Msg::Deleted {
                                req: item.req,
                                result: Ok(old),
                            },
                        );
                        true
                    }
                }
            }
        }
    }

    fn deleted(&self, sk: &SlotKey, result: Result<(), Failure>) {
        let mut st = self.state();
        let Some(slot) = st.slots.get_mut(sk) else {
            return;
        };
        let Stage::Deleting { item, old } = slot.stage else {
            return;
        };
        slot.stage = Stage::Idle;
        if result.is_ok() {
            slot.db = None;
        }
        st.post(
            item.loop_,
            Msg::Deleted {
                req: item.req,
                result: result.map(|()| old),
            },
        );
        self.pump(&mut st, sk);
    }

    /// The event loop fired (or skipped, the connection having closed) a `versionchange` event.
    pub fn versionchange_fired(&self, conn: u64) {
        let mut st = self.state();
        let Some(sk) = st.conn_slot.get(&conn).cloned() else {
            return;
        };
        if let Some(Stage::Waiting { acks, .. }) = st.slots.get_mut(&sk).map(|s| &mut s.stage) {
            acks.retain(|&c| c != conn);
        }
        self.pump(&mut st, &sk);
    }

    /// A connection closed (§5.2: its close pending flag is set and its transactions finished).
    pub fn connection_closed(&self, conn: u64) {
        let mut st = self.state();
        let Some(sk) = st.conn_slot.remove(&conn) else {
            return;
        };
        if let Some(slot) = st.slots.get_mut(&sk) {
            slot.conns.retain(|c| c.id != conn);
            if let Stage::Waiting { acks, .. } = &mut slot.stage {
                acks.retain(|&c| c != conn);
            }
        }
        self.pump(&mut st, &sk);
    }

    /// The upgrade transaction of an open finished (committed or aborted): the connection queue
    /// moves on. A database this open created goes again when its upgrade aborted.
    pub fn upgrade_finished(&self, txn: u64, committed: bool) {
        let mut st = self.state();
        self.logs().remove(&txn);
        let Some(sk) = st.txn_slot.remove(&txn) else {
            return;
        };
        if let Some(slot) = st.slots.get_mut(&sk) {
            slot.txns.retain(|t| t.id != txn);
        }
        Self::finish_upgrade(&mut st, &sk, txn, committed);
        self.schedule(&mut st, &sk);
        self.pump(&mut st, &sk);
    }

    fn finish_upgrade(st: &mut State, sk: &SlotKey, txn: u64, committed: bool) {
        let Some(slot) = st.slots.get_mut(sk) else {
            return;
        };
        if let Stage::Upgrading {
            txn: t, created, ..
        } = slot.stage
        {
            if t == txn {
                if !committed && created {
                    slot.db = None;
                }
                slot.stage = Stage::Idle;
            }
        }
    }

    // ----- transactions --------------------------------------------------------------------------------

    /// A new transaction of a connection, over the object stores `scope` (ids). The backend posts
    /// [`Msg::Started`] when it may start.
    pub fn create_transaction(&self, conn: u64, mode: Mode, scope: Vec<u64>) -> u64 {
        let mut st = self.state();
        let id = st.id();
        let Some(sk) = st.conn_slot.get(&conn).cloned() else {
            return id;
        };
        let loop_ = st
            .slots
            .get(&sk)
            .and_then(|s| s.conns.iter().find(|c| c.id == conn))
            .map(|c| c.loop_)
            .unwrap_or_default();
        if let Some(slot) = st.slots.get_mut(&sk) {
            slot.txns.push(Txn {
                id,
                loop_,
                mode,
                scope,
                started: false,
            });
        }
        st.txn_slot.insert(id, sk.clone());
        self.schedule(&mut st, &sk);
        id
    }

    /// §2.7.2: starts every transaction no earlier live transaction blocks.
    fn schedule(&self, st: &mut State, sk: &SlotKey) {
        let Some(slot) = st.slots.get_mut(sk) else {
            return;
        };
        let mut start = Vec::new();
        for i in 0..slot.txns.len() {
            if slot.txns[i].started {
                continue;
            }
            let t = &slot.txns[i];
            let blocked = slot.txns[..i].iter().any(|e| {
                let overlaps =
                    e.mode == Mode::VersionChange || e.scope.iter().any(|s| t.scope.contains(s));
                let conflicts = t.mode != Mode::ReadOnly || e.mode != Mode::ReadOnly;
                overlaps && conflicts
            });
            if !blocked {
                start.push(i);
            }
        }
        let mut posts = Vec::new();
        for i in start {
            slot.txns[i].started = true;
            posts.push((slot.txns[i].loop_, slot.txns[i].id));
        }
        for (l, id) in posts {
            st.post(l, Msg::Started { txn: id });
        }
    }

    /// §5.4 step 2.3: writes a transaction's changes; answers with [`Msg::Committed`].
    pub fn commit(&self, txn: u64, durability: Durability) {
        let st = self.state();
        let changes = match self.logs().get_mut(&txn) {
            Some(log) => {
                log.committing = true;
                std::mem::take(&mut log.changes)
            }
            None => Vec::new(),
        };
        let changes = &changes[..];
        let Some(sk) = st.txn_slot.get(&txn).cloned() else {
            return;
        };
        let loop_ = st
            .slots
            .get(&sk)
            .and_then(|s| s.txns.iter().find(|t| t.id == txn))
            .map(|t| t.loop_)
            .unwrap_or_default();
        match (&self.0.disk, changes.is_empty()) {
            (Some(disk), false) => {
                let mut bytes = Vec::new();
                for c in changes {
                    encode_change(c, &mut bytes);
                }
                drop(st);
                let weak = Arc::downgrade(&self.0);
                disk.append(
                    &sk.0,
                    &sk.1,
                    bytes,
                    durability == Durability::Strict,
                    Box::new(move |result| {
                        if let Some(shared) = weak.upgrade() {
                            LocalIdb(shared)
                                .state()
                                .post(loop_, Msg::Committed { txn, result });
                        }
                    }),
                );
            }
            _ => st.post(
                loop_,
                Msg::Committed {
                    txn,
                    result: Ok(()),
                },
            ),
        }
    }

    /// A transaction finished (committed or aborted): later ones may start.
    pub fn transaction_finished(&self, txn: u64) {
        let mut st = self.state();
        self.logs().remove(&txn);
        let Some(sk) = st.txn_slot.remove(&txn) else {
            return;
        };
        if let Some(slot) = st.slots.get_mut(&sk) {
            slot.txns.retain(|t| t.id != txn);
        }
        self.schedule(&mut st, &sk);
    }

    /// Waits until the disk thread has written everything queued so far (tests, shutdown).
    pub fn flush(&self) {
        if let Some(d) = &self.0.disk {
            d.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sites_quota_is_three_fifths_of_the_volume() {
        assert_eq!(quota_for_volume(1_000_000_000_000), 600_000_000_000);
        assert_eq!(quota_for_volume(0), 0);
        // In memory: a third of 15% to 20% of the machine's memory.
        let gib = 1u64 << 30;
        let near = |a: u64, b: u64| a.abs_diff(b) <= 1;
        assert!(near(
            quota_in_memory(30 * gib, 0.0),
            30 * gib * 15 / 100 / 3
        ));
        assert!(near(
            quota_in_memory(30 * gib, 1.0),
            30 * gib * 20 / 100 / 3
        ));
        let mid = quota_in_memory(30 * gib, 0.5);
        assert!(mid > quota_in_memory(30 * gib, 0.0) && mid < quota_in_memory(30 * gib, 1.0));
    }

    fn inbox() -> Arc<Inbox> {
        Arc::new(Inbox::new(Box::new(|| {})))
    }

    fn names(msgs: &[Msg]) -> Vec<&'static str> {
        msgs.iter()
            .map(|m| match m {
                Msg::VersionChange { .. } => "versionchange",
                Msg::Blocked { .. } => "blocked",
                Msg::Upgrade { .. } => "upgrade",
                Msg::Opened { .. } => "opened",
                Msg::OpenFailed { .. } => "failed",
                Msg::Deleted { .. } => "deleted",
                Msg::Started { .. } => "started",
                Msg::Committed { .. } => "committed",
                Msg::Databases { .. } => "databases",
                Msg::Estimate { .. } => "estimate",
                Msg::Answer { .. } => "answer",
            })
            .collect()
    }

    /// Opens `name` at `version` to the end of its upgrade (committed); returns the connection.
    fn open_upgraded(b: &LocalIdb, l: LoopId, ib: &Inbox, name: &str, version: u64) -> u64 {
        b.open(l, 1, "https://a.test", name, Some(version));
        let msgs = ib.drain();
        let Some(Msg::Upgrade { conn, txn, new, .. }) = msgs.into_iter().last() else {
            panic!("no upgrade");
        };
        b.apply(txn, Change::Version(new), false).unwrap();
        b.upgrade_finished(txn, true);
        conn
    }

    /// Opens `name` at version 1 with one object store (id 1, out-of-line keys) and returns the
    /// connection.
    fn open_with_store(b: &LocalIdb, l: LoopId, ib: &Inbox, name: &str) -> u64 {
        b.open(l, 1, "https://a.test", name, Some(1));
        let Some(Msg::Upgrade {
            conn, txn, schema, ..
        }) = ib.drain().into_iter().last()
        else {
            panic!("no upgrade");
        };
        b.apply(txn, Change::Version(1), false).unwrap();
        let store = Change::CreateStore {
            id: schema.next_id,
            name: "s".into(),
            key_path: None,
            auto_increment: false,
        };
        b.apply(txn, store, false).unwrap();
        b.upgrade_finished(txn, true);
        conn
    }

    fn put(store: u64, key: f64) -> Change {
        Change::Put {
            store,
            key: crate::key::Key::Number(key),
            record: crate::db::Record {
                value: Arc::from(&b"v"[..]),
                index_keys: Vec::new(),
            },
        }
    }

    /// Starts a transaction and returns it once the backend has started it.
    fn started(b: &LocalIdb, ib: &Inbox, conn: u64, mode: Mode, scope: Vec<u64>) -> u64 {
        let t = b.create_transaction(conn, mode, scope);
        assert!(matches!(ib.drain()[..], [Msg::Started { txn }] if txn == t));
        t
    }

    #[test]
    fn a_gone_loop_s_uncommitted_writes_are_reverted() {
        let b = LocalIdb::in_memory();
        let (ia, ib) = (inbox(), inbox());
        let (la, lb) = (b.register_loop(&ia), b.register_loop(&ib));
        let ca = open_with_store(&b, la, &ia, "db");
        let t = started(&b, &ia, ca, Mode::ReadWrite, vec![1]);
        b.apply(t, put(1, 1.0), false).unwrap();
        assert_eq!(b.count(t, Source::Store(1), &KeyRange::unbounded()), Ok(1));
        // The page goes away mid-transaction: its write is undone, and another page sees none.
        b.loop_gone(la);
        b.open(lb, 2, "https://a.test", "db", Some(1));
        let Some(Msg::Opened { conn, .. }) = ib.drain().into_iter().last() else {
            panic!("not opened");
        };
        let r = started(&b, &ib, conn, Mode::ReadOnly, vec![1]);
        assert_eq!(b.count(r, Source::Store(1), &KeyRange::unbounded()), Ok(0));
    }

    #[test]
    fn a_committed_transaction_s_writes_stay_when_its_loop_goes() {
        let b = LocalIdb::in_memory();
        let (ia, ib) = (inbox(), inbox());
        let (la, lb) = (b.register_loop(&ia), b.register_loop(&ib));
        let ca = open_with_store(&b, la, &ia, "db");
        let t = started(&b, &ia, ca, Mode::ReadWrite, vec![1]);
        b.apply(t, put(1, 1.0), false).unwrap();
        b.commit(t, Durability::Default);
        b.loop_gone(la);
        b.open(lb, 2, "https://a.test", "db", Some(1));
        let Some(Msg::Opened { conn, .. }) = ib.drain().into_iter().last() else {
            panic!("not opened");
        };
        let r = started(&b, &ib, conn, Mode::ReadOnly, vec![1]);
        assert_eq!(b.count(r, Source::Store(1), &KeyRange::unbounded()), Ok(1));
    }

    #[test]
    fn calls_outside_a_transaction_s_scope_or_mode_are_refused() {
        let b = LocalIdb::in_memory();
        let ia = inbox();
        let la = b.register_loop(&ia);
        let c = open_with_store(&b, la, &ia, "db");
        let r = started(&b, &ia, c, Mode::ReadOnly, vec![1]);
        assert_eq!(b.apply(r, put(1, 1.0), false), Err(WriteError::Refused));
        assert_eq!(b.next_key(r, 1), Err(Refused));
        assert_eq!(
            b.apply(r, Change::DeleteStore(1), false),
            Err(WriteError::Refused)
        );
        b.transaction_finished(r);
        let w = started(&b, &ia, c, Mode::ReadWrite, vec![1]);
        assert_eq!(b.apply(w, put(2, 1.0), false), Err(WriteError::Refused));
        assert_eq!(
            b.count(w, Source::Store(2), &KeyRange::unbounded()),
            Err(Refused)
        );
        assert_eq!(
            b.apply(w, Change::Version(9), false),
            Err(WriteError::Refused)
        );
        assert_eq!(
            b.count(999, Source::Store(1), &KeyRange::unbounded()),
            Err(Refused)
        );
        // In scope and mode: allowed; `add` over the same key is a ConstraintError.
        assert_eq!(b.apply(w, put(1, 1.0), true), Ok(()));
        assert_eq!(b.apply(w, put(1, 1.0), true), Err(WriteError::KeyExists));
        assert_eq!(b.apply(w, put(1, 1.0), false), Ok(()));
        // An aborted transaction leaves nothing.
        b.revert(w);
        assert_eq!(b.count(w, Source::Store(1), &KeyRange::unbounded()), Ok(0));
    }

    #[test]
    fn open_upgrade_then_versionchange_and_blocked() {
        let b = LocalIdb::in_memory();
        let (ia, ib) = (inbox(), inbox());
        let (la, lb) = (b.register_loop(&ia), b.register_loop(&ib));
        let c1 = open_upgraded(&b, la, &ia, "db", 1);
        // A second loop opens at version 2: the first connection gets versionchange.
        b.open(lb, 7, "https://a.test", "db", Some(2));
        assert_eq!(names(&ia.drain()), ["versionchange"]);
        assert!(ib.drain().is_empty());
        b.versionchange_fired(c1);
        // Still open: blocked, then the upgrade once it closes.
        assert_eq!(names(&ib.drain()), ["blocked"]);
        b.connection_closed(c1);
        assert_eq!(names(&ib.drain()), ["upgrade"]);
        // A lower version fails.
        b.open(la, 9, "https://a.test", "db", Some(1));
        assert!(ia.drain().is_empty(), "queued behind the upgrade");
    }

    #[test]
    fn version_error_and_delete() {
        let b = LocalIdb::in_memory();
        let ia = inbox();
        let la = b.register_loop(&ia);
        let c = open_upgraded(&b, la, &ia, "db", 3);
        b.open(la, 2, "https://a.test", "db", Some(2));
        assert!(matches!(
            ia.drain()[..],
            [Msg::OpenFailed {
                failure: Failure::Version,
                ..
            }]
        ));
        b.databases(la, 5, "https://a.test");
        match &ia.drain()[..] {
            [Msg::Databases { result: Ok(l), .. }] => assert_eq!(l, &[("db".to_owned(), 3)]),
            _ => panic!(),
        }
        b.connection_closed(c);
        b.delete_database(la, 3, "https://a.test", "db");
        assert!(matches!(
            ia.drain()[..],
            [Msg::Deleted { result: Ok(3), .. }]
        ));
        b.delete_database(la, 4, "https://a.test", "db");
        assert!(matches!(
            ia.drain()[..],
            [Msg::Deleted { result: Ok(0), .. }]
        ));
    }

    #[test]
    fn aborted_creation_forgets_the_database() {
        let b = LocalIdb::in_memory();
        let ia = inbox();
        let la = b.register_loop(&ia);
        b.open(la, 1, "k", "db", None);
        let Some(Msg::Upgrade { txn, conn, .. }) = ia.drain().pop() else {
            panic!()
        };
        b.upgrade_finished(txn, false);
        b.connection_closed(conn);
        b.databases(la, 1, "k");
        match &ia.drain()[..] {
            [Msg::Databases { result: Ok(l), .. }] => assert!(l.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn scheduling() {
        let b = LocalIdb::in_memory();
        let ia = inbox();
        let la = b.register_loop(&ia);
        let c = open_upgraded(&b, la, &ia, "db", 1);
        let started = |ib: &Inbox| -> Vec<u64> {
            ib.drain()
                .into_iter()
                .filter_map(|m| match m {
                    Msg::Started { txn } => Some(txn),
                    _ => None,
                })
                .collect()
        };
        let rw1 = b.create_transaction(c, Mode::ReadWrite, vec![1]);
        let ro1 = b.create_transaction(c, Mode::ReadOnly, vec![1, 2]);
        let ro2 = b.create_transaction(c, Mode::ReadOnly, vec![2]);
        let rw2 = b.create_transaction(c, Mode::ReadWrite, vec![2]);
        let rw3 = b.create_transaction(c, Mode::ReadWrite, vec![3]);
        // rw1 runs; ro1 waits for it; ro2 does not overlap; rw2 waits for ro1 and ro2; rw3 is free.
        assert_eq!(started(&ia), [rw1, ro2, rw3]);
        b.transaction_finished(rw1);
        assert_eq!(started(&ia), [ro1]);
        b.transaction_finished(ro2);
        assert!(started(&ia).is_empty());
        b.transaction_finished(ro1);
        assert_eq!(started(&ia), [rw2]);
    }

    #[test]
    fn loop_gone_releases_its_connections() {
        let b = LocalIdb::in_memory();
        let (ia, ib) = (inbox(), inbox());
        let (la, lb) = (b.register_loop(&ia), b.register_loop(&ib));
        open_upgraded(&b, la, &ia, "db", 1);
        b.open(lb, 1, "https://a.test", "db", Some(2));
        b.loop_gone(la);
        assert_eq!(names(&ib.drain()), ["upgrade"]);
    }
}
