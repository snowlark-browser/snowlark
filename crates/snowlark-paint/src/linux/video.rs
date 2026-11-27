// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Video slots on Linux: a store hands out slots with ids as on Windows; a player decodes on the
//! CPU and [`VideoSlot::present`]s each frame as BGRA in memory, which the compositor draws
//! ([`VideoStore::front_of`]) and a page copies ([`VideoSlot::read_front`]). A renderer process's
//! store sends its frames to the browser through pools of shared memory the browser makes
//! ([`VideoStore::make_pool`]: [`POOL_TEXTURES`] buffers of one frame each, passed as descriptors);
//! the renderer writes a frame into the next buffer and names it ([`VideoExport::frame`]), and the
//! browser copies it out as the slot's frame ([`VideoStore::imported_frame`]).

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use snowlark_graphics::Result;

use crate::gpu::{Driver, Gpu};
use crate::{Frame, VideoId};

/// Told when a slot has a new frame to show.
pub type FrameNotify = Arc<dyn Fn(VideoId) + Send + Sync>;

/// A frame to draw ([`VideoStore::front_of`]): opaque BGRA in memory.
#[derive(Clone, Debug)]
pub struct VideoFront(pub Arc<Frame>);

/// The buffers shared per video and size.
pub const POOL_TEXTURES: usize = 4;

/// The largest frame side a renderer's video gets shared textures for.
pub const MAX_SHARED_SIDE: u32 = 16384;

/// The shared textures the browser made for a renderer's video, as the renderer receives them.
pub struct PoolHandles {
    /// Counts the video's pools: a frame names the pool it is in.
    pub generation: u32,
    /// [`POOL_TEXTURES`] descriptors.
    pub handles: Vec<std::os::fd::OwnedFd>,
}

/// Where a renderer's store sends its videos' frames: the browser, which draws them.
pub trait VideoExport: Send + Sync {
    /// Which device to make for the videos. `None`: no device, no frames.
    fn driver(&self) -> Option<crate::Driver>;
    /// Shared textures for video `id` at this size, made by the browser.
    fn pool(&self, id: VideoId, width: u32, height: u32) -> Option<PoolHandles>;
    /// Video `id` has a new frame to show, in texture `index` of pool `generation`.
    fn frame(&self, id: VideoId, generation: u32, index: u32);
    /// Video `id`, which had a pool, is gone.
    fn removed(&self, id: VideoId);
}

type Notify = Arc<Mutex<Option<FrameNotify>>>;

/// The video slots of a window or a headless run.
pub struct VideoStore {
    gpu: Option<Arc<Gpu>>,
    slots: Mutex<HashMap<u32, Arc<VideoSlot>>>,
    next: AtomicU32,
    notify: Notify,
    /// A renderer's store: where its frames go.
    export: Option<Arc<dyn VideoExport>>,
    generation: AtomicU32,
}

impl std::fmt::Debug for VideoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoStore").finish_non_exhaustive()
    }
}

impl VideoStore {
    /// A store; its frames are in memory, so without a device it takes the CPU one.
    pub fn new(gpu: Option<Arc<Gpu>>) -> Arc<VideoStore> {
        Arc::new(VideoStore {
            gpu: gpu.or_else(|| Gpu::with_driver(Driver::Warp).ok()),
            slots: Mutex::new(HashMap::new()),
            next: AtomicU32::new(1),
            notify: Arc::new(Mutex::new(None)),
            export: None,
            generation: AtomicU32::new(1),
        })
    }

    /// A renderer process's store, whose videos' frames go to the browser through `export`.
    pub fn exporting(export: Arc<dyn VideoExport>) -> Arc<VideoStore> {
        Arc::new(VideoStore {
            gpu: Gpu::with_driver(Driver::Warp).ok(),
            slots: Mutex::new(HashMap::new()),
            next: AtomicU32::new(1),
            notify: Arc::new(Mutex::new(None)),
            export: Some(export),
            generation: AtomicU32::new(1),
        })
    }

    /// Asks the browser again for shared textures of the videos that have them (after the
    /// browser's device changed): none have them here, so nothing is asked (false).
    pub fn repool(&self) -> bool {
        false
    }

