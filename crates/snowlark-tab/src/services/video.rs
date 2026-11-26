// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tab's videos across processes (`docs/process-isolation.md` §8, "Video"; M4 step 4). The players
//! and their slots stay in the renderer ([`snowlark_paint::video::VideoStore::exporting`]); the device
//! the compositor draws with makes each video's shared textures, the browser hands the renderer a
//! handle to each, and the compositor draws the frames the renderer names
//! ([`VideoStore::imported_frame`], [`VideoStore::latch`]). The design and its Chromium sources are in
//! `snowlark_paint::video`'s documentation.
//!
//! Where the textures are made is a [`VideoBackend`]: the window's store in this process
//! ([`LocalVideos`]), or the GPU process's (M7 G4 step 4, `snowlark-shell`'s `gpu_compositor`), whose
//! handles come out of that process into the browser's table on their way to the renderer. The
//! browser's limits ([`MAX_VIDEOS`], [`MAX_POOL_BYTES`]) and its checks of the renderer's word are
//! [`VideoHost`]'s either way.
//!
//! A renderer knows its videos by its own ids, which the backend maps to ids of the store
//! ([`VideoBackend::ids`]): the tab's raster thread puts the store's ids in the frames it sends the
//! compositor, so a renderer's display list can name only its own videos. A frame that names a
//! video, pool or texture the renderer was never given, or a video it never had, kills it.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::OwnedFd as OwnedHandle;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use snowlark_ipc::{Error, RemoteHandle, Replier};
use snowlark_paint::video::{PoolHandles, VideoExport, VideoStore, MAX_SHARED_SIDE, POOL_TEXTURES};
use snowlark_paint::{Driver, VideoId};

use super::{answer, ask, BrowserPort, RendererPort};

/// How many videos with textures a renderer may have at once.
pub const MAX_VIDEOS: usize = 256;

/// How many bytes of shared textures the browser makes for one renderer at most (a 4K video's pool
/// is 127 MiB).
pub const MAX_POOL_BYTES: u64 = 2 << 30;

/// Renderer to browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// Which device the browser draws with (a call): a renderer makes its own on the same kind.
    Device,
    /// New shared textures for `width` × `height` frames of the renderer's video `id` (a call); the
    /// video's previous ones are let go.
    Pool { id: u32, width: u32, height: u32 },
    /// Video `id`'s newest frame is in texture `index` of pool `generation`.
    Frame {
        id: u32,
        generation: u32,
        index: u32,
    },
    /// Video `id` is gone.
    Removed { id: u32 },
}

/// The answer to [`Up::Pool`]: the pool's generation and a handle to each texture, in the
/// renderer's table.
#[derive(Debug, Serialize, Deserialize)]
pub struct PoolReply {
    pub generation: u32,
    pub handles: Vec<RemoteHandle>,
}

/// The renderer's end: where its video store's frames go.
pub struct RemoteVideo {
    port: RendererPort,
}

impl RemoteVideo {
    pub fn new(port: RendererPort) -> RemoteVideo {
        RemoteVideo { port }
    }
}

impl VideoExport for RemoteVideo {
    fn driver(&self) -> Option<Driver> {
        let warp: Option<bool> = ask(&self.port, &super::Up::Video(Up::Device))?;
        Some(if warp? {
            Driver::Warp
        } else {
            Driver::Hardware
        })
    }

    fn pool(&self, id: VideoId, width: u32, height: u32) -> Option<PoolHandles> {
        let reply: Option<PoolReply> = ask(
            &self.port,
            &super::Up::Video(Up::Pool {
                id: id.0,
                width,
                height,
            }),
        )?;
        let reply = reply?;
        #[cfg(windows)]
        let handles: Vec<OwnedHandle> = reply
            .handles
            .into_iter()
            .map(|h| self.port.take_handle(h))
            .collect();
        #[cfg(unix)]
        let handles: Vec<OwnedHandle> = reply
            .handles
            .into_iter()
            .map(|h| self.port.take_handle(h))
            .collect::<Result<_, _>>()
            .ok()?;
        Some(PoolHandles {
            generation: reply.generation,
            handles,
        })
    }

    fn frame(&self, id: VideoId, generation: u32, index: u32) {
        // (a browser that is gone ends this process soon)
        let _ = self.port.send(&super::Up::Video(Up::Frame {
            id: id.0,
            generation,
            index,
        }));
    }

    fn removed(&self, id: VideoId) {
        let _ = self.port.send(&super::Up::Video(Up::Removed { id: id.0 }));
    }
}

/// Maps a renderer's video id to the window's (an id the renderer was never given maps to none the
/// window has).
pub type VideoIds = Arc<dyn Fn(VideoId) -> VideoId + Send + Sync>;

/// Where one renderer's video textures are made and its frames drawn. Ids are the renderer's own;
/// [`VideoHost`] has checked each against what it gave the renderer before it gets here.
pub trait VideoBackend: Send + Sync {
    /// Whether the device draws with WARP (`None`: there is none).
    fn software(&self) -> Option<bool>;
    /// New textures for video `id` (made known now if it is new): their generation and a handle to
    /// each in this process. `None` when they cannot be made.
    fn pool(&self, id: u32, width: u32, height: u32) -> Option<PoolHandles>;
    /// Video `id`'s newest frame: false when it names no texture the renderer was given (a broken
    /// rule). A backend that checks it elsewhere answers true and refuses it there.
    fn frame(&self, id: u32, generation: u32, index: u32) -> bool;
    /// Video `id` is gone.
    fn removed(&self, id: u32);
    /// The renderer is gone: so are its videos.
    fn gone(&self);
    /// The store's id of each of the renderer's videos, for a raster thread of this process.
    fn ids(&self) -> VideoIds;
}

