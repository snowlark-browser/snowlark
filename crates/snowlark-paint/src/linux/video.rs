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
