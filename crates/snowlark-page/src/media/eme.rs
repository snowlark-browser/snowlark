// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Encrypted Media Extensions (W3C EME): `navigator.requestMediaKeySystemAccess`,
//! `MediaKeySystemAccess`, `MediaKeys`, `MediaKeySession`, `MediaKeyStatusMap`, the
//! `MediaKeyMessageEvent` and `MediaEncryptedEvent`, and the media element's `setMediaKeys`.
//!
//! The one key system is Clear Key (EME Ã‚Â§9.1, `org.w3.clearkey`): temporary sessions whose licence
//! request is the JSON key id list (Ã‚Â§9.1.3) and whose licence is a JSON Web Key Set (Ã‚Â§9.1.4). Its
//! keys decrypt Common Encryption MP4 appended through Media Source Extensions
//! (`snowlark_media::cenc`, run by each `SourceBuffer` in `mse.rs`).

use std::collections::HashMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use snowlark_bindings::generated::{
    ArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView as BufferSource,
    MediaEncryptedEventImpl, MediaEncryptedEventInit, MediaKeyMessageEventImpl,
    MediaKeyMessageEventInit, MediaKeyMessageType, MediaKeySessionImpl, MediaKeySessionType,
    MediaKeyStatus, MediaKeyStatusMapImpl, MediaKeyStatusOrUndefined, MediaKeySystemAccessImpl,
    MediaKeySystemConfiguration, MediaKeySystemMediaCapability, MediaKeysImpl, MediaKeysPolicy,
    MediaKeysRequirement,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsObject, JsPromise};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;
use snowlark_media::cenc::{cenc_key_ids, KeyId};

use super::cdm::{Cdm, CdmError, CdmEvent, KeyStatus, MessageKind};
use super::objects::MediaObject;
use crate::engine::events::EventExt;
use crate::engine::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// The Clear Key key system's name.
pub const CLEAR_KEY: &str = "org.w3.clearkey";

/// The largest initialization data or response a session takes (EME Ã‚Â§6.? "a reasonable size").
const MAX_DATA: usize = 64 * 1024;

/// A `MediaKeys` object's state.
pub struct KeysData {
    /// Its sessions, in creation order.
    pub sessions: Vec<Handle>,
    /// The media element using it (EME Ã‚Â§? a CDM instance serves one element).
    pub element: Option<NodeId>,
    /// Its instance in the embedder's module ([`Cdm`]); none for Clear Key.
    pub cdm: Option<u64>,
}

/// A `MediaKeySession`'s state.
pub struct SessionData {
    pub media_keys: Handle,
    pub id: String,
    pub uninitialized: bool,
    pub callable: bool,
    pub closing_or_closed: bool,
    pub closed: JsPromise,
    pub status_map: Handle,
    /// Its keys (id, key), sorted by id.
    pub keys: Vec<(KeyId, [u8; 16])>,
    /// Its key statuses, sorted by key id, each with the key id's ArrayBuffer the map's
    /// iteration gives.
    pub statuses: Vec<(Vec<u8>, MediaKeyStatus, JsObject)>,
    /// Its id in the embedder's module ([`Cdm`]); none for Clear Key.
    pub cdm: Option<u64>,
    /// EME `expiration`: milliseconds since 1970, NaN for none.
    pub expiration: f64,
}

/// A call to the embedder's module waiting for its answer ([`CdmEvent::Done`]).
pub enum CdmPending {
    /// `createMediaKeys()`: the instance made for it.
    Keys {
        promise: JsPromise,
        keys: u64,
    },
    /// `generateRequest()`, `update()`, `close()`, `remove()` of a session.
    Generate {
        promise: JsPromise,
        session: Handle,
    },
    Update {
        promise: JsPromise,
    },
    Close {
        promise: JsPromise,
        session: Handle,
    },
    Remove {
        promise: JsPromise,
    },
}

fn cdm_error(e: CdmError) -> rt::Error {
    match e {
        CdmError::NotSupported(m) => rt::Error::Dom(ExceptionName::NotSupportedError, m.into()),
        CdmError::Type(m) => rt::Error::Type(m.into()),
        CdmError::InvalidState(m) | CdmError::Module(m) => {
            rt::Error::Dom(ExceptionName::InvalidStateError, m.into())
        }
    }
}

/// EME Ã‚Â§3.1.1.1 "Get Supported Configuration" for a key system of the embedder's module: `cenc`
/// initialization data, temporary sessions, and the distinctive identifier and persistent state
/// the system's module needs both of; each capability asked of the module.
fn cdm_configuration(
    cdm: &dyn Cdm,
    key_system: &str,
    candidate: &MediaKeySystemConfiguration,
) -> Option<MediaKeySystemConfiguration> {
    // 2-3.
    let init_data_types: Vec<String> = candidate
        .init_data_types
        .iter()
        .filter(|t| *t == "cenc")
        .take(1)
        .cloned()
        .collect();
    if !candidate.init_data_types.is_empty() && init_data_types.is_empty() {
        return None;
    }
    // 4-9.
    if candidate.distinctive_identifier == MediaKeysRequirement::NotAllowed
        || candidate.persistent_state == MediaKeysRequirement::NotAllowed
    {
        return None;
    }
    // 10-12.
    let session_types = candidate
        .session_types
        .clone()
        .unwrap_or_else(|| vec!["temporary".to_string()]);
    if session_types.iter().any(|t| t != "temporary") {
        return None;
    }
    // 13.
    if candidate.video_capabilities.is_empty() && candidate.audio_capabilities.is_empty() {
        return None;
    }
    // 14-15.
    // (the key system's name says its robustness: a PlayReady name ends in its security level,
    // which a robustness string may repeat)
    let level = key_system
        .rsplit_once('.')
        .map(|(_, l)| l)
        .filter(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()));
    let capabilities = |requested: &[MediaKeySystemMediaCapability]| {
        let mut out: Vec<MediaKeySystemMediaCapability> = Vec::new();
        for c in requested {
            if c.content_type.is_empty() {
                return None;
            }
            if !(c.robustness.is_empty() || Some(c.robustness.as_str()) == level)
                || c.encryption_scheme
                    .as_deref()
                    .is_some_and(|s| !matches!(s, "cenc" | "cbcs" | "cbcs-1-9"))
            {
                continue;
            }
            let seen = out.iter().any(|o| {
                o.content_type == c.content_type
                    && o.robustness == c.robustness
                    && o.encryption_scheme == c.encryption_scheme
            });
            if !seen && cdm.supports(key_system, &c.content_type) {
                out.push(c.clone());
            }
        }
        (!out.is_empty()).then_some(out)
    };
    let video_capabilities = if candidate.video_capabilities.is_empty() {
        Vec::new()
    } else {
        capabilities(&candidate.video_capabilities)?
    };
    let audio_capabilities = if candidate.audio_capabilities.is_empty() {
        Vec::new()
    } else {
        capabilities(&candidate.audio_capabilities)?
    };
    // 16-22.
    Some(MediaKeySystemConfiguration {
        label: candidate.label.clone(),
        init_data_types,
        audio_capabilities,
        video_capabilities,
        distinctive_identifier: MediaKeysRequirement::Required,
        persistent_state: MediaKeysRequirement::Required,
        session_types: Some(session_types),
    })
}