/// The window's store in this process: the compositor's device makes the textures.
pub struct LocalVideos {
    store: Arc<VideoStore>,
    ids: Arc<Mutex<HashMap<u32, VideoId>>>,
}

impl LocalVideos {
    pub fn new(store: Arc<VideoStore>) -> LocalVideos {
        LocalVideos {
            store,
            ids: Arc::default(),
        }
    }

    fn ids_of(&self) -> std::sync::MutexGuard<'_, HashMap<u32, VideoId>> {
        self.ids.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl VideoBackend for LocalVideos {
    fn software(&self) -> Option<bool> {
        self.store.gpu().map(|g| g.driver() == Driver::Warp)
    }

    fn pool(&self, id: u32, width: u32, height: u32) -> Option<PoolHandles> {
        let window = *self
            .ids_of()
            .entry(id)
            .or_insert_with(|| self.store.import());
        match self.store.make_pool(window, width, height) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("[video] no shared textures for a {width}x{height} video: {e}");
                None
            }
        }
    }

    fn frame(&self, id: u32, generation: u32, index: u32) -> bool {
        let window = self.ids_of().get(&id).copied();
        window.is_some_and(|w| self.store.imported_frame(w, generation, index))
    }

    fn removed(&self, id: u32) {
        let window = self.ids_of().remove(&id);
        if let Some(w) = window {
            self.store.remove(w);
        }
    }

    fn gone(&self) {
        let ids: Vec<VideoId> = self.ids_of().drain().map(|(_, w)| w).collect();
        for id in ids {
            self.store.remove(id);
        }
    }

    fn ids(&self) -> VideoIds {
        let ids = self.ids.clone();
        Arc::new(move |id| {
            let ids = ids.lock().unwrap_or_else(|e| e.into_inner());
            ids.get(&id.0).copied().unwrap_or(VideoId(0))
        })
    }
}

/// The renderer's videos with textures, and the bytes of their pools.
#[derive(Default)]
struct Videos {
    bytes_of: HashMap<u32, u64>,
    bytes: u64,
}

/// The browser's end for one renderer's tab.
pub struct VideoHost {
    port: BrowserPort,
    backend: Option<Arc<dyn VideoBackend>>,
    videos: Mutex<Videos>,
}

impl VideoHost {
    pub(super) fn new(port: BrowserPort, backend: Option<Arc<dyn VideoBackend>>) -> VideoHost {
        VideoHost {
            port,
            backend,
            videos: Mutex::default(),
        }
    }

    fn videos(&self) -> std::sync::MutexGuard<'_, Videos> {
        self.videos.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The window's id of each of the renderer's videos, for its raster thread.
    pub fn ids(&self) -> VideoIds {
        match &self.backend {
            Some(b) => b.ids(),
            None => Arc::new(|_| VideoId(0)),
        }
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        let call = matches!(message, Up::Device | Up::Pool { .. });
        if call != replier.is_some() {
            return Err(self.bad(format!("{message:?} sent as the wrong kind of message")));
        }
        match message {
            Up::Device => {
                let warp = self.backend.as_ref().and_then(|b| b.software());
                answer(&self.port, replier, &warp)
            }
            Up::Pool { id, width, height } => {
                let reply = self.pool(id, width, height);
                answer(&self.port, replier, &reply)
            }
            Up::Frame {
                id,
                generation,
                index,
            } => {
                let had = self.videos().bytes_of.contains_key(&id);
                let shown = match (had, &self.backend) {
                    (true, Some(b)) => b.frame(id, generation, index),
                    _ => false,
                };
                if !shown {
                    return Err(self.bad(format!(
                        "a frame of video {id} in texture {index} of pool {generation}, which it was not given"
                    )));
                }
                Ok(())
            }
            Up::Removed { id } => {
                let removed = {
                    let mut videos = self.videos();
                    let removed = videos.bytes_of.remove(&id);
                    if let Some(bytes) = removed {
                        videos.bytes -= bytes;
                    }
                    removed
                };
                match (removed, &self.backend) {
                    (Some(_), Some(b)) => {
                        b.removed(id);
                        Ok(())
                    }
                    _ => Err(self.bad(format!("video {id}, which it never had, removed"))),
                }
            }
        }
    }

    /// Makes video `id`'s pool and hands its textures to the renderer. `None` (a refusal, not a
    /// broken rule: a page may play many or large videos) past the limits or without a device.
    fn pool(&self, id: u32, width: u32, height: u32) -> Option<PoolReply> {
        let backend = self.backend.as_ref()?;
        if width == 0 || height == 0 || width > MAX_SHARED_SIDE || height > MAX_SHARED_SIDE {
            return None;
        }
        let bytes = u64::from(width) * u64::from(height) * 4 * POOL_TEXTURES as u64;
        {
            let videos = self.videos();
            let had = videos.bytes_of.get(&id).copied();
            if videos.bytes - had.unwrap_or(0) + bytes > MAX_POOL_BYTES
                || (had.is_none() && videos.bytes_of.len() >= MAX_VIDEOS)
            {
                return None;
            }
        }
        let pool = backend.pool(id, width, height)?;
        {
            // (the video is the renderer's from now on, its previous pool let go)
            let mut videos = self.videos();
            let had = videos.bytes_of.insert(id, bytes).unwrap_or(0);
            videos.bytes = videos.bytes - had + bytes;
        }
        let mut handles = Vec::with_capacity(pool.handles.len());
        for h in &pool.handles {
            handles.push(self.port.duplicate_to_child(h).ok()?);
        }
        Some(PoolReply {
            generation: pool.generation,
            handles,
        })
    }

    /// The renderer is gone: so are its videos.
    pub(super) fn gone(&self) {
        *self.videos() = Videos::default();
        if let Some(b) = &self.backend {
            b.gone();
        }
    }
}
