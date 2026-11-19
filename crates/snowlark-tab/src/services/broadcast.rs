// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `BroadcastChannel` across processes (HTML §9.6). The browser's hub holds every channel; a
//! renderer's channels are there as one inbox of the renderer's, whose deliveries go over its pipe.
//!
//! A renderer names its channels with ids of its own, which the page sees, and the browser keeps
//! the hub's id for each. An id the renderer never subscribed, or one it subscribes twice, is a
//! broken renderer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};
use snowlark_ipc::{Bulk, Error, Replier};
use snowlark_page::{BroadcastBackend, Inbox};

use super::{BrowserPort, RendererPort, CALL_WAIT};

/// Renderer to browser: the calls of [`BroadcastBackend`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    Subscribe {
        id: u64,
        key: String,
        name: String,
    },
    Unsubscribe {
        id: u64,
    },
    Post {
        from: u64,
        key: String,
        origin: String,
        name: String,
        data: Bulk,
    },
}

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    /// One message for the renderer's channels `to`, in the hub's order.
    Deliver {
        to: Vec<u64>,
        origin: String,
        data: Bulk,
    },
}

/// The renderer's [`BroadcastBackend`].
pub struct RemoteBroadcast {
    port: RendererPort,
    next: AtomicU64,
    channels: Mutex<HashMap<u64, Weak<Inbox>>>,
}

impl RemoteBroadcast {
    pub fn new(port: RendererPort) -> RemoteBroadcast {
        RemoteBroadcast {
            port,
            next: AtomicU64::new(0),
            channels: Mutex::new(HashMap::new()),
        }
    }

    fn send(&self, message: Up) {
        // (a browser that is gone ends this process soon; nothing here can do better)
        let _ = self.port.send(&super::Up::Broadcast(message));
    }

    pub(super) fn dispatch(&self, message: Down) {
        let Down::Deliver { to, origin, data } = message;
        let Ok(data) = self.port.open_bulk(data) else {
            return;
        };
        let data = Arc::new(data);
        let mut wake: Vec<Arc<Inbox>> = Vec::new();
        {
            let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
            // (the channels of event loops that are gone go)
            channels.retain(|_, inbox| inbox.strong_count() > 0);
            for id in to {
                let Some(inbox) = channels.get(&id).and_then(Weak::upgrade) else {
                    continue;
                };
                inbox.deliver(id, data.clone(), origin.clone());
                if !wake.iter().any(|w| Arc::ptr_eq(w, &inbox)) {
                    wake.push(inbox);
                }
            }
        }
        for inbox in wake {
            inbox.wake();
        }
    }
}

impl BroadcastBackend for RemoteBroadcast {
    fn subscribe(&self, key: &str, name: &str, inbox: &Arc<Inbox>) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::downgrade(inbox));
        self.send(Up::Subscribe {
            id,
            key: key.to_owned(),
            name: name.to_owned(),
        });
        id
    }

    fn unsubscribe(&self, id: u64) {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        self.send(Up::Unsubscribe { id });
    }

    fn post(&self, from: u64, key: &str, origin: &str, name: &str, data: Vec<u8>) {
        let Ok(data) = self.port.bulk(data, CALL_WAIT) else {
            return;
        };
        self.send(Up::Post {
            from,
            key: key.to_owned(),
            origin: origin.to_owned(),
            name: name.to_owned(),
            data,
        });
    }
}

#[derive(Default)]
struct Ids {
    /// The renderer's id of a channel to the hub's.
    hub: HashMap<u64, u64>,
    /// The hub's id of a channel to the renderer's.
    renderer: HashMap<u64, u64>,
}

/// The browser's end of one renderer's channels.
pub struct BroadcastHost {
    port: BrowserPort,
    backend: Arc<dyn BroadcastBackend>,
    site_lock: super::site_lock::Mode,
    /// Where the hub puts messages for the renderer's channels.
    pub(super) inbox: Arc<Inbox>,
    /// Held while forwarding too, so messages leave in the order the hub queued them.
    ids: Mutex<Ids>,
}