fn invalid(msg: &'static str) -> rt::Error {
    exception(ExceptionName::InvalidStateError, msg)
}

fn not_supported(msg: &'static str) -> rt::Error {
    exception(ExceptionName::NotSupportedError, msg)
}

pub(crate) fn buffer_source_bytes(cx: &mut Cx, data: &BufferSource) -> Vec<u8> {
    use BufferSource as B;
    match data {
        B::ArrayBuffer(o)
        | B::Int8Array(o)
        | B::Int16Array(o)
        | B::Int32Array(o)
        | B::Uint8Array(o)
        | B::Uint16Array(o)
        | B::Uint32Array(o)
        | B::Uint8ClampedArray(o)
        | B::BigInt64Array(o)
        | B::BigUint64Array(o)
        | B::Float16Array(o)
        | B::Float32Array(o)
        | B::Float64Array(o)
        | B::DataView(o) => crate::engine::encoding::buffer_bytes(cx, o),
    }
}

/// Whether Clear Key decrypts media of this content type for playback (MP4 through Media Source
/// Extensions, with codecs the media stack plays).
fn content_type_supported(engine: &Engine, content_type: &str, video: bool) -> bool {
    // (a parameter other than `codecs`, or an empty codec in its list, is not understood)
    let mut params = content_type.split(';').skip(1);
    if params.any(|p| {
        let (name, value) = p.split_once('=').unwrap_or((p, ""));
        !name.trim().eq_ignore_ascii_case("codecs")
            || value
                .trim()
                .trim_matches('"')
                .split(',')
                .any(|c| c.trim().is_empty())
    }) {
        return false;
    }
    let Some(t) = snowlark_media::caps::parse_type(content_type) else {
        return false;
    };
    let mp4 = matches!(t.essence.as_str(), "video/mp4" | "audio/mp4");
    if !mp4 || (t.essence == "video/mp4") != video && t.codecs.is_none() {
        return false;
    }
    if let Some(codecs) = &t.codecs {
        if !codecs.iter().flatten().all(|c| c.is_video() == video) {
            return false;
        }
    }
    engine.mse_types_supported(content_type)
}

