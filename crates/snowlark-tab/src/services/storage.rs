// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `localStorage` across processes (HTML §12.2): the browser's [`WebStorage`] holds the areas, and a
//! renderer's event loops are clients of it through its pipe.
//!
//! A renderer names its clients with ids of its own. The browser keeps the service's id for each,
//! and a notice's writer (`by`) reaches the renderer as its own id when the writer is its own client,
//! and as 0, which no client of a renderer has, when it is not. Writes, areas and notices travel as
//! bulk, since one value may hold the whole quota.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};
use snowlark_ipc::{wire, Bulk, Error, Replier};
use snowlark_page::web_storage::{
    ClientId, Items, Notify, StorageNotice, StorageWrite, WebStorage,
};

use super::{BrowserPort, RendererPort, CALL_WAIT};

/// What a notice's `by` is when the writer is not one of the renderer's clients.
const ANOTHER: ClientId = 0;

/// Renderer to browser: the calls of [`WebStorage`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    Connect {
        client: ClientId,
    },
    Disconnect {
        client: ClientId,
    },
    /// A call: the answer is the area's [`Items`], encoded, as bulk.
    Open {
        client: ClientId,
        origin: String,
    },
    /// A [`StorageWrite`], encoded, as bulk.
    Write {
        client: ClientId,
        write: Bulk,
    },
}

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    /// A [`StorageNotice`] for the renderer's client, encoded, as bulk.
    Notice { client: ClientId, notice: Bulk },
}

/// The renderer's [`WebStorage`].
pub struct RemoteStorage {
    port: RendererPort,
    next: AtomicU64,
    clients: Mutex<HashMap<ClientId, Notify>>,
}

impl RemoteStorage {
    pub fn new(port: RendererPort) -> RemoteStorage {
        RemoteStorage {
            port,
            next: AtomicU64::new(ANOTHER),
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn send(&self, message: Up) {
        let _ = self.port.send(&super::Up::Storage(message));
    }

    pub(super) fn dispatch(&self, message: Down) {
        let Down::Notice { client, notice } = message;
        let notify = self
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client)
            .cloned();
        let notice = self
            .port
            .open_bulk(notice)
            .and_then(|bytes| wire::decode::<StorageNotice>(&bytes));
        if let (Some(notify), Ok(notice)) = (notify, notice) {
            notify(notice);
        }
    }
}

impl WebStorage for RemoteStorage {
    fn connect(&self, notify: Notify) -> ClientId {
        let client = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(client, notify);
        self.send(Up::Connect { client });
        client
    }

    fn disconnect(&self, client: ClientId) {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&client);
        self.send(Up::Disconnect { client });
    }

    fn open(&self, client: ClientId, origin: &str) -> Items {
        let message = super::Up::Storage(Up::Open {
            client,
            origin: origin.to_owned(),
        });
        // (with the browser gone, an empty area: this process ends soon)
        self.port
            .call::<Bulk>(&message, CALL_WAIT)
            .and_then(|bulk| self.port.open_bulk(bulk))
            .and_then(|bytes| wire::decode(&bytes))
            .unwrap_or_default()
    }

    fn write(&self, client: ClientId, write: StorageWrite) {
        let bulk = wire::to_bytes(&write).and_then(|bytes| self.port.bulk(bytes, CALL_WAIT));
        if let Ok(write) = bulk {
            self.send(Up::Write { client, write });
        }
    }
}

/// The browser's end of one renderer's clients.
pub struct StorageHost {
    port: BrowserPort,
    backend: Arc<dyn WebStorage>,
    site_lock: super::site_lock::Mode,
    /// The renderer's id of a client to the service's.
    clients: Mutex<HashMap<ClientId, ClientId>>,
    /// The service's id of a client to the renderer's. Its own lock: the service calls a client's
    /// notify with its own lock held, so this one is never held across a call to the service.
    renderer: Mutex<HashMap<ClientId, ClientId>>,
    me: Weak<StorageHost>,
}

impl StorageHost {
    pub fn new(
        port: BrowserPort,
        backend: Arc<dyn WebStorage>,
        site_lock: super::site_lock::Mode,
    ) -> Arc<StorageHost> {
        Arc::new_cyclic(|me| StorageHost {
            port,
            backend,
            site_lock,
            clients: Mutex::new(HashMap::new()),
            renderer: Mutex::new(HashMap::new()),
            me: me.clone(),
        })
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    /// The service's id of the renderer's client `client`.
    fn client(&self, client: ClientId) -> Result<ClientId, Error> {
        let known = self
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client)
            .copied();
        known.ok_or_else(|| self.bad(format!("client {client}, which it never connected")))
    }

    /// Sends `notice` to the renderer's client `client`.
    fn notify(&self, client: ClientId, mut notice: StorageNotice) {
        if let StorageNotice::Changed { by, .. } = &mut notice {
            *by = self
                .renderer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(by)
                .copied()
                .unwrap_or(ANOTHER);
        }
        let bulk = wire::to_bytes(&notice).and_then(|bytes| self.port.bulk(&bytes));
        if let Ok(notice) = bulk {
            let _ = self
                .port
                .send(&super::Down::Storage(Down::Notice { client, notice }));
        }
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        match message {
            Up::Connect { client } => {
                if client == ANOTHER
                    || self
                        .clients
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains_key(&client)
                {
                    return Err(self.bad(format!("client {client} connected twice")));
                }
                let me = self.me.clone();
                let notify: Notify = Arc::new(move |notice| {
                    if let Some(me) = me.upgrade() {
                        me.notify(client, notice);
                    }
                });
                let id = self.backend.connect(notify);
                self.clients
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(client, id);
                self.renderer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, client);
            }
            Up::Disconnect { client } => {
                let id = self.client(client)?;
                self.clients
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&client);
                self.renderer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                self.backend.disconnect(id);
            }
            Up::Open { client, origin } => {
                super::site_lock::check(
                    &self.port,
                    self.site_lock,
                    super::site_lock::origin_string_violation(
                        self.port.policy(),
                        "the storage area's origin",
                        &origin,
                    ),
                )?;
                let id = self.client(client)?;
                let Some(replier) = replier else {
                    return Err(self.bad("an area opened with nobody to answer".into()));
                };
                let items = self.backend.open(id, &origin);
                let bulk = self.port.bulk(&wire::to_bytes(&items)?)?;
                // (a renderer that stopped waiting is no reason to stop serving it)
                let _ = replier.reply(&bulk);
            }
            Up::Write { client, write } => {
                let id = self.client(client)?;
                let bytes = self.port.open_bulk(write)?;
                let write = wire::decode::<StorageWrite>(&bytes)
                    .map_err(|e| self.bad(format!("a storage write: {e}")))?;
                self.backend.write(id, write);
            }
        }
        Ok(())
    }

    /// How many clients the renderer has connected.
    pub fn clients(&self) -> usize {
        self.clients.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Drop for StorageHost {
    fn drop(&mut self) {
        let clients = std::mem::take(&mut *self.clients.lock().unwrap_or_else(|e| e.into_inner()));
        for id in clients.into_values() {
            self.backend.disconnect(id);
        }
    }
}
