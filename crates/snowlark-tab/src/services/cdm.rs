// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's key systems for a renderer ([`Cdm`]): every call is a message, every module
//! event comes back as one. A renderer names its module instances and sessions by its own ids,
//! which the host maps to the browser's; an id it never made, or an origin outside its site lock,
//! kills it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};
use snowlark_ipc::{Error, Replier};
use snowlark_page::media::cdm::{Cdm, CdmError, CdmEvent, CdmInbox};

use super::{answer, ask, BrowserPort, RendererPort};

/// The largest initialization data, response or message that crosses (a licence server's
/// answers and the module's requests are a few tens of KiB).
pub const MAX_DATA: usize = 512 * 1024;

/// Renderer to browser: the calls of [`Cdm`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// [`Cdm::supports`] (a call: answered with a `bool`).
    Supports {
        key_system: String,
        content_type: String,
    },
    CreateKeys {
        request: u64,
        keys: u64,
        key_system: String,
        origin: String,
    },
    CreateSession {
        keys: u64,
        session: u64,
    },
    Generate {
        request: u64,
        session: u64,
        init_data_type: String,
        data: Vec<u8>,
    },
    Update {
        request: u64,
        session: u64,
        response: Vec<u8>,
    },
    Close {
        request: u64,
        session: u64,
    },
    Remove {
        request: u64,
        session: u64,
    },
    Release {
        keys: u64,
    },
}

/// Browser to renderer: an event for the renderer's module instance `keys`, in its ids.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    Event { keys: u64, event: CdmEvent },
}

/// The renderer's [`Cdm`].
pub struct RemoteCdm {
    port: RendererPort,
    next: AtomicU64,
    inboxes: Mutex<HashMap<u64, Weak<CdmInbox>>>,
}

impl RemoteCdm {
    pub fn new(port: RendererPort) -> RemoteCdm {
        RemoteCdm {
            port,
            next: AtomicU64::new(0),
            inboxes: Mutex::default(),
        }
    }

    fn send(&self, message: Up) {
        // (a browser that is gone ends this process soon)
        let _ = self.port.send(&super::Up::Cdm(message));
    }

    pub(super) fn dispatch(&self, message: Down) {
        let Down::Event { keys, event } = message;
        let inbox = self
            .inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&keys)
            .and_then(Weak::upgrade);
        if let Some(inbox) = inbox {
            inbox.deliver(event);
        }
    }
}

impl Cdm for RemoteCdm {
    fn supports(&self, key_system: &str, content_type: &str) -> bool {
        ask(
            &self.port,
            &super::Up::Cdm(Up::Supports {
                key_system: key_system.to_owned(),
                content_type: content_type.to_owned(),
            }),
        )
        .unwrap_or(false)
    }

    fn create_keys(
        &self,
        request: u64,
        key_system: &str,
        origin: &str,
        inbox: &Arc<CdmInbox>,
    ) -> u64 {
        let keys = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(keys, Arc::downgrade(inbox));
        self.send(Up::CreateKeys {
            request,
            keys,
            key_system: key_system.to_owned(),
            origin: origin.to_owned(),
        });
        keys
    }

    fn create_session(&self, keys: u64) -> u64 {
        let session = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(Up::CreateSession { keys, session });
        session
    }

    fn generate_request(&self, request: u64, session: u64, init_data_type: &str, data: Vec<u8>) {
        self.send(Up::Generate {
            request,
            session,
            init_data_type: init_data_type.to_owned(),
            data,
        });
    }

    fn update(&self, request: u64, session: u64, response: Vec<u8>) {
        self.send(Up::Update {
            request,
            session,
            response,
        });
    }

    fn close(&self, request: u64, session: u64) {
        self.send(Up::Close { request, session });
    }

    fn remove(&self, request: u64, session: u64) {
        self.send(Up::Remove { request, session });
    }

    fn release(&self, keys: u64) {
        self.inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&keys);
        self.send(Up::Release { keys });
    }
}

