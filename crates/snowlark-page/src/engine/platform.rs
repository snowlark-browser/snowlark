// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Navigator APIs about the platform: the Network Information API (`navigator.connection`),
//! User-Agent Client Hints (`navigator.userAgentData`), the async Clipboard API
//! (`navigator.clipboard`, `ClipboardItem`) and Permissions (`navigator.permissions`).
//!
//! The clipboard is the embedder's ([`ClipboardAccess`], the system clipboard in the window): the
//! headless modes have none, so every clipboard operation rejects there. Reading needs transient user
//! activation; writing needs it or the document's focus (the clipboard-write permission is granted).

use std::sync::Arc;

use snowlark_bindings::generated::ids;
use snowlark_bindings::generated::{
    CaptureHandleConfig, ClipboardImpl, ClipboardItemImpl, ClipboardItemOptions,
    ClipboardUnsanitizedFormats, ConnectionType, EffectiveConnectionType, InputDeviceInfoImpl,
    MediaDeviceInfoImpl, MediaDeviceKind, MediaDevicesImpl, MediaTrackCapabilities,
    MediaTrackSupportedConstraints, NavigatorNetworkInformationImpl, NavigatorUABrandVersion,
    NavigatorUADataImpl, NavigatorUAImpl, NetworkInformationImpl, NetworkInformationSaveDataImpl,
    PermissionState, PermissionStatusImpl, PermissionsImpl, PresentationStyle, UADataValues,
    UALowEntropyJSON, UserActivationImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle, JsObject, JsPromise};
use snowlark_dom::NodeId;

use super::element::rejected;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// The system clipboard as the embedder lends it to pages (text only).
pub trait ClipboardAccess: Send + Sync {
    fn read_text(&self) -> Option<String>;
    fn write_text(&self, text: &str);
}

/// HTML §6.4.3 "transient activation duration": how long a user activation lets a page do what
/// needs one (Chromium's five seconds).
const TRANSIENT_ACTIVATION_MS: f64 = 5000.0;

/// A `ClipboardItem`: its types in order, each with the promise of its data.
pub struct ClipboardItemData {
    pub items: Vec<(String, JsObject)>,
    pub style: PresentationStyle,
}

fn brand(brand: &str, version: &str) -> NavigatorUABrandVersion {
    NavigatorUABrandVersion {
        brand: Some(brand.to_string()),
        version: Some(version.to_string()),
    }
}

/// UA-CH §5.1 "create brands": the user agent's brands (a grease brand among them, so sites cannot
/// match the list exactly), with the major versions or the full ones.
fn brands(info: &snowlark_net::ua::Profile, full: bool) -> Vec<NavigatorUABrandVersion> {
    info.brands(full).iter().map(|(b, v)| brand(b, v)).collect()
}

/// The permission names of the Permissions registry this engine knows, and their state here.
fn permission_state(name: &str) -> Option<PermissionState> {
    Some(match name {
        "clipboard-write" => PermissionState::Granted,
        "clipboard-read" => PermissionState::Prompt,
        "geolocation"
        | "notifications"
        | "push"
        | "midi"
        | "camera"
        | "microphone"
        | "speaker-selection"
        | "background-fetch"
        | "background-sync"
        | "bluetooth"
        | "persistent-storage"
        | "ambient-light-sensor"
        | "accelerometer"
        | "gyroscope"
        | "magnetometer"
        | "screen-wake-lock"
        | "nfc"
        | "display-capture"
        | "payment-handler"
        | "idle-detection"
        | "periodic-background-sync"
        | "system-wake-lock"
        | "storage-access"
        | "top-level-storage-access"
        | "window-management"
        | "local-fonts"
        | "captured-surface-control"
        | "keyboard-lock"
        | "pointer-lock"
        | "web-app-installation" => PermissionState::Denied,
        _ => return None,
    })
}

impl Engine {
    /// The window (or worker global) a Navigator singleton belongs to.
    fn navigator_global(&self, navigator: Handle) -> Option<Handle> {
        let f = self.frames.borrow();
        f.singletons
            .iter()
            .find_map(|((global, _), h)| (*h == navigator).then_some(*global))
    }

    fn navigator_singleton(
        &self,
        navigator: Handle,
        key: &'static str,
        make: impl FnOnce() -> Object,
    ) -> Handle {
        let global = self.navigator_global(navigator).unwrap_or(navigator);
        self.window_singleton(global, key, make)
    }

