// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Service worker registrations across processes (W3C Service Workers §3.4, §8;
//! `docs/service-workers.md`): the browser's registry serves every renderer of the profile, and a
//! renderer's event loops reach it through [`RemoteServiceWorkers`], one connection per loop and
//! storage key, as they would the registry in their own process.
//!
//! A renderer numbers its connections; the browser keeps the registry's number for each. Calls go
//! up encoded, as bulk; each connection's notices (answers, state changes, and to a runner the
//! script of a worker to start) come down in the registry's order, encoded, as bulk. The
//! connection's storage key is checked against the renderer's site lock, and every call that
//! names an origin or a URL must name one of that key: a renderer cannot register, update or
//! unregister for another site, or be a client of one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use snowlark_ipc::{wire, Bulk, Error, Replier};
use snowlark_sw::registry::{storage_key, Call, CallId, ConnId, Notice};
use snowlark_sw::{NoticeSink, ServiceWorkerHost};

use super::{BrowserPort, RendererPort, CALL_WAIT};

/// Renderer to browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// A new connection `conn` (the renderer's number) of an event loop of storage key `key`.
    Connect {
        conn: u64,
        key: String,
        runner: bool,
    },
    /// A [`Call`] on the connection, encoded, as bulk.
    Call {
        conn: u64,
        id: CallId,
        call: Bulk,
    },
    Disconnect {
        conn: u64,
    },
}

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    /// A [`Notice`] for the renderer's connection `conn`, encoded, as bulk.
    Notice { conn: u64, notice: Bulk },
}

/// The renderer's [`ServiceWorkerHost`].
pub struct RemoteServiceWorkers {
    port: RendererPort,
    next: AtomicU64,
    sinks: Mutex<HashMap<u64, Arc<dyn NoticeSink>>>,
}

impl RemoteServiceWorkers {
    pub fn new(port: RendererPort) -> RemoteServiceWorkers {
        RemoteServiceWorkers {
            port,
            next: AtomicU64::new(0),
            sinks: Mutex::new(HashMap::new()),
        }
    }

    fn send(&self, message: Up) {
        // (a browser that is gone ends this process soon; nothing here can do better)
        let _ = self.port.send(&super::Up::ServiceWorker(message));
    }

    pub(super) fn dispatch(&self, message: Down) {
        let Down::Notice { conn, notice } = message;
        let Some(notice) = self
            .port
            .open_bulk(notice)
            .ok()
            .and_then(|bytes| wire::decode::<Notice>(&bytes).ok())
        else {
            return;
        };
        let sink = self
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&conn)
            .cloned();
        if let Some(sink) = sink {
            sink.notice(notice);
        }
    }
}

impl ServiceWorkerHost for RemoteServiceWorkers {
    fn connect(&self, key: String, runner: bool, sink: Arc<dyn NoticeSink>) -> ConnId {
        let conn = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conn, sink);
        self.send(Up::Connect { conn, key, runner });
        conn
    }

    fn call(&self, conn: ConnId, id: CallId, call: Call) {
        let Ok(call) = wire::to_bytes(&call).and_then(|b| self.port.bulk(b, CALL_WAIT)) else {
            return;
        };
        self.send(Up::Call { conn, id, call });
    }

    fn disconnect(&self, conn: ConnId) {
        self.sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&conn);
        self.send(Up::Disconnect { conn });
    }

    /// (clearing site data is the browser's: a renderer cannot ask for it)
    fn clear_key(&self, _key: &str) {}
}

/// The browser's end of one renderer's connections.
pub struct ServiceWorkerHostSide {
    port: BrowserPort,
    backend: Arc<dyn ServiceWorkerHost>,
    site_lock: super::site_lock::Mode,
    /// The renderer's connection to the registry's, with its storage key.
    conns: Mutex<HashMap<u64, (ConnId, String)>>,
}

impl ServiceWorkerHostSide {
    pub fn new(
        port: BrowserPort,
        backend: Arc<dyn ServiceWorkerHost>,
        site_lock: super::site_lock::Mode,
    ) -> ServiceWorkerHostSide {
        ServiceWorkerHostSide {
            port,
            backend,
            site_lock,
            conns: Mutex::new(HashMap::new()),
        }
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    /// What is wrong with a call on a connection of storage key `key`: an origin or a URL of
    /// another key.
    fn call_violation(call: &Call, key: &str) -> Option<String> {
        let client = match call {
            Call::Register { client, .. }
            | Call::Update { client, .. }
            | Call::Unregister { client, .. } => Some(client.origin.ascii_serialization()),
            _ => None,
        };
        if let Some(origin) = client.filter(|o| o != key) {
            return Some(format!("a job client of {origin} on a connection of {key}"));
        }
        let url = match call {
            Call::Client { url, .. }
            | Call::GetRegistration { client_url: url }
            | Call::Ready { client_url: url } => Some(url),
            _ => None,
        };
        url.filter(|u| storage_key(u) != key)
            .map(|u| format!("a client URL {u} on a connection of {key}"))
    }

    pub(super) fn serve(&self, message: Up, _: Option<Replier>) -> Result<(), Error> {
        match message {
            Up::Connect { conn, key, runner } => {
                super::site_lock::check(
                    &self.port,
                    self.site_lock,
                    super::site_lock::storage_key_violation(
                        self.port.policy(),
                        "the service worker connection's storage key",
                        &key,
                    ),
                )?;
                let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
                if conns.contains_key(&conn) {
                    return Err(self.bad(format!("service worker connection {conn} made twice")));
                }
                let port = self.port.clone();
                let sink = Arc::new(move |notice: Notice| {
                    // (the registry's thread: in order, as the registry made them)
                    let Ok(bytes) = wire::to_bytes(&notice) else {
                        return;
                    };
                    if let Ok(notice) = port.bulk(&bytes) {
                        let _ =
                            port.send(&super::Down::ServiceWorker(Down::Notice { conn, notice }));
                    }
                });
                let id = self.backend.connect(key.clone(), runner, sink);
                conns.insert(conn, (id, key));
            }
            Up::Call { conn, id, call } => {
                let found = self
                    .conns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&conn)
                    .cloned();
                let Some((backend_conn, key)) = found else {
                    return Err(self.bad(format!(
                        "a service worker call on connection {conn}, which it never made"
                    )));
                };
                let bytes = self.port.open_bulk(call)?;
                let call = wire::decode::<Call>(&bytes)
                    .map_err(|e| self.bad(format!("a service worker call: {e}")))?;
                super::site_lock::check(
                    &self.port,
                    self.site_lock,
                    Self::call_violation(&call, &key),
                )?;
                self.backend.call(backend_conn, id, call);
            }
            Up::Disconnect { conn } => {
                let removed = self
                    .conns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&conn);
                let Some((backend_conn, _)) = removed else {
                    return Err(self.bad(format!(
                        "closing service worker connection {conn}, which it never made"
                    )));
                };
                self.backend.disconnect(backend_conn);
            }
        }
        Ok(())
    }

    /// How many connections the renderer has open.
    pub fn connections(&self) -> usize {
        self.conns.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Drop for ServiceWorkerHostSide {
    /// The renderer is gone: its connections close (its clients unload, the workers it ran stop).
    fn drop(&mut self) {
        let conns = std::mem::take(&mut *self.conns.lock().unwrap_or_else(|e| e.into_inner()));
        for (backend_conn, _) in conns.into_values() {
            self.backend.disconnect(backend_conn);
        }
    }
}