impl BroadcastHost {
    pub fn new(
        port: BrowserPort,
        backend: Arc<dyn BroadcastBackend>,
        site_lock: super::site_lock::Mode,
    ) -> Arc<BroadcastHost> {
        Arc::new_cyclic(|host: &Weak<BroadcastHost>| {
            let host = host.clone();
            BroadcastHost {
                port,
                backend,
                site_lock,
                inbox: Arc::new(Inbox::new(Arc::new(move || {
                    if let Some(host) = host.upgrade() {
                        host.forward();
                    }
                }))),
                ids: Mutex::new(Ids::default()),
            }
        })
    }

    /// Sends what the hub queued for the renderer: one message per post, naming every channel of
    /// the renderer it is for.
    fn forward(&self) {
        let ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
        let items = self.inbox.take();
        let mut rest = &items[..];
        while let Some((_, data, origin)) = rest.first() {
            let run = rest
                .iter()
                .take_while(|(_, d, o)| Arc::ptr_eq(d, data) && o == origin)
                .count();
            let to: Vec<u64> = rest[..run]
                .iter()
                .filter_map(|(hub, _, _)| ids.renderer.get(hub).copied())
                .collect();
            if !to.is_empty() {
                if let Ok(data) = self.port.bulk(data) {
                    let _ = self.port.send(&super::Down::Broadcast(Down::Deliver {
                        to,
                        origin: origin.clone(),
                        data,
                    }));
                }
            }
            rest = &rest[run..];
        }
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    /// Acts on a site-lock check ([`super::site_lock::check`] in this host's mode).
    fn check(&self, violation: Option<String>) -> Result<(), Error> {
        super::site_lock::check(&self.port, self.site_lock, violation)
    }

    pub(super) fn serve(&self, message: Up, _: Option<Replier>) -> Result<(), Error> {
        match message {
            Up::Subscribe { id, key, name } => {
                self.check(super::site_lock::broadcast_key_violation(
                    self.port.policy(),
                    "the channel's key",
                    &key,
                ))?;
                let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
                if ids.hub.contains_key(&id) {
                    return Err(self.bad(format!("channel {id} subscribed twice")));
                }
                let hub = self.backend.subscribe(&key, &name, &self.inbox);
                ids.hub.insert(id, hub);
                ids.renderer.insert(hub, id);
            }
            Up::Unsubscribe { id } => {
                let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
                let Some(hub) = ids.hub.remove(&id) else {
                    return Err(
                        self.bad(format!("unsubscribing channel {id}, which it never made"))
                    );
                };
                ids.renderer.remove(&hub);
                drop(ids);
                self.backend.unsubscribe(hub);
            }
            Up::Post {
                from,
                key,
                origin,
                name,
                data,
            } => {
                self.check(super::site_lock::broadcast_key_violation(
                    self.port.policy(),
                    "the channel's key",
                    &key,
                ))?;
                self.check(super::site_lock::posted_origin_violation(
                    self.port.policy(),
                    &origin,
                ))?;
                let hub = self
                    .ids
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .hub
                    .get(&from)
                    .copied();
                let Some(hub) = hub else {
                    return Err(
                        self.bad(format!("posting from channel {from}, which it never made"))
                    );
                };
                let data = self.port.open_bulk(data)?;
                // (with no lock held: the hub may deliver to this renderer's own channels, which
                // forwards on this thread)
                self.backend.post(hub, &key, &origin, &name, data);
            }
        }
        Ok(())
    }

    /// How many channels the renderer has in the hub.
    pub fn channels(&self) -> usize {
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).hub.len()
    }
}

impl Drop for BroadcastHost {
    fn drop(&mut self) {
        let ids = std::mem::take(&mut *self.ids.lock().unwrap_or_else(|e| e.into_inner()));
        for hub in ids.hub.into_values() {
            self.backend.unsubscribe(hub);
        }
    }
}
