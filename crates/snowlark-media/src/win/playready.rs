// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PlayReady through the content decryption module Windows ships (Windows 10 2004 and later):
//! `IMFMediaEngineClassFactory4::CreateContentDecryptionModuleFactory`, an access object for one
//! configuration, the module itself with a store folder, and sessions whose shape is the
//! Encrypted Media Extensions session's (`IMFContentDecryptionModuleSession`).

use std::path::Path;
use std::sync::mpsc::Sender;

use windows::core::{implement, Interface, Ref, Result, BSTR, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{
    CreateStreamOnHGlobal, PropVariantClear, PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0,
    PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemAlloc, IStream, CLSCTX_INPROC_SERVER, STATFLAG, STATFLAG_NONAME,
    STATFLAG_NOOPEN, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::{
    VARENUM, VT_BSTR, VT_LPWSTR, VT_UI4, VT_UNKNOWN, VT_VARIANT, VT_VECTOR,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PSCreateMemoryPropertyStore};

/// The key system with software security (SL2000).
pub const RECOMMENDATION: &str = "com.microsoft.playready.recommendation";
/// The key system that asks for hardware security (SL3000).
pub const RECOMMENDATION_3000: &str = "com.microsoft.playready.recommendation.3000";

const fn key(data1: u32, data2: u16, data3: u16, data4: [u8; 8], pid: u32) -> PROPERTYKEY {
    PROPERTYKEY {
        fmtid: GUID {
            data1,
            data2,
            data3,
            data4,
        },
        pid,
    }
}

// mfmediaengine.h and mfcontentdecryptionmodule.h (PID_FIRST_USABLE is 2).
const MF_EME_PERSISTEDSTATE: PROPERTYKEY = key(
    0x5d4df6ae,
    0x9af1,
    0x4e3d,
    [0x95, 0x5b, 0x0e, 0x4b, 0xd2, 0x2f, 0xed, 0xf0],
    5,
);
const MF_EME_DISTINCTIVEID: PROPERTYKEY = key(
    0x7dc9c4a5,
    0x12be,
    0x497e,
    [0x8b, 0xff, 0x9b, 0x60, 0xb2, 0xdc, 0x58, 0x45],
    4,
);
const MF_EME_AUDIOCAPABILITIES: PROPERTYKEY = key(
    0x980fbb84,
    0x297d,
    0x4ea7,
    [0x89, 0x5f, 0xbc, 0xf2, 0x8a, 0x46, 0x28, 0x81],
    6,
);
const MF_EME_VIDEOCAPABILITIES: PROPERTYKEY = key(
    0xb172f83d,
    0x30dd,
    0x4c10,
    [0x80, 0x06, 0xed, 0x53, 0xda, 0x4d, 0x3b, 0xdb],
    7,
);
const MF_EME_ROBUSTNESS: PROPERTYKEY = key(
    0x9d3d2b9e,
    0x7023,
    0x4944,
    [0xa8, 0xf5, 0xec, 0xca, 0x52, 0xa4, 0x69, 0x90],
    3,
);
const MF_CONTENTDECRYPTIONMODULE_STOREPATH: PROPERTYKEY = key(
    0x77d993b9,
    0xba61,
    0x4bb7,
    [0x92, 0xc6, 0x18, 0xc8, 0x6a, 0x18, 0x9c, 0x06],
    2,
);
/// The module's origin identifier: a key of the browser's own, not of the SDK.
const ORIGIN_ID: PROPERTYKEY = key(
    0x1218a3e2,
    0xcfb0,
    0x4c98,
    [0x90, 0xe5, 0x5f, 0x58, 0x18, 0xd4, 0xb6, 0x7e],
    2,
);
const MF_MEDIAKEYS_REQUIREMENT_REQUIRED: u32 = 1;

// The protected media path's EME store activation object and the attributes that name what it
// activates (the PMP host's `CreateObjectByCLSID` takes a serialized attribute store).
const CLSID_EME_STORE_ACTIVATE: GUID = GUID::from_u128(0x2df7b51e_797b_4d06_be71_d14a52cf8421);
const GUID_CLASS_NAME: GUID = GUID::from_u128(0x77631a31_e5e7_4785_bf17_20f57b224802);
const GUID_OBJECT_STREAM: GUID = GUID::from_u128(0x3e73735c_e6c0_481d_8260_ee5db1343b5f);

fn variant(vt: VARENUM, value: PROPVARIANT_0_0_0) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

fn set(store: &IPropertyStore, k: &PROPERTYKEY, mut v: PROPVARIANT) -> Result<()> {
    // SAFETY: the store copies the value (PropVariantCopy); ours is cleared after, which frees what
    // it owns (strings, interfaces, vector elements).
    unsafe {
        let r = store.SetValue(k, &v);
        let _ = PropVariantClear(&mut v);
        r
    }
}

fn u4(n: u32) -> PROPVARIANT {
    variant(VT_UI4, PROPVARIANT_0_0_0 { ulVal: n })
}

fn lpwstr(s: &str) -> PROPVARIANT {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a CoTaskMem buffer of the string's size, filled before use; PropVariantClear frees it.
    let p = unsafe { CoTaskMemAlloc(wide.len() * 2) } as *mut u16;
    // SAFETY: `p` holds `wide.len()` u16s (a null pointer is checked first).
    if !p.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len()) };
    }
    variant(
        VT_LPWSTR,
        PROPVARIANT_0_0_0 {
            pwszVal: windows::core::PWSTR(p),
        },
    )
}

