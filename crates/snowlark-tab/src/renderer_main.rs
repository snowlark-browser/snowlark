// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A renderer process's tab (`docs/process-isolation.md`, M4 step 3): what `snowlark --type=renderer`
//! runs once it has its pipe to the browser.
//!
//! The renderer is the same [`crate::tab`] thread as in one process, over a context built from the
//! pipe: every service is [`RemoteServices`]' client, events go up the pipe, and the raster seam's
//! encode thread sends its messages up to the tab's raster thread in the browser
//! ([`crate::tab::Surface::Remote`]). Videos play here and their frames go to the browser's
//! compositor through textures the browser makes ([`services::video`]).
//!
//! One thread reads the pipe ([`RemoteServices::run_with`]) and hands the tab's messages on: the start
//! (which tab, which device, the liveness section), commands to the tab's thread, `Taken` to the
//! encode thread, and `Terminate` to the page's terminator (the page thread is busy then).
//!
//! The tab's thread starts with the process, before the start: it initializes V8 and makes the text
//! engine meanwhile, so a spare renderer the browser keeps (`docs/process-isolation.md` Â§11,
//! [`crate::spare`]) has only the page left to make once it is given a tab.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use snowlark_ipc::wire;
use snowlark_net::NetClient;
use snowlark_paint::video::VideoStore;

use crate::raster_thread::RemoteRaster;
use crate::renderer_process::{LiveShare, TabDown, TabUp};
use crate::services::{self, RemoteServices, RendererPort, Up};
use crate::tab::{Assign, Begin, EventSink, RendererContext, Surface, TabCommand, TabEvent, TabId};
use crate::watchdog::{Liveness, LIVENESS_BYTES};

/// How long a renderer that is done waits for what it sent to leave.
const FLUSH_WAIT: Duration = Duration::from_secs(5);

/// Events up the pipe.
struct PipeSink(RendererPort);

impl EventSink for PipeSink {
    fn send(&self, id: TabId, event: TabEvent) {
        match services::pack(&self.0, &event) {
            Ok(event) => {
                // (a browser that is gone ends this process soon)
                let _ = self.0.send(&Up::Tab(TabUp::Event { tab: id, event }));
            }
            Err(e) => eprintln!("[tab {id}] an event does not go up: {e}"),
        }
    }
}

/// Runs the tab the browser starts on `port` with [`crate::page_driver::PageDriver`] until the browser
/// closes it or is gone. Returns the process's exit code.
pub fn run_renderer(port: RendererPort) -> i32 {
    // (a crash now has the browser write a minidump, when it launched this process with a folder
    // for them: `snowlark_ipc::Launch::crash_dumps`)
    snowlark_ipc::install_crash_handler();
    run_with(port, crate::page_driver::PageDriver::factory)
}

/// The command that repaints the tab, or its view `view`.
fn repaint_of(view: Option<TabId>) -> TabCommand {
    match view {
        Some(tab) => TabCommand::View {
            tab,
            command: Box::new(TabCommand::Repaint),
        },
        None => TabCommand::Repaint,
    }
}

/// Runs `videos.repool()` on a thread of its own (it calls the browser), then `then` if it asked for
/// any video's textures.
fn repool_then(videos: Arc<VideoStore>, then: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("snowlark video repool".into())
        .spawn(move || {
            if videos.repool() {
                then();
            }
        });
    if let Err(e) = spawned {
        eprintln!("[renderer] no thread for the videos' new textures: {e}");
    }
}