#[derive(Default)]
struct Ids {
    /// The renderer's instance id to the browser's, and back.
    keys: HashMap<u64, u64>,
    keys_back: HashMap<u64, u64>,
    /// The renderer's session id to the browser's, and the browser's to the renderer's session
    /// and instance.
    sessions: HashMap<u64, u64>,
    sessions_back: HashMap<u64, (u64, u64)>,
    /// Each request waiting for its answer: the renderer's instance it is for.
    requests: HashMap<u64, u64>,
}

/// The browser's end of one renderer's key systems.
pub struct CdmHost {
    port: BrowserPort,
    backend: Option<Arc<dyn Cdm>>,
    site_lock: super::site_lock::Mode,
    /// Where the browser's modules report for this renderer.
    inbox: Arc<CdmInbox>,
    ids: Mutex<Ids>,
}

impl CdmHost {
    pub fn new(
        port: BrowserPort,
        backend: Option<Arc<dyn Cdm>>,
        site_lock: super::site_lock::Mode,
    ) -> Arc<CdmHost> {
        Arc::new_cyclic(|host: &Weak<CdmHost>| {
            let host = host.clone();
            CdmHost {
                port,
                backend,
                site_lock,
                inbox: Arc::new(CdmInbox::new(Arc::new(move || {
                    if let Some(host) = host.upgrade() {
                        host.forward();
                    }
                }))),
                ids: Mutex::default(),
            }
        })
    }