    /// A slot in the browser for a renderer's video.
    pub fn import(&self) -> VideoId {
        self.create().id()
    }

    /// Shared buffers for a renderer's video `id` at this size (replacing any it had): their
    /// generation and a descriptor of each, for the renderer.
    pub fn make_pool(&self, id: VideoId, width: u32, height: u32) -> Result<PoolHandles> {
        let slot = self.get(id).ok_or(snowlark_graphics::Error::Unsupported)?;
        let len = frame_bytes(width, height).ok_or(snowlark_graphics::Error::Unsupported)?;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let mut maps = Vec::with_capacity(POOL_TEXTURES);
        let mut handles = Vec::with_capacity(POOL_TEXTURES);
        for _ in 0..POOL_TEXTURES {
            let shm = Shm::new(len).map_err(platform)?;
            handles.push(shm.fd.try_clone().map_err(platform)?);
            maps.push(shm);
        }
        *slot.pool.lock().unwrap() = Some(Pool {
            generation,
            width,
            height,
            maps,
            next: 0,
        });
        Ok(PoolHandles {
            generation,
            handles,
        })
    }

    /// A renderer's video `id` shows buffer `index` of pool `generation`: copied out as its frame.
    /// False when the pool or buffer is not one it was given.
    pub fn imported_frame(&self, id: VideoId, generation: u32, index: u32) -> bool {
        let Some(slot) = self.get(id) else {
            return false;
        };
        let frame = {
            let pool = slot.pool.lock().unwrap();
            let Some(pool) = pool.as_ref().filter(|p| p.generation == generation) else {
                return false;
            };
            let Some(shm) = pool.maps.get(index as usize) else {
                return false;
            };
            (pool.width, pool.height, shm.read())
        };
        slot.show(frame.0, frame.1, frame.2);
        true
    }

    /// Who to tell of new frames.
    pub fn set_notify(&self, notify: Option<FrameNotify>) {
        *self.notify.lock().unwrap() = notify;
    }

    /// The compositor's thread, between frames: the newest frame is already the one drawn here.
    pub fn latch(&self, _id: VideoId) {}

    /// The frame to draw for `id` (`_transparent`: a canvas's frames, which only a Windows
    /// renderer sends through shared textures).
    pub fn front_of(&self, id: VideoId, _transparent: bool) -> Option<VideoFront> {
        let slot = self.get(id)?;
        let front = slot.front.lock().unwrap().clone();
        front.map(VideoFront)
    }

    /// The device frames are drawn with.
    pub fn gpu(&self) -> Option<&Arc<Gpu>> {
        self.gpu.as_ref()
    }

    /// A new slot.
    pub fn create(&self) -> Arc<VideoSlot> {
        let id = VideoId(self.next.fetch_add(1, Ordering::Relaxed));
        let slot = Arc::new(VideoSlot {
            id,
            front: Mutex::new(None),
            count: AtomicU64::new(0),
            notify: self.notify.clone(),
            export: self.export.clone(),
            pool: Mutex::new(None),
        });
        self.slots.lock().unwrap().insert(id.0, slot.clone());
        slot
    }

    pub fn remove(&self, id: VideoId) {
        let slot = self.slots.lock().unwrap().remove(&id.0);
        if let (Some(slot), Some(export)) = (slot, &self.export) {
            if slot.pool.lock().unwrap().is_some() {
                export.removed(id);
            }
        }
    }

    pub fn get(&self, id: VideoId) -> Option<Arc<VideoSlot>> {
        self.slots.lock().unwrap().get(&id.0).cloned()
    }

    pub fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One video's frames: the one shown.
pub struct VideoSlot {
    id: VideoId,
    front: Mutex<Option<Arc<Frame>>>,
    count: AtomicU64,
    notify: Notify,
    export: Option<Arc<dyn VideoExport>>,
    /// The shared buffers: the browser's for an imported video, the renderer's mapping of them for
    /// an exported one.
    pool: Mutex<Option<Pool>>,
}

impl VideoSlot {
    pub fn id(&self) -> VideoId {
        self.id
    }

