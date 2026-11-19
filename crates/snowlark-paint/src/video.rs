// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Video frames on the GPU: each playing `<video>` gets a [`VideoSlot`] in the window's (or a
//! headless run's) [`VideoStore`]. The media player copies each decoded frame into one of the slot's
//! three textures on the shared [`Gpu`] device ([`VideoSlot::begin_frame`], [`VideoSlot::end_frame`]);
//! the compositor draws the slot's front frame wherever the page's display list has an
//! [`crate::Item::Video`] for it ([`crate::ContentPass::video`]), at the display's rate, without the
//! page painting anything.
//!
//! Three textures in turn, and a frame shown one frame late: the media engine's copy into a texture
//! runs on its own device and nothing orders it with ours, so a texture is shown only once the next
//! frame's copy started (the copy into it has had a frame's time to finish), and the texture being
//! written is never the one being drawn. A frame with no successor (a paused video, the last frame)
//! is shown once it is [`STALE_AFTER`] old ([`VideoSlot::promote_stale`]). This is what the C#
//! engine learned: showing frames as soon as the copy call returned showed
//! half-written frames ("scan lines") during motion.
//!
//! **A video in a renderer process** (`docs/process-isolation.md` §8, "Video"). The player and its
//! slot stay in the renderer, on a device of its own ([`VideoStore::exporting`]); the compositor
//! draws in the browser. Chromium's frame-server design, with the browser in the GPU process's
//! place: the textures a video's frames reach the compositor through are made by the device that
//! draws them, with shared NT handles and keyed mutexes (`D3DImageBackingFactory::CreateSharedImage`,
//! `gpu/command_buffer/service/shared_image/d3d_image_backing_factory.cc`, for a renderer's
//! `SharedImageStub::OnCreateSharedImage`), a pool of [`POOL_TEXTURES`] per video and size
//! (`MediaFoundationTexturePool::Initialize`, `media/renderers/win/media_foundation_texture_pool.cc`),
//! made again when the size changes. The renderer opens them; each frame it shows is copied into a
//! free one under its keyed mutex and named to the browser by its index
//! (`MediaFoundationRenderer::RequestNextFrame` then `OnFrameAvailable(token)`). A texture with no
//! free key drops the frame, as `AcquireTexture` finding none does.
//!
//! The keys say whose turn it is, as P0 measured (`docs/process-isolation-p0.md`, "Video"): the
//! renderer takes a texture with key 0 and gives it up with key 1; the browser's compositor takes the
//! video's newest frame with key 1 and gives up the one it showed before with key 0
//! ([`VideoStore::latch`]). So a frame the browser has not taken yet is never written over, the one
//! it shows is never written, and the browser holds the key of every texture it draws. Chromium's
//! release message (`NotifyFrameReleased`) is the key itself here. Unlike Chromium's GPU process,
//! which waits `INFINITE` for a key (`DXGISharedHandleState::AcquireKeyedMutex`), the browser never
//! waits: a renderer holds those textures too, and one that keeps a key only loses its own frames.
//! The renderer's frames are copied from its slot's textures (P0's "private texture" route:
//! `CopyFrameToVideoSurface` into a texture with the key held fails), so the one-frame-late rule
//! above still holds there.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{Interface, Result, PCWSTR};
use windows::Win32::Foundation::{E_INVALIDARG, HANDLE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use crate::display_list::VideoId;
use crate::gpu::Gpu;
use crate::image::Frame;

/// How many textures a slot cycles through.
pub const FRAME_TEXTURES: usize = 3;

/// How old the newest copied frame must be to show without a successor.
pub const STALE_AFTER: Duration = Duration::from_millis(40);

/// Called when a slot has a new frame to show (from the media engine's threads).
pub type FrameNotify = Arc<dyn Fn(VideoId) + Send + Sync>;

/// How many shared textures a video of a renderer process has per size (Chromium's
/// `kTexturePoolCount`, `media_foundation_texture_pool.cc`).
pub const POOL_TEXTURES: usize = 4;

/// The largest frame side a renderer's video gets shared textures for (Direct3D 11's texture limit).
pub const MAX_SHARED_SIDE: u32 = 16384;

/// The shared textures the browser made for a renderer's video, as the renderer receives them.
pub struct PoolHandles {
    /// Counts the video's pools: a frame names the pool it is in.
    pub generation: u32,
    /// [`POOL_TEXTURES`] NT handles, opened by the renderer's device.
    pub handles: Vec<OwnedHandle>,
}

/// Where a renderer process's video frames go: the browser, which makes the textures they are
/// copied into and draws them (see the module documentation). Its calls come from the media
/// engine's threads.
pub trait VideoExport: Send + Sync {
    /// Which device to make for the videos: the one the browser draws with (shared textures open
    /// only on the same adapter). `None`: no device, no frames.
    fn driver(&self) -> Option<crate::Driver>;
    /// New shared textures for `width` × `height` frames of video `id` (the previous ones are let
    /// go). `None` when the browser refuses.
    fn pool(&self, id: VideoId, width: u32, height: u32) -> Option<PoolHandles>;
    /// Video `id` has a new frame to show, in texture `index` of pool `generation`.
    fn frame(&self, id: VideoId, generation: u32, index: u32);
    /// Video `id`, which had a pool, is gone.
    fn removed(&self, id: VideoId);
}

/// The video slots of a window (or of a headless run): see the module documentation.
pub struct VideoStore {
    /// Set at the start, or for a renderer's store at the first use ([`VideoExport::driver`]).
    gpu: OnceLock<Option<Arc<Gpu>>>,
    slots: Mutex<HashMap<u32, Arc<VideoSlot>>>,
    /// A renderer process's videos, in the browser: their shared textures and the frame shown.
    imported: Mutex<HashMap<u32, Imported>>,
    /// In a renderer process: where the frames go.
    export: Option<Arc<dyn VideoExport>>,
    /// In a renderer process whose browser came to draw with another kind of device than the
    /// slots' (a GPU process that moved to WARP after its crash limit): a device of the browser's
    /// kind, which opens the shared textures (they open only on the adapter that made them). The
    /// slots and the players keep theirs; frames cross through memory ([`copy_through_memory`]).
    bridge: Mutex<Option<Arc<Gpu>>>,
    next: AtomicU32,
    notify: Mutex<Option<FrameNotify>>,
}

/// Shared textures of one size, made by the browser's device (see the module documentation).
struct SharedPool {
    generation: u32,
    width: u32,
    height: u32,
    keyed: Vec<IDXGIKeyedMutex>,
    bitmaps: Vec<ID2D1Bitmap1>,
}

// SAFETY: resources of the shared device (see `VideoSlot`); a pool's keyed mutexes are used only by
// the compositor's thread ([`VideoStore::latch`]) and its bitmaps only drawn.
unsafe impl Send for SharedPool {}
// SAFETY: as above.
unsafe impl Sync for SharedPool {}

/// A renderer's video, in the browser.
#[derive(Default)]
struct Imported {
    generations: u32,
    pool: Option<Arc<SharedPool>>,
    /// The newest frame named by the renderer, not taken yet.
    pending: Option<(Arc<SharedPool>, usize)>,
    /// The frame drawn, whose key the browser holds.
    front: Option<(Arc<SharedPool>, usize)>,
}

/// The browser's textures for a renderer's video, opened by the renderer's device.
struct ExportPool {
    generation: u32,
    width: u32,
    height: u32,
    textures: Vec<ID3D11Texture2D>,
    keyed: Vec<IDXGIKeyedMutex>,
    next: usize,
    /// The device that opened them when it is not the slots' ([`VideoStore::repool`]).
    bridge: Option<Arc<Gpu>>,
    /// With a bridge: the slots' device's texture a frame is read back through.
    staging: Option<ID3D11Texture2D>,
}

/// `AcquireSync` with `key`, waiting at most `ms`: false on a timeout (a success code, which the
/// binding's own wrapper reports as `Ok`).
fn acquire_sync(k: &IDXGIKeyedMutex, key: u64, ms: u32) -> Result<bool> {
    // SAFETY: a call through the interface's own vtable with its own pointer.
    let hr = unsafe { (Interface::vtable(k).AcquireSync)(Interface::as_raw(k), key, ms) };
    hr.ok()?;
    Ok(hr.0 == 0)
}

fn trace(what: &str, e: impl std::fmt::Display) {
    if std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1") {
        eprintln!("[media] {what}: {e}");
    }
}