/// EME Ã‚Â§3.1.1.2 "Get Supported Capabilities for Audio/Video Type", for Clear Key.
fn supported_capabilities(
    engine: &Engine,
    requested: &[MediaKeySystemMediaCapability],
    video: bool,
) -> Option<Vec<MediaKeySystemMediaCapability>> {
    let mut out: Vec<MediaKeySystemMediaCapability> = Vec::new();
    for c in requested {
        if c.content_type.is_empty() {
            return None;
        }
        // (Clear Key has no robustness levels; encryption schemes it decrypts)
        if !c.robustness.is_empty() {
            continue;
        }
        if c.encryption_scheme
            .as_deref()
            .is_some_and(|s| !matches!(s, "cenc" | "cbcs" | "cbcs-1-9"))
        {
            continue;
        }
        if !content_type_supported(engine, &c.content_type, video) {
            continue;
        }
        if !out.iter().any(|o| {
            o.content_type == c.content_type
                && o.robustness == c.robustness
                && o.encryption_scheme == c.encryption_scheme
        }) {
            out.push(c.clone());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// EME Ã‚Â§3.1.1.1 "Get Supported Configuration" (and its consent steps, which Clear Key does not
/// need), for Clear Key.
fn supported_configuration(
    engine: &Engine,
    candidate: &MediaKeySystemConfiguration,
) -> Option<MediaKeySystemConfiguration> {
    // 2-3. Initialization data types.
    let mut init_data_types = Vec::new();
    for t in &candidate.init_data_types {
        if matches!(t.as_str(), "cenc" | "keyids" | "webm") && !init_data_types.contains(t) {
            init_data_types.push(t.clone());
        }
    }
    if !candidate.init_data_types.is_empty() && init_data_types.is_empty() {
        return None;
    }
    // 4-6. Distinctive identifiers: Clear Key has none.
    if candidate.distinctive_identifier == MediaKeysRequirement::Required {
        return None;
    }
    // 7-9. Persistent state: temporary sessions keep none.
    if candidate.persistent_state == MediaKeysRequirement::Required {
        return None;
    }
    // 10-12. Session types.
    let session_types = candidate
        .session_types
        .clone()
        .unwrap_or_else(|| vec!["temporary".to_string()]);
    if session_types.iter().any(|t| t != "temporary") {
        return None;
    }
    // 13. Both capability lists empty: not supported.
    if candidate.video_capabilities.is_empty() && candidate.audio_capabilities.is_empty() {
        return None;
    }
    // 14-15.
    let video_capabilities = if candidate.video_capabilities.is_empty() {
        Vec::new()
    } else {
        supported_capabilities(engine, &candidate.video_capabilities, true)?
    };
    let audio_capabilities = if candidate.audio_capabilities.is_empty() {
        Vec::new()
    } else {
        supported_capabilities(engine, &candidate.audio_capabilities, false)?
    };
    // 16-22.
    Some(MediaKeySystemConfiguration {
        label: candidate.label.clone(),
        init_data_types,
        audio_capabilities,
        video_capabilities,
        distinctive_identifier: MediaKeysRequirement::NotAllowed,
        persistent_state: MediaKeysRequirement::NotAllowed,
        session_types: Some(session_types),
    })
}

/// The key ids of Clear Key initialization data (EME Ã‚Â§9.1.3 and the Initialization Data Format
/// Registry): `None` when it is not valid for the type.
fn init_data_key_ids(init_data_type: &str, data: &[u8]) -> Option<Vec<Vec<u8>>> {
    match init_data_type {
        "cenc" => cenc_key_ids(data).map(|k| k.into_iter().map(|k| k.to_vec()).collect()),
        "webm" => (1..=512).contains(&data.len()).then(|| vec![data.to_vec()]),
        "keyids" => {
            let v: serde_json::Value = serde_json::from_slice(data).ok()?;
            let kids = v.get("kids")?.as_array()?;
            if kids.is_empty() {
                return None;
            }
            let mut out = Vec::new();
            for k in kids {
                let id = URL_SAFE_NO_PAD.decode(k.as_str()?).ok()?;
                if !(1..=512).contains(&id.len()) {
                    return None;
                }
                out.push(id);
            }
            Some(out)
        }
        _ => None,
    }
}

/// The Clear Key licence request (EME Ã‚Â§9.1.3): `{"kids":[...],"type":"temporary"}`.
fn license_request(kids: &[Vec<u8>]) -> Vec<u8> {
    let kids: Vec<String> = kids.iter().map(|k| URL_SAFE_NO_PAD.encode(k)).collect();
    serde_json::json!({ "kids": kids, "type": "temporary" })
        .to_string()
        .into_bytes()
}

/// The keys of a Clear Key licence (EME Ã‚Â§9.1.4, a JSON Web Key Set of `oct` keys); `None` when
/// it is not one.
fn license_keys(response: &[u8]) -> Option<Vec<(KeyId, [u8; 16])>> {
    let v: serde_json::Value = serde_json::from_slice(response).ok()?;
    let keys = v.get("keys")?.as_array()?;
    if keys.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for k in keys {
        if k.get("kty")?.as_str()? != "oct" {
            return None;
        }
        let id = URL_SAFE_NO_PAD.decode(k.get("kid")?.as_str()?).ok()?;
        let key = URL_SAFE_NO_PAD.decode(k.get("k")?.as_str()?).ok()?;
        out.push((id.try_into().ok()?, key.try_into().ok()?));
    }
    Some(out)
}

impl Engine {
    fn ks<R>(&self, h: Handle, f: impl FnOnce(&mut KeysData) -> R) -> R {
        self.mo_mut(h, |o| match o {
            MediaObject::MediaKeys(k) => f(k),
            _ => panic!("not a MediaKeys"),
        })
    }

    fn sess<R>(&self, h: Handle, f: impl FnOnce(&mut SessionData) -> R) -> R {
        self.mo_mut(h, |o| match o {
            MediaObject::KeySession(s) => f(s),
            _ => panic!("not a MediaKeySession"),
        })
    }

    fn eme_rejected(&self, cx: &mut Cx, e: rt::Error) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.reject(cx, e);
        Ok(p)
    }

    /// `navigator.requestMediaKeySystemAccess` (EME Ã‚Â§3.1).
    pub(crate) fn request_media_key_system_access(
        &self,
        cx: &mut Cx,
        document: Option<NodeId>,
        key_system: &str,
        configurations: Vec<MediaKeySystemConfiguration>,
    ) -> rt::Result<JsPromise> {
        // 1-2.
        if key_system.is_empty() {
            return self.eme_rejected(cx, rt::Error::Type("The key system is empty".into()));
        }
        if configurations.is_empty() {
            return self.eme_rejected(
                cx,
                rt::Error::Type("The list of configurations is empty".into()),
            );
        }
        // 6. (in parallel; answered in a task)
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let answer = if key_system == CLEAR_KEY {
            configurations
                .iter()
                .find_map(|c| supported_configuration(self, c))
        } else if let Some(cdm) = self.cdm() {
            configurations
                .iter()
                .find_map(|c| cdm_configuration(&*cdm, key_system, c))
        } else {
            None
        };
        let key_system = key_system.to_string();
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| match answer {
            Some(config) => {
                let access = e.alloc(Object::Media(Box::new(MediaObject::KeySystemAccess {
                    key_system,
                    config: Box::new(config),
                    document,
                })));
                resolve.resolve(cx, access);
            }
            None => resolve.reject(
                cx,
                not_supported("The key system or its configurations are not supported"),
            ),
        });
        Ok(p)
    }

    /// The keys of the element's MediaKeys, for decryption.
    pub(crate) fn eme_keys_of(&self, el: NodeId) -> HashMap<KeyId, [u8; 16]> {
        let mut out = HashMap::new();
        let Some(mk) = self.media_el(el, |m| m.media_keys) else {
            return out;
        };
        for s in self.ks(mk, |k| k.sessions.clone()) {
            self.sess(s, |s| out.extend(s.keys.iter().copied()));
        }
        out
    }

    /// EME Ã‚Â§7.? "Initialization Data Encountered": `encrypted` at the element, with the
    /// initialization data (the media data came through the page, so it is same-origin).
    /// The instance of the embedder's module that `el`'s `MediaKeys` are, if they are one.
    pub(crate) fn eme_module_keys(&self, el: NodeId) -> Option<u64> {
        let mk = self.media_el(el, |m| m.media_keys)?;
        self.ks(mk, |k| k.cdm)
    }

    pub(crate) fn eme_init_data_encountered(&self, el: NodeId, init_data: Vec<u8>) {
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if !e.dom().is_alive(el) {
                return;
            }
            let data = crate::engine::blob::array_buffer(cx, init_data);
            let ev = e.trusted_event(
                InterfaceId::MediaEncryptedEvent,
                "encrypted",
                false,
                false,
                EventExt::Encrypted {
                    init_data_type: "cenc".into(),
                    init_data: Some(crate::engine::held::Held::object(data)),
                },
            );
            e.dispatch(cx, ev, crate::engine::node_handle(el), false);
        });
    }

    /// EME Ã‚Â§7.? "Wait for Key": playback cannot go on without a key; `waitingforkey` at the
    /// element.
    pub(crate) fn eme_wait_for_key(&self, el: NodeId) {
        self.media_fire(el, "waitingforkey");
    }

    /// Keys were added to `media_keys`: media waiting for them goes on (EME Ã‚Â§7.? "Attempt to
    /// Resume Playback If Necessary").
    fn eme_keys_added(&self, media_keys: Handle) {
        if let Some(el) = self.ks(media_keys, |k| k.element) {
            self.mse_keys_changed(el);
        }
    }

    /// EME Ã‚Â§6.? "Update Key Statuses": the statuses replace the session's, and
    /// `keystatuseschange` is queued.
    fn eme_update_statuses(
        &self,
        cx: &mut Cx,
        session: Handle,
        mut statuses: Vec<(Vec<u8>, MediaKeyStatus)>,
    ) {
        statuses.sort_by(|a, b| a.0.cmp(&b.0));
        let statuses: Vec<_> = statuses
            .into_iter()
            .map(|(id, st)| {
                let ab = crate::engine::blob::array_buffer(cx, id.clone());
                (id, st, ab)
            })
            .collect();
        self.sess(session, |s| s.statuses = statuses);
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            e.fire_event(cx, session, "keystatuseschange", false, false);
        });
    }

    /// EME Ã‚Â§6.? "Session Closed".
    fn eme_session_closed(&self, cx: &mut Cx, session: Handle, reason: &'static str) {
        let (had, closed, mk) = self.sess(session, |s| {
            s.closing_or_closed = true;
            s.keys.clear();
            (!s.statuses.is_empty(), s.closed.clone(), s.media_keys)
        });
        if had {
            self.eme_update_statuses(cx, session, Vec::new());
        }
        closed.resolve(cx, reason.to_string());
        let _ = mk;
    }
}

