// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IndexedDB across processes (`docs/process-isolation.md`, M4 step 2, and "IndexedDB: where a
//! transaction's working state lives"): the browser's [`IdbService`] holds the databases, every
//! transaction's working state and the connection queues, and a renderer's event loops are clients
//! of it through its pipe.
//!
//! Only `flush` is a call on the pipe that waits; the others are messages, in order. A
//! transaction's reads and writes ([`Request`]) are answered as [`Msg::Answer`]s, sent down with the
//! backend's other messages to the loop, so a page never waits for one. Each call crosses as a
//! [`Call`] in bulk, since keys, values, names and scopes have no bound. What the backend posts to
//! one of the renderer's loops a pump thread sends down in order, and the renderer posts it into the
//! loop's [`Inbox`], as the local backend would have.
//!
//! **Ids.** A renderer names its event loops with ids of its own, which only go up. Its connections
//! it knows by ids the browser gives it (1, 2, 3 and on). Its transactions it names itself, from one
//! count that only goes up, so `transaction()` waits for nothing: a new transaction's id comes with
//! the call that creates it, and the id of the upgrade transaction an open may run comes with the
//! open. Both are mapped to the backend's: it can name only its own. One of its own that has
//! finished (closed, committed, aborted), or that it named and the backend never made (an open that
//! needed no upgrade, a transaction of a closed connection), reaches the backend as 0, an id the
//! backend never makes, so the backend refuses or ignores the call as it would for a finished one.
//! A loop it never registered, a connection id it was never given, a transaction id it never named,
//! or a new transaction id not above every one it named before, kills it. Request ids are the
//! renderer's own and come back only to the loop that sent them: there is nothing to check.
//!
//! **What is checked where.** The backend checks that a transaction exists, has started and not
//! finished, and that a read or write is in its scope and mode and a schema change in an upgrade.
//! It does not check who calls: `commit`, `revert` and the `*_finished` calls take any transaction,
//! and any connection, which is why the ids are mapped. Here: the ids; that a transaction a
//! renderer creates is not an upgrade (only the backend makes those); keys and changes as they
//! decode (`snowlark_idb::wire`). Storage keys and database names are still the renderer's word, until
//! M5.
//!
//! A renderer that goes (its host dropped) has `loop_gone` called for each of its loops: its
//! unfinished transactions revert, its connections close and its queued requests go.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, Weak};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use snowlark_idb::wire::check_change;
use snowlark_idb::{Change, Durability, IdbService, Inbox, LoopId, Mode, Msg, Request};
use snowlark_ipc::{wire, Bulk, Error, Replier};

use super::{answer, unpack, BrowserPort, RendererPort};

/// The id a finished connection or transaction of the renderer's reaches the backend as: one it
/// never makes (its ids start at 1).
const GONE: u64 = 0;

/// Renderer to browser: one [`Call`], encoded, as bulk.
#[derive(Debug, Serialize, Deserialize)]
pub struct Up(pub(crate) Bulk);

