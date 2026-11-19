// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML's "list of preloaded resources" (HTML §4.6.7.15, "preload"), the part Fetch consults: a
//! preload (`<link rel=preload>`, `modulepreload`, a `Link:` header, or the speculative HTML parser)
//! starts its fetch through [`crate::NetClient::preload`], and a later fetch whose preload key
//! matches takes the preloaded response instead of going to the network ("consume a preloaded
//! resource", run by Fetch's "fetch" for a request from a document), whether the preload is still
//! in flight or done.
//!
//! The key is the spec's (URL, destination, mode, credentials mode) within the requesting document,
//! which the network layer knows as the client's URL (the page's elements and its renderer make
//! their own `Client`s for the same document). An entry nobody consumes is dropped
//! [`PRELOAD_TTL`] after its response completed, so a preload that the page never uses does not
//! keep its body.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use url::Url;

use crate::client::FetchEvent;
use crate::request::{CredentialsMode, Destination, Mode, Request};

/// How many preloads may be on the network at once (see [`crate::NetClient::preload`]).
pub const PRELOADS_IN_FLIGHT: usize = 8;

/// How long a completed preload waits for its consumer.
pub const PRELOAD_TTL: Duration = Duration::from_secs(10);
/// At most this many entries are kept (the oldest go first).
const MAX_ENTRIES: usize = 256;

/// HTML "preload key", with the document it is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreloadKey {
    pub document: String,
    pub url: Url,
    pub destination: Destination,
    pub mode: Mode,
    pub credentials: CredentialsMode,
}

impl PreloadKey {
    /// The key of `request`, if it is one a preload may serve: a GET from a document over HTTP(S) in
    /// mode same-origin, cors or no-cors, without script-set headers (Fetch "fetch", the step that
    /// consults the preloads).
    pub fn of(request: &Request) -> Option<PreloadKey> {
        let client = request.client.as_ref()?;
        let eligible = matches!(request.url().scheme(), "http" | "https")
            && matches!(request.mode, Mode::SameOrigin | Mode::Cors | Mode::NoCors)
            && request.method == "GET"
            && (!request.unsafe_request || request.headers.is_empty())
            && request.body.is_none();
        eligible.then(|| PreloadKey {
            document: client.referrer_source.as_str().to_owned(),
            url: request.url().clone(),
            destination: request.destination,
            mode: request.mode,
            credentials: request.credentials,
        })
    }
}

/// A watcher of a preload's own progress (the `link` element that fires `load` or `error`).
pub(crate) struct Watcher {
    pub tx: mpsc::UnboundedSender<FetchEvent>,
    pub notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct EntryState {
    /// Every event so far, for a consumer or watcher that comes later.
    events: Vec<FetchEvent>,
    ended_at: Option<Instant>,
    consumer: Option<Watcher>,
    watchers: Vec<Watcher>,
}

/// One preload: its key, its integrity metadata, and what its fetch produced.
pub(crate) struct PreloadEntry {
    key: PreloadKey,
    integrity: String,
    state: Mutex<EntryState>,
    /// A fetch consumed it: it starts now even if the preloads in flight are at their limit.
    pub urgent: tokio::sync::Notify,
}

impl PreloadEntry {
    /// An event of the preload's fetch (on the network thread).
    pub fn record(&self, event: &FetchEvent) {
        let mut s = self.state.lock().unwrap();
        if matches!(event, FetchEvent::Done | FetchEvent::Error(_)) {
            s.ended_at = Some(Instant::now());
        }
        let mut gone = false;
        if let Some(c) = &s.consumer {
            gone = c.tx.send(event.clone()).is_err();
            if let Some(n) = &c.notify {
                n();
            }
        }
        if gone {
            s.consumer = None;
        }
        s.watchers.retain(|w| {
            let ok = w.tx.send(event.clone()).is_ok();
            if let Some(n) = &w.notify {
                n();
            }
            ok
        });
        s.events.push(event.clone());
    }

