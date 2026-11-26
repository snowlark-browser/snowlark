// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The remote forms of the services (`docs/process-isolation.md`, M4 step 2): what a renderer process
//! uses in place of the browser's own service objects, and the browser's end that serves it.
//!
//! Each service has a client here that implements the same trait as its local form and turns each
//! call into a message on the renderer's pipe, and a host that takes those messages, checks them and
//! makes the same call on the browser's local form. What the browser sends back (a broadcast for one
//! of the renderer's channels, say) comes as [`Down`], which [`RemoteServices::dispatch`] routes.
//!
//! Every message from a renderer arrives as `Untrusted`, and one that breaks the protocol (an id the
//! renderer never made, bulk it does not hold) kills it. Keys and origins are still the renderer's
//! word: M5 derives them from frame tokens instead.
//!
//! The tab's own messages share the pipe ([`Up::Tab`], [`Down::Tab`]): one receive loop at each end
//! serves both ([`ServiceHost::run_with`], [`RemoteServices::run_with`]), and
//! [`crate::renderer_process`] is what handles them.

pub mod audio;
pub mod audio_out;
pub mod broadcast;
pub mod cache;
pub mod cdm;
pub mod code_cache;
pub mod embedder;
pub mod idb;
pub mod net;
pub mod site_lock;
pub mod storage;
pub mod video;

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use snowlark_ipc::{
    wire, BadMessage, BrowserEnd, Bulk, ChildEnd, Error, ProcessPolicy, Received, Replier, Validate,
};

/// How long a service call waits for the other side before it gives up.
pub const CALL_WAIT: Duration = Duration::from_secs(30);

/// Renderer to browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    Broadcast(broadcast::Up),
    Storage(storage::Up),
    CodeCache(code_cache::Up),
    Embedder(embedder::Up),
    Net(net::Up),
    Idb(idb::Up),
    Cache(cache::Up),
    Audio(audio::Up),
    AudioOut(audio_out::Up),
    Video(video::Up),
    Cdm(cdm::Up),
    /// The tab's events, its messages to the compositor and its frames.
    Tab(crate::renderer_process::TabUp),
}

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    Broadcast(broadcast::Down),
    Storage(storage::Down),
    CodeCache(code_cache::Down),
    Net(net::Down),
    Idb(idb::Down),
    Audio(audio::Down),
    AudioOut(audio_out::Down),
    Cdm(cdm::Down),
    /// The tab's start, its commands, and frames its raster thread has taken.
    Tab(crate::renderer_process::TabDown),
}

impl Validate for Up {
    fn validate(self, _policy: &ProcessPolicy) -> Result<Up, BadMessage> {
        // (each service's host checks ids against what this renderer made; nothing in the message
        // alone is wrong yet, until M5 checks keys and origins against the sender's frames)
        Ok(self)
    }
}

/// The renderer's end of its pipe to the browser.
pub type RendererPort = ChildEnd<Up, Down>;
/// The browser's end of a renderer's pipe.
pub type BrowserPort = BrowserEnd<Down, Up>;

/// The renderer's services, each a client of the browser's.
pub struct RemoteServices {
    pub broadcast: Arc<broadcast::RemoteBroadcast>,
    pub storage: Arc<storage::RemoteStorage>,
    pub code_cache: Arc<code_cache::RemoteCodeCache>,
    pub clipboard: Arc<embedder::RemoteClipboard>,
    pub internal: Arc<embedder::RemoteInternalPages>,
    pub content_filter: Arc<embedder::RemoteContentFilter>,
    pub net: Arc<net::RemoteNet>,
    pub idb: Arc<idb::RemoteIdb>,
    pub caches: Arc<cache::RemoteCaches>,
    pub audio: Arc<audio::RemoteAudio>,
    pub audio_out: Arc<audio_out::RemoteOutputs>,
    pub video: Arc<video::RemoteVideo>,
    pub cdm: Arc<cdm::RemoteCdm>,
}

