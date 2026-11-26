// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compositor in the GPU process (`docs/process-isolation.md`, M7 stage 3, step 2).
//!
//! With `--gpu-process` each window's compositor ([`crate::composite`]'s `State`, with its vsync
//! thread) and its tabs' raster threads run in `--type=gpu`, on that process's device. The window
//! keeps its visual tree with a composition surface (step 1) and a [`CompositorThread`] whose input
//! a proxy thread here passes on up the GPU process's pipe ([`start_proxy`]):
//!
//! - plain messages encoded, as bulk, so a chrome or tools' list past a pipe frame still crosses;
//! - the window's resources (fonts, images of its own lists) as the raster seam sends them;
//! - a tab's pipe stays in the browser: the GPU process hears only the tab's id
//!   ([`GpuDown::Bind`]), and what its compositor tells the tab goes on the tab's seam link while
//!   it holds (G4 step 3), else comes back up ([`GpuUp::Tab`]), checked and passed on here;
//! - a composition surface's handle duplicated into the GPU process.
//!
//! A renderer's raster seam has a link to this process once one is ready (G4 step 2,
//! [`GpuDown::Link`]): a thread here per link feeds the tab's raster thread what comes on it, and
//! answers its jobs there. The link is the tab's, so its frames draw that tab only. Until the link
//! is joined, or after it ends, the seam ends at the browser as before, which passes each message
//! on with its section handed over unread ([`snowlark_ipc::Connection::pass_bulk`]). Either way the
//! raster thread checks each message, and one that fails the checks kills the renderer
//! ([`GpuUp::Refused`]).
//!
//! What the window reads back (each tab's scroll, presented frames) comes up as messages into the
//! window's tables ([`Routes`]). When the GPU process ends, a window waiting for a frame stops
//! waiting ([`CompEvent::Lost`]).
//!
//! A renderer's videos (G4 step 4, [`GpuVideos`]): this process's store makes their shared
//! textures on its device, at the browser's call, and gives their handles to the browser, which
//! takes them out of this process and hands them to the renderer. The renderer names each frame to
//! the browser, which passes it on; here it must name a texture of a pool made for that renderer's
//! video, else the renderer is ended ([`GpuUp::Refused`]). The raster threads map the video ids a
//! renderer's lists name through that renderer's table only, so a list can draw no other
//! renderer's video. The window's compositor here draws them, as the browser's did.
//!
//! A GPU process started again after the last ended (G5 step 1) gets every window and tab again
//! before anything else ([`Routes::replay`]): what the browser keeps of each window (its surfaces,
//! its own lists' resources still in use, the newest of each lasting message), its tabs' surfaces,
//! and their raster threads; each renderer is then told its raster thread is new, and sends its
//! fonts, images and a whole frame again, and its videos ask the new process for textures (G5
//! step 3).
//!
//! Past the crash limit ([`Routes::fall_back`]) the same record makes each open window's
//! compositor again in the browser, its tabs' raster threads with it, and the window's input goes
//! there from then on: open windows composite in the browser, as new ones do. The renderers'
//! videos, told their raster thread is new, ask for textures again: the window's store in the
//! browser makes them, and the raster threads there map each renderer's ids through its own table
//! of that store ([`GpuVideos`]).
//!
//! [`CompositorThread`]: crate::composite::CompositorThread

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use snowlark_ipc::{ProcessKind, ProcessPolicy};
use snowlark_paint::video::{PoolHandles, VideoStore};
use snowlark_paint::{Gpu, VideoId};
use snowlark_tab::raster_thread::{
    GpuRaster, SeamDown, SeamMsg, SeamRaster, SeamServer, SEAM_BYTES_WAIT, SEAM_CONNECT_WAIT,
    SEAM_RING,
};
use snowlark_tab::services::video::{LocalVideos, VideoBackend};
use snowlark_tab::services::BrowserPort;
use snowlark_tab::watchdog::{PassMark, Passes, PASSES_BYTES};

use crate::composite::{
    CompEvent, CompInput, CompMsg, Handle, ScrollView, SharedScroll, WindowId, WindowOut, Windows,
};
use crate::gpu_process::{GpuChildPort, GpuDown, GpuLink, GpuPort, GpuUp, MadePool, PoolMade};
use crate::tab::{TabCommand, TabId};

/// How long a renderer's raster message waits for a GPU process that is starting.
const READY_WAIT: Duration = Duration::from_secs(20);

/// The browser's tables for what the GPU process says, by window and by tab.
#[derive(Default)]
pub(crate) struct Routes(Mutex<RouteTable>);

#[derive(Default)]
struct RouteTable {
    windows: HashMap<WindowId, WindowRoute>,
    rasters: HashMap<TabId, RasterRoute>,
    /// Past the crash limit, each renderer's videos (by the tab the browser names the renderer by)
    /// in the browser's own store: their textures are made there, as without a GPU process.
    videos: HashMap<TabId, Arc<LocalVideos>>,
}

/// Renderer `renderer`'s videos in the browser's store of window `window` (`windows`' record of
/// it), made on the first ask; none without that store.
fn local_videos(
    videos: &mut HashMap<TabId, Arc<LocalVideos>>,
    windows: &HashMap<WindowId, WindowRoute>,
    renderer: TabId,
    window: WindowId,
) -> Option<Arc<LocalVideos>> {
    if let Some(v) = videos.get(&renderer) {
        return Some(v.clone());
    }
    let store = windows.get(&window)?.replay.videos.clone()?;
    let v = Arc::new(LocalVideos::new(store));
    videos.insert(renderer, v.clone());
    Some(v)
}

struct WindowRoute {
    events: snowlark_ipc::Sender<CompEvent>,
    scroll: SharedScroll,
    /// The window's tabs' pipes: the only tabs its compositor may speak to.
    tabs: HashMap<TabId, snowlark_ipc::Sender<TabCommand>>,
    /// What makes the window's compositor again in a GPU process started after the last ended.
    replay: Replay,
    /// Why the GPU process could not make the window's compositor ([`GpuUp::NoCompositor`]);
    /// cleared for a GPU process started again.
    no_compositor: Option<String>,
    /// What the window's compositor is made of in the browser, past the crash limit.
    in_browser: InBrowser,
    /// The window's compositor in the browser, once no more GPU processes are started.
    local: Option<Local>,
}

/// What a window's compositor in the browser's own process is made of, should the GPU process
/// fall back (`docs/process-isolation.md`, M7 stage 5): the browser's device, the window, and the
/// other windows' compositors.
pub(crate) struct InBrowser {
    pub gpu: Arc<Gpu>,
    pub window: snowlark_paint::window::WindowHandle,
    pub windows: Windows,
}

