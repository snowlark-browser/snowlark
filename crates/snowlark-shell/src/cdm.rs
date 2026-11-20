// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's key systems for pages ([`snowlark_page::media::cdm::Cdm`]): PlayReady at the
//! hardware security level (`com.microsoft.playready.recommendation.3000`) through the module
//! Windows ships, run by `snowlark_media::win::playready_service` in this process. It is offered
//! only while `SNOWLARK_PLAYREADY=1` (a test setting): otherwise no page sees it.
//!
//! Each origin's module state is kept in a folder of its own under the profile's `cdm` folder,
//! named by a random id the browser keeps for that origin (`origins.json`); the module is given
//! that id, never the origin. An Incognito profile's lives in a temporary folder that is deleted
//! with the profile's partition, and its ids are never written down.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use snowlark_media::win::playready::{type_supported, RECOMMENDATION_3000};
use snowlark_media::win::playready_service::{system_supports, Event, Service};
use snowlark_page::media::cdm::{Cdm, CdmError, CdmEvent, CdmInbox, KeyStatus, MessageKind};

/// See the module.
pub struct BrowserCdm {
    enabled: bool,
    /// The profile's `cdm` folder, or the temporary folder of an Incognito profile.
    root: PathBuf,
    temporary: bool,
    service: OnceLock<Option<Service>>,
    /// Each origin's id.
    origins: Mutex<HashMap<String, String>>,
    next: AtomicU64,
    /// Each module instance's inbox, and each session's instance.
    inboxes: Mutex<HashMap<u64, Arc<CdmInbox>>>,
    sessions: Mutex<HashMap<u64, u64>>,
}

/// 128 random bits as hex (the standard library's random hash keys, from the system's generator).
fn random_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

