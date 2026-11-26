// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Video slots on Linux, before there is a media backend or a GPU device: a store hands out slots
//! with ids as on Windows, but no frame ever reaches one (playback is stage 7 of docs/linux.md, the
//! device stage 5), so a video element keeps showing its poster.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use snowlark_graphics::Result;

use crate::gpu::Gpu;
use crate::{Frame, VideoId};

/// Told when a slot has a new frame to show (none do here yet).
pub type FrameNotify = Arc<dyn Fn(VideoId) + Send + Sync>;

/// A frame to draw ([`VideoStore::front_of`]): none exists here yet, so no value of it can be made.
#[derive(Clone, Debug)]
pub enum VideoFront {}

/// The textures shared per video and size, as on Windows (none are made here yet).
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

/// Where a renderer's store sends its videos' frames: the browser, which draws them (the Windows
/// half's interface; no frame reaches a slot here yet, so none is sent).
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

/// The video slots of a window or a headless run.
pub struct VideoStore {
    gpu: Option<Arc<Gpu>>,
    slots: Mutex<HashMap<u32, Arc<VideoSlot>>>,
    next: AtomicU32,
}

impl std::fmt::Debug for VideoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoStore").finish_non_exhaustive()
    }
}

impl VideoStore {
    pub fn new(gpu: Option<Arc<Gpu>>) -> Arc<VideoStore> {
        Arc::new(VideoStore {
            gpu,
            slots: Mutex::new(HashMap::new()),
            next: AtomicU32::new(1),
        })
    }

    /// A renderer process's store, whose videos' frames would go to the browser through `export`:
    /// with no frames on Linux yet, a store like any other.
    pub fn exporting(_export: Arc<dyn VideoExport>) -> Arc<VideoStore> {
        VideoStore::new(None)
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

    /// Shared textures for a renderer's video `id`: none can be made without a GPU device.
    pub fn make_pool(&self, _id: VideoId, _width: u32, _height: u32) -> Result<PoolHandles> {
        Err(snowlark_graphics::Error::Unsupported)
    }

    /// A renderer's video `id` shows texture `index` of pool `generation`: there are no pools,
    /// so it never does (false).
    pub fn imported_frame(&self, _id: VideoId, _generation: u32, _index: u32) -> bool {
        false
    }

    /// Who to tell of new frames: no slot gets one here, so nobody is told.
    pub fn set_notify(&self, _notify: Option<FrameNotify>) {}

    /// The compositor's thread, between frames: makes an imported video's newest frame the one
    /// drawn. There are none here.
    pub fn latch(&self, _id: VideoId) {}

    /// The frame to draw for `id`: none here.
    pub fn front_of(&self, _id: VideoId) -> Option<VideoFront> {
        None
    }

    /// The device frames are drawn with.
    pub fn gpu(&self) -> Option<&Arc<Gpu>> {
        self.gpu.as_ref()
    }

    /// A new slot.
    pub fn create(&self) -> Arc<VideoSlot> {
        let id = VideoId(self.next.fetch_add(1, Ordering::Relaxed));
        let slot = Arc::new(VideoSlot { id });
        self.slots.lock().unwrap().insert(id.0, slot.clone());
        slot
    }

    pub fn remove(&self, id: VideoId) {
        self.slots.lock().unwrap().remove(&id.0);
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

/// One video's frames: none on Linux yet.
pub struct VideoSlot {
    id: VideoId,
}

impl VideoSlot {
    pub fn id(&self) -> VideoId {
        self.id
    }

    /// How many frames were shown.
    pub fn frame_count(&self) -> u64 {
        0
    }

    /// The size of the frame shown.
    pub fn size(&self) -> Option<(u32, u32)> {
        None
    }

    /// The frame shown, read back into memory.
    pub fn read_front(&self, _gpu: &Gpu) -> Result<Option<Frame>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_have_distinct_ids_and_no_frames() {
        let store = VideoStore::new(None);
        let a = store.create();
        let b = store.create();
        assert_ne!(a.id(), b.id());
        assert_eq!(store.len(), 2);
        assert_eq!(store.get(a.id()).unwrap().frame_count(), 0);
        store.remove(a.id());
        assert!(store.get(a.id()).is_none());
        assert!(store.gpu().is_none());
    }
}
