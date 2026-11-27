// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The registry's host: [`ServiceWorkerHost`] is how event loops reach the registrations (a
//! message in, notices out), and [`LocalServiceWorkers`] runs a [`Registry`] on a thread of its
//! own, fetches job scripts through the browser's network, and keeps each storage key's
//! registrations in a folder of the profile (`<folder>\<key as %XX>`), or in memory only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::registry::{Call, CallId, ConnId, Effect, Notice, Registry, StoredRegistration};

/// Where a connection's notices go (its event loop's queue; the sink wakes the loop).
pub trait NoticeSink: Send + Sync {
    fn notice(&self, notice: Notice);
}

impl<F: Fn(Notice) + Send + Sync> NoticeSink for F {
    fn notice(&self, notice: Notice) {
        self(notice)
    }
}

/// The registrations, as an event loop reaches them: message-shaped, so the registry can live in
/// the browser's process while the event loops run in renderers.
pub trait ServiceWorkerHost: Send + Sync {
    /// Opens a connection for an event loop of the storage key `key`. A `runner` can be asked to
    /// run the key's service workers ([`Notice::Start`]).
    fn connect(&self, key: String, runner: bool, sink: Arc<dyn NoticeSink>) -> ConnId;
    /// Sends a call; its answer comes as a [`Notice::Answer`] with `id`, in order with the
    /// connection's other notices.
    fn call(&self, conn: ConnId, id: CallId, call: Call);
    /// Closes a connection: the versions it ran stop.
    fn disconnect(&self, conn: ConnId);
    /// Removes a storage key's registrations (clearing site data).
    fn clear_key(&self, key: &str);
}

enum Msg {
    Connect(ConnId, String, bool, Arc<dyn NoticeSink>),
    Call(ConnId, CallId, Call),
    Disconnect(ConnId),
    Fetched(u64, Result<snowlark_net::Response, String>),
    ClearKey(String),
}

/// How a registry fetches a job's script.
pub type Fetch =
    Arc<dyn Fn(snowlark_net::Request) -> Result<snowlark_net::Response, String> + Send + Sync>;

/// A [`Registry`] on a thread of this process.
pub struct LocalServiceWorkers {
    tx: Mutex<mpsc::Sender<Msg>>,
    next_conn: AtomicU64,
}

impl LocalServiceWorkers {
    /// A registry whose scripts are fetched through `net` (none: every fetch fails), kept in
    /// `folder` (none: in memory).
    pub fn new(
        net: Option<Arc<dyn snowlark_net::NetBackend>>,
        folder: Option<PathBuf>,
    ) -> LocalServiceWorkers {
        let fetch: Fetch = Arc::new(move |request| match &net {
            Some(net) => snowlark_net::NetClient::with_backend(net.clone())
                .fetch(request)
                .read_all()
                .map_err(|e| format!("{e:?}")),
            None => Err("no network".into()),
        });
        LocalServiceWorkers::with_fetch(fetch, folder)
    }

    /// A registry whose scripts `fetch` gets (on a thread of its own per fetch).
    pub fn with_fetch(fetch: Fetch, folder: Option<PathBuf>) -> LocalServiceWorkers {
        let (tx, rx) = mpsc::channel();
        let back = tx.clone();
        std::thread::Builder::new()
            .name("service workers".into())
            .spawn(move || run(rx, back, fetch, folder))
            .expect("start the service worker registry's thread");
        LocalServiceWorkers {
            tx: Mutex::new(tx),
            next_conn: AtomicU64::new(1),
        }
    }

    fn send(&self, m: Msg) {
        let _ = self.tx.lock().expect("the registry's channel").send(m);
    }
}

impl ServiceWorkerHost for LocalServiceWorkers {
    fn connect(&self, key: String, runner: bool, sink: Arc<dyn NoticeSink>) -> ConnId {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.send(Msg::Connect(id, key, runner, sink));
        id
    }

    fn call(&self, conn: ConnId, id: CallId, call: Call) {
        self.send(Msg::Call(conn, id, call));
    }

    fn disconnect(&self, conn: ConnId) {
        self.send(Msg::Disconnect(conn));
    }