/// The calls of [`IdbService`], with the renderer's ids.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Call {
    RegisterLoop {
        loop_: LoopId,
    },
    LoopGone {
        loop_: LoopId,
    },
    /// `upgrade`: the renderer's id for the upgrade transaction the open may run.
    Open {
        loop_: LoopId,
        req: u64,
        key: String,
        name: String,
        version: Option<u64>,
        upgrade: u64,
    },
    DeleteDatabase {
        loop_: LoopId,
        req: u64,
        key: String,
        name: String,
    },
    Databases {
        loop_: LoopId,
        id: u64,
        key: String,
    },
    Estimate {
        loop_: LoopId,
        id: u64,
        key: String,
    },
    VersionChangeFired {
        conn: u64,
    },
    ConnectionClosed {
        conn: u64,
    },
    UpgradeFinished {
        txn: u64,
        committed: bool,
    },
    /// `txn`: the renderer's id for it.
    CreateTransaction {
        conn: u64,
        mode: Mode,
        scope: Vec<u64>,
        txn: u64,
    },
    Commit {
        txn: u64,
        durability: Durability,
    },
    TransactionFinished {
        txn: u64,
    },
    Change {
        txn: u64,
        change: Change,
    },
    Revert {
        txn: u64,
    },
    /// A read or write; the answer comes down as a [`Msg::Answer`] to the transaction's loop.
    Request {
        txn: u64,
        seq: u64,
        request: Request,
    },
    /// Answers `()` once everything queued is written.
    Flush,
}

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    /// What the backend posted to the renderer's loop `loop_`, in order: a `Vec<Msg>`, encoded, as
    /// bulk.
    Deliver { loop_: LoopId, msgs: Bulk },
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The renderer's [`IdbService`].
pub struct RemoteIdb {
    port: RendererPort,
    next: AtomicU64,
    /// The last transaction id named. Held while a call that names one is sent, so the ids reach
    /// the browser in the order they were named (it requires them to go up).
    last_txn: Mutex<u64>,
    loops: Mutex<HashMap<LoopId, Weak<Inbox>>>,
}

impl RemoteIdb {
    pub fn new(port: RendererPort) -> RemoteIdb {
        RemoteIdb {
            port,
            next: AtomicU64::new(0),
            last_txn: Mutex::new(0),
            loops: Mutex::new(HashMap::new()),
        }
    }

    /// Names a new transaction and sends the call `make` builds with its id: the id.
    fn send_naming(&self, make: impl FnOnce(u64) -> Call) -> u64 {
        let mut last = lock(&self.last_txn);
        *last += 1;
        let id = *last;
        self.send(make(id));
        id
    }

    fn message(&self, call: &Call) -> Option<super::Up> {
        super::pack(&self.port, call)
            .ok()
            .map(|bulk| super::Up::Idb(Up(bulk)))
    }

    fn send(&self, call: Call) {
        // (a browser that is gone ends this process soon; nothing here can do better)
        if let Some(message) = self.message(&call) {
            let _ = self.port.send(&message);
        }
    }

    /// Asks the browser: `None` if it is gone.
    fn ask<A: DeserializeOwned>(&self, call: Call) -> Option<A> {
        super::ask(&self.port, &self.message(&call)?)
    }

    pub(super) fn dispatch(&self, message: Down) {
        let Down::Deliver { loop_, msgs } = message;
        // (opened even for a loop that is gone: a section the browser gave is closed here)
        let msgs = self
            .port
            .open_bulk(msgs)
            .and_then(|bytes| wire::decode::<Vec<Msg>>(&bytes));
        let inbox = lock(&self.loops).get(&loop_).and_then(Weak::upgrade);
        if let (Some(inbox), Ok(msgs)) = (inbox, msgs) {
            for m in msgs {
                inbox.post(m);
            }
        }
    }
}

impl IdbService for RemoteIdb {
    fn register_loop(&self, inbox: &Arc<Inbox>) -> LoopId {
        let loop_ = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        lock(&self.loops).insert(loop_, Arc::downgrade(inbox));
        self.send(Call::RegisterLoop { loop_ });
        loop_
    }

    fn loop_gone(&self, loop_: LoopId) {
        lock(&self.loops).remove(&loop_);
        self.send(Call::LoopGone { loop_ });
    }

    fn open(&self, loop_: LoopId, req: u64, key: &str, name: &str, version: Option<u64>) {
        self.send_naming(|upgrade| Call::Open {
            loop_,
            req,
            key: key.to_owned(),
            name: name.to_owned(),
            version,
            upgrade,
        });
    }

    fn delete_database(&self, loop_: LoopId, req: u64, key: &str, name: &str) {
        self.send(Call::DeleteDatabase {
            loop_,
            req,
            key: key.to_owned(),
            name: name.to_owned(),
        });
    }