/// [`run_renderer`] with another driver.
pub(crate) fn run_with(port: RendererPort, factory: fn() -> crate::driver::DriverFactory) -> i32 {
    let services = Arc::new(RemoteServices::new(&port));
    let raster = RemoteRaster::new(port.clone());
    let views = crate::raster_thread::ViewSeams::default();
    let videos = VideoStore::exporting(services.video.clone());
    let (started, start) = mpsc::channel();
    let (commands, rx) = snowlark_ipc::channel::<TabCommand>();
    let own = commands.clone();
    {
        let (services, raster, port) = (services.clone(), raster.clone(), port.clone());
        let (views, videos) = (views.clone(), videos.clone());
        // A new raster thread is a new GPU process: the videos' textures were that process's,
        // so they ask for new ones (on a thread of their own: they call the browser). The page is
        // repainted at once, and again once they have them: the new process puts a video in the
        // lists it rasters only once it has made its textures, and a page that does not change is
        // not rastered again.
        let repaint = commands.clone();
        let repool = move |view: Option<TabId>| {
            let _ = repaint.send(repaint_of(view));
            let repaint = repaint.clone();
            repool_then(videos.clone(), move || {
                let _ = repaint.send(repaint_of(view));
            });
        };
        // (the page thread is busy when the browser asks to stop its script: this thread asks)
        let mut live: Option<Arc<Liveness>> = None;
        let spawned = std::thread::Builder::new()
            .name("snowlark renderer io".into())
            .spawn(move || {
                services.run_with(&port, &mut |message| match message {
                    TabDown::Start {
                        tab,
                        device,
                        live: share,
                    } => {
                        let l = Arc::new(liveness(&port, share));
                        live = Some(l.clone());
                        let _ = started.send(Begin::Tab((tab, device, l)));
                    }
                    TabDown::WarmUp => {
                        let _ = started.send(Begin::WarmUp);
                    }
                    TabDown::Terminate => {
                        if let Some(l) = &live {
                            l.kill();
                        }
                    }
                    TabDown::Command(bulk) => {
                        match port
                            .open_bulk(bulk)
                            .and_then(|bytes| wire::decode::<TabCommand>(&bytes))
                        {
                            Ok(command) => {
                                let _ = commands.send(command);
                            }
                            Err(e) => eprintln!("[renderer] a command does not decode: {e}"),
                        }
                    }
                    TabDown::Taken { view: None } => raster.taken(),
                    TabDown::Taken { view: Some(view) } => {
                        let seam = views
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&view)
                            .cloned();
                        // (a view gone meanwhile has nothing in flight to wait for)
                        if let Some(seam) = seam {
                            seam.taken();
                        }
                    }
                    TabDown::RasterNew { view: None } => {
                        raster.restarted();
                        repool(None);
                    }
                    TabDown::RasterNew { view: Some(view) } => {
                        let seam = views
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&view)
                            .cloned();
                        if let Some(seam) = seam {
                            seam.restarted();
                        }
                        repool(Some(view));
                    }
                    TabDown::RasterLink { view: None, end } => {
                        let commands = commands.clone();
                        raster.join_link(end, move |c| {
                            let _ = commands.send(c);
                        });
                    }
                    TabDown::RasterLink {
                        view: Some(view),
                        end,
                    } => {
                        let commands = commands.clone();
                        let command = move |c| {
                            let _ = commands.send(TabCommand::View {
                                tab: view,
                                command: Box::new(c),
                            });
                        };
                        // (it may come before the tab thread makes the view's seam: the seam is
                        // made here then, and the tab thread finds it)
                        let seam = views
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .entry(view)
                            .or_insert_with(|| RemoteRaster::of(port.clone(), Some(view)))
                            .clone();
                        seam.join_link(end, command);
                    }
                });
                // (the browser is gone: the tab closes as it would at its word)
                let _ = commands.send(TabCommand::Close);
            });
        if let Err(e) = spawned {
            eprintln!("[renderer] no IO thread: {e}");
            return 1;
        }
    }
    // The tab's thread starts now, before the browser says which tab this is: a spare renderer
    // waits warm (`docs/process-isolation.md` Â§11).
    let context = RendererContext {
        surface: Surface::Remote {
            port: port.clone(),
            raster,
            views,
        },
        videos: Some(videos),
        net: NetClient::with_backend(services.net.clone()),
        events: Arc::new(PipeSink(port.clone())),
        clipboard: services.clipboard.clone(),
        internal: services.internal.clone(),
        content_filter: Some(services.content_filter.clone()),
        audio: Some(services.audio.group().clone()),
        audio_out: Some(services.audio_out.clone()),
        broadcast: snowlark_page::BroadcastHub::with_backend(services.broadcast.clone()),
        web_storage: services.storage.clone(),
        idb: snowlark_page::IdbBackend::new(services.idb.clone()),
    };
    let tab = crate::tab::spawn_assigned(context, own, rx, factory(), Assign::Later(start));
    let _ = tab.join();
    let _ = port.flush(FLUSH_WAIT);
    0
}

/// The tab's liveness in the section the browser shared, on the browser's clock. One that does not
/// map is kept here only: the browser then never sees the tab stuck, and its Kill ends the process.
fn liveness(port: &RendererPort, share: LiveShare) -> Liveness {
    match port.map_section(share.section, LIVENESS_BYTES as u64) {
        Ok(section) => Liveness::shared_since(section, share.now),
        Err(e) => {
            eprintln!("[renderer] the liveness section does not map: {e}");
            Liveness::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_paint::video::{PoolHandles, VideoExport};
    use snowlark_paint::{Driver, VideoId};
    use std::sync::Mutex;

    /// A browser that takes a while to make a video's textures, and says when it is asked.
    struct SlowBrowser(Mutex<mpsc::Sender<&'static str>>);

    impl VideoExport for SlowBrowser {
        fn driver(&self) -> Option<Driver> {
            Some(Driver::Warp)
        }
        fn pool(&self, _: VideoId, _: u32, _: u32) -> Option<PoolHandles> {
            std::thread::sleep(Duration::from_millis(200));
            let _ = self.0.lock().unwrap().send("pool");
            None
        }
        fn frame(&self, _: VideoId, _: u32, _: u32) {}
        fn removed(&self, _: VideoId) {}
    }

    /// After a new GPU process, a renderer with videos repaints again once each video has asked
    /// for its new textures: the new process maps a video into the lists it rasters only once it
    /// made them. One without videos makes no device for them and does not.
    #[test]
    fn a_repaint_after_a_new_gpu_process_follows_the_videos_textures() {
        let (tx, rx) = mpsc::channel();
        let none = VideoStore::exporting(Arc::new(SlowBrowser(Mutex::new(tx.clone()))));
        let t = tx.clone();
        repool_then(none, move || {
            let _ = t.send("repaint without videos");
        });
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
        let store = VideoStore::exporting(Arc::new(SlowBrowser(Mutex::new(tx.clone()))));
        let Some(gpu) = store.gpu().cloned() else {
            return;
        };
        let slot = store.create();
        slot.upload(&gpu, 4, 4, &[0x80; 64]).unwrap();
        store.frame_ready(slot.id());
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok("pool"));
        repool_then(store, move || {
            let _ = tx.send("repaint");
        });
        let order: Vec<&str> = (0..2)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        assert_eq!(order, ["pool", "repaint"]);
    }
}