impl Engine {
    /// A new request id for a call to the embedder's module.
    fn cdm_request(&self) -> u64 {
        let mut m = self.media.borrow_mut();
        m.cdm_requests += 1;
        m.cdm_requests
    }

    /// Makes a call to the embedder's module, its answer awaited as `pending`; one with no module
    /// any more is rejected now.
    fn cdm_call(&self, pending: CdmPending, call: impl FnOnce(&dyn Cdm, u64)) {
        let Some(cdm) = self.cdm() else {
            return;
        };
        let request = self.cdm_request();
        self.media.borrow_mut().cdm_pending.insert(request, pending);
        call(&*cdm, request);
        self.drain_cdm();
    }

    /// `doc` unloaded: the module instances it made are released, and their sessions are closed
    /// (no call reaches the module for them again).
    pub(crate) fn eme_document_unloaded(&self, doc: NodeId) {
        let gone: Vec<(u64, Option<Handle>)> = {
            let mut m = self.media.borrow_mut();
            if m.cdm_keys.is_empty() {
                return;
            }
            let gone: Vec<(u64, Option<Handle>)> = m
                .cdm_keys
                .iter()
                .filter(|(_, (d, _))| *d == doc)
                .map(|(&k, &(_, mk))| (k, mk))
                .collect();
            for (k, _) in &gone {
                m.cdm_keys.remove(k);
            }
            gone
        };
        let cdm = self.cdm();
        for (keys, mk) in gone {
            if let Some(cdm) = &cdm {
                cdm.release(keys);
            }
            let Some(mk) = mk else {
                continue;
            };
            for session in self.ks(mk, |k| k.sessions.clone()) {
                let id = self.sess(session, |s| {
                    s.closing_or_closed = true;
                    s.cdm.take()
                });
                if let Some(id) = id {
                    self.media.borrow_mut().cdm_sessions.remove(&id);
                }
            }
        }
    }

