// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's PlayReady modules and their sessions, for pages that reach them by messages
//! (Encrypted Media Extensions with the module outside the page's process).
//!
//! One thread of the [`Service`] owns every module and session ([`super::playready`]): the calls
//! are queued to it and answered through each module's [`Sink`], as are the sessions' messages and
//! key status changes, which Media Foundation reports on its own threads. Ids are the caller's.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFContentDecryptionModule, IMFContentDecryptionModuleSession,
};

use super::playready::{self, SessionEvent};

/// What the service reports for one module's caller.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The module of [`Service::create`] `request` was made, or why not.
    Created {
        request: u64,
        result: Result<(), String>,
    },
    /// A session call ended: [`Service::generate_request`]'s with the session's id.
    Done {
        request: u64,
        result: Result<String, String>,
    },
    /// A message for the licence server: its EME type and bytes.
    Message {
        session: u64,
        kind: &'static str,
        data: Vec<u8>,
    },
    /// The session's key statuses: each key id (as the media carries it) and its EME status.
    Statuses {
        session: u64,
        statuses: Vec<([u8; 16], &'static str)>,
    },
    /// The session's expiration time in milliseconds since 1970 (NaN: none).
    Expiration { session: u64, time: f64 },
    /// The session is closed.
    Closed { session: u64 },
}

/// Where one module's events go (from any thread).
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

enum Job {
    Create {
        request: u64,
        keys: u64,
        key_system: String,
        origin_id: String,
        store: PathBuf,
        sink: Sink,
    },
    Generate {
        request: u64,
        keys: u64,
        session: u64,
        init_data_type: String,
        init_data: Vec<u8>,
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
    /// A session's key statuses changed (from Media Foundation's thread).
    Changed {
        session: u64,
    },
    /// The service is dropped.
    Stop,
}

/// See the module.
pub struct Service {
    tx: Mutex<Sender<Job>>,
}

impl Service {
    /// Starts the service's thread; it ends when the service is dropped.
    pub fn start() -> std::io::Result<Service> {
        let (tx, rx) = channel();
        let worker_tx = tx.clone();
        std::thread::Builder::new()
            .name("playready".into())
            .spawn(move || run(rx, worker_tx))?;
        Ok(Service { tx: Mutex::new(tx) })
    }

    fn send(&self, job: Job) {
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(job);
    }

    /// Makes module `keys` of `key_system` for one origin (its id for the module, its store
    /// folder); `Event::Created` answers `request`.
    pub fn create(
        &self,
        request: u64,
        keys: u64,
        key_system: &str,
        origin_id: &str,
        store: PathBuf,
        sink: Sink,
    ) {
        self.send(Job::Create {
            request,
            keys,
            key_system: key_system.to_owned(),
            origin_id: origin_id.to_owned(),
            store,
            sink,
        });
    }

    /// Makes temporary session `session` of module `keys` and has it generate a licence request.
    pub fn generate_request(
        &self,
        request: u64,
        keys: u64,
        session: u64,
        init_data_type: &str,
        init_data: Vec<u8>,
    ) {
        self.send(Job::Generate {
            request,
            keys,
            session,
            init_data_type: init_data_type.to_owned(),
            init_data,
        });
    }

    pub fn update(&self, request: u64, session: u64, response: Vec<u8>) {
        self.send(Job::Update {
            request,
            session,
            response,
        });
    }

    pub fn close(&self, request: u64, session: u64) {
        self.send(Job::Close { request, session });
    }

    pub fn remove(&self, request: u64, session: u64) {
        self.send(Job::Remove { request, session });
    }

    /// Module `keys` and its sessions are no longer used.
    pub fn release(&self, keys: u64) {
        self.send(Job::Release { keys });
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // (the sessions' callbacks hold senders too: the thread ends on this, not on the channel)
        self.send(Job::Stop);
    }
}

/// Whether this Windows has the PlayReady module this offers: Windows 10 2004 (build 19041) or
/// later, the first with `IMFMediaEngineClassFactory4`.
pub fn system_supports() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 32];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: the buffer and its byte length are passed together; the value is a string.
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows::core::w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            windows::core::w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if read.is_err() {
        return false;
    }
    let text = String::from_utf16_lossy(&buf[..(len as usize / 2).min(buf.len())]);
    text.trim_end_matches('\0')
        .parse::<u32>()
        .is_ok_and(|b| b >= 19041)
}

struct Module {
    cdm: IMFContentDecryptionModule,
    sink: Sink,
}

struct Session {
    keys: u64,
    session: IMFContentDecryptionModuleSession,
}

fn error(e: &windows::core::Error) -> String {
    format!("{:#010x}: {}", e.code().0 as u32, e.message())
}