/// A `VT_VARIANT | VT_VECTOR` of `VT_UNKNOWN` property stores.
fn stores(items: &[IPropertyStore]) -> PROPVARIANT {
    let n = items.len();
    let elems = if n == 0 {
        std::ptr::null_mut()
    } else {
        // SAFETY: CoTaskMem space for `n` PROPVARIANTs, each written below; PropVariantClear of the
        // vector frees it and releases each element.
        unsafe { CoTaskMemAlloc(n * std::mem::size_of::<PROPVARIANT>()) as *mut PROPVARIANT }
    };
    if !elems.is_null() {
        for (i, s) in items.iter().enumerate() {
            let unknown: windows::core::IUnknown = s.cast().expect("a property store is IUnknown");
            let v = variant(
                VT_UNKNOWN,
                PROPVARIANT_0_0_0 {
                    punkVal: std::mem::ManuallyDrop::new(Some(unknown)),
                },
            );
            // SAFETY: `i < n`, inside the allocation; written without reading the old bytes.
            unsafe { std::ptr::write(elems.add(i), v) };
        }
    }
    let mut v = variant(VT_VARIANT | VT_VECTOR, PROPVARIANT_0_0_0 { ulVal: 0 });
    // SAFETY: writing the vector's count and pointer into the union member VT_VECTOR uses.
    unsafe {
        let inner = &mut v.Anonymous.Anonymous;
        inner.Anonymous.capropvar.cElems = if elems.is_null() { 0 } else { n as u32 };
        inner.Anonymous.capropvar.pElems = elems;
    }
    v
}

fn property_store() -> Result<IPropertyStore> {
    let mut p = std::ptr::null_mut();
    // SAFETY: an out pointer for the requested interface, owned by the returned wrapper.
    unsafe {
        PSCreateMemoryPropertyStore(&IPropertyStore::IID, &mut p)?;
        Ok(IPropertyStore::from_raw(p))
    }
}

/// The factory for one key system. Windows makes one per process: asking for a second key system
/// in the same process fails.
pub fn factory(key_system: &str) -> Result<IMFContentDecryptionModuleFactory> {
    super::ensure_com();
    // SAFETY: plain COM activation and a factory call with an out pointer the wrapper owns.
    unsafe {
        let class: IMFMediaEngineClassFactory4 =
            CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
        let mut p = std::ptr::null_mut();
        class.CreateContentDecryptionModuleFactory(
            &HSTRING::from(key_system),
            &IMFContentDecryptionModuleFactory::IID,
            &mut p,
        )?;
        Ok(IMFContentDecryptionModuleFactory::from_raw(p))
    }
}

/// Whether the system's media engine answers "probably" for this content type under this key
/// system (the factory's own `IsTypeSupported` says yes to anything for the system's PlayReady).
pub fn type_supported(key_system: &str, content_type: &str) -> bool {
    super::ensure_com();
    // SAFETY: COM activation; the strings live across the call.
    unsafe {
        let Ok(ext) = CoCreateInstance::<_, IMFExtendedDRMTypeSupport>(
            &CLSID_MFMediaEngineClassFactory,
            None,
            CLSCTX_INPROC_SERVER,
        ) else {
            return false;
        };
        ext.IsTypeSupportedEx(&BSTR::from(content_type), &BSTR::from(key_system))
            .map(|a| a == MF_MEDIA_ENGINE_CANPLAY_PROBABLY)
            .unwrap_or(false)
    }
}

/// Creates the module: one configuration (hardware robustness when `hardware`, persistent state and a
/// distinctive identifier required, as the system's PlayReady needs both), the store folder, and
/// the protected media path host the module asks for before its first request.
pub fn create(
    key_system: &str,
    hardware: bool,
    origin_id: &str,
    store: &Path,
) -> Result<IMFContentDecryptionModule> {
    let factory = factory(key_system)?;
    // SAFETY: the key system string lives across the call.
    if !unsafe { factory.IsTypeSupported(&HSTRING::from(key_system), PCWSTR::null()) }.as_bool() {
        return Err(MF_E_NOT_AVAILABLE.into());
    }
    let video = property_store()?;
    if hardware {
        let s = BSTR::from("3000");
        set(
            &video,
            &MF_EME_ROBUSTNESS,
            variant(
                VT_BSTR,
                PROPVARIANT_0_0_0 {
                    bstrVal: std::mem::ManuallyDrop::new(s),
                },
            ),
        )?;
    }
    let config = property_store()?;
    set(&config, &MF_EME_AUDIOCAPABILITIES, stores(&[]))?;
    set(&config, &MF_EME_VIDEOCAPABILITIES, stores(&[video]))?;
    set(
        &config,
        &MF_EME_PERSISTEDSTATE,
        u4(MF_MEDIAKEYS_REQUIREMENT_REQUIRED),
    )?;
    set(
        &config,
        &MF_EME_DISTINCTIVEID,
        u4(MF_MEDIAKEYS_REQUIREMENT_REQUIRED),
    )?;
    // SAFETY: the configuration array lives across the call.
    let access = unsafe {
        factory.CreateContentDecryptionModuleAccess(&HSTRING::from(key_system), &[Some(config)])
    }?;
    std::fs::create_dir_all(store).map_err(|_| windows::core::Error::from(MF_E_NOT_AVAILABLE))?;
    let props = property_store()?;
    set(&props, &ORIGIN_ID, lpwstr(origin_id))?;
    set(
        &props,
        &MF_CONTENTDECRYPTIONMODULE_STOREPATH,
        lpwstr(&store.to_string_lossy()),
    )?;
    // SAFETY: a plain call with a live property store.
    let cdm = unsafe { access.CreateContentDecryptionModule(&props) }?;
    set_pmp_host(&cdm)?;
    Ok(cdm)
}