impl std::fmt::Debug for VideoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoStore").finish_non_exhaustive()
    }
}

impl VideoStore {
    /// A store whose slots live on `gpu` (without one, videos have no GPU frames: players copy
    /// frames to memory instead).
    pub fn new(gpu: Option<Arc<Gpu>>) -> Arc<VideoStore> {
        VideoStore::make(OnceLock::from(gpu), None)
    }

    /// A renderer process's store: its slots' frames go to the browser through `export`, and its
    /// device is made at the first use, on the browser's adapter (see the module documentation).
    pub fn exporting(export: Arc<dyn VideoExport>) -> Arc<VideoStore> {
        VideoStore::make(OnceLock::new(), Some(export))
    }

    fn make(
        gpu: OnceLock<Option<Arc<Gpu>>>,
        export: Option<Arc<dyn VideoExport>>,
    ) -> Arc<VideoStore> {
        Arc::new(VideoStore {
            gpu,
            slots: Mutex::new(HashMap::new()),
            imported: Mutex::new(HashMap::new()),
            export,
            bridge: Mutex::new(None),
            next: AtomicU32::new(1),
            notify: Mutex::new(None),
        })
    }

    pub fn gpu(&self) -> Option<&Arc<Gpu>> {
        self.gpu
            .get_or_init(|| {
                let driver = self.export.as_ref()?.driver()?;
                Gpu::with_driver(driver)
                    .inspect_err(|e| trace("no device for videos", e))
                    .ok()
            })
            .as_ref()
    }

    /// A new, empty slot.
    pub fn create(&self) -> Arc<VideoSlot> {
        let id = VideoId(self.next.fetch_add(1, Ordering::Relaxed));
        let slot = Arc::new(VideoSlot {
            id,
            inner: Mutex::new(SlotInner::default()),
            frames: AtomicU64::new(0),
            export: Mutex::new(None),
        });
        self.slots.lock().unwrap().insert(id.0, slot.clone());
        slot
    }

    /// Lets go of a slot, or of a renderer's video (its textures go when the last holder drops
    /// them).
    pub fn remove(&self, id: VideoId) {
        let slot = self.slots.lock().unwrap().remove(&id.0);
        if let (Some(slot), Some(export)) = (slot, &self.export) {
            if slot.export.lock().unwrap().take().is_some() {
                export.removed(id);
            }
        }
        self.imported.lock().unwrap().remove(&id.0);
    }

    pub fn get(&self, id: VideoId) -> Option<Arc<VideoSlot>> {
        self.slots.lock().unwrap().get(&id.0).cloned()
    }

    /// How many slots there are.
    pub fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Calls `notify` whenever a slot has a new frame to show (the compositor redraws).
    pub fn set_notify(&self, notify: Option<FrameNotify>) {
        *self.notify.lock().unwrap() = notify;
    }