    /// A module's event, in its task.
    pub(crate) fn eme_cdm_event(&self, cx: &mut Cx, event: CdmEvent) {
        let session_of = |id: u64| self.media.borrow().cdm_sessions.get(&id).copied();
        match event {
            CdmEvent::Done { request, result } => {
                let pending = self.media.borrow_mut().cdm_pending.remove(&request);
                let Some(pending) = pending else {
                    return;
                };
                match (pending, result) {
                    (CdmPending::Keys { promise, keys }, Ok(_)) => {
                        // (an instance whose document unloaded meanwhile is released already)
                        if !self.media.borrow().cdm_keys.contains_key(&keys) {
                            return;
                        }
                        let mk = self.alloc(Object::Media(Box::new(MediaObject::MediaKeys(
                            Box::new(KeysData {
                                sessions: Vec::new(),
                                element: None,
                                cdm: Some(keys),
                            }),
                        ))));
                        if let Some(entry) = self.media.borrow_mut().cdm_keys.get_mut(&keys) {
                            entry.1 = Some(mk);
                        }
                        promise.resolve(cx, mk);
                    }
                    (CdmPending::Keys { promise, keys }, Err(e)) => {
                        if self.media.borrow_mut().cdm_keys.remove(&keys).is_some() {
                            if let Some(cdm) = self.cdm() {
                                cdm.release(keys);
                            }
                        }
                        promise.reject(cx, cdm_error(e));
                    }
                    (CdmPending::Generate { promise, session }, Ok(id)) => {
                        // EME Ã‚Â§6.6.2 step 10.10: the session id; the message came as its own event.
                        self.sess(session, |s| {
                            s.id = id;
                            s.callable = true;
                        });
                        promise.resolve(cx, ());
                    }
                    (CdmPending::Close { promise, session }, Ok(_)) => {
                        self.eme_session_closed(cx, session, "closed-by-application");
                        promise.resolve(cx, ());
                    }
                    (
                        CdmPending::Update { promise, .. } | CdmPending::Remove { promise, .. },
                        Ok(_),
                    ) => promise.resolve(cx, ()),
                    (
                        CdmPending::Generate { promise, .. }
                        | CdmPending::Update { promise, .. }
                        | CdmPending::Close { promise, .. }
                        | CdmPending::Remove { promise, .. },
                        Err(e),
                    ) => promise.reject(cx, cdm_error(e)),
                }
            }
            CdmEvent::Message {
                session,
                kind,
                data,
            } => {
                let Some(session) = session_of(session) else {
                    return;
                };
                let message_type = match kind {
                    MessageKind::LicenseRequest => MediaKeyMessageType::LicenseRequest,
                    MessageKind::LicenseRenewal => MediaKeyMessageType::LicenseRenewal,
                    MessageKind::LicenseRelease => MediaKeyMessageType::LicenseRelease,
                    MessageKind::IndividualizationRequest => {
                        MediaKeyMessageType::IndividualizationRequest
                    }
                };
                let message = crate::engine::blob::array_buffer(cx, data);
                let ev = self.trusted_event(
                    InterfaceId::MediaKeyMessageEvent,
                    "message",
                    false,
                    false,
                    EventExt::MediaKeyMessage {
                        message_type,
                        message: Some(crate::engine::held::Held::object(message)),
                    },
                );
                self.dispatch(cx, ev, session, false);
            }
            CdmEvent::KeyStatuses { session, statuses } => {
                let Some(session) = session_of(session) else {
                    return;
                };
                let statuses = statuses
                    .into_iter()
                    .map(|(id, st)| {
                        let st = match st {
                            KeyStatus::Usable => MediaKeyStatus::Usable,
                            KeyStatus::Expired => MediaKeyStatus::Expired,
                            KeyStatus::Released => MediaKeyStatus::Released,
                            KeyStatus::OutputRestricted => MediaKeyStatus::OutputRestricted,
                            KeyStatus::OutputDownscaled => MediaKeyStatus::OutputDownscaled,
                            KeyStatus::StatusPending => MediaKeyStatus::StatusPending,
                            KeyStatus::InternalError => MediaKeyStatus::InternalError,
                            KeyStatus::UsableInFuture => MediaKeyStatus::UsableInFuture,
                        };
                        (id, st)
                    })
                    .collect();
                self.eme_update_statuses(cx, session, statuses);
                let mk = self.sess(session, |s| s.media_keys);
                self.eme_keys_added(mk);
            }
            CdmEvent::Expiration { session, time } => {
                if let Some(session) = session_of(session) {
                    self.sess(session, |s| s.expiration = time);
                }
            }
            CdmEvent::Player { player, report } => self.media_protected_report(player, report),
            CdmEvent::Closed { session } => {
                // (a session the page is closing closes when its `close()` is answered)
                if let Some(session) = session_of(session) {
                    if !self.sess(session, |s| s.closing_or_closed) {
                        self.eme_session_closed(cx, session, "internal-error");
                    }
                }
            }
        }
    }
}

impl MediaKeySystemAccessImpl for Engine {
    fn key_system(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.mo(this, |o| match o {
            MediaObject::KeySystemAccess { key_system, .. } => key_system.clone(),
            _ => String::new(),
        }))
    }
    fn get_configuration(
        &self,
        _cx: &mut Cx,
        this: Handle,
    ) -> rt::Result<MediaKeySystemConfiguration> {
        Ok(self.mo(this, |o| match o {
            MediaObject::KeySystemAccess { config, .. } => (**config).clone(),
            _ => unreachable!(),
        }))
    }
    fn create_media_keys(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        // A CDM's state is kept per origin: an opaque origin (a sandboxed frame) has none to
        // keep it under. The instance is the document's that asked for the access (it is
        // released when that document unloads).
        let asked_by = self.mo(this, |o| match o {
            MediaObject::KeySystemAccess { document, .. } => *document,
            _ => None,
        });
        let doc = match asked_by {
            Some(d) => d,
            None => self.current_document(cx)?,
        };
        if !self.net_origin_of_document(doc).is_tuple() {
            return self.eme_rejected(
                cx,
                not_supported("Media keys are not available to an opaque origin"),
            );
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let key_system = self.mo(this, |o| match o {
            MediaObject::KeySystemAccess { key_system, .. } => key_system.clone(),
            _ => String::new(),
        });
        if key_system != CLEAR_KEY {
            // The embedder's module: an instance for the document's origin, made in parallel.
            let Some(cdm) = self.cdm() else {
                p.reject(cx, not_supported("The key system is not available"));
                return Ok(p);
            };
            let origin = self.net_origin_of_document(doc).ascii_serialization();
            let request = self.cdm_request();
            let keys = cdm.create_keys(request, &key_system, &origin, &self.cdm_inbox());
            let mut m = self.media.borrow_mut();
            m.cdm_keys.insert(keys, (doc, None));
            m.cdm_pending.insert(
                request,
                CdmPending::Keys {
                    promise: p.clone(),
                    keys,
                },
            );
            drop(m);
            self.drain_cdm();
            return Ok(p);
        }
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let mk = e.alloc(Object::Media(Box::new(MediaObject::MediaKeys(Box::new(
                KeysData {
                    sessions: Vec::new(),
                    element: None,
                    cdm: None,
                },
            )))));
            resolve.resolve(cx, mk);
        });
        Ok(p)
    }
}