fn set_pmp_host(cdm: &IMFContentDecryptionModule) -> Result<()> {
    let services: IMFGetService = cdm.cast()?;
    // SAFETY: GetService with an interface id and a matching out pointer.
    unsafe {
        if let Ok(host) = services.GetService::<IMFPMPHost>(&MF_CONTENTDECRYPTIONMODULE_SERVICE) {
            let app: IMFPMPHostApp = PmpHostApp {
                host: Host::Pmp(host),
            }
            .into();
            return cdm.SetPMPHostApp(&app);
        }
        let host = services.GetService::<IMFPMPHostApp>(&MF_CONTENTDECRYPTIONMODULE_SERVICE)?;
        let app: IMFPMPHostApp = PmpHostApp {
            host: Host::App(host),
        }
        .into();
        cdm.SetPMPHostApp(&app)
    }
}

enum Host {
    Pmp(IMFPMPHost),
    App(IMFPMPHostApp),
}

/// The application's protected media path host: activations go to the EME store activator in the
/// protected process, named in a serialized attribute store.
#[implement(IMFPMPHostApp)]
struct PmpHostApp {
    host: Host,
}

impl IMFPMPHostApp_Impl for PmpHostApp_Impl {
    fn LockProcess(&self) -> Result<()> {
        // SAFETY: forwarding to the wrapped host.
        unsafe {
            match &self.host {
                Host::Pmp(h) => h.LockProcess(),
                Host::App(h) => h.LockProcess(),
            }
        }
    }

    fn UnlockProcess(&self) -> Result<()> {
        // SAFETY: forwarding to the wrapped host.
        unsafe {
            match &self.host {
                Host::Pmp(h) => h.UnlockProcess(),
                Host::App(h) => h.UnlockProcess(),
            }
        }
    }

    fn ActivateClassById(
        &self,
        id: &PCWSTR,
        stream: Ref<IStream>,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        // SAFETY: COM calls on live objects; `riid` and `ppv` come from the caller as the
        // interface contract defines them, and are passed through unchanged.
        unsafe {
            let host = match &self.host {
                Host::App(h) => {
                    let r =
                        h.ActivateClassById::<_, _, windows::core::IUnknown>(*id, stream.as_ref())?;
                    return r.query(riid, ppv).ok();
                }
                Host::Pmp(h) => h,
            };
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 3)?;
            let attrs = attrs.ok_or_else(|| windows::core::Error::from(MF_E_NOT_AVAILABLE))?;
            attrs.SetString(&GUID_CLASS_NAME, *id)?;
            if let Some(stream) = stream.as_ref() {
                let mut stat = STATSTG::default();
                stream.Stat(&mut stat, STATFLAG(STATFLAG_NOOPEN.0 | STATFLAG_NONAME.0))?;
                let mut blob = vec![0u8; stat.cbSize as usize];
                let mut read = 0u32;
                stream
                    .Read(blob.as_mut_ptr().cast(), blob.len() as u32, Some(&mut read))
                    .ok()?;
                attrs.SetBlob(&GUID_OBJECT_STREAM, &blob[..read as usize])?;
            }
            let out = CreateStreamOnHGlobal(Default::default(), true)?;
            MFSerializeAttributesToStream(&attrs, 0, &out)?;
            out.Seek(0, STREAM_SEEK_SET, None)?;
            let activate: IMFActivate =
                host.CreateObjectByCLSID(&CLSID_EME_STORE_ACTIVATE, &out)?;
            let object: windows::core::IUnknown = activate.ActivateObject()?;
            object.query(riid, ppv).ok()
        }
    }
}

