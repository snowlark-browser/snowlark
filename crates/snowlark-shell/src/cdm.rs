// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's key systems for pages ([`snowlark_page::media::cdm::Cdm`]): PlayReady at the
//! hardware security level (`com.microsoft.playready.recommendation.3000`) through the module
//! Windows ships, run by `snowlark_media::win::playready_service` in this process. It is offered
//! to pages in the window where Windows has it (Windows 10 2004 or later, with hardware DRM: the
//! system's media engine plays H.264 under it), unless `SNOWLARK_PLAYREADY=0`; the headless modes
//! have no key system of the browser's.
//!
//! Each origin's module state is kept in a folder of its own under the profile's `cdm` folder,
//! named by a random id the browser keeps for that origin (`origins.json`); the module is given
//! that id, never the origin. An Incognito profile's lives in a temporary folder that is deleted
//! with the profile's partition, and its ids are never written down.

use std::collections::HashMap;
use std::os::windows::io::OwnedHandle;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use snowlark_media::fmp4::{Sample, Track};
use snowlark_media::win::playready::{type_supported, RECOMMENDATION, RECOMMENDATION_3000};
use snowlark_media::win::playready_service::{
    system_supports, Control, EngineState, Event, PlayerEvent, Service,
};
use snowlark_page::media::cdm::{
    Cdm, CdmError, CdmEvent, CdmInbox, KeyStatus, MessageKind, PlayerControl, PlayerReport,
    PlayerState,
};
use snowlark_tab::comp::ProtectedPictures;

/// See the module.
pub struct BrowserCdm {
    enabled: bool,
    /// The profile's `cdm` folder, or the temporary folder of an Incognito profile.
    root: PathBuf,
    temporary: bool,
    service: OnceLock<Option<Arc<Service>>>,
    /// Each origin's id.
    origins: Mutex<HashMap<String, String>>,
    next: AtomicU64,
    /// Each module instance's inbox, and each session's instance.
    inboxes: Mutex<HashMap<u64, Arc<CdmInbox>>>,
    sessions: Mutex<HashMap<u64, u64>>,
    /// Each protected player's picture, for the compositor of the windows of this profile.
    pictures: Arc<Pictures>,
}

/// The pictures of a profile's protected players ([`ProtectedPictures`]): each one's composition
/// surface once its engine has one, and where it shows.
#[derive(Default)]
pub struct Pictures {
    players: Mutex<HashMap<u64, Picture>>,
}

struct Picture {
    surface: Option<Arc<OwnedHandle>>,
    /// Where it was last placed (screen device pixels).
    placed: Option<(i32, i32, i32, i32)>,
    service: Weak<Service>,
}

impl Pictures {
    fn players(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Picture>> {
        self.players.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ProtectedPictures for Pictures {
    fn surface(&self, player: u64) -> Option<Arc<OwnedHandle>> {
        self.players().get(&player)?.surface.clone()
    }

    fn place(&self, player: u64, x: i32, y: i32, width: i32, height: i32) {
        let service = {
            let mut players = self.players();
            let Some(p) = players.get_mut(&player) else {
                return;
            };
            let at = Some((x, y, width, height));
            if p.placed == at {
                return;
            }
            p.placed = at;
            p.service.upgrade()
        };
        // (the engine's output rectangle: the protection window goes there, the picture is
        // drawn at its size)
        if tracing() {
            eprintln!("[cdm] player {player}: output at {x},{y} {width}x{height}");
        }
        if let Some(service) = service {
            service.control(
                player,
                Control::Output {
                    x,
                    y,
                    width,
                    height,
                },
            );
        }
    }
}

/// The module key system that plays a key system a page asks for, as the page's EME names it (the
/// name, or the name and a capability's robustness level after a dot). The one module is
/// PlayReady's hardware level, `com.microsoft.playready.recommendation.3000`; its licences of a
/// lower level play on it too, so it also plays the names Microsoft's browser answers for lower
/// levels with the same module: `com.microsoft.playready.recommendation` with robustness `""`,
/// `"2000"` or `"150"`, and the older `com.microsoft.playready` (deprecated, still answered) with
/// those or `"3000"`.
fn module_system(key_system: &str) -> Option<&'static str> {
    const PLAIN: &str = "com.microsoft.playready";
    // (the longer name first: the older one is its prefix)
    let level = [RECOMMENDATION, PLAIN]
        .into_iter()
        .find_map(|n| key_system.strip_prefix(n))?;
    let level = match level {
        "" => "",
        l => l.strip_prefix('.')?,
    };
    matches!(level, "" | "150" | "2000" | "3000").then_some(RECOMMENDATION_3000)
}

/// Whether `SNOWLARK_CDM_TRACE=1`: a line on stderr for each protected player made, its controls,
/// every 200th sample, and what its engine reports (`[cdm]`).
fn tracing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("SNOWLARK_CDM_TRACE").is_ok_and(|v| v == "1"))
}