impl MediaKeysImpl for Engine {
    fn create_session(
        &self,
        cx: &mut Cx,
        this: Handle,
        session_type: MediaKeySessionType,
    ) -> rt::Result<Handle> {
        // 1-2. Clear Key and the embedder's modules: temporary sessions only.
        if session_type != MediaKeySessionType::Temporary {
            return Err(not_supported("The session type is not supported"));
        }
        let cdm = match self.ks(this, |k| k.cdm) {
            Some(keys) => match self.cdm() {
                // (an instance is released with the document that made it)
                Some(cdm) if self.media.borrow().cdm_keys.contains_key(&keys) => {
                    Some(cdm.create_session(keys))
                }
                _ => return Err(invalid("The key system is gone")),
            },
            None => None,
        };
        let closed = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        // (the map refers to the session: made first, then pointed at it)
        let map = self.alloc(Object::Media(Box::new(MediaObject::KeyStatusMap {
            session: this,
        })));
        let session = self.alloc(Object::Media(Box::new(MediaObject::KeySession(Box::new(
            SessionData {
                media_keys: this,
                id: String::new(),
                uninitialized: true,
                callable: false,
                closing_or_closed: false,
                closed,
                status_map: map,
                keys: Vec::new(),
                statuses: Vec::new(),
                cdm,
                expiration: f64::NAN,
            },
        )))));
        if let Some(id) = cdm {
            self.media.borrow_mut().cdm_sessions.insert(id, session);
        }
        self.mo_mut(map, |o| {
            if let MediaObject::KeyStatusMap { session: s } = o {
                *s = session;
            }
        });
        self.ks(this, |k| k.sessions.push(session));
        Ok(session)
    }
    fn get_status_for_policy(
        &self,
        cx: &mut Cx,
        _this: Handle,
        policy: MediaKeysPolicy,
    ) -> rt::Result<JsPromise> {
        // (no HDCP restriction applies to Clear Key: any valid policy is usable)
        let Some(v) = policy.min_hdcp_version else {
            return self.eme_rejected(cx, rt::Error::Type("The policy is empty".into()));
        };
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let resolve = p.clone();
        let known = matches!(
            v.as_str(),
            "" | "1.0" | "1.1" | "1.2" | "1.3" | "1.4" | "2.0" | "2.1" | "2.2" | "2.3"
        );
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            if known {
                resolve.resolve(cx, MediaKeyStatus::Usable);
            } else {
                resolve.reject(cx, rt::Error::Type("Unknown HDCP version".into()));
            }
        });
        Ok(p)
    }
    fn set_server_certificate(
        &self,
        cx: &mut Cx,
        _this: Handle,
        server_certificate: BufferSource,
    ) -> rt::Result<JsPromise> {
        if buffer_source_bytes(cx, &server_certificate).is_empty() {
            return self.eme_rejected(cx, rt::Error::Type("The certificate is empty".into()));
        }
        // (Clear Key uses no server certificates)
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            resolve.resolve(cx, false);
        });
        Ok(p)
    }
}