/// What a session reports, from whichever thread Media Foundation calls on.
#[derive(Debug)]
pub enum SessionEvent {
    /// A message for the licence server: its EME type (`license-request`, ...) and bytes.
    Message(&'static str, Vec<u8>),
    /// The key statuses changed: read them with [`key_statuses`].
    KeysChanged,
}

#[implement(IMFContentDecryptionModuleSessionCallbacks)]
struct Callbacks {
    notify: Box<dyn Fn(SessionEvent) + Send + Sync>,
}

impl IMFContentDecryptionModuleSessionCallbacks_Impl for Callbacks_Impl {
    fn KeyMessage(
        &self,
        kind: MF_MEDIAKEYSESSION_MESSAGETYPE,
        message: *const u8,
        size: u32,
        _destination: &PCWSTR,
    ) -> Result<()> {
        let bytes = if message.is_null() {
            Vec::new()
        } else {
            // SAFETY: Media Foundation passes `size` readable bytes at `message` for this call.
            unsafe { std::slice::from_raw_parts(message, size as usize) }.to_vec()
        };
        let kind = match kind {
            MF_MEDIAKEYSESSION_MESSAGETYPE_LICENSE_RENEWAL => "license-renewal",
            MF_MEDIAKEYSESSION_MESSAGETYPE_LICENSE_RELEASE => "license-release",
            MF_MEDIAKEYSESSION_MESSAGETYPE_INDIVIDUALIZATION_REQUEST => "individualization-request",
            _ => "license-request",
        };
        (self.notify)(SessionEvent::Message(kind, bytes));
        Ok(())
    }

    fn KeyStatusChanged(&self) -> Result<()> {
        (self.notify)(SessionEvent::KeysChanged);
        Ok(())
    }
}

/// A new temporary session whose events go to `tx`.
pub fn session(
    cdm: &IMFContentDecryptionModule,
    tx: Sender<SessionEvent>,
) -> Result<IMFContentDecryptionModuleSession> {
    let tx = std::sync::Mutex::new(tx);
    session_with(
        cdm,
        Box::new(move |e| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(e);
            }
        }),
    )
}

/// A new temporary session whose events `notify` hears, on Media Foundation's threads.
pub fn session_with(
    cdm: &IMFContentDecryptionModule,
    notify: Box<dyn Fn(SessionEvent) + Send + Sync>,
) -> Result<IMFContentDecryptionModuleSession> {
    let callbacks: IMFContentDecryptionModuleSessionCallbacks = Callbacks { notify }.into();
    // SAFETY: a plain call; the module keeps its own reference to the callbacks.
    unsafe { cdm.CreateSession(MF_MEDIAKEYSESSION_TYPE_TEMPORARY, &callbacks) }
}

/// The session's keys: each key id (in the byte order the media carries it) and its EME status.
pub fn key_statuses(
    session: &IMFContentDecryptionModuleSession,
) -> Result<Vec<([u8; 16], &'static str)>> {
    let mut p: *mut MFMediaKeyStatus = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: the out array and each key id are CoTaskMem allocations this function frees.
    unsafe {
        session.GetKeyStatuses(&mut p, &mut n)?;
        let mut out = Vec::new();
        for i in 0..n as usize {
            let s = &*p.add(i);
            let mut id = [0u8; 16];
            if s.cbKeyId as usize == 16 && !s.pbKeyId.is_null() {
                id.copy_from_slice(std::slice::from_raw_parts(s.pbKeyId, 16));
                // The module gives a GUID's in-memory bytes: the first three fields little-endian.
                id[0..4].reverse();
                id[4..6].reverse();
                id[6..8].reverse();
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(s.pbKeyId as *const _));
            let status = match s.eMediaKeyStatus {
                MF_MEDIAKEY_STATUS_USABLE => "usable",
                MF_MEDIAKEY_STATUS_EXPIRED => "expired",
                MF_MEDIAKEY_STATUS_OUTPUT_DOWNSCALED => "output-downscaled",
                MF_MEDIAKEY_STATUS_STATUS_PENDING => "status-pending",
                MF_MEDIAKEY_STATUS_RELEASED => "released",
                MF_MEDIAKEY_STATUS_OUTPUT_RESTRICTED => "output-restricted",
                _ => "internal-error",
            };
            out.push((id, status));
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(p as *const _));
        Ok(out)
    }
}

/// The content protection manager a protected media engine asks when a stream needs enabling: the
/// enabler it is given goes to the module, which completes the engine's asynchronous request.
#[implement(IMFContentProtectionManager, IMediaProtectionManagerAbi)]
pub struct ProtectionManager {
    cdm: IMFContentDecryptionModule,
    /// The Windows Runtime side's properties: the module's protected media path server, under
    /// the name the media engine looks it up by.
    properties: windows::Foundation::Collections::PropertySet,
}

/// `Windows.Media.Protection.IMediaProtectionManager` at the ABI: the media engine reads the
/// protected media path server from its `Properties`. Declared on `IUnknown` with the three
/// `IInspectable` methods first, which is the same vtable.
#[allow(non_snake_case)]
mod abi {
    use windows::core::GUID;

    #[windows::core::interface("45694947-c741-434b-a79e-474c12d93d2f")]
    pub unsafe trait IMediaProtectionManagerAbi: windows::core::IUnknown {
        fn GetIids(&self, count: *mut u32, iids: *mut *mut GUID) -> windows::core::HRESULT;
        fn GetRuntimeClassName(&self, name: *mut *mut core::ffi::c_void) -> windows::core::HRESULT;
        fn GetTrustLevel(&self, level: *mut i32) -> windows::core::HRESULT;
        fn ServiceRequested(
            &self,
            handler: *mut core::ffi::c_void,
            token: *mut i64,
        ) -> windows::core::HRESULT;
        fn RemoveServiceRequested(&self, token: i64) -> windows::core::HRESULT;
        fn RebootNeeded(
            &self,
            handler: *mut core::ffi::c_void,
            token: *mut i64,
        ) -> windows::core::HRESULT;
        fn RemoveRebootNeeded(&self, token: i64) -> windows::core::HRESULT;
        fn ComponentLoadFailed(
            &self,
            handler: *mut core::ffi::c_void,
            token: *mut i64,
        ) -> windows::core::HRESULT;
        fn RemoveComponentLoadFailed(&self, token: i64) -> windows::core::HRESULT;
        fn Properties(&self, value: *mut *mut core::ffi::c_void) -> windows::core::HRESULT;
    }
}
use abi::{IMediaProtectionManagerAbi, IMediaProtectionManagerAbi_Impl};