impl RemoteServices {
    pub fn new(port: &RendererPort) -> RemoteServices {
        RemoteServices {
            broadcast: Arc::new(broadcast::RemoteBroadcast::new(port.clone())),
            storage: Arc::new(storage::RemoteStorage::new(port.clone())),
            code_cache: Arc::new(code_cache::RemoteCodeCache::new(port.clone())),
            clipboard: Arc::new(embedder::RemoteClipboard(port.clone())),
            internal: Arc::new(embedder::RemoteInternalPages(port.clone())),
            content_filter: Arc::new(embedder::RemoteContentFilter(port.clone())),
            net: Arc::new(net::RemoteNet::new(port.clone())),
            idb: Arc::new(idb::RemoteIdb::new(port.clone())),
            caches: Arc::new(cache::RemoteCaches::new(port.clone())),
            audio: Arc::new(audio::RemoteAudio::new(port.clone())),
            audio_out: Arc::new(audio_out::RemoteOutputs::new(port.clone())),
            video: Arc::new(video::RemoteVideo::new(port.clone())),
            cdm: Arc::new(cdm::RemoteCdm::new(port.clone())),
        }
    }

    /// Hands what the browser sent to the service it is for.
    pub fn dispatch(&self, message: Down) {
        match message {
            Down::Broadcast(m) => self.broadcast.dispatch(m),
            Down::Storage(m) => self.storage.dispatch(m),
            Down::CodeCache(m) => self.code_cache.dispatch(m),
            Down::Net(m) => self.net.dispatch(m),
            Down::Idb(m) => self.idb.dispatch(m),
            Down::Audio(m) => self.audio.dispatch(m),
            Down::AudioOut(m) => self.audio_out.dispatch(m),
            Down::Cdm(m) => self.cdm.dispatch(m),
            // (a renderer with no tab hears none of the tab's messages)
            Down::Tab(_) => {}
        }
    }

    /// Takes messages off `port` until the browser is gone. The renderer's IO thread.
    pub fn run(&self, port: &RendererPort) -> Error {
        self.run_with(port, &mut |_| {})
    }

    /// [`RemoteServices::run`], handing the tab's own messages to `tab`.
    pub fn run_with(
        &self,
        port: &RendererPort,
        tab: &mut dyn FnMut(crate::renderer_process::TabDown),
    ) -> Error {
        loop {
            let message = match port.recv() {
                Ok(Received::Message(m)) => m,
                // (the browser calls nothing on a renderer)
                Ok(Received::Call(m, _)) => m,
                Err(e) => return e,
            };
            match message {
                Down::Tab(m) => tab(m),
                m => self.dispatch(m),
            }
        }
    }
}

/// What the browser serves one renderer from: its own services, shared by every tab, and the tab's
/// own (its clipboard, its internal pages, its content filter).
#[derive(Clone)]
pub struct LocalServices {
    pub broadcast: Arc<dyn snowlark_page::BroadcastBackend>,
    pub storage: Arc<dyn snowlark_page::web_storage::WebStorage>,
    pub code_cache: Arc<dyn snowlark_js::code_cache::CodeCacheStore>,
    pub clipboard: Arc<dyn crate::driver::Clipboard>,
    pub internal: Arc<dyn crate::driver::InternalPages>,
    pub content_filter: Option<Arc<dyn snowlark_page::ContentFilter>>,
    /// The browser's network, and the tab's request filter, which every request of the renderer's
    /// goes through.
    pub net: Arc<dyn snowlark_net::NetBackend>,
    pub request_filter: Option<Arc<dyn snowlark_net::RequestFilter>>,
    /// The profile's IndexedDB backend.
    pub idb: Arc<dyn snowlark_idb::IdbService>,
    /// The profile's Cache API storage.
    pub caches: Arc<dyn snowlark_sw::CacheStorageService>,
    /// What a failed site-lock check does ([`site_lock::Mode`]).
    pub site_lock: site_lock::Mode,
    /// What hears when the tab starts or stops making sound, and its mute when the renderer starts.
    pub audible: Arc<dyn Fn(bool) + Send + Sync>,
    pub muted: bool,
    /// The output device the renderer's AudioContexts play on (none: they are refused one).
    pub audio_out: Option<Arc<dyn snowlark_audio::realtime::AudioOutputs>>,
    /// The window's video store, which the compositor draws from: a renderer's videos are made
    /// there. `None`: the renderer's videos draw nothing.
    pub videos: Option<Arc<snowlark_paint::video::VideoStore>>,
    /// The browser's key systems (none: none are offered).
    pub cdm: Option<Arc<dyn snowlark_page::media::cdm::Cdm>>,
}