    fn clear_key(&self, key: &str) {
        self.send(Msg::ClearKey(key.to_owned()));
    }
}

/// The registry's thread: messages in, effects out, timers between.
fn run(rx: mpsc::Receiver<Msg>, back: mpsc::Sender<Msg>, fetch: Fetch, folder: Option<PathBuf>) {
    let mut registry = Registry::new();
    let now = Instant::now();
    if let Some(dir) = &folder {
        for (key, stored) in load_all(dir) {
            registry.restore(&key, stored, now);
        }
    }
    let mut sinks: HashMap<ConnId, Arc<dyn NoticeSink>> = HashMap::new();
    let mut next_tick: Option<Instant> = None;
    loop {
        let msg = match next_tick {
            Some(at) => {
                let wait = at.saturating_duration_since(Instant::now());
                match rx.recv_timeout(wait.max(Duration::from_millis(1))) {
                    Ok(m) => Some(m),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            },
        };
        let now = Instant::now();
        match msg {
            Some(Msg::Connect(conn, key, runner, sink)) => {
                sinks.insert(conn, sink);
                registry.connect(conn, key, runner);
            }
            Some(Msg::Call(conn, id, call)) => registry.call(conn, id, call, now),
            Some(Msg::Disconnect(conn)) => {
                registry.disconnect(conn, now);
                sinks.remove(&conn);
            }
            Some(Msg::Fetched(job, result)) => registry.fetched(job, result, now),
            Some(Msg::ClearKey(key)) => registry.clear_key(&key),
            None => {}
        }
        next_tick = registry.tick(now);
        for effect in registry.take_effects() {
            match effect {
                Effect::Notify(conn, notice) => {
                    if let Some(s) = sinks.get(&conn) {
                        s.notice(notice);
                    }
                }
                Effect::Fetch { job, request } => {
                    let back = back.clone();
                    let fetch = fetch.clone();
                    std::thread::Builder::new()
                        .name("service worker script".into())
                        .spawn(move || {
                            let result = fetch(*request);
                            let _ = back.send(Msg::Fetched(job, result));
                        })
                        .expect("start a service worker script fetch");
                }
                Effect::Persist(key) => {
                    if let Some(dir) = &folder {
                        save(dir, &key, &registry.stored(&key));
                    }
                }
            }
        }
        // (effects can set timers too)
        next_tick = match (next_tick, registry.tick(Instant::now())) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
}

const FILE: &str = "registrations";

fn save(dir: &std::path::Path, key: &str, stored: &[StoredRegistration]) {
    let folder = dir.join(crate::cache::folder_name(key));
    if stored.is_empty() {
        let _ = std::fs::remove_dir_all(&folder);
        return;
    }
    let Ok(bytes) = postcard::to_stdvec(stored) else {
        return;
    };
    if std::fs::create_dir_all(&folder).is_err() {
        return;
    }
    // (written beside, then moved over the old file, so a cut write leaves the old one)
    let tmp = folder.join(format!("{FILE}.new"));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, folder.join(FILE));
    }
}

fn load_all(dir: &std::path::Path) -> Vec<(String, Vec<StoredRegistration>)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Some(key) = e.file_name().to_str().and_then(crate::cache::key_of_folder) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(e.path().join(FILE)) else {
            continue;
        };
        if let Ok(stored) = postcard::from_bytes::<Vec<StoredRegistration>>(&bytes) {
            out.push((key, stored));
        }
    }
    out
}

/// The handle to a profile's service worker registrations that pages and workers share.
#[derive(Clone)]
pub struct ServiceWorkerBackend(Arc<dyn ServiceWorkerHost>);

impl ServiceWorkerBackend {
    pub fn new(host: Arc<dyn ServiceWorkerHost>) -> ServiceWorkerBackend {
        ServiceWorkerBackend(host)
    }

    /// The implementation behind the handle.
    pub fn host(&self) -> &Arc<dyn ServiceWorkerHost> {
        &self.0
    }
}

impl std::fmt::Debug for ServiceWorkerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceWorkerBackend")
            .finish_non_exhaustive()
    }
}

impl std::ops::Deref for ServiceWorkerBackend {
    type Target = dyn ServiceWorkerHost;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