    /// Tells the store's listener that `id` has a new frame. In a renderer process the slot's front
    /// frame goes to the browser first ([`VideoStore::exporting`]).
    pub fn frame_ready(&self, id: VideoId) {
        if let Some(export) = &self.export {
            if let Some(slot) = self.get(id) {
                self.export_front(export.as_ref(), &slot);
            }
        }
        let n = self.notify.lock().unwrap().clone();
        if let Some(n) = n {
            n(id);
        }
    }

    /// A renderer process's store whose shared textures are gone with the process that made them
    /// (the GPU process was started again): each video asks for new ones, and its newest frame
    /// goes to the browser again, so a paused video shows too. Nothing for another store.
    ///
    /// The new process may draw with another kind of device (WARP once the graphics card's crash
    /// limit is reached): the store asks again, and opens the new textures on a device of that kind
    /// when it is not the slots' (its `bridge`). True when there was any video to repool.
    pub fn repool(&self) -> bool {
        let Some(export) = &self.export else {
            return false;
        };
        // (a store that never had a video has no device, and makes none here)
        if let Some(Some(gpu)) = self.gpu.get() {
            let want = export.driver();
            let mut bridge = self.bridge.lock().unwrap();
            match want {
                Some(d) if d != gpu.driver() => {
                    if bridge
                        .as_ref()
                        .is_none_or(|b| b.driver() != d || b.is_lost())
                    {
                        *bridge = Gpu::with_driver(d)
                            .inspect_err(|e| trace("no device for the browser's video textures", e))
                            .ok();
                    }
                }
                _ => *bridge = None,
            }
        }
        let slots: Vec<Arc<VideoSlot>> = self.slots.lock().unwrap().values().cloned().collect();
        for slot in &slots {
            *slot.export.lock().unwrap() = None;
            self.export_front(export.as_ref(), slot);
        }
        !slots.is_empty()
    }

    /// Copies `slot`'s front frame into a free shared texture and names it to the browser.
    fn export_front(&self, export: &dyn VideoExport, slot: &VideoSlot) {
        let Some(gpu) = self.gpu() else { return };
        let Some((width, height)) = slot.size() else {
            return;
        };
        // (one export of a slot at a time: the media engine's thread and the player's both show
        // frames, and a frame names its pool)
        let mut export_pool = slot.export.lock().unwrap();
        if export_pool
            .as_ref()
            .is_none_or(|p| (p.width, p.height) != (width, height))
        {
            *export_pool = None;
            let Some(handles) = export.pool(slot.id, width, height) else {
                return;
            };
            let bridge = self.bridge.lock().unwrap().clone();
            match open_pool(gpu, bridge, handles, width, height) {
                Ok(p) => *export_pool = Some(p),
                Err(e) => {
                    trace("the browser's video textures do not open", e);
                    return;
                }
            }
        }
        let Some(pool) = export_pool.as_mut() else {
            return;
        };
        let copied = (|| -> Result<Option<usize>> {
            let inner = slot.inner.lock().unwrap();
            let (Some(buffers), Some(front)) = (inner.buffers.as_ref(), inner.front) else {
                return Ok(None);
            };
            if (buffers.width, buffers.height) != (pool.width, pool.height) {
                return Ok(None);
            }
            // (always the slots' device first, then the bridge: nothing takes them the other way)
            let _lock = gpu.lock();
            let _bridge_lock = pool.bridge.as_ref().map(|b| b.lock());
            // (a texture whose key is 1 holds a frame the browser has not taken, or shows)
            let mut free = None;
            for k in 0..pool.keyed.len() {
                let j = (pool.next + k) % pool.keyed.len();
                if acquire_sync(&pool.keyed[j], 0, 0)? {
                    free = Some(j);
                    break;
                }
            }
            let Some(j) = free else {
                trace("no free texture for a frame", pool.generation);
                return Ok(None);
            };
            let copy = match &pool.bridge {
                // SAFETY: a copy between two textures of this device of the same size and format
                // (the pool's was checked when it opened), under Direct2D's device lock, with the
                // shared texture's key held.
                None => unsafe {
                    gpu.d3d_device()
                        .GetImmediateContext()
                        .map(|ctx| ctx.CopyResource(&pool.textures[j], &buffers.textures[front]))
                },
                Some(bridge) => copy_through_memory(
                    gpu,
                    bridge,
                    &mut pool.staging,
                    &buffers.textures[front],
                    &pool.textures[j],
                    pool.width,
                    pool.height,
                ),
            };
            // SAFETY: the key was taken above; it goes to the browser with the frame, or back to
            // the renderer's turn when the copy failed.
            unsafe { pool.keyed[j].ReleaseSync(if copy.is_ok() { 1 } else { 0 })? };
            copy?;
            pool.next = j + 1;
            Ok(Some(j))
        })();
        match copied {
            Ok(Some(j)) => export.frame(slot.id, pool.generation, j as u32),
            Ok(None) => {}
            Err(e) => trace("a frame does not go to the browser", e),
        }
    }

    /// A renderer's video, in the browser: an id among this store's for the compositor, with no
    /// textures yet ([`VideoStore::make_pool`]).
    pub fn import(&self) -> VideoId {
        let id = VideoId(self.next.fetch_add(1, Ordering::Relaxed));
        self.imported
            .lock()
            .unwrap()
            .insert(id.0, Imported::default());
        id
    }