#[allow(non_snake_case)]
impl IMediaProtectionManagerAbi_Impl for ProtectionManager_Impl {
    unsafe fn GetIids(&self, count: *mut u32, iids: *mut *mut GUID) -> windows::core::HRESULT {
        // SAFETY: the caller's out pointers, written as IInspectable::GetIids defines (none).
        unsafe {
            *count = 0;
            *iids = std::ptr::null_mut();
        }
        windows::Win32::Foundation::S_OK
    }
    unsafe fn GetRuntimeClassName(
        &self,
        name: *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT {
        // SAFETY: an empty HSTRING is a null pointer.
        unsafe { *name = std::ptr::null_mut() };
        windows::Win32::Foundation::S_OK
    }
    unsafe fn GetTrustLevel(&self, level: *mut i32) -> windows::core::HRESULT {
        // SAFETY: the caller's out pointer; 0 is BaseTrust.
        unsafe { *level = 0 };
        windows::Win32::Foundation::S_OK
    }
    unsafe fn ServiceRequested(
        &self,
        _h: *mut core::ffi::c_void,
        token: *mut i64,
    ) -> windows::core::HRESULT {
        // SAFETY: the caller's out pointer.
        unsafe { *token = 1 };
        windows::Win32::Foundation::S_OK
    }
    unsafe fn RemoveServiceRequested(&self, _token: i64) -> windows::core::HRESULT {
        windows::Win32::Foundation::S_OK
    }
    unsafe fn RebootNeeded(
        &self,
        _h: *mut core::ffi::c_void,
        token: *mut i64,
    ) -> windows::core::HRESULT {
        // SAFETY: the caller's out pointer.
        unsafe { *token = 2 };
        windows::Win32::Foundation::S_OK
    }
    unsafe fn RemoveRebootNeeded(&self, _token: i64) -> windows::core::HRESULT {
        windows::Win32::Foundation::S_OK
    }
    unsafe fn ComponentLoadFailed(
        &self,
        _h: *mut core::ffi::c_void,
        token: *mut i64,
    ) -> windows::core::HRESULT {
        // SAFETY: the caller's out pointer.
        unsafe { *token = 3 };
        windows::Win32::Foundation::S_OK
    }
    unsafe fn RemoveComponentLoadFailed(&self, _token: i64) -> windows::core::HRESULT {
        windows::Win32::Foundation::S_OK
    }
    unsafe fn Properties(&self, value: *mut *mut core::ffi::c_void) -> windows::core::HRESULT {
        match self
            .properties
            .cast::<windows::Foundation::Collections::IPropertySet>()
        {
            Ok(set) => {
                // SAFETY: the caller's out pointer receives our reference.
                unsafe { *value = set.into_raw() };
                windows::Win32::Foundation::S_OK
            }
            Err(e) => e.code(),
        }
    }
}

/// The protection manager for a module: its protected media path server published where the
/// engine looks for it.
pub fn protection_manager(cdm: &IMFContentDecryptionModule) -> Result<IMFContentProtectionManager> {
    let services: IMFGetService = cdm.cast()?;
    // SAFETY: GetService with the runtime class's default interface id.
    let server: windows::Media::Protection::MediaProtectionPMPServer =
        unsafe { services.GetService(&MF_CONTENTDECRYPTIONMODULE_SERVICE) }?;
    let properties = windows::Foundation::Collections::PropertySet::new()?;
    properties.Insert(
        &HSTRING::from("Windows.Media.Protection.MediaProtectionPMPServer"),
        &server.cast::<windows::core::IInspectable>()?,
    )?;
    Ok(ProtectionManager {
        cdm: cdm.clone(),
        properties,
    }
    .into())
}

impl IMFContentProtectionManager_Impl for ProtectionManager_Impl {
    fn BeginEnableContent(
        &self,
        activate: Ref<IMFActivate>,
        _topology: Ref<IMFTopology>,
        callback: Ref<IMFAsyncCallback>,
        state: Ref<windows::core::IUnknown>,
    ) -> Result<()> {
        // SAFETY: COM calls on live objects the engine passed in.
        unsafe {
            let result = MFCreateAsyncResult(None, callback.as_ref(), state.as_ref())?;
            let activate = activate.ok()?;
            let object: windows::core::IUnknown = activate.ActivateObject()?;
            let enabler: IMFContentEnabler = object.cast()?;
            if let Ok(kind) = enabler.GetEnableType() {
                trace(&format!("enable content {kind:?}"));
            }
            self.cdm.SetContentEnabler(&enabler, &result)
        }
    }