    /// Lends the page the system clipboard (`None`: no clipboard, as in the headless modes).
    pub fn set_clipboard(&self, clipboard: Option<Arc<dyn ClipboardAccess>>) {
        self.embedder.borrow_mut().clipboard = clipboard;
    }

    /// A user activation in `doc` now (HTML §6.4.3 "activation notification"): the "last activation
    /// timestamp" of its window, of its ancestor navigables' active windows and of its descendant
    /// navigables' whose active document is same origin with `doc`, becomes the time now.
    pub(crate) fn note_activation_time(&self, doc: NodeId) {
        let now = self.now();
        let mut windows = vec![doc];
        let mut descendants = Vec::new();
        {
            let f = self.frames.borrow();
            if let Some(&id) = f.by_document.get(&doc) {
                let mut at = f.list[id].parent;
                while let Some(p) = at {
                    windows.push(f.list[p].document);
                    at = f.list[p].parent;
                }
                // (the contexts whose parent chain reaches `doc`'s)
                let descends = |mut c: usize| {
                    while let Some(p) = f.list[c].parent {
                        if p == id {
                            return true;
                        }
                        c = p;
                    }
                    false
                };
                descendants.extend(
                    (0..f.list.len())
                        .filter(|&c| !f.list[c].discarded && descends(c))
                        .map(|c| f.list[c].document),
                );
            }
        }
        let origin = self.origin_of_document(doc);
        windows.extend(
            descendants
                .into_iter()
                .filter(|&d| self.origin_of_document(d) == origin),
        );
        {
            let mut e = self.embedder.borrow_mut();
            for &d in &windows {
                e.activations.insert(d, now);
            }
        }
        for d in windows {
            self.close_watchers_note_activation(d);
        }
    }

    /// HTML §6.4.3 "sticky activation" of the window whose associated document is `doc`.
    pub(crate) fn has_sticky_activation(&self, doc: NodeId) -> bool {
        let now = self.now();
        self.embedder
            .borrow()
            .activations
            .get(&doc)
            .is_some_and(|&t| now >= t)
    }

    /// HTML "transient activation" of the current global's document.
    pub(crate) fn has_transient_activation(&self, doc: NodeId) -> bool {
        let now = self.now();
        self.embedder
            .borrow()
            .activations
            .get(&doc)
            .is_some_and(|t| now - t < TRANSIENT_ACTIVATION_MS)
    }

    /// Clipboard API §? "check clipboard read/write permission", with the embedder's clipboard.
    fn clipboard_allowed(&self, cx: &mut Cx, write: bool) -> Option<Arc<dyn ClipboardAccess>> {
        let clipboard = self.embedder.borrow().clipboard.clone()?;
        let doc = self.current_document(cx).ok()?;
        let focused = self.is_active_document(doc);
        let activation = self.has_transient_activation(doc);
        let allowed = if write {
            activation || focused
        } else {
            activation && focused
        };
        allowed.then_some(clipboard)
    }

    fn clipboard_item<R>(&self, h: Handle, f: impl FnOnce(&ClipboardItemData) -> R) -> R {
        match &*self.obj(h) {
            Object::ClipboardItem(d) => f(d),
            _ => f(&ClipboardItemData {
                items: Vec::new(),
                style: PresentationStyle::Unspecified,
            }),
        }
    }