    /// Shows a frame of opaque BGRA pixels, rows `width * 4` bytes apart (and, in a renderer, sends
    /// it to the browser).
    pub fn present(&self, width: u32, height: u32, pixels: Vec<u8>) {
        debug_assert_eq!(pixels.len(), width as usize * height as usize * 4);
        if let Some(export) = &self.export {
            self.export(export.as_ref(), width, height, &pixels);
        }
        self.show(width, height, pixels);
    }

    /// Writes a frame into the next shared buffer and names it to the browser (a pool is asked for
    /// at each new size; without one the frame stays here).
    fn export(&self, export: &dyn VideoExport, width: u32, height: u32, pixels: &[u8]) {
        let mut pool = self.pool.lock().unwrap();
        if pool
            .as_ref()
            .is_none_or(|p| (p.width, p.height) != (width, height))
        {
            *pool = None;
            if width > MAX_SHARED_SIDE || height > MAX_SHARED_SIDE {
                return;
            }
            let Some(handles) = export.pool(self.id, width, height) else {
                return;
            };
            let Some(len) = frame_bytes(width, height) else {
                return;
            };
            let maps: Option<Vec<Shm>> = handles
                .handles
                .into_iter()
                .map(|fd| Shm::map(fd, len).ok())
                .collect();
            let Some(maps) = maps.filter(|m| !m.is_empty()) else {
                return;
            };
            *pool = Some(Pool {
                generation: handles.generation,
                width,
                height,
                maps,
                next: 0,
            });
        }
        let Some(p) = pool.as_mut() else { return };
        let index = p.next;
        p.next = (p.next + 1) % p.maps.len();
        p.maps[index].write(pixels);
        export.frame(self.id, p.generation, index as u32);
    }

    fn show(&self, width: u32, height: u32, pixels: Vec<u8>) {
        *self.front.lock().unwrap() = Some(Arc::new(Frame {
            width,
            height,
            pixels,
            delay_ms: 0,
        }));
        self.count.fetch_add(1, Ordering::Release);
        let notify = self.notify.lock().unwrap().clone();
        if let Some(n) = notify {
            n(self.id);
        }
    }

    /// How many frames were shown.
    pub fn frame_count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    /// The size of the frame shown.
    pub fn size(&self) -> Option<(u32, u32)> {
        let front = self.front.lock().unwrap();
        front.as_ref().map(|f| (f.width, f.height))
    }