    fn EndEnableContent(&self, result: Ref<IMFAsyncResult>) -> Result<()> {
        if let Some(r) = result.as_ref() {
            // SAFETY: reading the status of a live result.
            trace(&format!("content enabled: {:?}", unsafe { r.GetStatus() }));
        }
        Ok(())
    }
}

pub(crate) fn trace_error(what: &str, e: &windows::core::Error) {
    trace(&format!("{what}: {e}"));
}

pub(crate) fn trace(what: &str) {
    if std::env::var_os("SNOWLARK_TRACE").is_some() {
        eprintln!("[playready] {what}");
    }
}

/// Media engine events: the event and its two parameters.
#[implement(IMFMediaEngineNotify)]
struct Notify {
    tx: std::sync::Mutex<Sender<(u32, usize, u32)>>,
}

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, event: u32, param1: usize, param2: u32) -> Result<()> {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send((event, param1, param2));
        }
        Ok(())
    }
}

/// A media engine for protected content: on a video device of its own, its frames shown only through
/// the composition surface it makes in windowless swap chain mode, its content enabled by `cdm`.
pub fn protected_engine(
    cdm: &IMFContentDecryptionModule,
    events: Sender<(u32, usize, u32)>,
    extension: Option<&IMFMediaEngineExtension>,
    opm_window: Option<isize>,
) -> Result<IMFMediaEngineEx> {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    super::ensure_com();
    // SAFETY: device, device manager and engine creation with out parameters owned by wrappers.
    unsafe {
        let mut device = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.ok_or_else(|| windows::core::Error::from(MF_E_NOT_AVAILABLE))?;
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or_else(|| windows::core::Error::from(MF_E_NOT_AVAILABLE))?;
        manager.ResetDevice(&device, token)?;
        let notify: IMFMediaEngineNotify = Notify {
            tx: std::sync::Mutex::new(events),
        }
        .into();
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 4)?;
        let attrs = attrs.ok_or_else(|| windows::core::Error::from(MF_E_NOT_AVAILABLE))?;
        attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
        attrs.SetUINT32(
            &MF_MEDIA_ENGINE_CONTENT_PROTECTION_FLAGS,
            MF_MEDIA_ENGINE_ENABLE_PROTECTED_CONTENT.0 as u32,
        )?;
        attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
        if let Some(e) = extension {
            attrs.SetUnknown(&MF_MEDIA_ENGINE_EXTENSION, e)?;
        }
        if let Some(w) = opm_window {
            attrs.SetUINT64(&MF_MEDIA_ENGINE_OPM_HWND, w as u64)?;
        }
        let class: IMFMediaEngineClassFactory =
            CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
        let engine = class.CreateInstance(0, &attrs)?;
        let pm = protection_manager(cdm)?;
        engine
            .cast::<IMFMediaEngineProtectedContent>()?
            .SetContentProtectionManager(&pm)?;
        let ex: IMFMediaEngineEx = engine.cast()?;
        ex.EnableWindowlessSwapchainMode(true)?;
        Ok(ex)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Microsoft's public PlayReady test content and licence server.
    const TEST_MP4: &str = "https://test.playready.microsoft.com/media/profficialsite/tearsofsteel_1080p_60s_24fps.6000kbps.1920x1080.h264-8b.2ch.128kbps.aac.avsep.cenc.mp4";
    pub(crate) const TEST_SERVER: &str =
        "https://test.playready.microsoft.com/service/rightsmanager.asmx?cfg=(persist:false,sl:150)";

    pub(crate) fn curl(args: &[&str]) -> Vec<u8> {
        let out = std::process::Command::new("curl.exe")
            .args(["-s", "--fail"])
            .args(args)
            .output()
            .expect("curl.exe runs");
        assert!(out.status.success(), "curl {args:?}: {:?}", out.status);
        out.stdout
    }

    /// The first `pssh` box of the test file's `moov`.
    pub(crate) fn pssh_of_test_file() -> Vec<u8> {
        let head = curl(&["-r", "0-4095", TEST_MP4]);
        let mut at = 0;
        let mut out = Vec::new();
        // ftyp, then moov's children.
        while at + 8 <= head.len() {
            let size = u32::from_be_bytes(head[at..at + 4].try_into().unwrap()) as usize;
            let kind = &head[at + 4..at + 8];
            if kind == b"moov" {
                at += 8;
                continue;
            }
            if kind == b"pssh" {
                out.extend_from_slice(&head[at..at + size]);
                break;
            }
            at += size.max(8);
        }
        out
    }

    /// The challenge inside the module's message: a UTF-16 `PlayReadyKeyMessage` envelope with the
    /// challenge in base64 (and the headers to send), or the bare challenge.
    pub(crate) fn challenge_of(msg: &[u8]) -> Vec<u8> {
        let text: String = if msg.len() > 2 && msg[1] == 0 {
            String::from_utf16_lossy(
                &msg.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect::<Vec<_>>(),
            )
        } else {
            String::from_utf8_lossy(msg).into_owned()
        };
        match (text.find("<Challenge"), text.find("</Challenge>")) {
            (Some(a), Some(b)) => {
                let inner = &text[a..b];
                let start = inner.find('>').unwrap() + 1;
                let b64 = inner[start..].trim();
                use windows::Security::Cryptography::CryptographicBuffer;
                let buf = CryptographicBuffer::DecodeFromBase64String(&HSTRING::from(b64)).unwrap();
                let mut bytes = windows_core::Array::<u8>::new();
                CryptographicBuffer::CopyToByteArray(&buf, &mut bytes).unwrap();
                bytes.to_vec()
            }
            _ => msg.to_vec(),
        }
    }

    /// A session of `cdm` with the test file's licence from the test server, its keys usable.
    fn licensed(cdm: &IMFContentDecryptionModule) -> IMFContentDecryptionModuleSession {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = session(cdm, tx).expect("session");
        let init = pssh_of_test_file();
        // SAFETY: test code calling the session with live data.
        unsafe { s.GenerateRequest(&HSTRING::from("cenc"), &init) }.expect("generateRequest");
        let ev = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("a message");
        let SessionEvent::Message(kind, msg) = ev else {
            panic!("{ev:?}")
        };
        eprintln!("message {kind} {} bytes", msg.len());
        let dir = std::env::temp_dir().join("snowlark-playready-test-msg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("challenge.xml"), challenge_of(&msg)).unwrap();
        let body = format!("@{}", dir.join("challenge.xml").display());
        let licence = curl(&[
            "-X",
            "POST",
            "-H",
            "Content-Type: text/xml; charset=utf-8",
            "-H",
            "SOAPAction: \"http://schemas.microsoft.com/DRM/2007/03/protocols/AcquireLicense\"",
            "--data-binary",
            &body,
            TEST_SERVER,
        ]);
        eprintln!("licence {} bytes", licence.len());
        // SAFETY: test code calling the session with live data.
        unsafe { s.Update(&licence) }.expect("update");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(SessionEvent::KeysChanged) =
                rx.recv_timeout(std::time::Duration::from_secs(1))
            {
                let statuses = key_statuses(&s).unwrap();
                eprintln!("keys {statuses:02x?}");
                if statuses.iter().any(|k| k.1 == "usable") {
                    return s;
                }
            }
        }
        panic!("no usable key");
    }

    /// What the system's hardware PlayReady answers for the content types players ask about (the
    /// answer depends on the machine's decoders; run by hand).
    #[test]
    #[ignore]
    fn types_the_system_module_plays() {
        for t in [
            "video/mp4; codecs=\"avc1.640028\"",
            "video/mp4;codecs=\"avc1.4d401e\"",
            "video/mp4",
            "audio/mp4; codecs=\"mp4a.40.2\"",
            "audio/mp4;codecs=\"mp4a.40.5\"",
            "audio/mp4; codecs=\"ec-3\"",
            "audio/mp4",
            "video/mp4; codecs=\"hev1.2.6.L153.B0\"",
            "video/mp4; codecs=\"vp09.00.10.08\"",
            "video/mp4; codecs=\"av01.0.08M.08\"",
            "video/mp4; codecs=\"avc1.640028\"; features=\"hdcp=1\"",
            "video/mp4; codecs=\"avc1.640028,mp4a.40.2\"",
        ] {
            println!("{t}: {}", type_supported(RECOMMENDATION_3000, t));
        }
    }

    /// A licence round trip with the system's PlayReady against Microsoft's public test server
    /// (network; run by hand).
    #[test]
    #[ignore]
    fn licence_round_trip() {
        super::super::ensure_com();
        for ks in [RECOMMENDATION, RECOMMENDATION_3000] {
            eprintln!(
                "{ks}: avc1 probably={} hevc probably={}",
                type_supported(ks, "video/mp4;codecs=\"avc1.640028\""),
                type_supported(ks, "video/mp4;codecs=\"hvc1.1.6.L120.90\"")
            );
        }
        let store = std::env::temp_dir().join("snowlark-playready-test");
        let cdm = create(RECOMMENDATION, false, "test-origin", &store).expect("module");
        let s = licensed(&cdm);
        // SAFETY: test code.
        eprintln!("expiration {:?}", unsafe { s.GetExpiration() });
        // SAFETY: test code.
        unsafe { s.Close() }.unwrap();
    }

    /// Microsoft's encrypted test file (`SNOWLARK_PLAYREADY_MP4`: a local copy of [`TEST_MP4`])
    /// played at the hardware security level by a protected media engine in windowless swap chain
    /// mode, its composition surface shown in a window off every monitor that never takes the
    /// foreground. Protected frames cannot be read back, so the engine's own count of rendered
    /// frames is the check (network and hardware DRM; run by hand).
    #[test]
    #[ignore]
    fn protected_playback() {
        use windows::Win32::Foundation::*;
        use windows::Win32::Graphics::DirectComposition::*;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;
        super::super::ensure_com();
        let Some(path) = std::env::var_os("SNOWLARK_PLAYREADY_MP4") else {
            eprintln!("SNOWLARK_PLAYREADY_MP4 is not set: skipped");
            return;
        };
        let store = std::env::temp_dir().join("snowlark-playready-test");
        let cdm = create(RECOMMENDATION_3000, true, "test-origin", &store).expect("module");
        let _session = licensed(&cdm);
        let (tx, rx) = std::sync::mpsc::channel();
        let (src, _feed) = demux_into_source(&path.to_string_lossy(), &cdm);
        let extension = super::super::protected_source::extension(src);
        let (w, h) = (960i32, 540i32);
        // SAFETY: test code: a plain window, DirectComposition and GDI calls with live handles.
        unsafe {
            unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
                // SAFETY: the default handling of a message for our own window.
                unsafe { DefWindowProcW(h, m, w, l) }
            }
            let class = windows::core::w!("SnowlarkPlayReadyTest");
            let instance = GetModuleHandleW(None).unwrap();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc_),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
                class,
                windows::core::w!("playready"),
                WS_POPUP,
                -4000,
                -4000,
                w,
                h,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let engine = protected_engine(&cdm, tx, Some(&extension), Some(hwnd.0 as isize))
                .expect("engine");
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(None).unwrap();
            let target = dcomp.CreateTargetForHwnd(hwnd, true).unwrap();
            let visual = dcomp.CreateVisual().unwrap();
            target.SetRoot(&visual).unwrap();
            engine.SetMuted(true).unwrap();
            engine
                .SetSource(&BSTR::from(super::super::protected_source::URL))
                .unwrap();
            engine.Play().unwrap();
            let start = std::time::Instant::now();
            let mut surface_set = false;
            let mut last_print = 0.0;
            while start.elapsed() < std::time::Duration::from_secs(12) {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = DispatchMessageW(&msg);
                }
                while let Ok((e, p1, p2)) = rx.try_recv() {
                    if e != 7 && e != 8 {
                        eprintln!("event {e} {p1:#x} {p2:#x}");
                    }
                    if e == 5 {
                        let err = engine
                            .GetError()
                            .map(|e| (e.GetErrorCode(), e.GetExtendedErrorCode()));
                        eprintln!("error {err:?}");
                    }
                }
                if !surface_set {
                    if let Ok(handle) = engine.GetVideoSwapchainHandle() {
                        if !handle.is_invalid() && handle.0 as usize != 0 {
                            eprintln!("surface handle {:?} at {:?}", handle, start.elapsed());
                            let surface = dcomp.CreateSurfaceFromHandle(handle).unwrap();
                            visual.SetContent(&surface).unwrap();
                            dcomp.Commit().unwrap();
                            let rect = RECT {
                                left: 0,
                                top: 0,
                                right: w,
                                bottom: h,
                            };
                            engine.UpdateVideoStream(None, Some(&rect), None).unwrap();
                            surface_set = true;
                        }
                    }
                }
                let t = engine.GetCurrentTime();
                if t - last_print >= 1.0 {
                    eprintln!("time {t:.2}");
                    last_print = t;
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            // Frames the engine's renderer presented: protected frames cannot be read back.
            let stat = |id| {
                engine
                    .GetStatistics(id)
                    .map(|v| v.Anonymous.Anonymous.Anonymous.ulVal)
                    .unwrap_or(0)
            };
            let rendered = stat(MF_MEDIA_ENGINE_STATISTIC_FRAMES_RENDERED);
            let dropped = stat(MF_MEDIA_ENGINE_STATISTIC_FRAMES_DROPPED);
            eprintln!(
                "frames rendered {rendered} dropped {dropped} time {:.2}",
                engine.GetCurrentTime()
            );
            let _ = engine.Shutdown();
            let _ = DestroyWindow(hwnd);
            assert!(surface_set, "no composition surface");
            assert!(rendered > 100, "{rendered} frames");
        }
    }

    /// The H.264 and AAC tracks of a fragmented, encrypted MP4 file into a source of ours, read
    /// by [`crate::fmp4::SampleReader`] in pieces as a `SourceBuffer` would get them.
    fn demux_into_source(
        path: &str,
        cdm: &IMFContentDecryptionModule,
    ) -> (IMFMediaSource, super::super::protected_source::Feed) {
        use super::super::protected_source::*;
        use crate::fmp4::{Output, SampleReader};
        let bytes = std::fs::read(path).unwrap();
        let mut reader = SampleReader::new();
        let mut out = Vec::new();
        for piece in bytes.chunks(1 << 20) {
            out.extend(reader.push(piece));
        }
        // Streams: each track with a codec Media Foundation is given, in track order.
        let tracks: Vec<_> = reader
            .tracks()
            .iter()
            .filter_map(|t| {
                media_type(t).map(|m| (t.id, t.video, m.unwrap(), t.encryption.is_some()))
            })
            .collect();
        for (id, video, _, protected) in &tracks {
            eprintln!("track {id} video {video} protected {protected}");
        }
        let trust = std::sync::Arc::new(Trust::new(cdm.clone()));
        let specs = tracks
            .iter()
            .map(|(_, _, media_type, protected)| StreamSpec {
                media_type: media_type.clone(),
                protected: *protected,
            })
            .collect();
        let (source, feed) = create(specs, trust).unwrap();
        let mut counts = vec![0; tracks.len()];
        for o in out {
            let Output::Sample(id, sample) = o else {
                continue;
            };
            if let Some(i) = tracks.iter().position(|t| t.0 == id) {
                counts[i] += 1;
                feed.push(i, sample);
            }
        }
        for i in 0..tracks.len() {
            feed.end(i);
        }
        eprintln!("samples queued per stream: {counts:?}");
        (source, feed)
    }
}