/// A track for a trace line: its codec and size or channels, its timescale and start, its
/// protection.
fn track_summary(t: &Track) -> String {
    use snowlark_media::fmp4::Codec;
    let codec = match &t.codec {
        Codec::Avc {
            parameter_sets,
            length_size,
        } => format!(
            "H.264 ({} bytes of parameter sets, {length_size}-byte lengths) {}x{}",
            parameter_sets.len(),
            t.width,
            t.height
        ),
        Codec::Aac { config } => format!(
            "AAC {config:02x?} {} channels {} Hz",
            t.channels, t.sample_rate
        ),
        Codec::Other(f) => format!("{}", String::from_utf8_lossy(f)),
    };
    let protection = match &t.encryption {
        Some(e) => format!("{:?} key {:02x?}", e.scheme, e.key_id),
        None => "clear".into(),
    };
    format!(
        "track {}: {codec}, timescale {}, start {}, {protection}",
        t.id, t.timescale, t.start
    )
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
            enabled: std::env::var("SNOWLARK_PLAYREADY").map_or(true, |v| v != "0"),
            root,
            temporary,
            service: OnceLock::new(),
            origins: Mutex::new(origins),
            next: AtomicU64::new(0),
            inboxes: Mutex::default(),
            sessions: Mutex::default(),
            pictures: Arc::default(),
        }
    }

    /// The pictures of this profile's protected players, for its windows' compositors.
    pub fn pictures(&self) -> Arc<dyn ProtectedPictures> {
        self.pictures.clone()
    }

    fn service(&self) -> Option<&Arc<Service>> {
        self.service
            .get_or_init(|| Service::start().ok().map(Arc::new))
            .as_ref()
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

    /// The module key system that plays `key_system`, when it is offered.
    fn offered(&self, key_system: &str) -> Option<&'static str> {
        static SYSTEM: OnceLock<bool> = OnceLock::new();
        // (offered where the system's media engine answers "probably" for H.264 under it)
        let module = module_system(key_system)?;
        (self.enabled
            && *SYSTEM.get_or_init(|| {
                system_supports()
                    && type_supported(RECOMMENDATION_3000, "video/mp4;codecs=\"avc1.42e01e\"")
            }))
        .then_some(module)
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
    if tracing() {
        match &event {
            Event::Message { session, data, .. } => {
                eprintln!("[cdm] session {session}: message of {} bytes", data.len())
            }
            other => eprintln!("[cdm] {other:?}"),
        }
    }
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

/// A player event as the page's report (none: it stays in the browser).
fn player_report(event: PlayerEvent) -> Option<PlayerReport> {
    Some(match event {
        PlayerEvent::Error(why) => PlayerReport::Error(why),
        PlayerEvent::Surface(_) => return None,
        PlayerEvent::Engine { state, time } => PlayerReport::State {
            state: match state {
                EngineState::FirstFrame => PlayerState::FirstFrame,
                EngineState::Playing => PlayerState::Playing,
                EngineState::Waiting => PlayerState::Waiting,
                EngineState::Paused => PlayerState::Paused,
                EngineState::Seeked => PlayerState::Seeked,
                EngineState::Ended => PlayerState::Ended,
            },
            time,
        },
        PlayerEvent::Progress {
            time,
            rendered,
            dropped,
        } => PlayerReport::Progress {
            time,
            rendered,
            dropped,
        },
    })
}

impl Cdm for BrowserCdm {
    fn supports(&self, key_system: &str, content_type: &str) -> bool {
        self.offered(key_system)
            .is_some_and(|module| type_supported(module, content_type))
    }

    fn create_keys(
        &self,
        request: u64,
        key_system: &str,
        origin: &str,
        inbox: &Arc<CdmInbox>,
    ) -> u64 {
        let keys = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let offered = self.offered(key_system);
        let (Some(service), Some(key_system)) = (self.service(), offered) else {
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

    fn play(&self, keys: u64, tracks: Vec<Track>) -> u64 {
        let player = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let inbox = self
            .inboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&keys)
            .cloned();
        if tracing() {
            let kinds: Vec<String> = tracks.iter().map(track_summary).collect();
            eprintln!(
                "[cdm] player {player} of instance {keys}: {}",
                kinds.join("; ")
            );
        }
        let (Some(inbox), Some(service)) = (inbox, self.service.get().and_then(|s| s.as_ref()))
        else {
            if tracing() {
                eprintln!("[cdm] player {player}: no instance or no service");
            }
            return player;
        };
        self.pictures.players().insert(
            player,
            Picture {
                surface: None,
                placed: None,
                service: Arc::downgrade(service),
            },
        );
        let pictures = Arc::downgrade(&self.pictures);
        service.play(
            player,
            keys,
            tracks,
            Arc::new(move |e| {
                if tracing() && !matches!(e, PlayerEvent::Progress { .. }) {
                    eprintln!("[cdm] player {player}: {e:?}");
                }
                if let PlayerEvent::Progress {
                    time,
                    rendered,
                    dropped,
                } = &e
                {
                    if tracing() {
                        eprintln!(
                            "[cdm] player {player}: at {time:.2} s, {rendered} frames, {dropped} dropped"
                        );
                    }
                }
                if let PlayerEvent::Surface(h) = e {
                    if let Some(pictures) = pictures.upgrade() {
                        if let Some(p) = pictures.players().get_mut(&player) {
                            p.surface = Some(Arc::new(h));
                        }
                    }
                } else if let Some(report) = player_report(e) {
                    inbox.deliver(CdmEvent::Player { player, report });
                }
            }),
        );
        player
    }

    fn sample(&self, player: u64, track: u32, sample: Sample) {
        if tracing() {
            static SAMPLES: AtomicU64 = AtomicU64::new(0);
            let n = SAMPLES.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(200) {
                eprintln!(
                    "[cdm] sample {n}: player {player} track {track}, {} bytes",
                    sample.data.len()
                );
            }
        }
        if let Some(service) = self.service.get().and_then(|s| s.as_ref()) {
            service.sample(player, track, sample);
        }
    }

    fn control(&self, player: u64, control: PlayerControl) {
        let control = match control {
            PlayerControl::Play => Control::Play,
            PlayerControl::Pause => Control::Pause,
            PlayerControl::Flush => Control::Flush,
            PlayerControl::Seek(t) => Control::Seek(t),
            PlayerControl::Rate(r) => Control::Rate(r),
            PlayerControl::Volume(v) => Control::Volume(v),
            PlayerControl::Muted(m) => Control::Muted(m),
            PlayerControl::End(t) => Control::End(t),
        };
        if tracing() {
            eprintln!("[cdm] player {player}: {control:?}");
        }
        if let Some(service) = self.service.get().and_then(|s| s.as_ref()) {
            service.control(player, control);
        }
    }

    fn stop(&self, player: u64) {
        self.pictures.players().remove(&player);
        if let Some(service) = self.service.get().and_then(|s| s.as_ref()) {
            service.stop(player);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names a page's EME asks of the module (the key system, and a robustness level after a
    /// dot) that the hardware level plays: Microsoft's names for PlayReady at that level or a lower
    /// one, as their browser answers them, and nothing else.
    #[test]
    fn the_names_the_hardware_level_plays() {
        for name in [
            "com.microsoft.playready.recommendation.3000",
            "com.microsoft.playready.recommendation",
            "com.microsoft.playready.recommendation.2000",
            "com.microsoft.playready.recommendation.150",
            "com.microsoft.playready",
            "com.microsoft.playready.3000",
            "com.microsoft.playready.2000",
            "com.microsoft.playready.150",
        ] {
            assert_eq!(module_system(name), Some(RECOMMENDATION_3000), "{name}");
        }
        for name in [
            "com.microsoft.playready.recommendation.3000.2000",
            "com.microsoft.playready.recommendation.3000.3000",
            "com.microsoft.playready.recommendation.1000",
            "com.microsoft.playready.recommendationx",
            "com.microsoft.playready.hardware",
            "com.microsoft.playready.software",
            "com.microsoft.playreadyx",
            "com.microsoft.play",
            "com.widevine.alpha",
            "",
        ] {
            assert_eq!(module_system(name), None, "{name}");
        }
    }

    /// The real module through the page's calls (`SNOWLARK_PLAYREADY=1`, hardware DRM, and
    /// `SNOWLARK_PLAYREADY_MP4` naming a PlayReady `cenc` file; run by hand): an instance for an
    /// origin, a session, and the licence request its `pssh` leads to; the instance is asked by the
    /// older name, which the hardware level plays.
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
        for name in [
            "com.microsoft.playready.3000",
            "com.microsoft.playready",
            RECOMMENDATION,
        ] {
            assert!(
                cdm.supports(name, "video/mp4;codecs=\"avc1.42E01E\""),
                "{name}"
            );
            assert!(
                cdm.supports(name, "audio/mp4; codecs=\"mp4a.40.2\""),
                "{name}"
            );
        }
        assert!(!cdm.supports("com.microsoft.playready.hardware", "video/mp4"));
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let inbox = Arc::new(CdmInbox::new(Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        })));
        let keys = cdm.create_keys(1, "com.microsoft.playready", "https://example.test", &inbox);
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