    /// New shared textures for `width` × `height` frames of the imported video `id`, made by this
    /// store's device, and an NT handle to each for the renderer. The frame drawn stays until a
    /// frame of the new ones replaces it.
    pub fn make_pool(&self, id: VideoId, width: u32, height: u32) -> Result<PoolHandles> {
        let invalid = || windows::core::Error::from(E_INVALIDARG);
        if width == 0 || height == 0 || width > MAX_SHARED_SIDE || height > MAX_SHARED_SIDE {
            return Err(invalid());
        }
        let gpu = self.gpu().ok_or_else(invalid)?;
        let (pool, handles) = make_shared(gpu, width, height)?;
        let mut imported = self.imported.lock().unwrap();
        let video = imported.get_mut(&id.0).ok_or_else(invalid)?;
        video.generations += 1;
        let generation = video.generations;
        video.pool = Some(Arc::new(SharedPool { generation, ..pool }));
        video.pending = None;
        Ok(PoolHandles {
            generation,
            handles,
        })
    }

    /// The renderer named texture `index` of pool `generation` as imported video `id`'s newest
    /// frame. False when that is no such texture (the renderer broke the protocol). The compositor
    /// takes it at [`VideoStore::latch`], which the store's listener is told to call.
    pub fn imported_frame(&self, id: VideoId, generation: u32, index: u32) -> bool {
        {
            let mut imported = self.imported.lock().unwrap();
            let Some(video) = imported.get_mut(&id.0) else {
                return false;
            };
            let Some(pool) = video
                .pool
                .clone()
                .filter(|p| p.generation == generation && (index as usize) < p.keyed.len())
            else {
                return false;
            };
            let skipped = video.pending.replace((pool, index as usize));
            // A frame the compositor never took goes back to the renderer: nothing draws it, and
            // with its key at 1 the renderer could not use its texture again.
            if let (Some((p, i)), Some(gpu)) = (skipped, self.gpu()) {
                let _lock = gpu.lock();
                if acquire_sync(&p.keyed[i], 1, 0).unwrap_or(false) {
                    // SAFETY: the key was taken just above.
                    let _ = unsafe { p.keyed[i].ReleaseSync(0) };
                }
            }
        }
        let n = self.notify.lock().unwrap().clone();
        if let Some(n) = n {
            n(id);
        }
        true
    }

    /// The compositor's thread, between frames: imported video `id`'s newest frame becomes the one
    /// drawn, if its key can be had at once; the one drawn before goes back to the renderer.
    /// Nothing for a slot of this process.
    pub fn latch(&self, id: VideoId) {
        let Some(gpu) = self.gpu() else { return };
        let mut imported = self.imported.lock().unwrap();
        let Some(video) = imported.get_mut(&id.0) else {
            return;
        };
        let Some((pool, index)) = video.pending.take() else {
            return;
        };
        let _lock = gpu.lock();
        match acquire_sync(&pool.keyed[index], 1, 0) {
            Ok(true) => {}
            Ok(false) => {
                trace("a renderer's frame is not given up yet", index);
                return;
            }
            Err(e) => {
                trace("a renderer's frame cannot be taken", e);
                return;
            }
        }
        if let Some((old, i)) = video.front.replace((pool, index)) {
            // SAFETY: the browser holds key 1 of the frame it drew (taken above, at an earlier
            // latch); no draw of it is under way, since draws run on this thread.
            let _ = unsafe { old.keyed[i].ReleaseSync(0) };
        }
    }

    /// The frame to draw for `id` (a slot's or a renderer's video): its bitmap and size.
    pub fn front_of(&self, id: VideoId) -> Option<(ID2D1Bitmap1, u32, u32)> {
        if let Some(slot) = self.get(id) {
            return slot.front();
        }
        let imported = self.imported.lock().unwrap();
        let (pool, i) = imported.get(&id.0)?.front.as_ref()?;
        Some((pool.bitmaps[*i].clone(), pool.width, pool.height))
    }
}

/// The shared textures of a new pool (generation 0 until it is installed) and an NT handle to each.
fn make_shared(gpu: &Gpu, width: u32, height: u32) -> Result<(SharedPool, Vec<OwnedHandle>)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        // (Chromium's pool: `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | ..._SHARED_KEYEDMUTEX`)
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
            as u32,
    };
    let mut keyed = Vec::with_capacity(POOL_TEXTURES);
    let mut bitmaps = Vec::with_capacity(POOL_TEXTURES);
    let mut handles = Vec::with_capacity(POOL_TEXTURES);
    // SAFETY: texture creation with a valid description on the shared device; each bitmap wraps its
    // texture's surface and keeps it alive; each NT handle is new, owned and closed with its
    // `OwnedHandle`.
    unsafe {
        let ctx = gpu
            .d2d_device()
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        for _ in 0..POOL_TEXTURES {
            let mut texture = None;
            gpu.d3d_device()
                .CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            let resource: IDXGIResource1 = texture.cast()?;
            let handle = resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                PCWSTR::null(),
            )?;
            handles.push(OwnedHandle::from_raw_handle(handle.0));
            let surface: IDXGISurface = texture.cast()?;
            bitmaps.push(ctx.CreateBitmapFromDxgiSurface(
                &surface,
                Some(&D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                    colorContext: ManuallyDrop::new(None),
                }),
            )?);
            keyed.push(texture.cast()?);
        }
    }
    Ok((
        SharedPool {
            generation: 0,
            width,
            height,
            keyed,
            bitmaps,
        },
        handles,
    ))
}