    /// Adds a watcher: it gets every event so far, then the rest as they come.
    fn watch(&self, w: Watcher) {
        let mut s = self.state.lock().unwrap();
        for e in &s.events {
            let _ = w.tx.send(e.clone());
        }
        if let Some(n) = &w.notify {
            n();
        }
        s.watchers.push(w);
    }

    /// Attaches the consumer: the events so far, and a receiver of the rest.
    fn consume(&self) -> (Vec<FetchEvent>, mpsc::UnboundedReceiver<FetchEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut s = self.state.lock().unwrap();
        let events = std::mem::take(&mut s.events);
        s.consumer = Some(Watcher { tx, notify: None });
        (events, rx)
    }

    /// Whether its fetch ended with a network error.
    fn failed(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.events.iter().any(|e| matches!(e, FetchEvent::Error(_)))
    }

    fn expired(&self, now: Instant) -> bool {
        let s = self.state.lock().unwrap();
        s.ended_at
            .is_some_and(|t| now.duration_since(t) > PRELOAD_TTL)
    }
}

/// The preloads of every document of this network client.
pub(crate) struct PreloadList {
    entries: Mutex<Vec<Arc<PreloadEntry>>>,
    /// Permits for the preloads on the network.
    pub slots: Arc<tokio::sync::Semaphore>,
    /// Preloads fetched, and those a fetch consumed.
    pub started: std::sync::atomic::AtomicUsize,
    pub consumed: std::sync::atomic::AtomicUsize,
}

impl Default for PreloadList {
    fn default() -> Self {
        PreloadList {
            entries: Mutex::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(PRELOADS_IN_FLIGHT)),
            started: Default::default(),
            consumed: Default::default(),
        }
    }
}

pub(crate) enum Registered {
    /// A new entry: start its fetch, recording into it.
    New(Arc<PreloadEntry>),
    /// The same preload is already in flight or done (now watched as well).
    Existing,
}

impl PreloadList {
    /// Registers a preload of `request` (not yet consumed) and adds `watcher` to it.
    pub fn register(&self, request: &Request, watcher: Watcher) -> Option<Registered> {
        let key = PreloadKey::of(request)?;
        let mut entries = self.entries.lock().unwrap();
        let now = Instant::now();
        entries.retain(|e| !e.expired(now));
        if let Some(e) = entries.iter().find(|e| e.key == key) {
            e.watch(watcher);
            return Some(Registered::Existing);
        }
        if entries.len() >= MAX_ENTRIES {
            entries.remove(0);
        }
        let entry = Arc::new(PreloadEntry {
            key,
            integrity: request.integrity.clone(),
            state: Mutex::default(),
            urgent: tokio::sync::Notify::new(),
        });
        entry.watch(watcher);
        entries.push(entry.clone());
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(Registered::New(entry))
    }

    /// HTML "consume a preloaded resource": the entry for `request`'s key (removed from the list),
    /// when its integrity metadata agrees, as the events so far and a receiver of the rest.
    pub fn consume(
        &self,
        request: &Request,
    ) -> Option<(Vec<FetchEvent>, mpsc::UnboundedReceiver<FetchEvent>)> {
        let key = PreloadKey::of(request)?;
        let mut entries = self.entries.lock().unwrap();
        let now = Instant::now();
        entries.retain(|e| !e.expired(now));
        let i = entries.iter().position(|e| e.key == key)?;
        if entries[i].failed() {
            // A preload that failed (blocked, a network error) leaves the fetch to the consumer.
            entries.remove(i);
            return None;
        }
        let e = &entries[i];
        // (a consumer with integrity metadata takes only a preload made with the same)
        if !request.integrity.is_empty() && request.integrity != e.integrity {
            return None;
        }
        let entry = entries.remove(i);
        entry.urgent.notify_one();
        self.consumed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(entry.consume())
    }

    /// Whether a preload for `request`'s key is waiting for its consumer.
    pub fn contains(&self, request: &Request) -> bool {
        let Some(key) = PreloadKey::of(request) else {
            return false;
        };
        self.entries.lock().unwrap().iter().any(|e| e.key == key)
    }
}