    /// The text of a clipboard item's representation: a string, or a Blob's bytes as UTF-8.
    fn clipboard_text_of(&self, cx: &mut Cx, value: &rt::JsValue) -> Option<String> {
        let scope = cx.scope();
        let v = v8::Local::new(scope, value.global());
        if v.is_string() {
            return Some(v.to_rust_string_lossy(scope));
        }
        let o = v8::Local::<v8::Object>::try_from(v).ok()?;
        let h = rt::unwrap(&o, ids::BLOB, ids::BLOB_END)?;
        matches!(&*self.obj(h), Object::Blob(_)).then(|| {
            let (bytes, _) = self.blob_contents(h);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    }
}

impl NavigatorNetworkInformationImpl for Engine {
    fn connection(&self, this: Handle) -> Handle {
        self.navigator_singleton(this, "connection", || Object::NetworkInformation)
    }
}

/// Network Information: what the engine knows of the connection. It does not measure it: the
/// values are the specification's for an unknown fast connection (rtt and downlink as Chromium
/// reports a typical broadband one, rounded as §4 requires).
impl NetworkInformationImpl for Engine {
    fn type_(&self, _this: Handle) -> ConnectionType {
        ConnectionType::Unknown
    }
    fn effective_type(&self, _this: Handle) -> EffectiveConnectionType {
        EffectiveConnectionType::_4g
    }
    fn downlink_max(&self, _this: Handle) -> f64 {
        f64::INFINITY
    }
    fn downlink(&self, _this: Handle) -> f64 {
        10.0
    }
    fn rtt(&self, _this: Handle) -> u64 {
        50
    }
}

impl NetworkInformationSaveDataImpl for Engine {
    fn save_data(&self, _this: Handle) -> bool {
        false
    }
}

impl NavigatorUAImpl for Engine {
    fn user_agent_data(&self, this: Handle) -> Handle {
        self.navigator_singleton(this, "userAgentData", || Object::NavigatorUAData)
    }
}

impl NavigatorUADataImpl for Engine {
    /// The brands of the user agent sent to this navigator's origin (a listed site's leave out
    /// the browser's own: `snowlark_net::ua::UserAgent::unbranded`).
    fn brands(&self, this: Handle) -> Vec<NavigatorUABrandVersion> {
        brands(&self.navigator_ua_profile(this), false)
    }
    fn mobile(&self, _this: Handle) -> bool {
        self.ua_profile().mobile
    }
    fn platform(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.ua_profile().os)
    }
    /// UA-CH §6.1 `getHighEntropyValues()`: the low-entropy values and each hint asked for.
    fn get_high_entropy_values(
        &self,
        cx: &mut Cx,
        this: Handle,
        hints: Vec<String>,
    ) -> rt::Result<JsPromise> {
        let info = self.navigator_ua_profile(this);
        let has = |h: &str| hints.iter().any(|x| x == h);
        let values = UADataValues {
            brands: Some(brands(&info, false)),
            mobile: Some(info.mobile),
            platform: Some(info.os.to_string()),
            architecture: has("architecture").then(|| info.architecture.to_string()),
            bitness: has("bitness").then(|| info.bitness.to_string()),
            form_factors: has("formFactors")
                .then(|| vec![if info.mobile { "Mobile" } else { "Desktop" }.to_string()]),
            full_version_list: has("fullVersionList").then(|| brands(&info, true)),
            model: has("model").then(String::new),
            platform_version: has("platformVersion").then(|| info.platform_version.clone()),
            ua_full_version: has("uaFullVersion").then(|| info.ua_full_version()),
            wow64: has("wow64").then_some(info.wow64),
        };
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        p.resolve(cx, values);
        Ok(p)
    }
    fn to_json(&self, _cx: &mut Cx, this: Handle) -> rt::Result<UALowEntropyJSON> {
        let info = self.navigator_ua_profile(this);
        Ok(UALowEntropyJSON {
            brands: Some(brands(&info, false)),
            mobile: Some(info.mobile),
            platform: Some(info.os.to_string()),
        })
    }
}

impl Engine {
    pub(crate) fn navigator_clipboard(&self, navigator: Handle) -> Handle {
        self.navigator_singleton(navigator, "clipboard", || Object::Clipboard)
    }

    pub(crate) fn navigator_permissions(&self, navigator: Handle) -> Handle {
        self.navigator_singleton(navigator, "permissions", || Object::Permissions)
    }

    /// Permissions §? `query()`: the descriptor's name, a registered one, and its state.
    fn permission_status(&self, cx: &mut Cx, desc: &JsObject) -> rt::Result<Handle> {
        let name = {
            let scope = cx.scope();
            let o = v8::Local::new(scope, desc.global());
            let key = rt::name(scope, "name");
            let v = o.get(scope, key.into()).ok_or(Error::Pending)?;
            if v.is_undefined() {
                return Err(Error::Type("The permission descriptor has no name".into()));
            }
            v.to_rust_string_lossy(scope)
        };
        let state = permission_state(&name).ok_or_else(|| {
            Error::Type(format!("'{name}' is not a valid permission name").into())
        })?;
        Ok(self.alloc(Object::PermissionStatus(Box::new((name, state)))))
    }
}

impl PermissionsImpl for Engine {
    fn query(
        &self,
        cx: &mut Cx,
        _this: Handle,
        permission_desc: JsObject,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        match self.permission_status(cx, &permission_desc) {
            Ok(status) => p.resolve(cx, status),
            Err(e) => p.reject(cx, e),
        }
        Ok(p)
    }
    /// Permissions (request): this engine shows no prompt: the state as it is.
    fn request(
        &self,
        cx: &mut Cx,
        this: Handle,
        permission_desc: JsObject,
    ) -> rt::Result<JsPromise> {
        self.query(cx, this, permission_desc)
    }
    fn revoke(
        &self,
        cx: &mut Cx,
        this: Handle,
        permission_desc: JsObject,
    ) -> rt::Result<JsPromise> {
        self.query(cx, this, permission_desc)
    }
}

impl PermissionStatusImpl for Engine {
    fn state(&self, this: Handle) -> PermissionState {
        match &*self.obj(this) {
            Object::PermissionStatus(s) => s.1,
            _ => PermissionState::Denied,
        }
    }
    fn name(&self, this: Handle) -> DomStr<'_> {
        match &*self.obj(this) {
            Object::PermissionStatus(s) => DomStr::Owned(s.0.clone()),
            _ => DomStr::Borrowed(""),
        }
    }
}