    fn databases(&self, loop_: LoopId, id: u64, key: &str) {
        self.send(Call::Databases {
            loop_,
            id,
            key: key.to_owned(),
        });
    }

    fn estimate(&self, loop_: LoopId, id: u64, key: &str) {
        self.send(Call::Estimate {
            loop_,
            id,
            key: key.to_owned(),
        });
    }

    fn versionchange_fired(&self, conn: u64) {
        self.send(Call::VersionChangeFired { conn });
    }

    fn connection_closed(&self, conn: u64) {
        self.send(Call::ConnectionClosed { conn });
    }

    fn upgrade_finished(&self, txn: u64, committed: bool) {
        self.send(Call::UpgradeFinished { txn, committed });
    }

    fn create_transaction(&self, conn: u64, mode: Mode, scope: Vec<u64>) -> u64 {
        self.send_naming(|txn| Call::CreateTransaction {
            conn,
            mode,
            scope,
            txn,
        })
    }

    fn commit(&self, txn: u64, durability: Durability) {
        self.send(Call::Commit { txn, durability });
    }

    fn transaction_finished(&self, txn: u64) {
        self.send(Call::TransactionFinished { txn });
    }

    fn change(&self, txn: u64, change: Change) {
        self.send(Call::Change { txn, change });
    }

    fn revert(&self, txn: u64) {
        self.send(Call::Revert { txn });
    }

    fn request(&self, _loop: LoopId, txn: u64, seq: u64, request: Request) {
        // (the browser knows the transaction's loop)
        self.send(Call::Request { txn, seq, request });
    }

    fn flush(&self) {
        let _: Option<()> = self.ask(Call::Flush);
    }
}

/// A connection or transaction of the renderer's that the backend has.
#[derive(Clone, Copy)]
struct Live {
    /// The backend's id.
    backend: u64,
    /// The renderer's loop it belongs to.
    loop_: LoopId,
}

/// One of the renderer's loops.
struct Loop {
    backend: LoopId,
    /// Where the backend posts for it; the backend holds it weakly.
    inbox: Arc<Inbox>,
}

/// The renderer's ids and the backend's.
#[derive(Default)]
struct Ids {
    loops: HashMap<LoopId, Loop>,
    /// The highest loop id the renderer registered.
    last_loop: LoopId,
    /// By the renderer's id.
    conns: HashMap<u64, Live>,
    txns: HashMap<u64, Live>,
    /// The backend's id of each live connection and transaction to the renderer's (the backend
    /// gives every connection and transaction an id of its own, so one table holds both).
    renderer: HashMap<u64, u64>,
    /// The last connection id given to the renderer.
    issued: u64,
    /// The last transaction id the renderer named.
    named: u64,
    /// The renderer's id for the upgrade transaction of each of its opens not yet answered, by
    /// (its loop, its request).
    upgrades: HashMap<(LoopId, u64), u64>,
}

impl Ids {
    /// Gives the renderer an id for the backend's connection `backend`.
    fn give(&mut self, backend: u64, loop_: LoopId) -> u64 {
        self.issued += 1;
        let id = self.issued;
        self.conns.insert(id, Live { backend, loop_ });
        self.renderer.insert(backend, id);
        id
    }

    /// The renderer's transaction `id` is the backend's `backend`.
    fn adopt(&mut self, id: u64, backend: u64, loop_: LoopId) {
        self.txns.insert(id, Live { backend, loop_ });
        self.renderer.insert(backend, id);
    }

    fn forget(&mut self, id: u64, txn: bool) {
        let live = if txn {
            self.txns.remove(&id)
        } else {
            self.conns.remove(&id)
        };
        if let Some(live) = live {
            self.renderer.remove(&live.backend);
        }
    }

