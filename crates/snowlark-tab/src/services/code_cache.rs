// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! V8's code cache across processes: the browser owns the folder, and a renderer reads and stores
//! entries through its pipe. A read is answered later, as [`Down::Entry`], like the local form's,
//! which reads on a thread of its own.
//!
//! Not yet done (`docs/process-isolation.md` section 7, "Code cache"): the browser keying entries by
//! the renderer's site, and dropping what a renderer stored once it dies of a broken rule.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use serde::{Deserialize, Serialize};
use snowlark_ipc::{Bulk, Error, Replier};
use snowlark_js::code_cache::{CacheKind, CodeCacheStore, Entry};

use super::{BrowserPort, RendererPort, CALL_WAIT};

/// The most reads one renderer may have in flight; past it, a read finds nothing.
pub const MAX_READS: usize = 64;

/// Renderer to browser: the calls of [`CodeCacheStore`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// A call: the answer is a `bool`.
    Has {
        url: String,
    },
    /// Answered by [`Down::Entry`] with the same `req`.
    Read {
        req: u64,
        url: String,
    },
    Store {
        url: String,
        kind: u8,
        hash: [u8; 32],
        data: Bulk,
    },
    Remove {
        url: String,
    },
}

/// An [`Entry`] on the wire.
#[derive(Debug, Serialize, Deserialize)]
pub struct WireEntry {
    pub kind: u8,
    pub response_hash: [u8; 32],
    pub data: Bulk,
}

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    Entry { req: u64, entry: Option<WireEntry> },
}

fn kind_of(byte: u8) -> Option<CacheKind> {
    CacheKind::from_u8(byte)
}

/// The renderer's [`CodeCacheStore`].
pub struct RemoteCodeCache {
    port: RendererPort,
    next: AtomicU64,
    reads: Mutex<HashMap<u64, mpsc::Sender<Option<Entry>>>>,
}

impl RemoteCodeCache {
    pub fn new(port: RendererPort) -> RemoteCodeCache {
        RemoteCodeCache {
            port,
            next: AtomicU64::new(0),
            reads: Mutex::new(HashMap::new()),
        }
    }

    fn send(&self, message: Up) {
        let _ = self.port.send(&super::Up::CodeCache(message));
    }

    pub(super) fn dispatch(&self, message: Down) {
        let Down::Entry { req, entry } = message;
        let Some(reader) = self
            .reads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&req)
        else {
            return;
        };
        let entry = entry.and_then(|e| {
            Some(Entry {
                kind: kind_of(e.kind)?,
                response_hash: e.response_hash,
                data: self.port.open_bulk(e.data).ok()?,
            })
        });
        let _ = reader.send(entry);
    }
}

impl CodeCacheStore for RemoteCodeCache {
    fn has(&self, url: &str) -> bool {
        let message = super::Up::CodeCache(Up::Has {
            url: url.to_owned(),
        });
        self.port.call::<bool>(&message, CALL_WAIT).unwrap_or(false)
    }

    fn read_async(&self, url: &str) -> mpsc::Receiver<Option<Entry>> {
        let (tx, rx) = mpsc::channel();
        let req = self.next.fetch_add(1, Ordering::Relaxed);
        self.reads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(req, tx);
        let sent = self.port.send(&super::Up::CodeCache(Up::Read {
            req,
            url: url.to_owned(),
        }));
        if sent.is_err() {
            // (the reader's sender goes with it, which the caller hears as no entry)
            self.reads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&req);
        }
        rx
    }

    fn store(&self, url: &str, kind: CacheKind, hash: [u8; 32], data: Vec<u8>) {
        if let Ok(data) = self.port.bulk(data, CALL_WAIT) {
            self.send(Up::Store {
                url: url.to_owned(),
                kind: kind as u8,
                hash,
                data,
            });
        }
    }

    fn remove(&self, url: &str) {
        self.send(Up::Remove {
            url: url.to_owned(),
        });
    }
}

/// The browser's end of one renderer's code cache.
pub struct CodeCacheHost {
    port: BrowserPort,
    backend: Arc<dyn CodeCacheStore>,
    site_lock: super::site_lock::Mode,
    reads: Arc<AtomicUsize>,
}

impl CodeCacheHost {
    pub fn new(
        port: BrowserPort,
        backend: Arc<dyn CodeCacheStore>,
        site_lock: super::site_lock::Mode,
    ) -> CodeCacheHost {
        CodeCacheHost {
            port,
            backend,
            site_lock,
            reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        // (the key is the partition and the script: both the renderer's own)
        let key = match &message {
            Up::Has { url } | Up::Read { url, .. } | Up::Store { url, .. } | Up::Remove { url } => {
                url
            }
        };
        super::site_lock::check(
            &self.port,
            self.site_lock,
            super::site_lock::code_cache_key_violation(self.port.policy(), key),
        )?;
        match message {
            Up::Has { url } => {
                let Some(replier) = replier else {
                    return Err(self.bad("a code cache question with nobody to answer".into()));
                };
                let _ = replier.reply(&self.backend.has(&url));
            }
            Up::Read { req, url } => {
                let port = self.port.clone();
                let answer = move |entry: Option<Entry>| {
                    let entry = entry.and_then(|e| {
                        Some(WireEntry {
                            kind: e.kind as u8,
                            response_hash: e.response_hash,
                            data: port.bulk(&e.data).ok()?,
                        })
                    });
                    let _ = port.send(&super::Down::CodeCache(Down::Entry { req, entry }));
                };
                if self.reads.fetch_add(1, Ordering::AcqRel) >= MAX_READS {
                    self.reads.fetch_sub(1, Ordering::AcqRel);
                    answer(None);
                    return Ok(());
                }
                let read = self.backend.read_async(&url);
                let reads = self.reads.clone();
                let waiter = std::thread::Builder::new()
                    .name("snowlark-code-cache-answer".into())
                    .spawn(move || {
                        answer(read.recv().ok().flatten());
                        reads.fetch_sub(1, Ordering::AcqRel);
                    });
                if waiter.is_err() {
                    self.reads.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Up::Store {
                url,
                kind,
                hash,
                data,
            } => {
                let Some(kind) = kind_of(kind) else {
                    return Err(self.bad(format!("a code cache entry of kind {kind}")));
                };
                let data = self.port.open_bulk(data)?;
                self.backend.store(&url, kind, hash, data);
            }
            Up::Remove { url } => self.backend.remove(&url),
        }
        Ok(())
    }
}