/// Opens the browser's textures on this process's device (`bridge`, when there is one, else the
/// slots' `gpu`), each checked to be a `width` × `height` BGRA texture with a keyed mutex.
fn open_pool(
    gpu: &Gpu,
    bridge: Option<Arc<Gpu>>,
    pool: PoolHandles,
    width: u32,
    height: u32,
) -> Result<ExportPool> {
    let invalid = || windows::core::Error::from(E_INVALIDARG);
    if pool.handles.len() != POOL_TEXTURES {
        return Err(invalid());
    }
    let device: ID3D11Device1 = bridge.as_deref().unwrap_or(gpu).d3d_device().cast()?;
    let mut textures = Vec::with_capacity(POOL_TEXTURES);
    let mut keyed = Vec::with_capacity(POOL_TEXTURES);
    for handle in &pool.handles {
        // SAFETY: the handle is an open NT handle of this process's (`OwnedHandle`), which the
        // runtime only reads; the resource holds its own reference, so the handle closes after.
        let texture: ID3D11Texture2D =
            unsafe { device.OpenSharedResource1(HANDLE(handle.as_raw_handle()))? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: an out parameter of this function's.
        unsafe { texture.GetDesc(&mut desc) };
        if (desc.Width, desc.Height, desc.Format) != (width, height, DXGI_FORMAT_B8G8R8A8_UNORM)
            || desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 == 0
        {
            return Err(invalid());
        }
        keyed.push(texture.cast()?);
        textures.push(texture);
    }
    Ok(ExportPool {
        generation: pool.generation,
        width,
        height,
        textures,
        keyed,
        next: 0,
        bridge,
        staging: None,
    })
}

/// Copies frame texture `from` of the slots' device `gpu` into shared texture `to` of the bridge's
/// device, through memory: read back into `staging` (made at the first copy), written with
/// `UpdateSubresource`. Both are `width` × `height` BGRA; the caller holds both devices' locks and
/// `to`'s key. The players' device stays, so their decoders keep working; only a frame's last copy
/// changes.
fn copy_through_memory(
    gpu: &Gpu,
    bridge: &Gpu,
    staging: &mut Option<ID3D11Texture2D>,
    from: &ID3D11Texture2D,
    to: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<()> {
    if staging.is_none() {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: texture creation with a valid description; the out pointer is a local.
        unsafe {
            gpu.d3d_device()
                .CreateTexture2D(&desc, None, Some(&mut texture))?
        };
        *staging = texture;
    }
    let staging = staging
        .as_ref()
        .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
    // SAFETY: both devices' locks are held (the caller's); `staging` and `from` are textures of
    // `gpu`'s device of the same size and format, `to` one of `bridge`'s; the mapping holds `height`
    // rows of `RowPitch` bytes, which `UpdateSubresource` reads, and is unmapped after.
    unsafe {
        let ctx = gpu.d3d_device().GetImmediateContext()?;
        ctx.CopyResource(staging, from);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let written = bridge.d3d_device().GetImmediateContext().map(|to_ctx| {
            to_ctx.UpdateSubresource(to, 0, None, mapped.pData, mapped.RowPitch, 0);
        });
        ctx.Unmap(staging, 0);
        written
    }
}

struct Buffers {
    width: u32,
    height: u32,
    textures: Vec<ID3D11Texture2D>,
    surfaces: Vec<IDXGISurface>,
    bitmaps: Vec<ID2D1Bitmap1>,
}

#[derive(Default)]
struct SlotInner {
    buffers: Option<Buffers>,
    /// The buffer generation (new buffers when the size changes).
    generation: u64,
    /// The texture the compositor draws.
    front: Option<usize>,
    /// The texture copied last and when (shown when the next copy starts, or once stale).
    copied: Option<(usize, Instant)>,
    next: usize,
}

/// One video's frames (see the module documentation).
pub struct VideoSlot {
    id: VideoId,
    inner: Mutex<SlotInner>,
    frames: AtomicU64,
    /// In a renderer process: the browser's textures its frames are copied into.
    export: Mutex<Option<ExportPool>>,
}

// SAFETY: the COM objects inside are resources of the shared D3D11/Direct2D device, which is
// free-threaded (see `gpu`), and every access to them goes through the mutex.
unsafe impl Send for VideoSlot {}
// SAFETY: as above.
unsafe impl Sync for VideoSlot {}

/// A buffer the next frame is copied into ([`VideoSlot::begin_frame`]).
pub struct FrameTarget {
    pub index: usize,
    /// Changes when the slot made new textures (a new size): surfaces cached by the writer for
    /// another generation are stale.
    pub generation: u64,
    pub surface: IDXGISurface,
}

impl VideoSlot {
    pub fn id(&self) -> VideoId {
        self.id
    }

    /// How many frames were shown so far.
    pub fn frame_count(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// The size of the textures (the video's own size), once a frame came.
    pub fn size(&self) -> Option<(u32, u32)> {
        let inner = self.inner.lock().unwrap();
        inner.buffers.as_ref().map(|b| (b.width, b.height))
    }

    /// The texture to copy the next `width` × `height` frame into (made on `gpu` when the size
    /// changed). The caller copies, then calls [`VideoSlot::end_frame`] with the index.
    pub fn begin_frame(&self, gpu: &Gpu, width: u32, height: u32) -> Result<FrameTarget> {
        if width == 0 || height == 0 {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        let mut inner = self.inner.lock().unwrap();
        let fits = inner
            .buffers
            .as_ref()
            .is_some_and(|b| b.width == width && b.height == height);
        if !fits {
            inner.buffers = Some(make_buffers(gpu, width, height)?);
            inner.generation += 1;
            inner.front = None;
            inner.copied = None;
            inner.next = 0;
        }
        // (never the texture shown, nor the one copied last: it may still be in flight)
        let mut index = inner.next % FRAME_TEXTURES;
        for _ in 0..FRAME_TEXTURES {
            let busy = inner.front == Some(index) || inner.copied.is_some_and(|(c, _)| c == index);
            if !busy {
                break;
            }
            index = (index + 1) % FRAME_TEXTURES;
        }
        inner.next = (index + 1) % FRAME_TEXTURES;
        let surface = inner.buffers.as_ref().unwrap().surfaces[index].clone();
        Ok(FrameTarget {
            index,
            generation: inner.generation,
            surface,
        })
    }

    /// The copy into `index` was issued: the frame copied before it shows now. Returns whether a
    /// new frame shows.
    pub fn end_frame(&self, index: usize) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let previous = inner.copied.replace((index, Instant::now()));
        match previous {
            Some((p, _)) => {
                inner.front = Some(p);
                self.frames.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Shows the last copied frame if no newer one followed it within [`STALE_AFTER`]. Returns
    /// whether a new frame shows.
    pub fn promote_stale(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.copied {
            Some((c, at)) if at.elapsed() >= STALE_AFTER && inner.front != Some(c) => {
                inner.front = Some(c);
                inner.copied = None;
                self.frames.fetch_add(1, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// The frame to draw: its bitmap (a resource of the shared Direct2D device, drawable by any of
    /// its contexts) and size.
    pub fn front(&self) -> Option<(ID2D1Bitmap1, u32, u32)> {
        let inner = self.inner.lock().unwrap();
        let b = inner.buffers.as_ref()?;
        let i = inner.front?;
        Some((b.bitmaps[i].clone(), b.width, b.height))
    }

    /// Copies the front frame to memory (premultiplied BGRA, opaque), waiting for the GPU.
    pub fn read_front(&self, gpu: &Gpu) -> Result<Option<Frame>> {
        match self.front() {
            Some((bitmap, w, h)) => read_bitmap(gpu, &bitmap, w, h).map(Some),
            None => Ok(None),
        }
    }

    /// Fills the next texture with `pixels` (BGRA, `width` × `height`) and shows it at once: frames
    /// that come from memory (tests, a software path) need no copy-ordering delay.
    pub fn upload(&self, gpu: &Gpu, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
        if pixels.len() < width as usize * height as usize * 4 {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        let target = self.begin_frame(gpu, width, height)?;
        let texture = {
            let inner = self.inner.lock().unwrap();
            inner.buffers.as_ref().unwrap().textures[target.index].clone()
        };
        // SAFETY: UpdateSubresource reads `height` rows of `width * 4` bytes from `pixels` (checked
        // above), under Direct2D's device lock (the immediate context is shared with Direct2D).
        unsafe {
            let _lock = gpu.lock();
            let ctx = gpu.d3d_device().GetImmediateContext()?;
            ctx.UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), width * 4, 0);
        }
        let mut inner = self.inner.lock().unwrap();
        inner.front = Some(target.index);
        inner.copied = None;
        self.frames.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Copies `bitmap` (a `w` × `h` resource of `gpu`'s device) to memory (premultiplied BGRA, opaque),
/// waiting for the GPU.
pub fn read_bitmap(gpu: &Gpu, bitmap: &ID2D1Bitmap1, w: u32, h: u32) -> Result<Frame> {
    // SAFETY: a device context of the shared device copies the frame's bitmap into a
    // CPU-readable bitmap under Direct2D's device lock; the mapping is read row by row within
    // its pitch and unmapped.
    unsafe {
        let _lock = gpu.lock();
        let ctx = gpu
            .d2d_device()
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        let staging = ctx.CreateBitmap(
            D2D_SIZE_U {
                width: w,
                height: h,
            },
            None,
            0,
            &D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            },
        )?;
        staging.CopyFromBitmap(None, bitmap, None)?;
        let mapped = staging.Map(D2D1_MAP_OPTIONS_READ)?;
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        for y in 0..h as usize {
            let src = std::slice::from_raw_parts(
                mapped.bits.add(y * mapped.pitch as usize),
                w as usize * 4,
            );
            pixels[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(src);
        }
        staging.Unmap()?;
        for px in pixels.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        Ok(Frame {
            width: w,
            height: h,
            pixels,
            delay_ms: 0,
        })
    }
}

fn make_buffers(gpu: &Gpu, width: u32, height: u32) -> Result<Buffers> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut textures = Vec::with_capacity(FRAME_TEXTURES);
    let mut surfaces = Vec::with_capacity(FRAME_TEXTURES);
    let mut bitmaps = Vec::with_capacity(FRAME_TEXTURES);
    // SAFETY: texture creation with a valid description on the shared device; each bitmap wraps
    // its texture's surface and keeps it alive. The context is only used to make the bitmaps
    // (device resources, shared by every context of the device).
    unsafe {
        let ctx = gpu
            .d2d_device()
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        for _ in 0..FRAME_TEXTURES {
            let mut texture = None;
            gpu.d3d_device()
                .CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            let surface: IDXGISurface = texture.cast()?;
            let bitmap = ctx.CreateBitmapFromDxgiSurface(
                &surface,
                Some(&D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                    colorContext: ManuallyDrop::new(None),
                }),
            )?;
            textures.push(texture);
            surfaces.push(surface);
            bitmaps.push(bitmap);
        }
    }
    Ok(Buffers {
        width,
        height,
        textures,
        surfaces,
        bitmaps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu() -> Option<Arc<Gpu>> {
        Gpu::with_driver(crate::Driver::Warp).ok()
    }

    #[test]
    fn frames_show_one_late_and_stale_ones_show() {
        let Some(gpu) = gpu() else { return };
        let store = VideoStore::new(Some(gpu.clone()));
        let slot = store.create();
        assert!(store.get(slot.id()).is_some());
        let a = slot.begin_frame(&gpu, 4, 2).unwrap();
        assert!(!slot.end_frame(a.index));
        assert!(slot.front().is_none());
        let b = slot.begin_frame(&gpu, 4, 2).unwrap();
        assert_ne!(a.index, b.index);
        assert!(slot.end_frame(b.index));
        assert_eq!(slot.front().map(|f| (f.1, f.2)), Some((4, 2)));
        // The next target is neither the front nor the one in flight.
        let c = slot.begin_frame(&gpu, 4, 2).unwrap();
        assert!(c.index != a.index && c.index != b.index);
        assert!(slot.end_frame(c.index));
        std::thread::sleep(STALE_AFTER + Duration::from_millis(5));
        assert!(slot.promote_stale());
        assert!(!slot.promote_stale());
        assert_eq!(slot.frame_count(), 3);
        // A new size makes new textures.
        let d = slot.begin_frame(&gpu, 8, 8).unwrap();
        assert_eq!(d.generation, a.generation + 1);
        assert!(slot.front().is_none());
        store.remove(slot.id());
        assert!(store.is_empty());
    }

    #[test]
    fn uploaded_frames_read_back() {
        let Some(gpu) = gpu() else { return };
        let store = VideoStore::new(Some(gpu.clone()));
        let slot = store.create();
        let mut pixels = Vec::new();
        for i in 0..4 * 4 {
            pixels.extend_from_slice(&[i as u8 * 10, 0x20, 0x30, 0xFF]);
        }
        slot.upload(&gpu, 4, 4, &pixels).unwrap();
        let frame = slot.read_front(&gpu).unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (4, 4));
        assert_eq!(&frame.pixels[..4], &[0, 0x20, 0x30, 0xFF]);
        assert_eq!(frame.pixels[5 * 4], 50);
    }

    #[test]
    fn listeners_hear_new_frames() {
        let store = VideoStore::new(None);
        let heard = Arc::new(Mutex::new(Vec::new()));
        let h = heard.clone();
        store.set_notify(Some(Arc::new(move |id| h.lock().unwrap().push(id))));
        let slot = store.create();
        store.frame_ready(slot.id());
        assert_eq!(*heard.lock().unwrap(), vec![slot.id()]);
    }

    /// The browser's side of a renderer's videos, in one process: what the renderer's
    /// `VideoExport` would send, handed straight to a browser store on another device.
    struct Browser {
        store: Arc<VideoStore>,
        ids: Mutex<HashMap<u32, VideoId>>,
        frames: AtomicU32,
        /// The kind of device the browser says it draws with.
        driver: Mutex<crate::Driver>,
    }

    impl Browser {
        fn new(gpu: Arc<Gpu>, driver: crate::Driver) -> Arc<Browser> {
            Arc::new(Browser {
                store: VideoStore::new(Some(gpu)),
                ids: Mutex::default(),
                frames: AtomicU32::new(0),
                driver: Mutex::new(driver),
            })
        }

        /// Checks that the frame the browser draws for the renderer's video `id` is `bgr`.
        fn shows(&self, gpu: &Gpu, id: VideoId, bgr: [u8; 3]) {
            let id = self.ids.lock().unwrap()[&id.0];
            self.store.latch(id);
            let (bitmap, w, h) = self.store.front_of(id).unwrap();
            let f = read_bitmap(gpu, &bitmap, w, h).unwrap();
            assert_eq!(&f.pixels[..3], &bgr, "the frame drawn");
        }
    }

    impl VideoExport for Browser {
        fn driver(&self) -> Option<crate::Driver> {
            Some(*self.driver.lock().unwrap())
        }

        fn pool(&self, id: VideoId, width: u32, height: u32) -> Option<PoolHandles> {
            let w = *self
                .ids
                .lock()
                .unwrap()
                .entry(id.0)
                .or_insert_with(|| self.store.import());
            self.store.make_pool(w, width, height).ok()
        }

        fn frame(&self, id: VideoId, generation: u32, index: u32) {
            let w = self.ids.lock().unwrap()[&id.0];
            assert!(self.store.imported_frame(w, generation, index));
            self.frames.fetch_add(1, Ordering::Relaxed);
        }

        fn removed(&self, id: VideoId) {
            let w = self.ids.lock().unwrap().remove(&id.0).unwrap();
            self.store.remove(w);
        }
    }

    fn solid(w: u32, h: u32, bgr: [u8; 3]) -> Vec<u8> {
        (0..w * h)
            .flat_map(|_| [bgr[0], bgr[1], bgr[2], 0xFF])
            .collect()
    }

    #[test]
    fn a_renderers_frames_reach_the_browser_through_its_textures() {
        let (Some(browser_gpu), Some(_)) = (gpu(), gpu()) else {
            return;
        };
        let browser = Browser::new(browser_gpu.clone(), crate::Driver::Warp);
        let told = Arc::new(Mutex::new(Vec::new()));
        let t = told.clone();
        browser
            .store
            .set_notify(Some(Arc::new(move |id| t.lock().unwrap().push(id))));
        // The renderer's store makes its own device (a second one: not the browser's).
        let renderer = VideoStore::exporting(browser.clone());
        let rgpu = renderer.gpu().unwrap().clone();
        assert!(!Arc::ptr_eq(&rgpu, &browser_gpu));
        let slot = renderer.create();
        let shows = |bgr: [u8; 3]| {
            let id = browser.ids.lock().unwrap()[&slot.id().0];
            browser.store.latch(id);
            let (bitmap, w, h) = browser.store.front_of(id).unwrap();
            let f = read_bitmap(&browser_gpu, &bitmap, w, h).unwrap();
            assert_eq!(&f.pixels[..3], &bgr, "the frame drawn");
        };
        slot.upload(&rgpu, 4, 4, &solid(4, 4, [0, 0, 0xFF]))
            .unwrap();
        renderer.frame_ready(slot.id());
        assert_eq!(told.lock().unwrap().len(), 1);
        shows([0, 0, 0xFF]);
        slot.upload(&rgpu, 4, 4, &solid(4, 4, [0, 0xFF, 0]))
            .unwrap();
        renderer.frame_ready(slot.id());
        shows([0, 0xFF, 0]);
        // Frames the compositor does not take go back to the renderer: none is ever dropped for
        // want of a texture, and the one drawn stays until a newer one is taken.
        for n in 0..10u8 {
            slot.upload(&rgpu, 4, 4, &solid(4, 4, [n, n, n])).unwrap();
            renderer.frame_ready(slot.id());
        }
        assert_eq!(browser.frames.load(Ordering::Relaxed), 12);
        shows([9, 9, 9]);
        // A new size is a new pool.
        slot.upload(&rgpu, 8, 2, &solid(8, 2, [0xFF, 0, 0]))
            .unwrap();
        renderer.frame_ready(slot.id());
        shows([0xFF, 0, 0]);
        let id = browser.ids.lock().unwrap()[&slot.id().0];
        assert_eq!(browser.store.front_of(id).map(|f| (f.1, f.2)), Some((8, 2)));
        // A frame of no pool, or no texture, is refused.
        assert!(!browser.store.imported_frame(id, 1, 0));
        assert!(!browser.store.imported_frame(id, 2, POOL_TEXTURES as u32));
        assert!(!browser.store.imported_frame(VideoId(999), 2, 0));
        // Removed in the renderer, it is gone in the browser.
        renderer.remove(slot.id());
        assert!(browser.store.front_of(id).is_none());
        assert!(browser.ids.lock().unwrap().is_empty());
    }

    /// The browser came to draw with another kind of device than the renderer's videos (a GPU
    /// process that moved to WARP past the graphics card's crash limit): its textures do not open
    /// on the renderer's device, so once repooled the frames reach them through a device of the
    /// browser's kind, and the players keep theirs.
    #[test]
    fn frames_reach_a_browser_on_another_kind_of_device_after_a_repool() {
        let (Some(browser_gpu), Ok(_)) = (gpu(), Gpu::with_driver(crate::Driver::Hardware)) else {
            return;
        };
        // (the browser said "graphics card" when the renderer made its device)
        let browser = Browser::new(browser_gpu.clone(), crate::Driver::Hardware);
        let renderer = VideoStore::exporting(browser.clone());
        let rgpu = renderer.gpu().unwrap().clone();
        assert_eq!(rgpu.driver(), crate::Driver::Hardware);
        let slot = renderer.create();
        slot.upload(&rgpu, 4, 4, &solid(4, 4, [0, 0, 0xFF]))
            .unwrap();
        renderer.frame_ready(slot.id());
        assert_eq!(
            browser.frames.load(Ordering::Relaxed),
            0,
            "the browser's WARP textures opened on the graphics card"
        );
        *browser.driver.lock().unwrap() = crate::Driver::Warp;
        renderer.repool();
        assert_eq!(browser.frames.load(Ordering::Relaxed), 1);
        browser.shows(&browser_gpu, slot.id(), [0, 0, 0xFF]);
        assert!(Arc::ptr_eq(renderer.gpu().unwrap(), &rgpu));
        slot.upload(&rgpu, 4, 4, &solid(4, 4, [0, 0xFF, 0]))
            .unwrap();
        renderer.frame_ready(slot.id());
        browser.shows(&browser_gpu, slot.id(), [0, 0xFF, 0]);
        // Many frames: each texture goes back and forth.
        for n in 0..10u8 {
            slot.upload(&rgpu, 4, 4, &solid(4, 4, [n, n, n])).unwrap();
            renderer.frame_ready(slot.id());
        }
        browser.shows(&browser_gpu, slot.id(), [9, 9, 9]);
        // A new size is a new pool, on the same device.
        slot.upload(&rgpu, 8, 2, &solid(8, 2, [0xFF, 0, 0]))
            .unwrap();
        renderer.frame_ready(slot.id());
        browser.shows(&browser_gpu, slot.id(), [0xFF, 0, 0]);
        assert_eq!(browser.frames.load(Ordering::Relaxed), 13);
    }

    #[test]
    fn the_browser_holds_the_key_of_the_frame_it_draws() {
        let (Some(browser_gpu), Some(rgpu)) = (gpu(), gpu()) else {
            return;
        };
        let store = VideoStore::new(Some(browser_gpu));
        let id = store.import();
        let pool = store.make_pool(id, 4, 4).unwrap();
        let opened = open_pool(&rgpu, None, pool, 4, 4).unwrap();
        // The renderer writes texture 0 and gives it to the browser (key 1).
        assert!(acquire_sync(&opened.keyed[0], 0, 0).unwrap());
        // SAFETY: the key was taken above.
        unsafe { opened.keyed[0].ReleaseSync(1).unwrap() };
        assert!(store.imported_frame(id, 1, 0));
        // Not taken by the browser yet: the renderer cannot write it.
        assert!(!acquire_sync(&opened.keyed[0], 0, 0).unwrap());
        store.latch(id);
        // Drawn by the browser: still not.
        assert!(!acquire_sync(&opened.keyed[0], 0, 0).unwrap());
        // Once a newer frame is taken, it is the renderer's again.
        assert!(acquire_sync(&opened.keyed[1], 0, 0).unwrap());
        // SAFETY: the key was taken above.
        unsafe { opened.keyed[1].ReleaseSync(1).unwrap() };
        assert!(store.imported_frame(id, 1, 1));
        store.latch(id);
        assert!(acquire_sync(&opened.keyed[0], 0, 0).unwrap());
        // A pool past the texture limit is refused.
        assert!(store.make_pool(id, MAX_SHARED_SIDE + 1, 4).is_err());
    }
}