impl Engine {
    /// The window of a `UserActivation` or `MediaDevices` object.
    fn navigator_object_window(&self, this: Handle) -> Option<Handle> {
        match &*self.obj(this) {
            Object::UserActivation(w) | Object::MediaDevices(w) => Some(*w),
            _ => None,
        }
    }

    /// Media Capture and Streams §9.2 "device enumeration can proceed": the User Agent MAY proceed
    /// when device information can be exposed (never here: no capture device was ever granted), else
    /// when `doc` "is in view" (fully active, visibility state "visible").
    fn device_enumeration_can_proceed(&self, doc: NodeId) -> bool {
        self.is_active_document(doc) && self.document_visible(doc)
    }

    /// Resolves the `enumerateDevices()` promises of `doc` that waited for it to be in view.
    pub(crate) fn resume_device_enumeration(&self, cx: &mut Cx, doc: NodeId) {
        if !self.device_enumeration_can_proceed(doc) {
            return;
        }
        let ready: Vec<JsPromise> = {
            let mut e = self.embedder.borrow_mut();
            let (ready, waiting) = std::mem::take(&mut e.device_enumerations)
                .into_iter()
                .partition(|(d, _)| *d == doc);
            e.device_enumerations = waiting;
            ready.into_iter().map(|(_, p)| p).collect()
        };
        for p in ready {
            self.resolve_device_list(cx, p);
        }
    }

    /// "Creating a list of device info objects" from the [[storedDeviceList]], "a list of all media
    /// input and output devices available to the User Agent": the engine captures from no camera or
    /// microphone and selects no audio output, so none is available and the list is empty. The
    /// promise is resolved in a task (the algorithm runs in parallel).
    fn resolve_device_list(&self, _cx: &mut Cx, p: JsPromise) {
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            p.resolve(cx, Vec::<Handle>::new());
        });
    }
}

impl UserActivationImpl for Engine {
    /// HTML §6.4.4: whether the relevant global object has sticky activation.
    fn has_been_active(&self, this: Handle) -> bool {
        self.navigator_object_window(this)
            .and_then(|w| self.associated_document(w))
            .is_some_and(|d| self.has_sticky_activation(d))
    }
    /// HTML §6.4.4: whether the relevant global object has transient activation.
    fn is_active(&self, this: Handle) -> bool {
        self.navigator_object_window(this)
            .and_then(|w| self.associated_document(w))
            .is_some_and(|d| self.has_transient_activation(d))
    }
}