impl MediaKeySessionImpl for Engine {
    fn session_id(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.sess(this, |s| s.id.clone()))
    }
    fn expiration(&self, this: Handle) -> f64 {
        self.sess(this, |s| s.expiration)
    }
    fn closed(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        Ok(self.sess(this, |s| s.closed.clone()))
    }
    fn key_statuses(&self, this: Handle) -> Handle {
        self.sess(this, |s| s.status_map)
    }
    /// EME Ã‚Â§6.6.2.
    fn generate_request(
        &self,
        cx: &mut Cx,
        this: Handle,
        init_data_type: &str,
        init_data: BufferSource,
    ) -> rt::Result<JsPromise> {
        let data = buffer_source_bytes(cx, &init_data);
        let (closing, uninitialized) = self.sess(this, |s| (s.closing_or_closed, s.uninitialized));
        // 1-2.
        if closing {
            return self.eme_rejected(cx, invalid("The session is closed"));
        }
        if !uninitialized {
            return self.eme_rejected(cx, invalid("The session is already initialized"));
        }
        // 3.
        self.sess(this, |s| s.uninitialized = false);
        // 4-5.
        if init_data_type.is_empty() {
            return self.eme_rejected(cx, rt::Error::Type("The initDataType is empty".into()));
        }
        if data.is_empty() {
            return self.eme_rejected(cx, rt::Error::Type("The initData is empty".into()));
        }
        // 6.
        if !matches!(init_data_type, "cenc" | "keyids" | "webm") {
            return self.eme_rejected(cx, not_supported("The initDataType is not supported"));
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if let Some(id) = self.sess(this, |s| s.cdm) {
            // 10. The embedder's module: it sanitizes the data and makes the request.
            if init_data_type != "cenc" || data.len() > MAX_DATA {
                p.reject(cx, not_supported("The initData is not supported"));
                return Ok(p);
            }
            self.cdm_call(
                CdmPending::Generate {
                    promise: p.clone(),
                    session: this,
                },
                |cdm, request| cdm.generate_request(request, id, init_data_type, data),
            );
            return Ok(p);
        }
        // 10. (in parallel) Sanitized initialization data.
        let kids = if data.len() > MAX_DATA {
            None
        } else {
            init_data_key_ids(init_data_type, &data)
        };
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let Some(kids) = kids else {
                resolve.reject(cx, rt::Error::Type("The initData is not valid".into()));
                return;
            };
            if kids.is_empty() {
                resolve.reject(cx, not_supported("The initData has no Clear Key key ids"));
                return;
            }
            // 10.9-10.10: a session id, the licence request, the message.
            let id = {
                let mut m = e.media.borrow_mut();
                m.eme_sessions += 1;
                m.eme_sessions.to_string()
            };
            e.sess(this, |s| {
                s.id = id;
                s.callable = true;
            });
            // 10.10.4: "Queue a 'message' Event" (a task of its own), then 10.10.5 resolve.
            let request = license_request(&kids);
            e.queue_task(TaskSource::DomManipulation, move |e, cx| {
                let message = crate::engine::blob::array_buffer(cx, request);
                let ev = e.trusted_event(
                    InterfaceId::MediaKeyMessageEvent,
                    "message",
                    false,
                    false,
                    EventExt::MediaKeyMessage {
                        message_type: MediaKeyMessageType::LicenseRequest,
                        message: Some(crate::engine::held::Held::object(message)),
                    },
                );
                e.dispatch(cx, ev, this, false);
            });
            resolve.resolve(cx, ());
        });
        Ok(p)
    }
    fn load(&self, cx: &mut Cx, this: Handle, session_id: &str) -> rt::Result<JsPromise> {
        let (closing, uninitialized) = self.sess(this, |s| (s.closing_or_closed, s.uninitialized));
        if closing {
            return self.eme_rejected(cx, invalid("The session is closed"));
        }
        if !uninitialized {
            return self.eme_rejected(cx, invalid("The session is already initialized"));
        }
        self.sess(this, |s| s.uninitialized = false);
        if session_id.is_empty() {
            return self.eme_rejected(cx, rt::Error::Type("The sessionId is empty".into()));
        }
        // (temporary sessions are not persistent)
        self.eme_rejected(cx, rt::Error::Type("The session is not persistent".into()))
    }
    /// EME Ã‚Â§6.6.4.
    fn update(&self, cx: &mut Cx, this: Handle, response: BufferSource) -> rt::Result<JsPromise> {
        let data = buffer_source_bytes(cx, &response);
        let (closing, callable) = self.sess(this, |s| (s.closing_or_closed, s.callable));
        if closing {
            return self.eme_rejected(cx, invalid("The session is closed"));
        }
        if !callable {
            return self.eme_rejected(cx, invalid("The session has no request yet"));
        }
        if data.is_empty() {
            return self.eme_rejected(cx, rt::Error::Type("The response is empty".into()));
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if let Some(id) = self.sess(this, |s| s.cdm) {
            self.cdm_call(CdmPending::Update { promise: p.clone() }, |cdm, request| {
                cdm.update(request, id, data)
            });
            return Ok(p);
        }
        let keys = if data.len() > MAX_DATA {
            None
        } else {
            license_keys(&data)
        };
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let Some(keys) = keys else {
                resolve.reject(
                    cx,
                    rt::Error::Type("The response is not a valid licence".into()),
                );
                return;
            };
            if e.sess(this, |s| s.closing_or_closed) {
                resolve.reject(cx, invalid("The session is closed"));
                return;
            }
            let statuses = e.sess(this, |s| {
                for (id, key) in keys {
                    match s.keys.iter_mut().find(|k| k.0 == id) {
                        Some(k) => k.1 = key,
                        None => s.keys.push((id, key)),
                    }
                }
                s.keys.sort_by_key(|k| k.0);
                s.keys
                    .iter()
                    .map(|k| (k.0.to_vec(), MediaKeyStatus::Usable))
                    .collect()
            });
            e.eme_update_statuses(cx, this, statuses);
            let mk = e.sess(this, |s| s.media_keys);
            e.eme_keys_added(mk);
            resolve.resolve(cx, ());
        });
        Ok(p)
    }
    /// EME Ã‚Â§6.6.5.
    fn close(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let (closing, callable) = self.sess(this, |s| (s.closing_or_closed, s.callable));
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if closing {
            p.resolve(cx, ());
            return Ok(p);
        }
        if !callable {
            p.reject(cx, invalid("The session has no request yet"));
            return Ok(p);
        }
        self.sess(this, |s| s.closing_or_closed = true);
        if let Some(id) = self.sess(this, |s| s.cdm) {
            self.cdm_call(
                CdmPending::Close {
                    promise: p.clone(),
                    session: this,
                },
                |cdm, request| cdm.close(request, id),
            );
            return Ok(p);
        }
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            e.eme_session_closed(cx, this, "closed-by-application");
            resolve.resolve(cx, ());
        });
        Ok(p)
    }
    /// EME Ã‚Â§6.6.6: a temporary session's keys are released.
    fn remove(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let (closing, callable) = self.sess(this, |s| (s.closing_or_closed, s.callable));
        if closing {
            return self.eme_rejected(cx, invalid("The session is closed"));
        }
        if !callable {
            return self.eme_rejected(cx, invalid("The session has no request yet"));
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if let Some(id) = self.sess(this, |s| s.cdm) {
            self.cdm_call(CdmPending::Remove { promise: p.clone() }, |cdm, request| {
                cdm.remove(request, id)
            });
            return Ok(p);
        }
        let resolve = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let statuses = e.sess(this, |s| {
                s.keys.clear();
                s.statuses
                    .iter()
                    .map(|(id, _, _)| (id.clone(), MediaKeyStatus::Released))
                    .collect()
            });
            e.eme_update_statuses(cx, this, statuses);
            resolve.resolve(cx, ());
        });
        Ok(p)
    }
}