    fn ids(&self) -> std::sync::MutexGuard<'_, Ids> {
        self.ids.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends what the modules reported, in the renderer's ids.
    fn forward(&self) {
        let mut ids = self.ids();
        for event in self.inbox.take() {
            let (keys, event) = match event {
                CdmEvent::Done { request, result } => {
                    let Some(keys) = ids.requests.remove(&request) else {
                        continue;
                    };
                    (keys, CdmEvent::Done { request, result })
                }
                CdmEvent::Message {
                    session,
                    kind,
                    data,
                } => {
                    let Some(&(session, keys)) = ids.sessions_back.get(&session) else {
                        continue;
                    };
                    let event = CdmEvent::Message {
                        session,
                        kind,
                        data,
                    };
                    (keys, event)
                }
                CdmEvent::KeyStatuses { session, statuses } => {
                    let Some(&(session, keys)) = ids.sessions_back.get(&session) else {
                        continue;
                    };
                    (keys, CdmEvent::KeyStatuses { session, statuses })
                }
                CdmEvent::Expiration { session, time } => {
                    let Some(&(session, keys)) = ids.sessions_back.get(&session) else {
                        continue;
                    };
                    (keys, CdmEvent::Expiration { session, time })
                }
                CdmEvent::Closed { session } => {
                    let Some(&(session, keys)) = ids.sessions_back.get(&session) else {
                        continue;
                    };
                    (keys, CdmEvent::Closed { session })
                }
            };
            let _ = self
                .port
                .send(&super::Down::Cdm(Down::Event { keys, event }));
        }
        drop(ids);
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    /// The browser's session for the renderer's `session`, with `request` recorded for its
    /// instance; a session it never made kills it.
    fn session(&self, request: u64, session: u64) -> Result<u64, Error> {
        let mut ids = self.ids();
        let Some(&browser) = ids.sessions.get(&session) else {
            drop(ids);
            return Err(self.bad(format!("a call on session {session}, which it never made")));
        };
        let keys = ids.sessions_back.get(&browser).map_or(0, |s| s.1);
        if ids.requests.insert(request, keys).is_some() {
            drop(ids);
            return Err(self.bad(format!("request {request} made twice")));
        }
        Ok(browser)
    }

    fn too_big(&self, len: usize) -> Result<(), Error> {
        if len > MAX_DATA {
            return Err(self.bad(format!("{len} bytes for a key system")));
        }
        Ok(())
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        let Some(backend) = self.backend.clone() else {
            // (no key systems in this browser: nothing is offered, and nothing can be made)
            if let (Up::Supports { .. }, Some(r)) = (&message, replier) {
                return answer(&self.port, Some(r), &false);
            }
            if let Up::CreateKeys { request, keys, .. } = message {
                let _ = self.port.send(&super::Down::Cdm(Down::Event {
                    keys,
                    event: CdmEvent::Done {
                        request,
                        result: Err(CdmError::NotSupported("No key systems".into())),
                    },
                }));
            }
            return Ok(());
        };
        match message {
            Up::Supports {
                key_system,
                content_type,
            } => {
                let Some(r) = replier else {
                    return Err(self.bad("a key system question that is not a call".into()));
                };
                answer(
                    &self.port,
                    Some(r),
                    &backend.supports(&key_system, &content_type),
                )
            }
            Up::CreateKeys {
                request,
                keys,
                key_system,
                origin,
            } => {
                super::site_lock::check(
                    &self.port,
                    self.site_lock,
                    super::site_lock::posted_origin_violation(self.port.policy(), &origin),
                )?;
                let mut ids = self.ids();
                if ids.keys.contains_key(&keys) || ids.requests.contains_key(&request) {
                    drop(ids);
                    return Err(self.bad(format!("media keys {keys} made twice")));
                }
                ids.requests.insert(request, keys);
                drop(ids);
                let browser = backend.create_keys(request, &key_system, &origin, &self.inbox);
                let mut ids = self.ids();
                ids.keys.insert(keys, browser);
                ids.keys_back.insert(browser, keys);
                Ok(())
            }
            Up::CreateSession { keys, session } => {
                let ids = self.ids();
                let Some(&browser_keys) = ids.keys.get(&keys) else {
                    drop(ids);
                    return Err(self.bad(format!("a session of media keys {keys}, never made")));
                };
                if ids.sessions.contains_key(&session) {
                    drop(ids);
                    return Err(self.bad(format!("session {session} made twice")));
                }
                drop(ids);
                let browser = backend.create_session(browser_keys);
                let mut ids = self.ids();
                ids.sessions.insert(session, browser);
                ids.sessions_back.insert(browser, (session, keys));
                Ok(())
            }
            Up::Generate {
                request,
                session,
                init_data_type,
                data,
            } => {
                self.too_big(data.len())?;
                let s = self.session(request, session)?;
                backend.generate_request(request, s, &init_data_type, data);
                Ok(())
            }
            Up::Update {
                request,
                session,
                response,
            } => {
                self.too_big(response.len())?;
                let s = self.session(request, session)?;
                backend.update(request, s, response);
                Ok(())
            }
            Up::Close { request, session } => {
                let s = self.session(request, session)?;
                backend.close(request, s);
                Ok(())
            }
            Up::Remove { request, session } => {
                let s = self.session(request, session)?;
                backend.remove(request, s);
                Ok(())
            }
            Up::Release { keys } => {
                let mut ids = self.ids();
                let Some(browser) = ids.keys.remove(&keys) else {
                    drop(ids);
                    return Err(self.bad(format!("releasing media keys {keys}, never made")));
                };
                ids.keys_back.remove(&browser);
                let gone: Vec<u64> = ids
                    .sessions_back
                    .values()
                    .filter(|(_, k)| *k == keys)
                    .map(|(s, _)| *s)
                    .collect();
                for s in gone {
                    if let Some(b) = ids.sessions.remove(&s) {
                        ids.sessions_back.remove(&b);
                    }
                }
                drop(ids);
                backend.release(browser);
                Ok(())
            }
        }
    }

    /// The renderer is gone: so are its module instances.
    pub(super) fn gone(&self) {
        let keys: Vec<u64> = self.ids().keys.drain().map(|(_, b)| b).collect();
        if let Some(backend) = &self.backend {
            for k in keys {
                backend.release(k);
            }
        }
        *self.ids() = Ids::default();
    }
}