    /// What the backend posted to the renderer's loop `loop_`, with the renderer's ids: a new
    /// connection or transaction gets one, and a message about one that finished goes (the page
    /// would have ignored it, and the backend no longer waits on it).
    fn translate(&mut self, loop_: LoopId, msg: Msg) -> Option<Msg> {
        Some(match msg {
            Msg::VersionChange { conn, old, new } => Msg::VersionChange {
                conn: *self.renderer.get(&conn)?,
                old,
                new,
            },
            Msg::Upgrade {
                req,
                conn,
                txn,
                schema,
                old,
                new,
            } => {
                // (none only for a loop that is gone, whose page no longer waits)
                let id = self.upgrades.remove(&(loop_, req))?;
                self.adopt(id, txn, loop_);
                Msg::Upgrade {
                    req,
                    conn: self.give(conn, loop_),
                    txn: id,
                    schema,
                    old,
                    new,
                }
            }
            Msg::Opened {
                req,
                conn,
                schema,
                version,
            } => {
                self.upgrades.remove(&(loop_, req));
                Msg::Opened {
                    req,
                    conn: self.give(conn, loop_),
                    schema,
                    version,
                }
            }
            Msg::OpenFailed { req, failure } => {
                self.upgrades.remove(&(loop_, req));
                Msg::OpenFailed { req, failure }
            }
            Msg::Started { txn } => Msg::Started {
                txn: *self.renderer.get(&txn)?,
            },
            Msg::Committed { txn, result } => Msg::Committed {
                txn: *self.renderer.get(&txn)?,
                result,
            },
            Msg::Answer { txn, seq, answer } => Msg::Answer {
                txn: *self.renderer.get(&txn)?,
                seq,
                answer,
            },
            m @ (Msg::Blocked { .. }
            | Msg::Deleted { .. }
            | Msg::Databases { .. }
            | Msg::Estimate { .. }) => m,
        })
    }
}

/// The browser's end of one renderer's IndexedDB calls.
pub struct IdbHost {
    port: BrowserPort,
    backend: Arc<dyn IdbService>,
    site_lock: super::site_lock::Mode,
    /// Shared with the pump, which adds the connections and transactions the backend makes.
    ids: Arc<Mutex<Ids>>,
    /// To the pump: the backend posted to this loop (each inbox holds a copy).
    ready: mpsc::Sender<LoopId>,
}