impl MediaKeyStatusMapImpl for Engine {
    fn pair_at(&self, this: Handle, index: u32) -> Option<(BufferSource, MediaKeyStatus)> {
        let session = self.mo(this, |o| match o {
            MediaObject::KeyStatusMap { session } => *session,
            _ => unreachable!(),
        });
        self.sess(session, |s| {
            s.statuses
                .get(index as usize)
                .map(|(_, st, ab)| (BufferSource::ArrayBuffer(ab.clone()), *st))
        })
    }
    fn size(&self, this: Handle) -> u32 {
        let session = self.mo(this, |o| match o {
            MediaObject::KeyStatusMap { session } => *session,
            _ => unreachable!(),
        });
        self.sess(session, |s| s.statuses.len() as u32)
    }
    fn has(&self, cx: &mut Cx, this: Handle, key_id: BufferSource) -> rt::Result<bool> {
        Ok(!matches!(
            self.get(cx, this, key_id)?,
            MediaKeyStatusOrUndefined::Undefined(())
        ))
    }
    fn get(
        &self,
        cx: &mut Cx,
        this: Handle,
        key_id: BufferSource,
    ) -> rt::Result<MediaKeyStatusOrUndefined> {
        let id = buffer_source_bytes(cx, &key_id);
        let session = self.mo(this, |o| match o {
            MediaObject::KeyStatusMap { session } => *session,
            _ => unreachable!(),
        });
        Ok(self
            .sess(session, |s| {
                s.statuses
                    .iter()
                    .find(|(k, _, _)| *k == id)
                    .map(|(_, st, _)| *st)
            })
            .map_or(
                MediaKeyStatusOrUndefined::Undefined(()),
                MediaKeyStatusOrUndefined::MediaKeyStatus,
            ))
    }
}

impl MediaKeyMessageEventImpl for Engine {
    fn message_type(&self, this: Handle) -> MediaKeyMessageType {
        self.ev(this, |e| match &e.ext {
            EventExt::MediaKeyMessage { message_type, .. } => *message_type,
            _ => MediaKeyMessageType::LicenseRequest,
        })
    }
    fn message(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsObject> {
        let scope = cx.scope();
        let m = self.ev(this, |e| match &e.ext {
            EventExt::MediaKeyMessage { message, .. } => {
                message.as_ref().and_then(|m| m.get(scope))
            }
            _ => None,
        });
        match m {
            Some(m) => Ok(m),
            None => Ok(crate::engine::blob::array_buffer(cx, Vec::new())),
        }
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: MediaKeyMessageEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::MediaKeyMessageEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::MediaKeyMessage {
                message_type: init.message_type,
                message: Some(crate::engine::held::Held::object(init.message)),
            },
        ))
    }
}

impl MediaEncryptedEventImpl for Engine {
    fn init_data_type(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Encrypted { init_data_type, .. } => init_data_type.clone(),
            _ => String::new(),
        }))
    }
    fn init_data(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<JsObject>> {
        let scope = cx.scope();
        Ok(self.ev(this, |e| match &e.ext {
            EventExt::Encrypted { init_data, .. } => init_data.as_ref().and_then(|d| d.get(scope)),
            _ => None,
        }))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: MediaEncryptedEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::MediaEncryptedEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Encrypted {
                init_data_type: init.init_data_type,
                init_data: init.init_data.map(crate::engine::held::Held::object),
            },
        ))
    }
}

impl Engine {
    /// `HTMLMediaElement.mediaKeys`.
    pub(crate) fn media_media_keys(&self, el: NodeId) -> Option<Handle> {
        self.media_el(el, |m| m.media_keys)
    }

    /// `HTMLMediaElement.setMediaKeys` (EME Ã‚Â§7.?).
    pub(crate) fn media_set_media_keys(
        &self,
        cx: &mut Cx,
        el: NodeId,
        media_keys: Option<Handle>,
    ) -> rt::Result<JsPromise> {
        let (current, attaching) = self.media_el(el, |m| (m.media_keys, m.attaching_keys));
        // 1.
        if current == media_keys {
            let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
            p.resolve(cx, ());
            return Ok(p);
        }
        // 2-3.
        if attaching {
            return self.eme_rejected(cx, invalid("MediaKeys are being attached"));
        }
        self.media_el(el, |m| m.attaching_keys = true);
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let resolve = p.clone();
        // 5. (in parallel)
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            // 5.1. A MediaKeys serves one element.
            if let Some(mk) = media_keys {
                if e.ks(mk, |k| {
                    k.element.is_some_and(|o| o != el && e.dom().is_alive(o))
                }) {
                    e.media_el(el, |m| m.attaching_keys = false);
                    resolve.reject(
                        cx,
                        exception(
                            ExceptionName::QuotaExceededError,
                            "The MediaKeys are in use by another media element",
                        ),
                    );
                    return;
                }
            }
            // 5.2. The old association ends.
            if let Some(old) = current {
                e.ks(old, |k| k.element = None);
            }
            // 5.3-5.4.
            if let Some(mk) = media_keys {
                e.ks(mk, |k| k.element = Some(el));
            }
            e.media_el(el, |m| {
                m.media_keys = media_keys;
                m.attaching_keys = false;
            });
            if media_keys.is_some() {
                e.mse_keys_changed(el);
            }
            resolve.resolve(cx, ());
        });
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyids_init_data_and_licence_request() {
        let kids = init_data_key_ids("keyids", br#"{"kids":["AAECAwQFBgcICQoLDA0ODw"]}"#).unwrap();
        assert_eq!(kids, vec![(0..16u8).collect::<Vec<_>>()]);
        assert_eq!(
            String::from_utf8(license_request(&kids)).unwrap(),
            r#"{"kids":["AAECAwQFBgcICQoLDA0ODw"],"type":"temporary"}"#
        );
        assert!(init_data_key_ids("keyids", b"{}").is_none());
        assert!(init_data_key_ids("keyids", br#"{"kids":[]}"#).is_none());
    }

    #[test]
    fn licence_keys() {
        let jwk = br#"{"keys":[{"kty":"oct","kid":"AAECAwQFBgcICQoLDA0ODw","k":"AAECAwQFBgcICQoLDA0ODw"}],"type":"temporary"}"#;
        let keys = license_keys(jwk).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(license_keys(br#"{"keys":[{"kty":"RSA","kid":"AA","k":"AA"}]}"#).is_none());
        assert!(license_keys(b"not json").is_none());
    }
}