/// A window's compositor in the browser, made past the crash limit: its input, and its thread,
/// which is asked to quit and joined when this goes.
struct Local {
    tx: Sender<CompInput>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Local {
    fn drop(&mut self) {
        let _ = self.tx.send(CompMsg::Quit.into());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct RasterRoute {
    taken: Arc<dyn Fn() + Send + Sync>,
    refused: Box<dyn Fn(String) + Send + Sync>,
    /// The window the tab's raster thread draws for, and whose videos its lists name.
    window: WindowId,
    videos: TabId,
    /// Tells the renderer its raster thread is new (a GPU process started again).
    restarted: Arc<dyn Fn() + Send + Sync>,
    /// The tab's raster thread in the browser, once no more GPU processes are started.
    local: Option<Arc<SeamRaster>>,
}

/// What a window's compositor in the GPU process was given that lasts, kept by the browser so that
/// a GPU process started after the last ended gets it again (`docs/process-isolation.md`, M7 stage
/// 5, step 1): the composition surfaces, the resources of the window's own lists still in use, and
/// the newest of each message whose effect lasts. What is passed up is put here first, so a GPU
/// process that is ready meanwhile gets it at worst twice, never not at all.
#[derive(Default)]
struct Replay {
    /// By the window shown (the window itself, the undocked developer tools'): its visual tree,
    /// which gives a GPU process started again a new surface (a surface takes one swap chain for
    /// good, G5 step 2).
    surfaces: HashMap<WindowId, snowlark_paint::compositor::WindowSurface>,
    fonts: std::collections::BTreeMap<snowlark_paint::FontId, snowlark_paint::Resource>,
    images: std::collections::BTreeMap<snowlark_paint::ImageId, snowlark_paint::Resource>,
    /// In the order they are given again.
    state: std::collections::BTreeMap<Lasting, CompMsg>,
    /// The browser's own video store, which a compositor in the browser draws from.
    videos: Option<Arc<VideoStore>>,
}

/// A message whose effect lasts, by what it sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Lasting {
    Resize,
    Chrome,
    Overlay,
    PageArea,
    Dark,
    ScrollbarActive,
    Activate,
    Devtools,
    Forward(TabId),
    BeginFrames(TabId),
}

impl Replay {
    /// Keeps `msg`, a message to window `window`'s compositor, when its effect lasts.
    fn message(&mut self, window: WindowId, msg: CompMsg) {
        let key = match &msg {
            CompMsg::Resize { .. } => {
                let CompMsg::Resize {
                    width,
                    height,
                    chrome_height,
                    dpi_scale,
                    chrome,
                    overlay,
                } = msg
                else {
                    unreachable!()
                };
                // (the chrome and overlay it carries are the newest, kept as their own)
                if let Some((list, background)) = chrome {
                    self.state
                        .insert(Lasting::Chrome, CompMsg::Chrome(list, background));
                }
                if let Some(list) = overlay {
                    self.state.insert(Lasting::Overlay, CompMsg::Overlay(list));
                }
                let size = CompMsg::Resize {
                    width,
                    height,
                    chrome_height,
                    dpi_scale,
                    chrome: None,
                    overlay: None,
                };
                self.state.insert(Lasting::Resize, size);
                return;
            }
            CompMsg::Chrome(..) => Lasting::Chrome,
            CompMsg::Overlay(_) => Lasting::Overlay,
            CompMsg::PageArea(_) => Lasting::PageArea,
            CompMsg::Dark(_) => Lasting::Dark,
            CompMsg::ScrollbarActive(_) => Lasting::ScrollbarActive,
            CompMsg::Activate(_) => Lasting::Activate,
            CompMsg::DevtoolsWindow(Some(_)) => Lasting::Devtools,
            CompMsg::DevtoolsWindow(None) => {
                // (the tools' window went, and its surface with it)
                self.state.remove(&Lasting::Devtools);
                self.surfaces.retain(|shown, _| *shown == window);
                return;
            }
            CompMsg::Forward {
                tab,
                window: Some(_),
            } => Lasting::Forward(*tab),
            CompMsg::Forward { tab, window: None } => {
                self.state.remove(&Lasting::Forward(*tab));
                return;
            }
            CompMsg::NeedsBeginFrames { tab, .. } => Lasting::BeginFrames(*tab),
            CompMsg::RemoveTab(tab) => {
                self.state.remove(&Lasting::Forward(*tab));
                self.state.remove(&Lasting::BeginFrames(*tab));
                return;
            }
            // (moments: scrolls, waits, an autoscroll's step; the scroll positions come again with
            // each tab's frame)
            _ => return,
        };
        self.state.insert(key, msg);
    }

    /// Keeps `resource`, for the window's own lists, while it is in use.
    fn resource(&mut self, resource: &snowlark_paint::Resource) {
        use snowlark_paint::Resource;
        match resource {
            Resource::FontAdd { id, .. } => {
                self.fonts.insert(*id, resource.clone());
            }
            Resource::ImageAdd { id, .. } => {
                self.images.insert(*id, resource.clone());
            }
            Resource::ImageRemove(id) => {
                self.images.remove(id);
            }
            Resource::ImagesClear => self.images.clear(),
        }
    }

    /// Gives window `window`'s compositor, whose tabs are `tabs`, to the GPU process at `port`;
    /// `scroll` has where each tab was last scrolled to.
    fn send(
        &self,
        port: &GpuPort,
        window: WindowId,
        tabs: impl Iterator<Item = TabId>,
        scroll: &SharedScroll,
    ) -> Result<(), snowlark_ipc::Error> {
        let comp = |msg: &CompMsg| -> Result<(), snowlark_ipc::Error> {
            let bytes = snowlark_ipc::wire::encode(msg)?;
            port.send(&GpuDown::Comp {
                window,
                msg: port.bulk(&bytes)?,
            })
        };
        port.send(&GpuDown::Window { window })?;
        for (shown, tree) in &self.surfaces {
            #[cfg(windows)]
            let handle = tree.renew().map_err(snowlark_ipc::Error::Os)?;
            // (no window has a composition surface there: `renew` cannot make one)
            #[cfg(not(windows))]
            let Ok(handle) = tree.renew() else {
                continue;
            };
            let surface = port.duplicate_to_child(&handle)?;
            port.send(&GpuDown::Surface {
                to: window,
                window: *shown,
                surface,
            })?;
        }
        for r in self.fonts.values().chain(self.images.values()) {
            let msg = SeamMsg::resource(r.clone(), |bytes| port.bulk(&bytes))?;
            port.send(&GpuDown::Resource { window, msg })?;
        }
        // (a tab's surface before what names the tab)
        let tabs: Vec<TabId> = tabs.collect();
        for &tab in &tabs {
            port.send(&GpuDown::Bind { window, tab })?;
        }
        for msg in self.state.values() {
            comp(msg)?;
        }
        for msg in places(&tabs, scroll) {
            comp(&msg)?;
        }
        Ok(())
    }

    /// Gives window `window`'s compositor made in the browser (input `tx`) what the one in the GPU
    /// process had, as [`Replay::send`] does: `tabs` are the window's tabs' pipes.
    fn give(
        &self,
        tx: &Sender<CompInput>,
        tabs: &HashMap<TabId, snowlark_ipc::Sender<TabCommand>>,
        scroll: &SharedScroll,
    ) {
        let give = |input: CompInput| {
            let _ = tx.send(input);
        };
        for (shown, tree) in &self.surfaces {
            // (a surface takes one swap chain for good: the ended process's had it)
            match tree.renew() {
                Ok(h) => give(CompInput::surface(*shown, h)),
                Err(e) => eprintln!("[gpu] no new surface for a window: {e}"),
            }
        }
        if let Some(v) = &self.videos {
            give(CompInput::videos(v.clone()));
        }
        for r in self.fonts.values().chain(self.images.values()) {
            give(CompInput::Resource(r.clone()));
        }
        for (tab, pipe) in tabs {
            give(CompInput::bind(*tab, pipe.clone()));
        }
        for msg in self.state.values() {
            give(CompInput::Msg(msg.clone()));
        }
        let tabs: Vec<TabId> = tabs.keys().copied().collect();
        for msg in places(&tabs, scroll) {
            give(CompInput::Msg(msg));
        }
    }
}

/// Where each of `tabs` was going, as a scroll to give a compositor made again: its first frame
/// there is placed so. A frame its renderer painted before it heard of the new compositor carries
/// no place of its own, and would otherwise be taken for the top of the page (and the tab told
/// so). A smooth scroll under way ends where it was headed (the wheel turned counts in full).
fn places(tabs: &[TabId], scroll: &SharedScroll) -> Vec<CompMsg> {
    let scroll = scroll.lock().unwrap_or_else(|e| e.into_inner());
    tabs.iter()
        .filter_map(|t| {
            scroll.get(t).map(|v| CompMsg::ScrollTo {
                tab: *t,
                to: v.target,
                smooth: false,
                with: None,
            })
        })
        .collect()
}

impl Routes {
    fn table(&self) -> std::sync::MutexGuard<'_, RouteTable> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes one message of the GPU process's (checked by [`GpuUp`]'s `Validate`). What names a
    /// window or tab the browser does not route there is dropped: it went meanwhile.
    pub(crate) fn deliver(&self, up: GpuUp) {
        let table = self.table();
        match up {
            GpuUp::Ready { .. } => {}
            GpuUp::Presented { window, seq } => {
                if let Some(w) = table.windows.get(&window) {
                    let _ = w.events.send(CompEvent::Presented(seq));
                }
            }
            GpuUp::Scroll { window, tab, view } => {
                if let Some(w) = table.windows.get(&window) {
                    if !w.tabs.contains_key(&tab) && view.is_some() {
                        return;
                    }
                    let mut scroll = w.scroll.lock().unwrap();
                    match view {
                        Some(v) => scroll.insert(tab, v),
                        None => scroll.remove(&tab),
                    };
                }
            }
            GpuUp::Tab {
                window,
                tab,
                command,
            } => {
                if let Some(pipe) = table.windows.get(&window).and_then(|w| w.tabs.get(&tab)) {
                    let _ = pipe.send(command);
                }
            }
            GpuUp::Taken { tab } => {
                if let Some(r) = table.rasters.get(&tab) {
                    (r.taken)();
                }
            }
            GpuUp::Refused { tab, why } => {
                if let Some(r) = table.rasters.get(&tab) {
                    (r.refused)(why);
                }
            }
            GpuUp::NoCompositor { window, why } => {
                drop(table);
                eprintln!("[gpu] a window has no compositor in the GPU process: {why}");
                if let Some(w) = self.table().windows.get_mut(&window) {
                    w.no_compositor = Some(why);
                }
            }
        }
    }

    /// Why window `window` has no compositor in the GPU process now, if it has none.
    #[cfg_attr(not(all(test, windows)), allow(dead_code))]
    pub(crate) fn no_compositor(&self, window: WindowId) -> Option<String> {
        self.table().windows.get(&window)?.no_compositor.clone()
    }

    /// The GPU process ended: every window stops waiting for a frame.
    pub(crate) fn lost(&self) {
        for w in self.table().windows.values() {
            let _ = w.events.send(CompEvent::Lost);
        }
    }

    /// Gives a GPU process started after the last ended, at `port`, every window and tab raster
    /// thread again (G5 step 1), ahead of anything else. Returns what tells each tab's renderer its
    /// raster thread is new, to be run once the process is the one the windows send to.
    pub(crate) fn replay(&self, port: &GpuPort) -> Vec<Arc<dyn Fn() + Send + Sync>> {
        let mut table = self.table();
        for (window, w) in &mut table.windows {
            w.no_compositor = None;
            if let Err(e) = w
                .replay
                .send(port, *window, w.tabs.keys().copied(), &w.scroll)
            {
                eprintln!("[gpu] a window does not go to the new GPU process: {e}");
            }
        }
        let mut restarted = Vec::new();
        for (tab, r) in &table.rasters {
            let start = GpuDown::RasterStart {
                tab: *tab,
                window: r.window,
                videos: r.videos,
            };
            if port.send(&start).is_ok() {
                restarted.push(r.restarted.clone());
            }
        }
        restarted
    }

    /// Runs `f` on window `window`'s record, while the window is routed and its compositor is in
    /// the GPU process. With its compositor in the browser it returns that compositor's input
    /// instead: under one lock with [`Routes::fall_back`], so what the window sends is either kept
    /// for the compositor made in the browser or sent to it, never neither.
    fn keep(
        &self,
        window: WindowId,
        f: impl FnOnce(&mut WindowRoute),
    ) -> Option<Sender<CompInput>> {
        let mut table = self.table();
        let w = table.windows.get_mut(&window)?;
        if let Some(local) = &w.local {
            return Some(local.tx.clone());
        }
        f(w);
        None
    }

    /// No more GPU processes are started (G5): each window's compositor is made again in the
    /// browser and given what the last one had (as a GPU process started again is: [`Replay`]),
    /// each tab's raster thread with it, and each renderer is told its raster thread is new, so it
    /// sends a whole frame again. Windows and tabs already moved are left as they are.
    pub(crate) fn fall_back(&self) {
        let mut restarted = Vec::new();
        {
            let mut table = self.table();
            let RouteTable {
                windows,
                rasters,
                videos,
            } = &mut *table;
            for w in windows.values_mut().filter(|w| w.local.is_none()) {
                w.no_compositor = None;
                let (tx, rx) = std::sync::mpsc::channel();
                let InBrowser {
                    gpu,
                    window,
                    windows: all,
                } = &w.in_browser;
                let out = crate::composite::local_out(w.scroll.clone(), w.events.clone());
                let thread = crate::composite::spawn(
                    gpu.clone(),
                    *window,
                    out,
                    all.clone(),
                    tx.clone(),
                    rx,
                    PassMark::default(),
                );
                w.replay.give(&tx, &w.tabs, &w.scroll);
                w.local = Some(Local {
                    tx,
                    thread: Some(thread),
                });
            }
            for (tab, r) in rasters.iter_mut().filter(|(_, r)| r.local.is_none()) {
                let Some(w) = windows.get(&r.window) else {
                    continue;
                };
                let Some(local) = &w.local else { continue };
                // (the renderer's videos are made again in the browser's store as it repools them:
                // its lists name them through its own table there, so they can name no other
                // renderer's)
                let ids: snowlark_tab::services::video::VideoIds =
                    match local_videos(videos, windows, r.videos, r.window) {
                        Some(v) => v.ids(),
                        None => Arc::new(|_| VideoId(0)),
                    };
                r.local = SeamRaster::start_marked(
                    *tab,
                    w.in_browser.gpu.clone(),
                    local.tx.clone(),
                    r.taken.clone(),
                    Some(ids),
                    PassMark::default(),
                )
                .map(Arc::new);
                restarted.push(r.restarted.clone());
            }
        }
        for told in restarted {
            told();
        }
    }
}

/// Starts the thread that passes window `window`'s compositor input from `rx` on to the GPU
/// process at `link`; what comes back for the window goes to `events` and `scroll`. Past the crash
/// limit the window's compositor is made again in the browser, of `in_browser`, and the thread
/// passes the input on to it ([`Routes::fall_back`]).
pub(crate) fn start_proxy(
    link: GpuLink,
    window: WindowId,
    rx: Receiver<CompInput>,
    events: snowlark_ipc::Sender<CompEvent>,
    scroll: SharedScroll,
    in_browser: InBrowser,
) -> JoinHandle<()> {
    link.routes().table().windows.insert(
        window,
        WindowRoute {
            events: events.clone(),
            scroll,
            tabs: HashMap::new(),
            replay: Replay::default(),
            no_compositor: None,
            in_browser,
            local: None,
        },
    );
    link.send(GpuDown::Window { window });
    // (the GPU process fell back while the window was being made: it was not in the routes then)
    if link.status() == crate::gpu_process::GpuStatus::FellBack {
        link.routes().fall_back();
    }
    std::thread::Builder::new()
        .name("snowlark compositor proxy".into())
        .spawn(move || {
            let mut proxy = Proxy {
                link,
                window,
                events,
                noted: Noted::default(),
                local: None,
            };
            for input in rx {
                if !proxy.pass(input) {
                    break;
                }
            }
            // (a compositor made in the browser is joined here, out of the routes' lock)
            let route = proxy.link.routes().table().windows.remove(&window);
            drop(route);
        })
        .expect("compositor proxy thread")
}

struct Proxy {
    link: GpuLink,
    window: WindowId,
    events: snowlark_ipc::Sender<CompEvent>,
    noted: Noted,
    /// The window's compositor in the browser, once it is there.
    local: Option<Sender<CompInput>>,
}

/// What the proxy has said once it cannot pass on.
#[derive(Default)]
struct Noted {
    frames: bool,
}

impl Proxy {
    /// Records `f` for the window, unless its compositor is in the browser now: true then, and
    /// what was not recorded goes there instead ([`Routes::keep`]).
    fn keep(&mut self, f: impl FnOnce(&mut WindowRoute)) -> bool {
        if self.local.is_none() {
            self.local = self.link.routes().keep(self.window, f);
        }
        self.local.is_some()
    }

    /// Passes `input` on to the window's compositor in the browser.
    fn to_local(&self, input: CompInput) {
        if let Some(local) = &self.local {
            let _ = local.send(input);
        }
    }

    /// Passes one input on; false when the window closed.
    fn pass(&mut self, input: CompInput) -> bool {
        let window = self.window;
        if let CompInput::Msg(CompMsg::Quit) = input {
            // (unrouted first: a GPU process started meanwhile does not get it again; a
            // compositor in the browser is asked to quit and joined, out of the routes' lock)
            let route = self.link.routes().table().windows.remove(&window);
            match route {
                Some(WindowRoute { local: Some(_), .. }) => drop(route),
                _ => self.link.send(GpuDown::CloseWindow { window }),
            }
            return false;
        }
        if self.keep(|_| {}) {
            self.to_local(input);
            return true;
        }
        match input {
            CompInput::Msg(CompMsg::Quit) => unreachable!(),
            // (a frame of the browser's own store: the GPU process's store tells its compositors of
            // its videos' frames, and has ids of its own)
            CompInput::Msg(CompMsg::VideoFrame(_)) => {}
            CompInput::Msg(msg) => {
                let ack = match msg {
                    CompMsg::Ack(seq) => Some(seq),
                    _ => None,
                };
                let bytes = snowlark_ipc::wire::encode(&msg).ok();
                let mut kept = Some(msg);
                let local = self.keep(|w| {
                    let Some(msg) = kept.take() else { return };
                    if let CompMsg::RemoveTab(tab) = &msg {
                        w.tabs.remove(tab);
                    }
                    w.replay.message(window, msg);
                });
                if let Some(msg) = kept.filter(|_| local) {
                    self.to_local(CompInput::Msg(msg));
                    return true;
                }
                let sent = bytes.and_then(|bytes| {
                    // (bytes past the inline cap go in a section made for the process that
                    // gets them: one starting is waited for)
                    self.link.with_port(READY_WAIT, |port| {
                        port.send(&GpuDown::Comp {
                            window,
                            msg: port.bulk(&bytes)?,
                        })
                    })
                });
                match sent {
                    Some(Ok(())) => {}
                    _ if ack.is_some() => {
                        let _ = self.events.send(CompEvent::Lost);
                    }
                    Some(Err(e)) => eprintln!("[gpu] a window message does not go up: {e}"),
                    None => {}
                }
            }
            CompInput::Handles(msg, handles) => self.handles(msg, handles),
            CompInput::Resource(r) => {
                if self.keep(|w| w.replay.resource(&r)) {
                    self.to_local(CompInput::Resource(r));
                    return true;
                }
                let sent = self.link.with_port(READY_WAIT, |port| {
                    let msg = SeamMsg::resource(r, |bytes| port.bulk(&bytes))?;
                    port.send(&GpuDown::Resource { window, msg })
                });
                if let Some(Err(e)) = sent {
                    eprintln!("[gpu] a window resource does not go up: {e}");
                }
            }
            CompInput::Frame(_, data) => {
                // (only renderers' frames are drawn there: none comes from this process's tabs
                // with a GPU process)
                if !std::mem::replace(&mut self.noted.frames, true) {
                    eprintln!("[gpu] a frame drawn in the browser is not shown with a GPU process");
                }
                data.recycle();
            }
            CompInput::VSync(..) => {}
            // (a GPU process shows no protected player: their pictures are the browser's)
            CompInput::Protected(_) => {}
        }
        true
    }

    fn handles(&mut self, msg: CompMsg, handles: Vec<Handle>) {
        let window = self.window;
        let mut handles: Vec<Option<Handle>> = handles.into_iter().map(Some).collect();
        let mut take = |i: u32| handles.get_mut(i as usize).and_then(Option::take);
        match msg {
            CompMsg::Bind { tab, pipe } => {
                let Some(Handle::Pipe(tx)) = take(pipe) else {
                    return;
                };
                if self.keep(|w| {
                    w.tabs.insert(tab, tx.clone());
                }) {
                    return self.to_local(CompInput::bind(tab, tx));
                }
                self.link.send(GpuDown::Bind { window, tab });
            }
            CompMsg::Surface {
                window: shown,
                handle,
            } => {
                let Some(Handle::Tree(tree)) = take(handle) else {
                    return;
                };
                let h = match tree.handle() {
                    Ok(h) => h,
                    Err(e) => return eprintln!("[gpu] a window's surface has no handle: {e}"),
                };
                if self.keep(|w| {
                    w.replay.surfaces.insert(shown, tree.clone());
                }) {
                    return self.to_local(CompInput::tree(shown, tree));
                }
                let given = self.link.with_port(READY_WAIT, |port| {
                    let surface = port.duplicate_to_child(&h)?;
                    port.send(&GpuDown::Surface {
                        to: window,
                        window: shown,
                        surface,
                    })
                });
                match given {
                    Some(Ok(())) => {}
                    Some(Err(e)) => eprintln!("[gpu] a window's surface does not go over: {e}"),
                    None => eprintln!("[gpu] no GPU process for a window's surface"),
                }
            }
            // (the renderers' videos are the GPU process's store's, G4 step 4; the browser's own
            // store is kept for a compositor made in the browser)
            CompMsg::Videos { store } => {
                let Some(Handle::Videos(store)) = take(store) else {
                    return;
                };
                if self.keep(|w| w.replay.videos = Some(store.clone())) {
                    self.to_local(CompInput::videos(store));
                }
            }
            _ => {}
        }
    }
}

/// The GPU process's raster threads for window `window`'s renderers.
pub(crate) fn window_raster(link: GpuLink, window: WindowId) -> Arc<dyn GpuRaster> {
    Arc::new(WindowRaster { link, window })
}

struct WindowRaster {
    link: GpuLink,
    window: WindowId,
}

impl GpuRaster for WindowRaster {
    fn start(
        &self,
        tab: TabId,
        videos: TabId,
        taken: Box<dyn Fn() + Send + Sync>,
        refused: Box<dyn Fn(String) + Send + Sync>,
        restarted: Box<dyn Fn() + Send + Sync>,
    ) {
        let route = RasterRoute {
            taken: Arc::from(taken),
            refused,
            window: self.window,
            videos,
            restarted: Arc::from(restarted),
            local: None,
        };
        self.link.routes().table().rasters.insert(tab, route);
        self.link.send(GpuDown::RasterStart {
            tab,
            window: self.window,
            videos,
        });
        // (past the crash limit: a raster thread in the browser)
        if self.link.status() == crate::gpu_process::GpuStatus::FellBack {
            self.link.routes().fall_back();
        }
    }

    fn videos(&self, tab: TabId) -> Arc<dyn VideoBackend> {
        Arc::new(GpuVideos {
            link: self.link.clone(),
            tab,
            window: self.window,
            launches: Mutex::default(),
        })
    }

    fn send(
        &self,
        tab: TabId,
        from: &BrowserPort,
        message: SeamMsg,
    ) -> Result<(), snowlark_ipc::Error> {
        let local = self
            .link
            .routes()
            .table()
            .rasters
            .get(&tab)
            .and_then(|r| r.local.clone());
        if let Some(local) = local {
            // (past the crash limit: the tab's raster thread is the browser's, which checks the
            // message as the GPU process's did)
            return match local.receive(message, |b| from.open_bulk(b)) {
                Ok(()) => Ok(()),
                Err(why) => {
                    let why: String = why.chars().take(256).collect();
                    if let Some(r) = self.link.routes().table().rasters.get(&tab) {
                        (r.refused)(why.clone());
                    }
                    Err(snowlark_ipc::Error::BadMessage(why))
                }
            };
        }
        let job = message.is_job();
        let sent = self.link.with_port(READY_WAIT, |port| {
            let msg = message.pass(from, port)?;
            port.send(&GpuDown::Raster { tab, msg })
        });
        match sent {
            Some(Ok(())) => Ok(()),
            // (the renderer broke a rule: it is ended)
            Some(Err(e @ snowlark_ipc::Error::BadMessage(_))) => Err(e),
            _ => {
                // (no GPU process takes it: the renderer may send its next frame)
                if job {
                    if let Some(r) = self.link.routes().table().rasters.get(&tab) {
                        (r.taken)();
                    }
                }
                Ok(())
            }
        }
    }

    fn end(&self, tab: TabId) {
        self.link.routes().table().rasters.remove(&tab);
        self.link.send(GpuDown::RasterEnd { tab });
    }

    fn link(&self, tab: TabId, renderer: &BrowserPort) -> Option<snowlark_ipc::LinkEnd> {
        // (under the GPU link's lock: the end goes to the process the link was made for, after
        // the tab's `RasterStart`)
        let made = self.link.with_port(READY_WAIT, |gpu| {
            let [mine, theirs] = renderer.link(gpu, [SEAM_RING, 0])?;
            // (the renderer's end goes to it all the same, to be closed there: a GPU process that
            // did not get its end is ending, and the renderer's join fails at once)
            let _ = gpu.send(&GpuDown::Link { tab, end: theirs });
            Ok::<_, snowlark_ipc::Error>(mine)
        });
        match made {
            Some(Ok(end)) => Some(end),
            Some(Err(e)) => {
                eprintln!("[gpu] no link for tab {tab}'s raster seam: {e}");
                None
            }
            None => None,
        }
    }
}

/// How long the browser waits for a video's textures.
const POOL_WAIT: Duration = Duration::from_secs(5);

/// The browser's end of one renderer's videos in the GPU process (G4 step 4): the renderer of tab
/// `tab`, in window `window`. [`snowlark_tab::services::video::VideoHost`] has checked what the
/// renderer said first. Past the crash limit the textures are made in the window's store in the
/// browser ([`LocalVideos`], kept in the [`Routes`] for the raster threads made there), and the
/// frames go there: the renderer asks for them again when it is told its raster thread is new.
struct GpuVideos {
    link: GpuLink,
    tab: TabId,
    window: WindowId,
    /// The launch of the GPU process that made each video's textures: its frames go to that
    /// process only (another has no such video, and would end the renderer for naming it).
    launches: Mutex<HashMap<u32, u64>>,
}

impl GpuVideos {
    fn launches(&self) -> std::sync::MutexGuard<'_, HashMap<u32, u64>> {
        self.launches.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The renderer's videos in the browser's store, made now if need be, once no more GPU
    /// processes are started.
    fn fallen_back(&self) -> Option<Arc<LocalVideos>> {
        if self.link.status() != crate::gpu_process::GpuStatus::FellBack {
            return None;
        }
        let mut table = self.link.routes().table();
        let RouteTable {
            windows, videos, ..
        } = &mut *table;
        local_videos(videos, windows, self.tab, self.window)
    }

    /// The renderer's videos in the browser's store, if any were made there.
    fn in_browser(&self) -> Option<Arc<LocalVideos>> {
        self.link.routes().table().videos.get(&self.tab).cloned()
    }
}

impl VideoBackend for GpuVideos {
    fn software(&self) -> Option<bool> {
        if let Some(local) = self.fallen_back() {
            return local.software();
        }
        match self.link.ready(READY_WAIT) {
            Some((_, _, software)) => Some(software),
            // (it fell back meanwhile)
            None => self.fallen_back()?.software(),
        }
    }

    fn pool(&self, id: u32, width: u32, height: u32) -> Option<PoolHandles> {
        let ready = match self.fallen_back() {
            Some(_) => None,
            None => self.link.ready(READY_WAIT),
        };
        let Some((port, launch, _)) = ready else {
            let pool = self.fallen_back()?.pool(id, width, height)?;
            // (its frames go to the browser's store from now on)
            self.launches().remove(&id);
            return Some(pool);
        };
        let ask = GpuDown::VideoPool {
            tab: self.tab,
            id,
            width,
            height,
        };
        // (an answer that breaks a rule ends the GPU process)
        let made = port
            .call::<PoolMade>(&ask, POOL_WAIT)
            .and_then(|m| port.check(m));
        let made = match made {
            Ok(PoolMade(Some(made))) => made,
            Ok(PoolMade(None)) => return None,
            Err(e) => {
                eprintln!("[gpu] no textures for tab {}'s video: {e}", self.tab);
                return None;
            }
        };
        // (each handle the GPU process gave is taken, so none stays open there)
        let handles: Vec<_> = made
            .handles
            .into_iter()
            .map(|h| port.take_from_child(h))
            .collect();
        let handles = handles.into_iter().collect::<Result<Vec<_>, _>>();
        match handles {
            Ok(handles) => {
                self.launches().insert(id, launch);
                Some(PoolHandles {
                    generation: made.generation,
                    handles,
                })
            }
            Err(e) => {
                eprintln!("[gpu] a video texture does not come out of the GPU process: {e}");
                None
            }
        }
    }

    fn frame(&self, id: u32, generation: u32, index: u32) -> bool {
        let launch = self.launches().get(&id).copied();
        if let Some(launch) = launch {
            let frame = GpuDown::VideoFrame {
                tab: self.tab,
                id,
                generation,
                index,
            };
            self.link.send_to(launch, frame);
        } else if let Some(local) = self.in_browser() {
            // (checked here, as without a GPU process)
            return local.frame(id, generation, index);
        }
        // (checked there: a texture it was not given ends it through the tab's `refused`)
        true
    }

    fn removed(&self, id: u32) {
        let launch = self.launches().remove(&id);
        if let Some(launch) = launch {
            let tab = self.tab;
            self.link.send_to(launch, GpuDown::VideoRemoved { tab, id });
        }
        if let Some(local) = self.in_browser() {
            local.removed(id);
        }
    }

    fn gone(&self) {
        let launches: std::collections::HashSet<u64> =
            self.launches().drain().map(|(_, l)| l).collect();
        for launch in launches {
            self.link
                .send_to(launch, GpuDown::VideosGone { tab: self.tab });
        }
        let local = self.link.routes().table().videos.remove(&self.tab);
        if let Some(local) = local {
            local.gone();
        }
    }

    fn ids(&self) -> snowlark_tab::services::video::VideoIds {
        // (the GPU process's raster threads map them: none draws here)
        Arc::new(|_| snowlark_paint::VideoId(0))
    }
}

/// The GPU process's videos: the renderers' ids of each (by the tab the browser named the renderer
/// by) in its store, and the window each renderer's tab draws in.
#[derive(Default)]
struct VideoTable {
    of: HashMap<TabId, HashMap<u32, VideoId>>,
    owner: HashMap<u32, TabId>,
    window: HashMap<TabId, WindowId>,
}

type SharedVideos = Arc<Mutex<VideoTable>>;

fn videos_of(table: &SharedVideos) -> std::sync::MutexGuard<'_, VideoTable> {
    table.lock().unwrap_or_else(|e| e.into_inner())
}

/// The GPU process's end: its windows' compositors and its tabs' raster threads.
pub(crate) struct Child {
    gpu: Arc<Gpu>,
    port: GpuChildPort,
    /// The compositors, by window (another window's is found here: [`CompMsg::Forward`]).
    windows: Windows,
    threads: HashMap<WindowId, JoinHandle<()>>,
    /// Window inputs made before their window's compositor runs (a raster thread may start first).
    waiting: HashMap<WindowId, Receiver<CompInput>>,
    rasters: HashMap<TabId, TabRaster>,
    /// The tabs' seam links, for the compositors to speak to their tabs on ([`GpuDown::Bind`]).
    links: Arc<Mutex<HashMap<TabId, LinkSlot>>>,
    /// Where the threads mark their passes, once the browser has shared the page.
    passes: Option<Arc<Passes>>,
    /// The renderers' videos: their textures, on this process's device, which every window's
    /// compositor draws from.
    videos: Arc<VideoStore>,
    video_ids: SharedVideos,
}

/// A tab's raster thread here, and its seam's link to the renderer once it has one.
struct TabRaster {
    raster: Arc<SeamRaster>,
    link: LinkSlot,
}

/// Where a tab's raster thread answers a job: its link while it holds, else the browser.
type LinkSlot = Arc<Mutex<LinkState>>;

#[derive(Default)]
enum LinkState {
    /// None joined yet.
    #[default]
    Waiting,
    Linked(SeamServer),
    /// The link ended, or the raster thread did: none is joined again.
    Ended,
}

impl LinkState {
    /// Ends it (and the link, whose thread then ends).
    fn end(&mut self) {
        if let LinkState::Linked(link) = std::mem::replace(self, LinkState::Ended) {
            link.close();
        }
    }
}

impl Drop for TabRaster {
    fn drop(&mut self) {
        // (the link's thread ends, and with it its hold on the raster thread)
        self.link.lock().unwrap_or_else(|e| e.into_inner()).end();
    }
}

/// What a renderer's raster seam may send on its link: checked as it is joined to its bytes.
fn seam_policy() -> ProcessPolicy {
    ProcessPolicy {
        process: 0,
        kind: ProcessKind::Renderer,
        lock: Default::default(),
    }
}

/// Serves tab `tab`'s seam link from its renderer: each message goes to the tab's raster thread (the
/// link's tab is the only one it can draw for), until the link or the raster thread ends. A message
/// that fails its checks ends the link and is reported, so the browser ends the renderer.
fn serve_link(
    tab: TabId,
    link: SeamServer,
    raster: std::sync::Weak<SeamRaster>,
    slot: LinkSlot,
    port: GpuChildPort,
    mark: PassMark,
) {
    let policy = seam_policy();
    // (the link ends first, if it has not for the rule already)
    let refused = |why: String| {
        let why = link.broke().unwrap_or_else(|| {
            link.bad_message(&why);
            why
        });
        let why: String = why.chars().take(256).collect();
        let _ = port.send(&GpuUp::Refused { tab, why });
    };
    loop {
        mark.end();
        let received = link.recv();
        mark.begin();
        let msg = match received {
            Ok(snowlark_ipc::Received::Message(m)) => m,
            Ok(snowlark_ipc::Received::Call(..)) => {
                refused("a call on a raster seam".into());
                break;
            }
            // (the renderer is gone, or the tab's raster thread; or the link ended for a rule
            // the renderer broke: a frame or a message it may not send)
            Err(_) => {
                if let Some(why) = link.broke() {
                    refused(why);
                }
                break;
            }
        };
        let Ok(msg) = link.check(msg, &policy) else {
            break;
        };
        let Some(raster) = raster.upgrade() else {
            break;
        };
        let mut gone = false;
        let open = |b| {
            link.open_bulk(b, SEAM_BYTES_WAIT)
                .inspect_err(|e| gone = matches!(e, snowlark_ipc::Error::Closed))
        };
        match raster.receive(msg, open) {
            Ok(()) => {}
            // (the renderer ended mid-message)
            Err(_) if gone => break,
            Err(why) => {
                refused(why);
                break;
            }
        }
    }
    mark.end();
    slot.lock().unwrap_or_else(|e| e.into_inner()).end();
}

impl Child {
    pub(crate) fn new(gpu: Arc<Gpu>, port: GpuChildPort) -> Child {
        let windows = Windows::default();
        let videos = VideoStore::new(Some(gpu.clone()));
        let video_ids = SharedVideos::default();
        {
            // A video's new frame: its renderer's tab's window draws again (as the browser's store
            // tells its window).
            let (windows, table) = (windows.clone(), video_ids.clone());
            videos.set_notify(Some(Arc::new(move |id: VideoId| {
                let window = {
                    let t = videos_of(&table);
                    t.owner
                        .get(&id.0)
                        .and_then(|tab| t.window.get(tab))
                        .copied()
                };
                let tx = window.and_then(|w| windows.lock().unwrap().get(&w).cloned());
                if let Some(tx) = tx {
                    let _ = tx.send(CompMsg::VideoFrame(id).into());
                }
            })));
        }
        Child {
            gpu,
            port,
            windows,
            threads: HashMap::new(),
            waiting: HashMap::new(),
            rasters: HashMap::new(),
            links: Arc::default(),
            passes: None,
            videos,
            video_ids,
        }
    }

    /// Takes one call of the browser's: a video's new textures ([`GpuDown::VideoPool`]). Their
    /// handles are left in this process's table for the browser to take out.
    pub(crate) fn call(&mut self, msg: GpuDown, replier: snowlark_ipc::Replier) {
        let made = match msg {
            GpuDown::VideoPool {
                tab,
                id,
                width,
                height,
            } => {
                let video = {
                    let mut t = videos_of(&self.video_ids);
                    let known = t.of.get(&tab).and_then(|m| m.get(&id)).copied();
                    known.unwrap_or_else(|| {
                        let v = self.videos.import();
                        t.of.entry(tab).or_default().insert(id, v);
                        t.owner.insert(v.0, tab);
                        v
                    })
                };
                match self.videos.make_pool(video, width, height) {
                    Ok(pool) => {
                        #[cfg(windows)]
                        let handles = Some(
                            pool.handles
                                .into_iter()
                                .map(|h| self.port.give_handle(h))
                                .collect(),
                        );
                        #[cfg(not(windows))]
                        let handles = pool
                            .handles
                            .into_iter()
                            .map(|h| self.port.give_handle(h).ok())
                            .collect::<Option<Vec<_>>>();
                        handles.map(|handles| MadePool {
                            generation: pool.generation,
                            handles,
                        })
                    }
                    Err(e) => {
                        eprintln!("[gpu] no shared textures for a {width}x{height} video: {e}");
                        None
                    }
                }
            }
            // (the browser calls for nothing else)
            _ => None,
        };
        let _ = replier.reply(&PoolMade(made));
    }

    /// Video `id` of tab `tab`'s renderer, in the store.
    fn video(&self, tab: TabId, id: u32) -> Option<VideoId> {
        videos_of(&self.video_ids)
            .of
            .get(&tab)
            .and_then(|m| m.get(&id))
            .copied()
    }

    fn refuse(&self, tab: TabId, why: String) {
        let why: String = why.chars().take(256).collect();
        let _ = self.port.send(&GpuUp::Refused { tab, why });
    }

    fn links_of(&self) -> std::sync::MutexGuard<'_, HashMap<TabId, LinkSlot>> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Maps the page the browser shared for the threads' passes ([`GpuDown::Passes`]): the
    /// threads started from now on mark theirs there. `None` when it does not map, or a page was
    /// shared already.
    pub(crate) fn watch(&mut self, section: snowlark_ipc::RemoteHandle) -> Option<Arc<Passes>> {
        if self.passes.is_some() {
            return None;
        }
        match self.port.map_section(section, PASSES_BYTES as u64) {
            Ok(s) => {
                let passes = Arc::new(Passes::over(s));
                self.passes = Some(passes.clone());
                Some(passes)
            }
            Err(e) => {
                eprintln!("[gpu] the pass page does not map: {e}");
                None
            }
        }
    }

    /// A new thread's mark: marks nothing until the browser has shared the page.
    pub(crate) fn mark(&self) -> PassMark {
        self.passes.as_ref().map(|p| p.mark()).unwrap_or_default()
    }

    /// Window `window`'s compositor input, made now when it has none yet.
    fn input(&mut self, window: WindowId) -> Sender<CompInput> {
        let mut windows = self.windows.lock().unwrap();
        windows
            .entry(window)
            .or_insert_with(|| {
                let (tx, rx) = std::sync::mpsc::channel();
                self.waiting.insert(window, rx);
                tx
            })
            .clone()
    }

    /// Takes one message of the browser's.
    pub(crate) fn handle(&mut self, msg: GpuDown) {
        match msg {
            GpuDown::Quit => {}
            // (one page: the browser shares it first)
            GpuDown::Passes { .. } => {}
            GpuDown::Window { window } => {
                let tx = self.input(window);
                let Some(rx) = self.waiting.remove(&window) else {
                    return;
                };
                let _ = tx.send(CompInput::videos(self.videos.clone()));
                let out = Box::new(RemoteOut {
                    port: self.port.clone(),
                    window,
                });
                let thread = crate::composite::spawn(
                    self.gpu.clone(),
                    window.handle(),
                    out,
                    self.windows.clone(),
                    tx,
                    rx,
                    self.mark(),
                );
                self.threads.insert(window, thread);
            }
            GpuDown::Surface {
                to,
                window,
                surface,
            } => {
                #[cfg(windows)]
                let handle = Some(self.port.take_handle(surface));
                #[cfg(not(windows))]
                let handle = self.port.take_handle(surface).ok();
                if let Some(handle) = handle {
                    let _ = self.input(to).send(CompInput::surface(window, handle));
                }
            }
            GpuDown::Comp { window, msg } => {
                let msg = self
                    .port
                    .open_bulk(msg)
                    .and_then(|bytes| snowlark_ipc::wire::decode::<CompMsg>(&bytes));
                match msg {
                    // (a message that names handles has them only from this end: `Bind` and
                    // `Surface` come as their own messages; and video ids are this process's)
                    Ok(
                        CompMsg::Bind { .. }
                        | CompMsg::Videos { .. }
                        | CompMsg::Surface { .. }
                        | CompMsg::VideoFrame(_),
                    ) => {}
                    Ok(m) => {
                        let _ = self.input(window).send(CompInput::Msg(m));
                    }
                    Err(e) => eprintln!("[gpu] a window message: {e}"),
                }
            }
            GpuDown::Resource { window, msg } => {
                match msg.open_resource(|b| self.port.open_bulk(b)) {
                    Ok(r) => {
                        let _ = self.input(window).send(CompInput::Resource(r));
                    }
                    Err(e) => eprintln!("[gpu] a window resource: {e}"),
                }
            }
            GpuDown::Bind { window, tab } => {
                // What the compositor tells the tab goes on the tab's seam link while it holds
                // (G4 step 3), else up to the browser, from the compositor's thread either way.
                let (port, links) = (self.port.clone(), self.links.clone());
                let tx = snowlark_ipc::Sender::to(move |command: TabCommand| {
                    let slot = links
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&tab)
                        .cloned();
                    let down = SeamDown::Tab(command);
                    if let Some(slot) = slot {
                        if let LinkState::Linked(link) =
                            &*slot.lock().unwrap_or_else(|e| e.into_inner())
                        {
                            if link.send(&down).is_ok() {
                                return Ok(());
                            }
                        }
                    }
                    let SeamDown::Tab(command) = down else {
                        unreachable!()
                    };
                    let up = GpuUp::Tab {
                        window,
                        tab,
                        command,
                    };
                    port.send(&up).map_err(|_| match up {
                        GpuUp::Tab { command, .. } => command,
                        _ => unreachable!(),
                    })
                });
                let _ = self.input(window).send(CompInput::bind(tab, tx));
            }
            GpuDown::RasterStart {
                tab,
                window,
                videos,
            } => {
                let compositor = self.input(window);
                if videos == tab {
                    videos_of(&self.video_ids).window.insert(tab, window);
                }
                // (its lists name the videos of that renderer, and no other's)
                let table = self.video_ids.clone();
                let video_ids: snowlark_tab::services::video::VideoIds = Arc::new(move |id| {
                    let t = videos_of(&table);
                    t.of.get(&videos)
                        .and_then(|m| m.get(&id.0))
                        .copied()
                        .unwrap_or(VideoId(0))
                });
                let port = self.port.clone();
                let link = LinkSlot::default();
                let answer = link.clone();
                // (on the link the job came on, while it holds: a job that came by the browser
                // before the link was joined is answered there too, which the renderer takes the
                // same)
                let taken = Arc::new(move || {
                    let answer = answer.lock().unwrap_or_else(|e| e.into_inner());
                    if let LinkState::Linked(link) = &*answer {
                        if link.send(&SeamDown::Taken).is_ok() {
                            return;
                        }
                    }
                    let _ = port.send(&GpuUp::Taken { tab });
                });
                match SeamRaster::start_marked(
                    tab,
                    self.gpu.clone(),
                    compositor,
                    taken,
                    Some(video_ids),
                    self.mark(),
                ) {
                    Some(r) => {
                        let raster = Arc::new(r);
                        self.links_of().insert(tab, link.clone());
                        self.rasters.insert(tab, TabRaster { raster, link });
                    }
                    None => {
                        self.rasters.remove(&tab);
                        self.links_of().remove(&tab);
                    }
                }
            }
            GpuDown::Link { tab, end } => {
                let Some(r) = self.rasters.get(&tab) else {
                    // (no raster thread: the seam stays on the browser's way, which takes and
                    // drops its jobs)
                    end.close();
                    return;
                };
                let (raster, slot) = (Arc::downgrade(&r.raster), r.link.clone());
                let (port, mark) = (self.port.clone(), self.mark());
                let spawned = std::thread::Builder::new()
                    .name(format!("snowlark gpu link {tab}"))
                    .spawn(move || {
                        // (the renderer's hello is waited for here, never on the dispatcher)
                        let link = match snowlark_ipc::connect_link::<
                            SeamDown,
                            SeamMsg,
                            snowlark_ipc::FromClient,
                        >(end, SEAM_CONNECT_WAIT)
                        {
                            Ok(link) => link,
                            Err(e) => {
                                eprintln!("[gpu] tab {tab}'s raster seam link: {e}");
                                return;
                            }
                        };
                        {
                            let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
                            // (the raster thread ended meanwhile)
                            if !matches!(*held, LinkState::Waiting) {
                                link.close();
                                return;
                            }
                            *held = LinkState::Linked(link.clone());
                        }
                        if std::env::var("SNOWLARK_GPU_STATS").is_ok_and(|v| v == "1") {
                            eprintln!("[gpu] tab {tab}'s raster seam is on its link");
                        }
                        serve_link(tab, link, raster, slot, port, mark);
                    });
                if let Err(e) = spawned {
                    eprintln!("[gpu] no thread for tab {tab}'s raster seam link: {e}");
                }
            }
            GpuDown::Raster { tab, msg } => {
                let open = |b| self.port.open_bulk(b);
                let done = match self.rasters.get(&tab) {
                    Some(r) => r.raster.receive(msg, open),
                    // (no raster thread: a job is taken and dropped)
                    None => {
                        let job = msg.is_job();
                        let _ = msg.open_resource(open);
                        if job {
                            let _ = self.port.send(&GpuUp::Taken { tab });
                        }
                        Ok(())
                    }
                };
                if let Err(why) = done {
                    let why: String = why.chars().take(256).collect();
                    let _ = self.port.send(&GpuUp::Refused { tab, why });
                }
            }
            GpuDown::RasterEnd { tab } => {
                self.rasters.remove(&tab);
                self.links_of().remove(&tab);
            }
            // (a call: answered in `call`)
            GpuDown::VideoPool { .. } => {}
            GpuDown::VideoFrame {
                tab,
                id,
                generation,
                index,
            } => {
                let shown = self
                    .video(tab, id)
                    .is_some_and(|v| self.videos.imported_frame(v, generation, index));
                if !shown {
                    self.refuse(
                        tab,
                        format!(
                            "a frame of video {id} in texture {index} of pool {generation}, which it was not given"
                        ),
                    );
                }
            }
            GpuDown::VideoRemoved { tab, id } => {
                let video = {
                    let mut t = videos_of(&self.video_ids);
                    let v = t.of.get_mut(&tab).and_then(|m| m.remove(&id));
                    if let Some(v) = v {
                        t.owner.remove(&v.0);
                    }
                    v
                };
                match video {
                    Some(v) => self.videos.remove(v),
                    None => self.refuse(tab, format!("video {id}, which it never had, removed")),
                }
            }
            GpuDown::VideosGone { tab } => {
                let gone: Vec<VideoId> = {
                    let mut t = videos_of(&self.video_ids);
                    t.window.remove(&tab);
                    let gone: Vec<VideoId> =
                        t.of.remove(&tab)
                            .into_iter()
                            .flat_map(|m| m.into_values())
                            .collect();
                    for v in &gone {
                        t.owner.remove(&v.0);
                    }
                    gone
                };
                for v in gone {
                    self.videos.remove(v);
                }
            }
            GpuDown::CloseWindow { window } => {
                self.waiting.remove(&window);
                if let Some(tx) = self.windows.lock().unwrap().remove(&window) {
                    let _ = tx.send(CompMsg::Quit.into());
                }
                if let Some(t) = self.threads.remove(&window) {
                    let _ = t.join();
                }
            }
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // (raster threads first: their frames hold the compositors' tiles)
        self.rasters.clear();
        for tx in self.windows.lock().unwrap().values() {
            let _ = tx.send(CompMsg::Quit.into());
        }
    }
}

/// A compositor in the GPU process tells its window through the pipe.
struct RemoteOut {
    port: GpuChildPort,
    window: WindowId,
}

impl WindowOut for RemoteOut {
    fn scroll(&self, tab: TabId, view: Option<ScrollView>) {
        let _ = self.port.send(&GpuUp::Scroll {
            window: self.window,
            tab,
            view,
        });
    }

    fn presented(&self, seq: u64) {
        let _ = self.port.send(&GpuUp::Presented {
            window: self.window,
            seq,
        });
    }

    fn no_compositor(&self, why: &str) {
        let mut why = why.to_owned();
        if why.len() > crate::gpu_process::MAX_WHY {
            let mut end = crate::gpu_process::MAX_WHY;
            while !why.is_char_boundary(end) {
                end -= 1;
            }
            why.truncate(end);
        }
        let _ = self.port.send(&GpuUp::NoCompositor {
            window: self.window,
            why,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_ipc::{ProcessKind, ProcessPolicy, Received, Validate};

    const WAIT: Duration = Duration::from_secs(10);

    /// What a compositor in the GPU process tells tabs reaches only the tabs the browser bound in
    /// that window, and only a compositor's commands pass the check.
    #[test]
    fn a_compositors_word_reaches_only_the_tabs_bound_in_its_window() {
        let routes = Routes::default();
        let (events, _events) = snowlark_ipc::channel();
        let scroll = SharedScroll::default();
        let (tx, rx) = snowlark_ipc::channel::<TabCommand>();
        routes.table().windows.insert(
            WindowId(1),
            WindowRoute {
                events,
                scroll: scroll.clone(),
                tabs: [(5, tx)].into(),
                replay: Replay::default(),
                no_compositor: None,
                in_browser: InBrowser {
                    gpu: Gpu::new().unwrap(),
                    window: WindowId(1).handle(),
                    windows: Windows::default(),
                },
                local: None,
            },
        );
        let to = |window, tab| GpuUp::Tab {
            window: WindowId(window),
            tab,
            command: TabCommand::UseBeginFrames,
        };
        routes.deliver(to(1, 5));
        routes.deliver(to(1, 6));
        routes.deliver(to(2, 5));
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [TabCommand::UseBeginFrames]
        );
        let view = |tab| GpuUp::Scroll {
            window: WindowId(1),
            tab,
            view: Some(ScrollView::default()),
        };
        routes.deliver(view(5));
        routes.deliver(view(6));
        assert_eq!(scroll.lock().unwrap().keys().collect::<Vec<_>>(), [&5]);
        let policy = ProcessPolicy {
            process: 0,
            kind: ProcessKind::Gpu,
            lock: Default::default(),
        };
        let command = |command| GpuUp::Tab {
            window: WindowId(1),
            tab: 5,
            command,
        };
        assert!(command(TabCommand::Close).validate(&policy).is_err());
        assert!(command(TabCommand::Scrolled(
            snowlark_paint::Point::new(0.0, f32::NAN),
            Default::default()
        ))
        .validate(&policy)
        .is_err());
        assert!(command(TabCommand::UseBeginFrames)
            .validate(&policy)
            .is_ok());
    }

    /// G5 step 1: what the browser keeps of a window for a GPU process started again is the newest
    /// of each lasting message (a resize's chrome kept as the chrome), the resources still in use
    /// and the tabs bound, given in the order a compositor needs them; moments are not kept, nor a
    /// tab removed.
    #[test]
    fn a_window_is_given_again_as_it_stands() {
        use snowlark_paint::{Color, DisplayList, ImageId, Point, Resource};
        let w = WindowId(1);
        let mut r = Replay::default();
        let image = |id| Resource::ImageAdd {
            id: ImageId(id),
            frame: 0,
            version: 1,
            width: 1,
            height: 1,
            pixels: vec![7; 4],
        };
        r.resource(&image(1));
        r.resource(&image(2));
        r.resource(&Resource::ImageRemove(ImageId(1)));
        r.message(w, CompMsg::Dark(true));
        r.message(
            w,
            CompMsg::Resize {
                width: 200,
                height: 150,
                chrome_height: 30,
                dpi_scale: 1.0,
                chrome: Some((DisplayList::new(), Color::WHITE)),
                overlay: None,
            },
        );
        r.message(w, CompMsg::Chrome(DisplayList::new(), Color::BLACK));
        for tab in [5, 6] {
            r.message(w, CompMsg::NeedsBeginFrames { tab, needs: true });
        }
        r.message(w, CompMsg::RemoveTab(6));
        r.message(
            w,
            CompMsg::ScrollBy {
                tab: 5,
                delta: Point::new(0.0, 10.0),
                smooth: None,
                input: None,
            },
        );
        r.message(w, CompMsg::Ack(3));
        r.message(w, CompMsg::Dark(false));
        let (browser, child) = snowlark_ipc::loopback::pair::<GpuDown, GpuUp>(1 << 20).unwrap();
        let scroll = SharedScroll::default();
        // (a smooth scroll under way, at 640 on its way to 700: the new compositor starts at 700)
        scroll.lock().unwrap().insert(
            5,
            ScrollView {
                scroll: snowlark_paint::Point::new(0.0, 640.0),
                target: snowlark_paint::Point::new(0.0, 700.0),
                ..Default::default()
            },
        );
        r.send(&browser, w, [5].into_iter(), &scroll).unwrap();
        let mut got = Vec::new();
        while let Ok(Received::Message(m)) = child.recv_timeout(Duration::from_millis(300)) {
            got.push(match m {
                GpuDown::Window { window } => format!("window {}", window.0),
                GpuDown::Resource { msg, .. } => match msg.open_resource(|b| child.open_bulk(b)) {
                    Ok(Resource::ImageAdd { id, .. }) => format!("image {}", id.0),
                    other => format!("{other:?}"),
                },
                GpuDown::Bind { tab, .. } => format!("bind {tab}"),
                GpuDown::Comp { msg, .. } => {
                    let bytes = child.open_bulk(msg).unwrap();
                    match snowlark_ipc::wire::decode::<CompMsg>(&bytes).unwrap() {
                        CompMsg::Resize {
                            width,
                            chrome: None,
                            ..
                        } => format!("resize {width}"),
                        CompMsg::Chrome(_, c) => format!("chrome black {}", c == Color::BLACK),
                        CompMsg::Dark(d) => format!("dark {d}"),
                        CompMsg::NeedsBeginFrames { tab, .. } => format!("frames {tab}"),
                        CompMsg::ScrollTo { tab, to, .. } => format!("scroll {tab} to {}", to.y),
                        other => format!("{other:?}"),
                    }
                }
                other => format!("{other:?}"),
            });
        }
        assert_eq!(
            got,
            [
                "window 1",
                "image 2",
                "bind 5",
                "resize 200",
                "chrome black true",
                "dark false",
                "frames 5",
                "scroll 5 to 700"
            ]
        );
    }

    /// A raster message that fails its checks in the GPU process is reported, so the browser ends
    /// the renderer that sent it; one that passes is taken without a word.
    #[test]
    fn a_raster_message_that_fails_its_checks_is_reported_refused() {
        use snowlark_paint::{ImageId, Resource};
        let (browser, child) = snowlark_ipc::loopback::pair::<GpuDown, GpuUp>(1 << 20).unwrap();
        let mut child = Child::new(Gpu::new().unwrap(), child);
        child.handle(GpuDown::RasterStart {
            tab: 3,
            window: WindowId(9),
            videos: 3,
        });
        let image = |pixels: usize| Resource::ImageAdd {
            id: ImageId(1),
            frame: 0,
            version: 1,
            width: 1,
            height: 1,
            pixels: vec![7; pixels],
        };
        let send = |child: &mut Child, r| {
            let msg = SeamMsg::resource(r, |b| browser.bulk(&b)).unwrap();
            child.handle(GpuDown::Raster { tab: 3, msg });
        };
        send(&mut child, image(4));
        send(&mut child, image(3));
        let up = match browser.recv_timeout(WAIT) {
            Ok(Received::Message(m)) => browser.check(m).unwrap(),
            other => panic!("{:?}", other.err()),
        };
        assert!(
            matches!(&up, GpuUp::Refused { tab: 3, why } if !why.is_empty()),
            "{up:?}"
        );
        assert!(browser.recv_timeout(Duration::from_millis(200)).is_err());
    }

    /// G4 step 2: a tab's raster seam on a link to its renderer. What comes on tab 3's link draws
    /// tab 3 only (the tab is the link's: a frame cannot name another), its jobs' `Taken` goes
    /// back on that link and nothing to the browser, an image past the ring streams through, and a
    /// frame that fails its checks ends the link and is reported for tab 4 alone.
    #[test]
    fn a_seam_link_draws_only_its_tab_and_answers_on_itself() {
        use snowlark_ipc::{Bulk, ClientEnd, FromHost};
        use snowlark_paint::{ImageId, Resource};
        type Client = ClientEnd<SeamMsg, SeamDown>;
        let (browser, port) = snowlark_ipc::loopback::pair::<GpuDown, GpuUp>(1 << 20).unwrap();
        let mut child = Child::new(Gpu::new().unwrap(), port);
        for tab in [3, 4] {
            child.handle(GpuDown::RasterStart {
                tab,
                window: WindowId(9),
                videos: tab,
            });
        }
        let frames = child.waiting.remove(&WindowId(9)).unwrap();
        let mut link = |tab| -> Client {
            let (renderer, _renderer) = snowlark_ipc::loopback::pair::<(), ()>(0).unwrap();
            let [mine, theirs] = renderer.link(&browser, [SEAM_RING, 0]).unwrap();
            child.handle(GpuDown::Link { tab, end: theirs });
            snowlark_ipc::connect_link::<_, _, FromHost>(mine, WAIT).unwrap()
        };
        let (three, four) = (link(3), link(4));
        let taken = |link: &Client| match link.recv_timeout(WAIT) {
            Ok(Received::Message(SeamDown::Taken)) => {}
            other => panic!("{:?}", other.err()),
        };
        let quiet = |link: &Client| {
            assert!(matches!(
                link.recv_timeout(Duration::from_millis(200)),
                Err(snowlark_ipc::Error::Timeout)
            ));
        };
        let drawn = || loop {
            match frames.recv_timeout(WAIT).unwrap() {
                CompInput::Frame(tab, data) => {
                    data.recycle();
                    return tab;
                }
                _ => continue,
            }
        };
        // (12 MiB of pixels: past the 8 MiB ring, so they stream as the GPU end reads)
        let (w, h) = (2048, 1536);
        let image = Resource::ImageAdd {
            id: ImageId(1),
            frame: 0,
            version: 1,
            width: w,
            height: h,
            pixels: vec![7; (w * h * 4) as usize],
        };
        let image = SeamMsg::resource(image, |b| Ok(Bulk::Inline(b))).unwrap();
        image.send_on(&three, WAIT).unwrap();
        SeamMsg::blank_frame(1, 0).send_on(&three, WAIT).unwrap();
        taken(&three);
        assert_eq!(drawn(), 3);
        quiet(&four);
        SeamMsg::blank_frame(1, 0).send_on(&four, WAIT).unwrap();
        taken(&four);
        assert_eq!(drawn(), 4);
        quiet(&three);
        // (no word to the browser: the answers went on the links)
        assert!(browser.recv_timeout(Duration::from_millis(200)).is_err());
        // A frame that fails its checks on tab 4's link.
        SeamMsg::blank_frame(2, 1).send_on(&four, WAIT).unwrap();
        let up = match browser.recv_timeout(WAIT) {
            Ok(Received::Message(m)) => browser.check(m).unwrap(),
            other => panic!("{:?}", other.err()),
        };
        assert!(
            matches!(&up, GpuUp::Refused { tab: 4, why } if !why.is_empty()),
            "{up:?}"
        );
        assert!(matches!(
            four.recv_timeout(WAIT),
            Err(snowlark_ipc::Error::Closed)
        ));
        // Tab 3's link goes on; once its raster thread ends, so does the link.
        SeamMsg::blank_frame(3, 0).send_on(&three, WAIT).unwrap();
        taken(&three);
        child.handle(GpuDown::RasterEnd { tab: 3 });
        assert!(matches!(
            three.recv_timeout(WAIT),
            Err(snowlark_ipc::Error::Closed)
        ));
    }

    /// G4 step 4: a renderer's video textures are made in the GPU process, which leaves their
    /// handles for the browser to take out. A frame must name a texture of a pool made for that
    /// renderer's video (another renderer's id, or a pool it was not given, is refused for the tab
    /// that sent it), and a list names its own renderer's videos only: another's id draws nothing.
    /// (Shared video textures need a GPU device: Windows only so far.)
    #[cfg(windows)]
    #[test]
    fn a_renderers_videos_are_made_here_and_its_own_only() {
        use snowlark_paint::{DisplayListBuilder, Rect};
        let (browser, port) = snowlark_ipc::loopback::pair::<GpuDown, GpuUp>(1 << 20).unwrap();
        let mut child = Child::new(Gpu::new().unwrap(), port.clone());
        for tab in [3, 4] {
            child.handle(GpuDown::RasterStart {
                tab,
                window: WindowId(9),
                videos: tab,
            });
        }
        let frames = child.waiting.remove(&WindowId(9)).unwrap();
        // The browser's call, answered by the GPU process's dispatcher (this thread).
        let pool = |child: &mut Child, tab, id| {
            let ask = GpuDown::VideoPool {
                tab,
                id,
                width: 64,
                height: 32,
            };
            let answer = std::thread::scope(|s| {
                let call = s.spawn(|| browser.call::<PoolMade>(&ask, WAIT));
                match port.recv_timeout(WAIT) {
                    Ok(Received::Call(m, r)) => child.call(m, r),
                    other => panic!("{:?}", other.err()),
                }
                call.join().unwrap()
            });
            let made = browser.check(answer.unwrap()).unwrap().0.unwrap();
            assert_eq!(made.handles.len(), snowlark_paint::video::POOL_TEXTURES);
            for h in made.handles {
                browser.take_from_child(h).unwrap();
            }
            made.generation
        };
        let generation = pool(&mut child, 3, 1);
        let refused = |child: &mut Child, msg| {
            child.handle(msg);
            loop {
                match browser.recv_timeout(Duration::from_millis(500)) {
                    Ok(Received::Message(m)) => match browser.check(m).unwrap() {
                        GpuUp::Refused { tab, .. } => return Some(tab),
                        // (a job drawn below, answered by the browser's way: no link here)
                        GpuUp::Taken { .. } => continue,
                        other => panic!("{other:?}"),
                    },
                    _ => return None,
                }
            }
        };
        let frame = |tab, id, generation| GpuDown::VideoFrame {
            tab,
            id,
            generation,
            index: 0,
        };
        assert_eq!(refused(&mut child, frame(3, 1, generation)), None);
        assert_eq!(refused(&mut child, frame(4, 1, generation)), Some(4));
        assert_eq!(refused(&mut child, frame(3, 1, generation + 1)), Some(3));
        assert_eq!(refused(&mut child, frame(3, 2, generation)), Some(3));
        // Each tab's list names "video 1": tab 3's is the store's video, tab 4's none.
        let store_id = child.video(3, 1).unwrap();
        let drawn = |child: &mut Child, tab| {
            let mut b = DisplayListBuilder::new();
            let r = Rect::new(0.0, 0.0, 8.0, 8.0);
            b.video(VideoId(1), None, r, r);
            let msg = SeamMsg::frame_of(1, &b.build(), 0);
            child.handle(GpuDown::Raster { tab, msg });
            loop {
                match frames.recv_timeout(WAIT).unwrap() {
                    CompInput::Frame(t, data) if t == tab => {
                        let ids: Vec<VideoId> = data
                            .videos
                            .iter()
                            .filter_map(|q| match q.picture {
                                snowlark_tab::video::Picture::Frames(v) => Some(v),
                                _ => None,
                            })
                            .collect();
                        data.recycle();
                        return ids;
                    }
                    _ => continue,
                }
            }
        };
        assert_eq!(drawn(&mut child, 3), [store_id]);
        assert_eq!(drawn(&mut child, 4), [VideoId(0)]);
        // Removed, or the renderer gone: its frames are refused, its ids name nothing.
        child.handle(GpuDown::VideoRemoved { tab: 3, id: 1 });
        assert_eq!(refused(&mut child, frame(3, 1, generation)), Some(3));
        let five = pool(&mut child, 3, 5);
        let in_store = child.video(3, 5).unwrap();
        child.handle(GpuDown::VideosGone { tab: 3 });
        assert_eq!(child.video(3, 5), None);
        assert!(!child.videos.imported_frame(in_store, five, 0));
    }

    /// G4 step 3: what a compositor tells a tab goes on the tab's seam link while it holds, with no
    /// word to the browser; a tab without one, or once its link ended, is told through the browser.
    #[test]
    fn a_compositors_word_to_a_tab_goes_on_its_link() {
        use snowlark_ipc::FromHost;
        let (browser, port) = snowlark_ipc::loopback::pair::<GpuDown, GpuUp>(1 << 20).unwrap();
        let mut child = Child::new(Gpu::new().unwrap(), port);
        child.handle(GpuDown::RasterStart {
            tab: 3,
            window: WindowId(9),
            videos: 3,
        });
        let input = child.waiting.remove(&WindowId(9)).unwrap();
        let (renderer, _renderer) = snowlark_ipc::loopback::pair::<(), ()>(0).unwrap();
        let [mine, theirs] = renderer.link(&browser, [SEAM_RING, 0]).unwrap();
        child.handle(GpuDown::Link {
            tab: 3,
            end: theirs,
        });
        let three = snowlark_ipc::connect_link::<SeamMsg, SeamDown, FromHost>(mine, WAIT).unwrap();
        // (a job answered on the link: the link is held there now)
        SeamMsg::blank_frame(1, 0).send_on(&three, WAIT).unwrap();
        assert!(matches!(
            three.recv_timeout(WAIT),
            Ok(Received::Message(SeamDown::Taken))
        ));
        let mut pipes = HashMap::new();
        for tab in [3, 5] {
            child.handle(GpuDown::Bind {
                window: WindowId(9),
                tab,
            });
        }
        while pipes.len() < 2 {
            if let CompInput::Handles(CompMsg::Bind { tab, .. }, mut handles) =
                input.recv_timeout(WAIT).unwrap()
            {
                if let Some(Handle::Pipe(tx)) = handles.pop() {
                    pipes.insert(tab, tx);
                }
            }
        }
        let up =
            |browser: &snowlark_ipc::BrowserEnd<GpuDown, GpuUp>| match browser.recv_timeout(WAIT) {
                Ok(Received::Message(m)) => browser.check(m).unwrap(),
                other => panic!("{:?}", other.err()),
            };
        pipes[&3].send(TabCommand::UseBeginFrames).unwrap();
        assert!(matches!(
            three.recv_timeout(WAIT),
            Ok(Received::Message(SeamDown::Tab(TabCommand::UseBeginFrames)))
        ));
        assert!(browser.recv_timeout(Duration::from_millis(200)).is_err());
        pipes[&5].send(TabCommand::UseBeginFrames).unwrap();
        assert_eq!(
            up(&browser),
            GpuUp::Tab {
                window: WindowId(9),
                tab: 5,
                command: TabCommand::UseBeginFrames
            }
        );
        child.handle(GpuDown::RasterEnd { tab: 3 });
        pipes[&3].send(TabCommand::UseBeginFrames).unwrap();
        assert_eq!(
            up(&browser),
            GpuUp::Tab {
                window: WindowId(9),
                tab: 3,
                command: TabCommand::UseBeginFrames
            }
        );
    }

    /// A link for a tab with no raster thread here is closed at once: the renderer's join fails, and
    /// its seam stays on the browser's way.
    #[test]
    fn a_link_for_a_tab_without_a_raster_thread_is_closed() {
        let (browser, port) = snowlark_ipc::loopback::pair::<GpuDown, GpuUp>(1 << 20).unwrap();
        let mut child = Child::new(Gpu::new().unwrap(), port);
        let (renderer, _renderer) = snowlark_ipc::loopback::pair::<(), ()>(0).unwrap();
        let [mine, theirs] = renderer.link(&browser, [SEAM_RING, 0]).unwrap();
        child.handle(GpuDown::Link {
            tab: 5,
            end: theirs,
        });
        let t = std::time::Instant::now();
        let joined =
            snowlark_ipc::connect_link::<SeamMsg, SeamDown, snowlark_ipc::FromHost>(mine, WAIT);
        assert!(joined.is_err());
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }
}