impl MediaDevicesImpl for Engine {
    /// Media Capture and Streams §9.2 `enumerateDevices()`.
    fn enumerate_devices(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let doc = self
            .navigator_object_window(this)
            .and_then(|w| self.associated_document(w));
        match doc {
            Some(doc) if !self.device_enumeration_can_proceed(doc) => {
                // (waits until a task would find that it can: the document becomes visible)
                self.embedder
                    .borrow_mut()
                    .device_enumerations
                    .push((doc, p.clone()));
            }
            _ => self.resolve_device_list(cx, p.clone()),
        }
        Ok(p)
    }
    /// Capture Handle Identity §4.1 `setCaptureHandleConfig()`. The engine captures nothing, so a
    /// valid config is observable by no capturer and is not kept.
    fn set_capture_handle_config(
        &self,
        _cx: &mut Cx,
        this: Handle,
        config: CaptureHandleConfig,
    ) -> rt::Result<()> {
        if config.handle.encode_utf16().count() > 1024 {
            return Err(Error::Type(
                "The handle is longer than 1024 code units".into(),
            ));
        }
        let origins = &config.permitted_origins;
        let any = origins.iter().any(|o| o == "*");
        if any && origins.len() > 1 {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "\"*\" must be the only permitted origin",
            ));
        }
        if !any && origins.iter().any(|o| url::Url::parse(o).is_err()) {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "A permitted origin is not a valid origin",
            ));
        }
        let top = self.navigator_object_window(this).is_some_and(|w| {
            let f = self.frames.borrow();
            f.by_window
                .get(&w)
                .is_some_and(|&id| f.list[id].parent.is_none())
        });
        if !top {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Only a top-level document can set a capture handle",
            ));
        }
        Ok(())
    }
    /// The constrainable properties the User Agent knows: the dictionary's members are each present
    /// and true by its IDL defaults (as in Chrome); none applies, since no track can be captured.
    fn get_supported_constraints(
        &self,
        _cx: &mut Cx,
        _this: Handle,
    ) -> rt::Result<MediaTrackSupportedConstraints> {
        Ok(MediaTrackSupportedConstraints::default())
    }
}