impl BrowserCdm {
    /// The regular profile's: state under `dir`.
    pub fn persistent(dir: PathBuf) -> Arc<BrowserCdm> {
        let origins = std::fs::read(dir.join("origins.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(BrowserCdm::new(dir, false, origins))
    }

    /// An Incognito profile's: state in a temporary folder deleted with it.
    pub fn off_the_record() -> Arc<BrowserCdm> {
        let dir = std::env::temp_dir().join(format!(
            "snowlark-cdm-{}-{}",
            std::process::id(),
            random_id()
        ));
        Arc::new(BrowserCdm::new(dir, true, HashMap::new()))
    }

    fn new(root: PathBuf, temporary: bool, origins: HashMap<String, String>) -> BrowserCdm {
        BrowserCdm {
            enabled: std::env::var("SNOWLARK_PLAYREADY").is_ok_and(|v| v == "1"),
            root,
            temporary,
            service: OnceLock::new(),
            origins: Mutex::new(origins),
            next: AtomicU64::new(0),
            inboxes: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    fn service(&self) -> Option<&Service> {
        self.service.get_or_init(|| Service::start().ok()).as_ref()
    }

    /// The id the module knows `origin` by (made and kept on first use).
    fn origin_id(&self, origin: &str) -> String {
        let mut origins = self.origins.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = origins.get(origin) {
            return id.clone();
        }
        let id = random_id();
        origins.insert(origin.to_owned(), id.clone());
        if !self.temporary {
            let _ = std::fs::create_dir_all(&self.root);
            if let Ok(json) = serde_json::to_vec_pretty(&*origins) {
                let _ = std::fs::write(self.root.join("origins.json"), json);
            }
        }
        id
    }

    fn offered(&self, key_system: &str) -> bool {
        static SYSTEM: OnceLock<bool> = OnceLock::new();
        self.enabled && key_system == RECOMMENDATION_3000 && *SYSTEM.get_or_init(system_supports)
    }

    fn inbox_of_session(&self, session: u64) -> Option<(u64, Arc<CdmInbox>)> {
        let keys = *self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)?;
        let inbox = self
            .inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&keys)?
            .clone();
        Some((keys, inbox))
    }
}

impl Drop for BrowserCdm {
    fn drop(&mut self) {
        // (the modules first: they hold files in the folder)
        drop(self.service.take());
        if self.temporary {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn message_kind(kind: &str) -> MessageKind {
    match kind {
        "license-renewal" => MessageKind::LicenseRenewal,
        "license-release" => MessageKind::LicenseRelease,
        "individualization-request" => MessageKind::IndividualizationRequest,
        _ => MessageKind::LicenseRequest,
    }
}

fn key_status(status: &str) -> KeyStatus {
    match status {
        "usable" => KeyStatus::Usable,
        "expired" => KeyStatus::Expired,
        "released" => KeyStatus::Released,
        "output-restricted" => KeyStatus::OutputRestricted,
        "output-downscaled" => KeyStatus::OutputDownscaled,
        "status-pending" => KeyStatus::StatusPending,
        "usable-in-future" => KeyStatus::UsableInFuture,
        _ => KeyStatus::InternalError,
    }
}

/// A service event as the page's.
fn page_event(event: Event) -> CdmEvent {
    match event {
        Event::Created { request, result } => CdmEvent::Done {
            request,
            result: result
                .map(|()| String::new())
                .map_err(CdmError::NotSupported),
        },
        Event::Done { request, result } => CdmEvent::Done {
            request,
            result: result.map_err(CdmError::Module),
        },
        Event::Message {
            session,
            kind,
            data,
        } => CdmEvent::Message {
            session,
            kind: message_kind(kind),
            data,
        },
        Event::Statuses { session, statuses } => CdmEvent::KeyStatuses {
            session,
            statuses: statuses
                .into_iter()
                .map(|(id, s)| (id.to_vec(), key_status(s)))
                .collect(),
        },
        Event::Expiration { session, time } => CdmEvent::Expiration { session, time },
        Event::Closed { session } => CdmEvent::Closed { session },
    }
}

impl Cdm for BrowserCdm {
    fn supports(&self, key_system: &str, content_type: &str) -> bool {
        self.offered(key_system) && type_supported(key_system, content_type)
    }

    fn create_keys(
        &self,
        request: u64,
        key_system: &str,
        origin: &str,
        inbox: &Arc<CdmInbox>,
    ) -> u64 {
        let keys = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let service = self.service().filter(|_| self.offered(key_system));
        let Some(service) = service else {
            inbox.deliver(CdmEvent::Done {
                request,
                result: Err(CdmError::NotSupported(
                    "The key system is not available".into(),
                )),
            });
            return keys;
        };
        self.inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(keys, inbox.clone());
        let id = self.origin_id(origin);
        let store = self.root.join(&id).join(key_system);
        let to = inbox.clone();
        service.create(
            request,
            keys,
            key_system,
            &id,
            store,
            Arc::new(move |e| to.deliver(page_event(e))),
        );
        keys
    }

    fn create_session(&self, keys: u64) -> u64 {
        let session = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session, keys);
        session
    }

    fn generate_request(&self, request: u64, session: u64, init_data_type: &str, data: Vec<u8>) {
        if let (Some((keys, _)), Some(service)) = (self.inbox_of_session(session), self.service()) {
            service.generate_request(request, keys, session, init_data_type, data);
        }
    }

    fn update(&self, request: u64, session: u64, response: Vec<u8>) {
        if let Some(service) = self.service() {
            service.update(request, session, response);
        }
    }

    fn close(&self, request: u64, session: u64) {
        let known = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session)
            .is_some();
        if let (true, Some(service)) = (known, self.service()) {
            service.close(request, session);
        }
    }

    fn remove(&self, request: u64, session: u64) {
        if let Some(service) = self.service() {
            service.remove(request, session);
        }
    }

    fn release(&self, keys: u64) {
        self.inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&keys);
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, k| *k != keys);
        if let Some(service) = self.service.get().and_then(|s| s.as_ref()) {
            service.release(keys);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real module through the page's calls (`SNOWLARK_PLAYREADY=1`, hardware DRM, and
    /// `SNOWLARK_PLAYREADY_MP4` naming a PlayReady `cenc` file; run by hand): an instance for an
    /// origin, a session, and the licence request its `pssh` leads to.
    #[test]
    #[ignore]
    fn the_module_makes_a_licence_request() {
        let Some(path) = std::env::var_os("SNOWLARK_PLAYREADY_MP4") else {
            eprintln!("SNOWLARK_PLAYREADY_MP4 is not set: skipped");
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        let (_, found) = snowlark_media::cenc::Decryptor::new().push(
            &bytes[..bytes.len().min(1 << 20)],
            &HashMap::<[u8; 16], [u8; 16]>::new(),
        );
        let pssh = found.init_data.first().expect("a pssh").clone();
        let dir = std::env::temp_dir().join(format!("snowlark-cdm-test-{}", std::process::id()));
        let cdm = BrowserCdm::persistent(dir.clone());
        assert!(cdm.supports(RECOMMENDATION_3000, "video/mp4; codecs=\"avc1.64001f\""));
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let inbox = Arc::new(CdmInbox::new(Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        })));
        let keys = cdm.create_keys(1, RECOMMENDATION_3000, "https://example.test", &inbox);
        let session = cdm.create_session(keys);
        cdm.generate_request(2, session, "cenc", pssh);
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline && seen.len() < 3 {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
            for e in inbox.take() {
                eprintln!(
                    "{:?}",
                    match &e {
                        CdmEvent::Message { kind, data, .. } =>
                            format!("message {kind:?} {} bytes", data.len()),
                        other => format!("{other:?}"),
                    }
                );
                seen.push(e);
            }
        }
        cdm.release(keys);
        drop(cdm);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.contains(&CdmEvent::Done {
            request: 1,
            result: Ok(String::new())
        }));
        assert!(seen
            .iter()
            .any(|e| matches!(e, CdmEvent::Done { request: 2, result: Ok(id) } if !id.is_empty())));
        assert!(seen.iter().any(|e| matches!(e, CdmEvent::Message { kind: MessageKind::LicenseRequest, data, .. } if data.len() > 1000)));
    }
}