/// The session's id (`GetSessionId`).
fn session_id(s: &IMFContentDecryptionModuleSession) -> Option<String> {
    // SAFETY: the string is a CoTaskMem allocation this function reads and frees.
    unsafe {
        let p: PWSTR = s.GetSessionId().ok()?;
        if p.is_null() {
            return None;
        }
        let id = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        id.filter(|i| !i.is_empty())
    }
}

fn run(rx: Receiver<Job>, tx: Sender<Job>) {
    super::ensure_com();
    let mut modules: HashMap<u64, Module> = HashMap::new();
    let mut sessions: HashMap<u64, Session> = HashMap::new();
    let tx = Arc::new(Mutex::new(tx));
    while let Ok(job) = rx.recv() {
        match job {
            Job::Create {
                request,
                keys,
                key_system,
                origin_id,
                store,
                sink,
            } => {
                let hardware = key_system == playready::RECOMMENDATION_3000;
                let result = playready::create(&key_system, hardware, &origin_id, &store);
                match result {
                    Ok(cdm) => {
                        sink(Event::Created {
                            request,
                            result: Ok(()),
                        });
                        modules.insert(keys, Module { cdm, sink });
                    }
                    Err(e) => sink(Event::Created {
                        request,
                        result: Err(error(&e)),
                    }),
                }
            }
            Job::Generate {
                request,
                keys,
                session,
                init_data_type,
                init_data,
            } => {
                let Some(m) = modules.get(&keys) else {
                    continue;
                };
                let sink = m.sink.clone();
                let notify = {
                    let sink = sink.clone();
                    let tx = tx.clone();
                    Box::new(move |e| match e {
                        SessionEvent::Message(kind, data) => sink(Event::Message {
                            session,
                            kind,
                            data,
                        }),
                        SessionEvent::KeysChanged => {
                            let _ = tx
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .send(Job::Changed { session });
                        }
                    })
                };
                let made = playready::session_with(&m.cdm, notify).and_then(|s| {
                    // SAFETY: a plain call with live data.
                    unsafe { s.GenerateRequest(&HSTRING::from(init_data_type), &init_data) }
                        .map(|()| s)
                });
                let result = match made {
                    Ok(s) => {
                        let id = session_id(&s).unwrap_or_default();
                        sessions.insert(session, Session { keys, session: s });
                        Ok(id)
                    }
                    Err(e) => Err(error(&e)),
                };
                sink(Event::Done { request, result });
            }
            Job::Update {
                request,
                session,
                response,
            } => {
                let Some(s) = sessions.get(&session) else {
                    continue;
                };
                // SAFETY: a plain call with live data.
                let result = unsafe { s.session.Update(&response) }
                    .map(|()| String::new())
                    .map_err(|e| error(&e));
                if let Some(m) = modules.get(&s.keys) {
                    (m.sink)(Event::Done { request, result });
                }
            }
            Job::Close { request, session } => {
                let Some(s) = sessions.remove(&session) else {
                    continue;
                };
                // SAFETY: a plain call on the live session.
                let result = unsafe { s.session.Close() }
                    .map(|()| String::new())
                    .map_err(|e| error(&e));
                if let Some(m) = modules.get(&s.keys) {
                    (m.sink)(Event::Closed { session });
                    (m.sink)(Event::Done { request, result });
                }
            }
            Job::Remove { request, session } => {
                let Some(s) = sessions.get(&session) else {
                    continue;
                };
                // SAFETY: a plain call on the live session.
                let result = unsafe { s.session.Remove() }
                    .map(|()| String::new())
                    .map_err(|e| error(&e));
                if let Some(m) = modules.get(&s.keys) {
                    (m.sink)(Event::Done { request, result });
                }
            }
            Job::Release { keys } => {
                sessions.retain(|_, s| {
                    if s.keys != keys {
                        return true;
                    }
                    // SAFETY: a plain call on the live session.
                    let _ = unsafe { s.session.Close() };
                    false
                });
                modules.remove(&keys);
            }
            Job::Stop => break,
            Job::Changed { session } => {
                let Some(s) = sessions.get(&session) else {
                    continue;
                };
                let Some(m) = modules.get(&s.keys) else {
                    continue;
                };
                if let Ok(statuses) = playready::key_statuses(&s.session) {
                    (m.sink)(Event::Statuses { session, statuses });
                }
                // SAFETY: a plain call on the live session.
                if let Ok(time) = unsafe { s.session.GetExpiration() } {
                    (m.sink)(Event::Expiration { session, time });
                }
            }
        }
    }
    for (_, s) in sessions {
        // SAFETY: a plain call on the live session.
        let _ = unsafe { s.session.Close() };
    }
}