/// The browser's end for one renderer. Dropping it undoes what the renderer left behind in the
/// browser's services (its channels, storage clients and fetches, its IndexedDB loops, whose
/// unfinished transactions revert, and its sound).
pub struct ServiceHost {
    port: BrowserPort,
    broadcast: Arc<broadcast::BroadcastHost>,
    storage: Arc<storage::StorageHost>,
    code_cache: code_cache::CodeCacheHost,
    embedder: embedder::EmbedderHost,
    net: Arc<net::NetHost>,
    idb: idb::IdbHost,
    caches: cache::CacheHost,
    audio: Arc<audio::AudioHost>,
    audio_out: audio_out::AudioOutHost,
    video: video::VideoHost,
    cdm: Arc<cdm::CdmHost>,
}

impl ServiceHost {
    pub fn new(port: BrowserPort, local: &LocalServices) -> ServiceHost {
        let videos = local
            .videos
            .clone()
            .map(|s| Arc::new(video::LocalVideos::new(s)) as Arc<dyn video::VideoBackend>);
        ServiceHost::with_videos(port, local, videos)
    }

    /// [`ServiceHost::new`], with the renderer's videos made and drawn by `videos` (the GPU
    /// process's) in place of the window's store.
    pub fn with_videos(
        port: BrowserPort,
        local: &LocalServices,
        videos: Option<Arc<dyn video::VideoBackend>>,
    ) -> ServiceHost {
        ServiceHost {
            broadcast: broadcast::BroadcastHost::new(
                port.clone(),
                local.broadcast.clone(),
                local.site_lock,
            ),
            storage: storage::StorageHost::new(
                port.clone(),
                local.storage.clone(),
                local.site_lock,
            ),
            code_cache: code_cache::CodeCacheHost::new(
                port.clone(),
                local.code_cache.clone(),
                local.site_lock,
            ),
            embedder: embedder::EmbedderHost::new(port.clone(), local.clone()),
            net: Arc::new(net::NetHost::new(
                port.clone(),
                local.net.clone(),
                local.request_filter.clone(),
                local.site_lock,
            )),
            idb: idb::IdbHost::new(port.clone(), local.idb.clone(), local.site_lock),
            caches: cache::CacheHost::new(port.clone(), local.caches.clone(), local.site_lock),
            audio: {
                let audible = local.audible.clone();
                let notify = Box::new(move |a| audible(a));
                Arc::new(audio::AudioHost::new(port.clone(), notify, local.muted))
            },
            audio_out: audio_out::AudioOutHost::new(port.clone(), local.audio_out.clone()),
            video: video::VideoHost::new(port.clone(), videos),
            cdm: cdm::CdmHost::new(port.clone(), local.cdm.clone(), local.site_lock),
            port,
        }
    }

    /// What the browser knows about the renderer: its site lock.
    pub fn policy(&self) -> &ProcessPolicy {
        self.port.policy()
    }

    /// The tab's audio: what the window mutes it through.
    pub fn audio(&self) -> &Arc<audio::AudioHost> {
        &self.audio
    }

    /// The renderer's network host (what the browser hands navigation responses through).
    pub fn net(&self) -> &Arc<net::NetHost> {
        &self.net
    }

    /// The window's id of each of the renderer's videos, for its raster thread.
    pub fn video_ids(&self) -> video::VideoIds {
        self.video.ids()
    }

    /// The browser's id of each of the renderer's protected players, for its raster thread.
    pub fn player_ids(&self) -> cdm::RendererPlayers {
        self.cdm.player_ids()
    }