    /// The frame shown, as a copy.
    pub fn read_front(&self, _gpu: &Gpu) -> Result<Option<Frame>> {
        Ok(self.front.lock().unwrap().as_deref().cloned())
    }
}

/// The bytes of a BGRA frame, when it is a size a pool takes.
fn frame_bytes(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_SHARED_SIDE || height > MAX_SHARED_SIDE {
        return None;
    }
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

fn platform(e: std::io::Error) -> snowlark_graphics::Error {
    snowlark_graphics::Error::Platform(e.to_string())
}

/// One video's shared buffers at one size.
struct Pool {
    generation: u32,
    width: u32,
    height: u32,
    maps: Vec<Shm>,
    /// The buffer the next exported frame goes in.
    next: usize,
}

/// A buffer of shared memory (`memfd`), mapped.
struct Shm {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory, read and written only through `read` and `write`.
unsafe impl Send for Shm {}
// SAFETY: as above.
unsafe impl Sync for Shm {}

impl Shm {
    fn new(len: usize) -> std::io::Result<Shm> {
        // SAFETY: memfd_create with a NUL-ended name; the descriptor is owned from here on.
        let fd = unsafe {
            let fd = libc::memfd_create(c"snowlark-video".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };
        // SAFETY: sizing a descriptor this function owns.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Shm::map(fd, len)
    }

    /// Maps `len` bytes of a shared buffer; fails when it is shorter.
    fn map(fd: OwnedFd, len: usize) -> std::io::Result<Shm> {
        // SAFETY: fstat into a zeroed struct; mmap of a descriptor checked to hold `len` bytes,
        // shared, unmapped on drop.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd.as_raw_fd(), &mut st) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if (st.st_size as u64) < len as u64 {
                return Err(std::io::Error::other("shared buffer too short"));
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Shm {
                fd,
                ptr: ptr.cast(),
                len,
            })
        }
    }

    fn write(&self, data: &[u8]) {
        let n = data.len().min(self.len);
        // SAFETY: `n` bytes fit the mapping; the other process only reads it.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr, n) }
    }

    fn read(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.len];
        // SAFETY: the mapping holds `len` bytes; the renderer may be writing another frame into it
        // (it rotates through the pool), which at worst shows a torn frame, never unsafety here:
        // the bytes are copied, never interpreted.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr, out.as_mut_ptr(), self.len) }
        out
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: the mapping is this one's.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_have_distinct_ids_and_show_what_is_presented() {
        let store = VideoStore::new(None);
        let told = Arc::new(Mutex::new(Vec::new()));
        let t = told.clone();
        store.set_notify(Some(Arc::new(move |id| t.lock().unwrap().push(id))));
        let a = store.create();
        let b = store.create();
        assert_ne!(a.id(), b.id());
        assert_eq!(store.len(), 2);
        assert_eq!(a.frame_count(), 0);
        assert!(store.front_of(a.id(), false).is_none());
        a.present(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!(a.frame_count(), 1);
        assert_eq!(a.size(), Some((2, 1)));
        assert_eq!(*told.lock().unwrap(), vec![a.id()]);
        let front = store.front_of(a.id(), false).unwrap();
        assert_eq!(front.0.pixels[4..], [4, 5, 6, 255]);
        let gpu = store.gpu().unwrap().clone();
        assert_eq!(a.read_front(&gpu).unwrap().unwrap().width, 2);
        store.remove(a.id());
        assert!(store.get(a.id()).is_none());
    }

    /// A browser's store and a renderer's, the renderer's export calling the browser directly
    /// (as the tab's messages do, descriptors and all).
    struct Direct(Arc<VideoStore>, Mutex<Vec<String>>);

    impl VideoExport for Direct {
        fn driver(&self) -> Option<crate::Driver> {
            Some(Driver::Warp)
        }
        fn pool(&self, id: VideoId, width: u32, height: u32) -> Option<PoolHandles> {
            self.1
                .lock()
                .unwrap()
                .push(format!("pool {width}x{height}"));
            // (the renderer's id is the browser's here: one video each)
            self.0.make_pool(id, width, height).ok()
        }
        fn frame(&self, id: VideoId, generation: u32, index: u32) {
            self.1.lock().unwrap().push(format!("frame {index}"));
            assert!(self.0.imported_frame(id, generation, index));
        }
        fn removed(&self, id: VideoId) {
            self.1.lock().unwrap().push("removed".into());
            self.0.remove(id);
        }
    }

    #[test]
    fn a_renderers_frames_reach_the_browser_through_shared_buffers() {
        let browser = VideoStore::new(None);
        let direct = Arc::new(Direct(browser.clone(), Mutex::new(Vec::new())));
        let renderer = VideoStore::exporting(direct.clone());
        let window = browser.import();
        let slot = renderer.create();
        assert_eq!(slot.id(), window, "one video on each side");
        for i in 0..5u8 {
            slot.present(2, 1, vec![i, 0, 0, 255, 0, i, 0, 255]);
            let front = browser.front_of(window, false).unwrap();
            assert_eq!(front.0.pixels, [i, 0, 0, 255, 0, i, 0, 255]);
        }
        slot.present(1, 1, vec![9, 9, 9, 255]);
        assert_eq!(browser.front_of(window, false).unwrap().0.width, 1);
        assert!(
            !browser.imported_frame(window, 0, 0),
            "an old pool's buffer"
        );
        renderer.remove(slot.id());
        assert!(browser.get(window).is_none());
        assert_eq!(
            *direct.1.lock().unwrap(),
            [
                "pool 2x1", "frame 0", "frame 1", "frame 2", "frame 3", "frame 0", "pool 1x1",
                "frame 0", "removed"
            ]
        );
    }
}