/// No `MediaDeviceInfo` is made: no device is available (see `resolve_device_list`).
impl MediaDeviceInfoImpl for Engine {
    fn device_id(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn kind(&self, _this: Handle) -> MediaDeviceKind {
        MediaDeviceKind::Audioinput
    }
    fn label(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn group_id(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
}

impl InputDeviceInfoImpl for Engine {
    fn get_capabilities(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<MediaTrackCapabilities> {
        Ok(MediaTrackCapabilities::default())
    }
}

impl ClipboardImpl for Engine {
    /// Clipboard API `read()`: one item with the clipboard's text.
    fn read(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _formats: ClipboardUnsanitizedFormats,
    ) -> rt::Result<JsPromise> {
        let Some(clipboard) = self.clipboard_allowed(cx, false) else {
            return rejected(
                cx,
                ExceptionName::NotAllowedError,
                "Reading the clipboard is not allowed",
            );
        };
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let result = p.clone();
        self.queue_task(TaskSource::UserInteraction, move |e, cx| {
            let text = clipboard.read_text().unwrap_or_default();
            let blob = e.new_blob(text.into_bytes(), "text/plain");
            let Some(data) = JsPromise::new(cx) else {
                return;
            };
            data.resolve(cx, blob);
            let promise = data.promise(cx);
            let promise = {
                let scope = cx.scope();
                let v = v8::Local::new(scope, promise.global());
                match v8::Local::<v8::Object>::try_from(v) {
                    Ok(o) => JsObject::new(scope, o),
                    Err(_) => return,
                }
            };
            let item = e.alloc(Object::ClipboardItem(Box::new(ClipboardItemData {
                items: vec![("text/plain".into(), promise)],
                style: PresentationStyle::Unspecified,
            })));
            result.resolve(cx, vec![item]);
        });
        Ok(p)
    }
    /// Clipboard API `readText()`.
    fn read_text(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        let Some(clipboard) = self.clipboard_allowed(cx, false) else {
            return rejected(
                cx,
                ExceptionName::NotAllowedError,
                "Reading the clipboard is not allowed",
            );
        };
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let result = p.clone();
        self.queue_task(TaskSource::UserInteraction, move |_, cx| {
            result.resolve(cx, clipboard.read_text().unwrap_or_default());
        });
        Ok(p)
    }
    /// Clipboard API `write()`: the text of the first item with a `text/plain` representation.
    fn write(&self, cx: &mut Cx, _this: Handle, data: Vec<Handle>) -> rt::Result<JsPromise> {
        let Some(clipboard) = self.clipboard_allowed(cx, true) else {
            return rejected(
                cx,
                ExceptionName::NotAllowedError,
                "Writing the clipboard is not allowed",
            );
        };
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let text_promise = data.iter().find_map(|&h| {
            self.clipboard_item(h, |d| {
                d.items
                    .iter()
                    .find(|(t, _)| t == "text/plain")
                    .map(|(_, p)| p.clone())
            })
        });
        let Some(text_promise) = text_promise else {
            p.reject(
                cx,
                exception(
                    ExceptionName::NotAllowedError,
                    "Only text can be written to the clipboard",
                ),
            );
            return Ok(p);
        };
        let value = {
            let scope = cx.scope();
            let o = v8::Local::new(scope, text_promise.global());
            rt::JsValue::new(scope, o.into())
        };
        let result = p.clone();
        self.upon(
            cx,
            &value,
            Box::new(move |e, cx, r| match r {
                Ok(v) => match e.clipboard_text_of(cx, &v) {
                    Some(text) => {
                        clipboard.write_text(&text);
                        result.resolve(cx, ());
                    }
                    None => result.reject(
                        cx,
                        exception(ExceptionName::NotAllowedError, "The data is not text"),
                    ),
                },
                Err(reason) => result.reject_value(cx, &reason),
            }),
        );
        Ok(p)
    }
    /// Clipboard API `writeText()`.
    fn write_text(&self, cx: &mut Cx, _this: Handle, data: &str) -> rt::Result<JsPromise> {
        let Some(clipboard) = self.clipboard_allowed(cx, true) else {
            return rejected(
                cx,
                ExceptionName::NotAllowedError,
                "Writing the clipboard is not allowed",
            );
        };
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let result = p.clone();
        let text = data.to_owned();
        self.queue_task(TaskSource::UserInteraction, move |_, cx| {
            clipboard.write_text(&text);
            result.resolve(cx, ());
        });
        Ok(p)
    }
}

impl ClipboardItemImpl for Engine {
    fn presentation_style(&self, this: Handle) -> PresentationStyle {
        self.clipboard_item(this, |d| d.style)
    }
    fn types(&self, this: Handle) -> Vec<String> {
        self.clipboard_item(this, |d| d.items.iter().map(|(t, _)| t.clone()).collect())
    }
    /// Clipboard API `getType()`: a Blob of the representation's data.
    fn get_type(&self, cx: &mut Cx, this: Handle, type_: &str) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let found = self.clipboard_item(this, |d| {
            d.items
                .iter()
                .find(|(t, _)| t == type_)
                .map(|(_, p)| p.clone())
        });
        let Some(data) = found else {
            p.reject(
                cx,
                exception(ExceptionName::NotFoundError, "The item has no such type"),
            );
            return Ok(p);
        };
        let value = {
            let scope = cx.scope();
            let o = v8::Local::new(scope, data.global());
            rt::JsValue::new(scope, o.into())
        };
        let result = p.clone();
        let type_ = type_.to_owned();
        self.upon(
            cx,
            &value,
            Box::new(move |e, cx, r| match r {
                Ok(v) => {
                    let blob = {
                        let scope = cx.scope();
                        let l = v8::Local::new(scope, v.global());
                        v8::Local::<v8::Object>::try_from(l)
                            .ok()
                            .and_then(|o| rt::unwrap(&o, ids::BLOB, ids::BLOB_END))
                            .filter(|&h| matches!(&*e.obj(h), Object::Blob(_)))
                    };
                    let blob = match blob {
                        Some(b) => b,
                        None => {
                            let text = e.clipboard_text_of(cx, &v).unwrap_or_default();
                            e.new_blob(text.into_bytes(), &type_)
                        }
                    };
                    result.resolve(cx, blob);
                }
                Err(reason) => result.reject_value(cx, &reason),
            }),
        );
        Ok(p)
    }
    /// Clipboard API `ClipboardItem.supports()`: the types this engine can write (text).
    fn static_supports(&self, _cx: &mut Cx, type_: &str) -> rt::Result<bool> {
        Ok(type_ == "text/plain")
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        items: rt::Record<JsObject>,
        options: ClipboardItemOptions,
    ) -> rt::Result<Handle> {
        if items.0.is_empty() {
            return Err(Error::Type(
                "A ClipboardItem needs at least one type".into(),
            ));
        }
        Ok(
            self.alloc(Object::ClipboardItem(Box::new(ClipboardItemData {
                items: items.0,
                style: options.presentation_style,
            }))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hints_follow_the_user_agent_string() {
        let i = snowlark_net::ua::UserAgent::default().profile();
        assert_eq!(
            (
                i.brand.as_str(),
                i.major.as_str(),
                i.os,
                i.platform_version.as_str(),
                i.architecture,
                i.bitness,
                i.mobile
            ),
            ("Snowlark", "0", "Windows", "10.0.0", "x86", "64", false)
        );
        assert_eq!(
            permission_state("clipboard-write"),
            Some(PermissionState::Granted)
        );
        assert_eq!(
            permission_state("notifications"),
            Some(PermissionState::Denied)
        );
        assert_eq!(permission_state("bogus"), None);
    }
}