    /// Serves the renderer until it is gone or breaks a rule (and dies for it). The browser's
    /// thread for this renderer's services. A renderer with no tab that sends the tab's messages dies.
    pub fn run(&self) -> Error {
        self.run_with(&mut |_, _| {
            let why = "a tab's message from a renderer with no tab".to_owned();
            self.port.bad_message(&why);
            Err(Error::BadMessage(why))
        })
    }

    /// [`ServiceHost::run`], handing the tab's own messages to `tab`. An error from it ends the
    /// loop; for a message that breaks a rule, `tab` kills the renderer first
    /// ([`BrowserPort::bad_message`]), as the services do.
    pub fn run_with(
        &self,
        tab: &mut dyn FnMut(crate::renderer_process::TabUp, Option<Replier>) -> Result<(), Error>,
    ) -> Error {
        let end = self.serve(tab);
        // (a renderer that is gone or killed makes no more sound, shows no more video, and takes no
        // more of the network: its fetches end, but for keepalive ones)
        self.audio.gone();
        self.audio_out.gone();
        self.video.gone();
        self.net.gone();
        self.cdm.gone();
        end
    }

    fn serve(
        &self,
        tab: &mut dyn FnMut(crate::renderer_process::TabUp, Option<Replier>) -> Result<(), Error>,
    ) -> Error {
        loop {
            let (message, replier) = match self.port.recv() {
                Ok(Received::Message(m)) => (m, None),
                Ok(Received::Call(m, r)) => (m, Some(r)),
                Err(e) => return e,
            };
            let message = match self.port.check(message) {
                Ok(m) => m,
                Err(e) => return e,
            };
            let served = match message {
                Up::Broadcast(m) => self.broadcast.serve(m, replier),
                Up::Storage(m) => self.storage.serve(m, replier),
                Up::CodeCache(m) => self.code_cache.serve(m, replier),
                Up::Embedder(m) => self.embedder.serve(m, replier),
                Up::Net(m) => self.net.serve(m, replier),
                Up::Idb(m) => self.idb.serve(m, replier),
                Up::Cache(m) => self.caches.serve(m, replier),
                Up::Audio(m) => self.audio.serve(m, replier),
                Up::AudioOut(m) => self.audio_out.serve(m, replier),
                Up::Video(m) => self.video.serve(m, replier),
                Up::Cdm(m) => self.cdm.serve(m, replier),
                Up::Tab(m) => tab(m, replier),
            };
            if let Err(e) = served {
                return e;
            }
        }
    }
}

impl Drop for ServiceHost {
    fn drop(&mut self) {
        self.audio.gone();
        self.audio_out.gone();
        self.video.gone();
    }
}

/// Asks the browser, and decodes its answer: `None` if the browser is gone or the answer is not one.
pub(crate) fn ask<A: DeserializeOwned>(port: &RendererPort, message: &Up) -> Option<A> {
    port.call::<Bulk>(message, CALL_WAIT)
        .and_then(|bulk| port.open_bulk(bulk))
        .and_then(|bytes| wire::decode(&bytes))
        .ok()
}

/// A value, encoded, as bulk for the browser.
pub(crate) fn pack<T: Serialize>(port: &RendererPort, value: &T) -> Result<Bulk, Error> {
    port.bulk(wire::to_bytes(value)?, CALL_WAIT)
}

/// A value the renderer sent as bulk. One that does not decode kills the renderer.
pub(crate) fn unpack<T: DeserializeOwned>(port: &BrowserPort, bulk: Bulk) -> Result<T, Error> {
    let bytes = port.open_bulk(bulk)?;
    wire::decode(&bytes).inspect_err(|e| port.bad_message(&e.to_string()))
}

/// Answers a call with `value`, encoded, as bulk. A message that should have been a call kills the
/// renderer.
pub(crate) fn answer<A: Serialize>(
    port: &BrowserPort,
    replier: Option<Replier>,
    value: &A,
) -> Result<(), Error> {
    let Some(replier) = replier else {
        let why = "a question sent with nobody to answer".to_owned();
        port.bad_message(&why);
        return Err(Error::BadMessage(why));
    };
    let bulk = port.bulk(&wire::to_bytes(value)?)?;
    // (a renderer that stopped waiting is no reason to stop serving it)
    let _ = replier.reply(&bulk);
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