impl IdbHost {
    pub fn new(
        port: BrowserPort,
        backend: Arc<dyn IdbService>,
        site_lock: super::site_lock::Mode,
    ) -> IdbHost {
        let (ready, woken) = mpsc::channel();
        let ids: Arc<Mutex<Ids>> = Arc::default();
        {
            let (port, ids) = (port.clone(), ids.clone());
            let _ = std::thread::Builder::new()
                .name("snowlark-idb-pump".into())
                .spawn(move || pump(&port, &ids, &woken));
        }
        IdbHost {
            port,
            backend,
            site_lock,
            ids,
            ready,
        }
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    /// The site lock's check of a database's storage key.
    fn check_key(&self, key: &str) -> Result<(), Error> {
        super::site_lock::check(
            &self.port,
            self.site_lock,
            super::site_lock::storage_key_violation(
                self.port.policy(),
                "the database's storage key",
                key,
            ),
        )
    }

    /// The backend's id of the renderer's loop `loop_`.
    fn loop_(&self, loop_: LoopId) -> Result<LoopId, Error> {
        let known = lock(&self.ids).loops.get(&loop_).map(|l| l.backend);
        known.ok_or_else(|| self.bad(format!("loop {loop_}, which it never registered")))
    }

    /// The renderer's connection or transaction `id` in `ids`: `None` if it finished (or names
    /// nothing the backend made), and a dead renderer if it was never given or named.
    fn live(&self, ids: &Ids, id: u64, txn: bool) -> Result<Option<Live>, Error> {
        let (table, last) = if txn {
            (&ids.txns, ids.named)
        } else {
            (&ids.conns, ids.issued)
        };
        if let Some(live) = table.get(&id) {
            return Ok(Some(*live));
        }
        if id == GONE || id > last {
            let what = if txn { "transaction" } else { "connection" };
            return Err(self.bad(format!("{what} {id}, which it was never given")));
        }
        Ok(None)
    }

    /// The renderer names transaction `id`: it must be above every id it named before.
    fn name_txn(&self, ids: &mut Ids, id: u64) -> Result<(), Error> {
        if id <= ids.named {
            let last = ids.named;
            return Err(self.bad(format!("transaction {id} named after {last}")));
        }
        ids.named = id;
        Ok(())
    }

    /// The backend's id of the renderer's connection or transaction `id` ([`GONE`] if it finished).
    fn backend_id(&self, id: u64, txn: bool) -> Result<u64, Error> {
        let ids = lock(&self.ids);
        Ok(self.live(&ids, id, txn)?.map_or(GONE, |l| l.backend))
    }

    /// [`IdbHost::backend_id`], and the renderer's id goes: the call ends it.
    fn take_id(&self, id: u64, txn: bool) -> Result<u64, Error> {
        let mut ids = lock(&self.ids);
        let backend = self.live(&ids, id, txn)?.map_or(GONE, |l| l.backend);
        ids.forget(id, txn);
        Ok(backend)
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        let call: Call = unpack(&self.port, message.0)?;
        let (port, b) = (&self.port, &*self.backend);
        match call {
            Call::RegisterLoop { loop_ } => {
                let mut ids = lock(&self.ids);
                if loop_ <= ids.last_loop {
                    let last = ids.last_loop;
                    return Err(self.bad(format!("loop {loop_} registered after loop {last}")));
                }
                ids.last_loop = loop_;
                let ready = self.ready.clone();
                let inbox = Arc::new(Inbox::new(Box::new(move || {
                    let _ = ready.send(loop_);
                })));
                let backend = b.register_loop(&inbox);
                ids.loops.insert(loop_, Loop { backend, inbox });
            }
            Call::LoopGone { loop_ } => {
                let gone = {
                    let mut ids = lock(&self.ids);
                    let gone = ids.loops.remove(&loop_);
                    let ended: Vec<(u64, bool)> = ids
                        .conns
                        .iter()
                        .map(|(id, l)| (*id, l.loop_, false))
                        .chain(ids.txns.iter().map(|(id, l)| (*id, l.loop_, true)))
                        .filter(|(_, l, _)| *l == loop_)
                        .map(|(id, _, txn)| (id, txn))
                        .collect();
                    for (id, txn) in ended {
                        ids.forget(id, txn);
                    }
                    ids.upgrades.retain(|(l, _), _| *l != loop_);
                    gone
                };
                let Some(gone) = gone else {
                    return Err(self.bad(format!("loop {loop_} gone, which it never registered")));
                };
                b.loop_gone(gone.backend);
            }
            Call::Open {
                loop_,
                req,
                key,
                name,
                version,
                upgrade,
            } => {
                self.check_key(&key)?;
                let backend_loop = self.loop_(loop_)?;
                {
                    let mut ids = lock(&self.ids);
                    self.name_txn(&mut ids, upgrade)?;
                    if ids.upgrades.insert((loop_, req), upgrade).is_some() {
                        return Err(self.bad(format!("open request {req} sent twice")));
                    }
                }
                b.open(backend_loop, req, &key, &name, version)
            }
            Call::DeleteDatabase {
                loop_,
                req,
                key,
                name,
            } => {
                self.check_key(&key)?;
                b.delete_database(self.loop_(loop_)?, req, &key, &name)
            }
            Call::Databases { loop_, id, key } => {
                self.check_key(&key)?;
                b.databases(self.loop_(loop_)?, id, &key)
            }
            Call::Estimate { loop_, id, key } => {
                self.check_key(&key)?;
                b.estimate(self.loop_(loop_)?, id, &key)
            }
            Call::VersionChangeFired { conn } => {
                b.versionchange_fired(self.backend_id(conn, false)?)
            }
            Call::ConnectionClosed { conn } => b.connection_closed(self.take_id(conn, false)?),
            Call::UpgradeFinished { txn, committed } => {
                b.upgrade_finished(self.take_id(txn, true)?, committed)
            }
            Call::CreateTransaction {
                conn,
                mode,
                scope,
                txn,
            } => {
                if mode == Mode::VersionChange {
                    return Err(self.bad("an upgrade transaction it made itself".into()));
                }
                // (held across the call, so the pump, which takes it to send `Started`, finds the
                // new transaction's id)
                let mut ids = lock(&self.ids);
                let conn = self.live(&ids, conn, false)?;
                self.name_txn(&mut ids, txn)?;
                // (a closed connection's names nothing, as the backend's id would)
                if let Some(c) = conn {
                    let backend = b.create_transaction(c.backend, mode, scope);
                    ids.adopt(txn, backend, c.loop_);
                }
            }
            Call::Commit { txn, durability } => b.commit(self.backend_id(txn, true)?, durability),
            Call::TransactionFinished { txn } => b.transaction_finished(self.take_id(txn, true)?),
            Call::Change { txn, change } => {
                let txn = self.backend_id(txn, true)?;
                check_change(&change).map_err(|why| self.bad(why))?;
                b.change(txn, change);
            }
            Call::Revert { txn } => b.revert(self.backend_id(txn, true)?),
            Call::Request { txn, seq, request } => {
                if let Request::Apply { change, .. } = &request {
                    check_change(change).map_err(|why| self.bad(why))?;
                }
                let target = {
                    let ids = lock(&self.ids);
                    self.live(&ids, txn, true)?
                        .and_then(|l| Some((l.backend, ids.loops.get(&l.loop_)?.backend)))
                };
                // (one that finished answers nothing: its page no longer waits for answers)
                if let Some((txn, loop_)) = target {
                    b.request(loop_, txn, seq, request);
                }
            }
            Call::Flush => {
                b.flush();
                answer(port, replier, &())?;
            }
        }
        Ok(())
    }

    /// How many loops, connections and transactions the renderer has in the backend.
    pub fn open(&self) -> (usize, usize, usize) {
        let ids = lock(&self.ids);
        (ids.loops.len(), ids.conns.len(), ids.txns.len())
    }
}

impl Drop for IdbHost {
    /// The renderer is gone: each of its loops is, so what it left unfinished reverts. The pump ends
    /// with the last inbox and the host's own `ready`.
    fn drop(&mut self) {
        let loops = {
            let mut ids = lock(&self.ids);
            ids.conns.clear();
            ids.txns.clear();
            ids.renderer.clear();
            ids.upgrades.clear();
            std::mem::take(&mut ids.loops)
        };
        for l in loops.into_values() {
            self.backend.loop_gone(l.backend);
        }
    }
}

/// The pump: for each of the renderer's loops the backend posted to, takes what it posted and sends
/// it in order, with the renderer's ids. Ends with the host, or with the renderer.
fn pump(port: &BrowserPort, ids: &Mutex<Ids>, woken: &mpsc::Receiver<LoopId>) {
    while let Ok(loop_) = woken.recv() {
        let msgs: Vec<Msg> = {
            let mut ids = lock(ids);
            let Some(inbox) = ids.loops.get(&loop_).map(|l| l.inbox.clone()) else {
                continue;
            };
            inbox
                .drain()
                .into_iter()
                .filter_map(|m| ids.translate(loop_, m))
                .collect()
        };
        // (one wake per post: the first took them all)
        if msgs.is_empty() {
            continue;
        }
        let Ok(bytes) = wire::to_bytes(&msgs) else {
            continue;
        };
        let Ok(msgs) = port.bulk(&bytes) else {
            return;
        };
        if !super::net::send_waiting(port, &super::Down::Idb(Down::Deliver { loop_, msgs })) {
            return;
        }
    }
}
