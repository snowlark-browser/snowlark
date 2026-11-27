// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tab's renderer half: a thread of its own that owns its [`TabDriver`] (the document) and a
//! raster thread. The window's half is the [`crate::tab_host::TabHost`], which talks to the thread
//! through channels of plain data: commands in ([`TabCommand`]), events out ([`TabEvent`], with a
//! wake of the window). Frames go straight to the compositor thread. Nothing on the window's side
//! ever waits for a tab.
//!
//! The thread fetches the favicon and decides what to rasterize: a band of the document around the
//! viewport (see [`band`]), so that the compositor can scroll through it without the tab. The
//! session history is the host's: the thread loads what it is told to, and says what the page did.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use snowlark_dom::NodeId;
use snowlark_net::{Client, Destination, FetchEvent, NetClient, Request, ResponseStream, Url};
use snowlark_paint::image::DecodedImage;
use snowlark_paint::input::Cursor;
use snowlark_paint::scroll_curve::ScrollKind;
use snowlark_paint::{
    DisplayList, Gpu, LayerKind, Matrix, Point, Rect, Size, SplitLayer, TextEngine, Viewport,
};

use serde::{Deserialize, Serialize};

use crate::comp::{CompInput, CompMsg};
use crate::driver::*;
use crate::watchdog::Liveness;

pub type TabId = u64;

/// From the window (and the compositor) to a tab's thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TabCommand {
    /// Load `url`: the host has made its session history entry current.
    Navigate {
        url: Url,
        /// Where the new document starts scrolled to (back, forward, reload, a restored session).
        restore: Option<Point>,
        /// The host's number for it ([`TabState::navigations`]).
        navigation: u64,
        /// The user reloads the document: what the HTTP cache holds is validated.
        reload: bool,
    },
    /// M5: commit a top-level navigation the browser began: the response it fetched (its head
    /// in the parameters, the rest of its body handed over as the stream `body`, which the
    /// renderer adopts from its network), or the error it met. Chromium's
    /// `NavigationRequest::CommitNavigation`.
    Commit(Box<Commit>),
    /// M5: the browser traversed its session history to an entry of the document shown: the
    /// document's own entry `index` becomes current (`popstate`). Chromium's same-document history
    /// navigation (`NavigationControllerImpl::NavigateToExistingPendingEntry`).
    GoInDocument {
        url: Url,
        index: usize,
        /// The host's number for it ([`TabState::navigations`]).
        navigation: u64,
    },
    Stop,
    /// How much memory the tab uses: answered with [`TabEvent::Memory`] once the tab gets to it.
    Memory,
    Device(Device),
    /// The compositor scrolled the viewport there, showing the frame of the page's scrolls it
    /// names.
    Scrolled(Point, crate::comp::ShownScroll),
    Pointer(Pointer),
    /// A drag from another program over the page: answered with [`TabEvent::DragOperation`].
    Drag(DragInput),
    Key(KeyInput),
    ScrollContainer(NodeId, Point),
    /// A mouse wheel notch over a scroll container (`None`: over the viewport, sent when
    /// [`TabState::snap_viewport`] says it snaps): the page scrolls the box to the snap position the
    /// notch chooses (CSS Scroll Snap 1 §6.2), or by the delta when the box does not snap.
    SnapScroll(Option<NodeId>, Point),
    /// The user chose an option in a select's list: (select, option index).
    SelectChosen(NodeId, usize),
    /// Input method composition.
    Ime(snowlark_paint::input::ImeEvent),
    Devtools(crate::devtools::protocol::DevtoolsCommand),
    /// The find box (find in page).
    Find(FindCommand),
    /// Clear the local and session storage of a site (a host), or of every site.
    ClearSiteData(Option<String>),
    /// Pages prefer a dark color scheme, or not.
    ColorScheme(bool),
    /// The window shows the tab (the active one), or not.
    Shown(bool),
    /// The keyboard asks for the context menu (at this viewport point, or for the focused element).
    ContextMenu(Option<Point>),
    /// Something the driver waits for made progress.
    Wake,
    /// The compositor sends this tab begin frames while it asks for them
    /// ([`CompMsg::NeedsBeginFrames`]): the page's rendering opportunities follow the display from
    /// now on, not the 60 Hz timer.
    UseBeginFrames,
    /// The display's vsync: a rendering opportunity.
    BeginFrame(crate::comp::BeginFrameArgs),
    /// The window shows the tab fullscreen now, or no longer (the Fullscreen API): the answer to
    /// a [`TabEvent::Fullscreen`], sent after the tab's new [`Device`], or the user's own exit.
    Fullscreen(bool),
    /// The browser's password manager fills a saved login into the page (its values hidden from
    /// script until the user activates the page).
    FillPassword(snowlark_page::passwords::PasswordFill),
    /// The browser's answer to [`TabEvent::OpenWindow`]: it shows the page's window `window` as
    /// its tab `tab`, laid out in `device`. The renderer serves that tab as a view of this tab's
    /// page (its events go up as `tab`'s), and the browser sends the tab's commands here as
    /// [`TabCommand::View`].
    AttachView {
        window: u64,
        tab: TabId,
        device: Device,
    },
    /// A command of tab `tab`, a view of this tab's page ([`TabCommand::AttachView`]). A view's
    /// `Close` closes its window.
    View {
        tab: TabId,
        command: Box<TabCommand>,
    },
    /// The raster thread that draws the tab's frames is new and has none of them (the GPU process
    /// was started again): the tab paints a whole frame again, even one that shows what the last
    /// did, at the scroll position it has.
    Repaint,
    Close,
}

/// What a renderer commits ([`TabCommand::Commit`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Commit {
    /// The host's number for the navigation ([`TabState::navigations`]).
    pub navigation: u64,
    /// Where the new document starts scrolled to (back, forward, reload, a restored session).
    pub restore: Option<Point>,
    pub params: snowlark_page::CommitParams,
    /// The rest of the response's body: a stream handed over by the browser's network
    /// (`NetClient::adopt`), or none (an error, a `Direct` commit, a response with no body).
    pub body: Option<u64>,
    /// The navigation was to a `view-source:` address: the response is shown as source.
    pub view_source: Option<Url>,
}

impl PartialEq for Commit {
    fn eq(&self, other: &Commit) -> bool {
        self.navigation == other.navigation
            && self.restore == other.restore
            && self.params.url == other.params.url
            && self.body == other.body
            && self.view_source == other.view_source
    }
}

/// The memory a tab's thread attributes to the tab, by what it is (bytes). Process memory is not
/// per tab (the tabs are threads of one process), so this is what each counter the tab can read says,
/// not a measurement of the process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUse {
    /// V8's heap (physical) and what it keeps outside the heap.
    pub js: u64,
    /// The DOM arena's slots.
    pub dom: u64,
    /// Decoded images (and animation frames).
    pub images: u64,
    /// The raster band's pixels (a tab's tiles).
    pub tiles: u64,
    /// Not memory: the body bytes the tab's fetches have taken so far (the task manager's Network
    /// rate is how fast it grows). Not in [`MemoryUse::total`].
    pub received: u64,
}

impl MemoryUse {
    pub fn total(&self) -> u64 {
        self.js + self.dom + self.images + self.tiles
    }
}

/// What the window shows about a tab.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TabState {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub scrollers: Vec<Scroller>,
    /// The viewport snaps: the window sends its wheel notches to the page
    /// ([`TabCommand::SnapScroll`]) instead of scrolling it itself.
    pub snap_viewport: bool,
    /// An error page shows (the address bar never shows it as secure).
    pub error_page: bool,
    /// The latest [`TabCommand::Navigate`] the tab has heard of: each one changes the state, so the
    /// window always hears how a navigation it asked for went (one to the page already shown, like
    /// the start page, finishes before the tab publishes anything else).
    pub navigations: u64,
}

/// A favicon as the window gets it: the first frame (the tab strip plays no animation), premultiplied
/// BGRA with rows `width * 4` bytes apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IconBitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// The EXIF orientation ([`DecodedImage::orientation`]).
    pub orientation: u8,
}

impl IconBitmap {
    pub fn new(image: &DecodedImage) -> IconBitmap {
        let frame = &image.frames[0];
        IconBitmap {
            width: frame.width,
            height: frame.height,
            pixels: frame.pixels.clone(),
            orientation: image.orientation,
        }
    }

    pub fn into_image(self) -> DecodedImage {
        let mut image = DecodedImage::still(snowlark_paint::Frame {
            width: self.width,
            height: self.height,
            pixels: self.pixels,
            delay_ms: 0,
        });
        image.orientation = self.orientation;
        image
    }
}

/// From a tab's thread to the window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TabEvent {
    State(TabState),
    /// The drag operation the page chose for a drag from another program
    /// ([`TabCommand::Drag`]).
    DragOperation(snowlark_page::dnd::DragOperation),
    /// The page went to `url` on its own (a link followed in this tab, a form, `location`): a new
    /// session history entry. `scroll` is where the entry it leaves was scrolled to.
    Committed {
        url: Url,
        scroll: Point,
    },
    /// The document shown is at `url`, which its session history entry takes: a new document (a
    /// redirect's end), or the same one after `pushState`, `replaceState`, a fragment navigation
    /// or a traversal of its own entries. `navigation` is the latest [`TabCommand::Navigate`] the
    /// tab had heard of when the document arrived.
    DocumentUrl {
        url: Url,
        navigation: u64,
    },
    /// M5: the page asks the browser to begin a top-level navigation (a link, a form,
    /// `location`): the browser fetches and commits it ([`TabCommand::Commit`]).
    BeginNavigation(Box<snowlark_page::embedder::BeginNavigation>),
    /// M5: a same-document change of the document's URL (`pushState`, `replaceState`, a fragment
    /// navigation): an entry of the browser's session history. `navigation` is the latest
    /// navigation the tab had heard of (whose document this is).
    SameDocument {
        change: snowlark_page::embedder::SameDocumentChange,
        navigation: u64,
    },
    /// M5: `history.go(n)`: the browser traverses its session history by `n` steps.
    Traverse(isize),
    /// The page asks the window to show it fullscreen (`true`) or to leave fullscreen.
    Fullscreen(bool),
    /// The top-level document's login forms, for the browser's password manager (`docs/password-manager.md`).
    Passwords(snowlark_page::passwords::PasswordEvent),
    /// The browser's own: the response of the navigation numbered so has its head (or failed),
    /// for the tab's host to decide on. Never a renderer's: one that sends it dies.
    NavigationReady(u64),
    Hover {
        cursor: Cursor,
        link: Option<Url>,
        title: Option<String>,
        /// The answer to a press (the window decides autoscroll from it).
        press: Option<crate::driver::PressOutcome>,
    },
    /// Open a link in a new tab.
    Open(Url, Disposition),
    /// The page's icon, decoded, and its file (kept with a bookmark of the page).
    Favicon(Box<IconBitmap>, Vec<u8>),
    /// Show a drop-down select's list.
    SelectPopup(Box<snowlark_page::popup::SelectPopup>),
    /// The thread panicked (with this message) and ended: the window shows the crashed tab's page
    /// and takes nothing more from this tab.
    Crashed(String),
    Devtools(Box<crate::devtools::protocol::DevtoolsEvent>),
    /// The find box's matches changed.
    Find(FindStatus),
    /// A page of the browser's own asked for something.
    Internal(InternalRequest),
    /// Show the context menu.
    ContextMenu(Box<ContextInfo>),
    /// The tab's content filter blocked a request (or a new page started a new count).
    Blocked,
    /// What the tab's own thread can attribute to it (the answer to [`TabCommand::Memory`]).
    Memory(MemoryUse),
    /// The tab's page started (`true`) or stopped making sound (sent from a media player's
    /// thread, through the tab's [`snowlark_media::AudioGroup`]).
    Audible(bool),
    /// The page opened a window with an opener (`window.open()`, or a link or form with
    /// `rel=opener`), which it hosts: the browser shows it as a tab of its own and answers with
    /// [`TabCommand::AttachView`]. `window` is the page's number for it, `popup` whether a popup
    /// window was asked for, `size` its viewport (CSS px), `position` where the window's left and
    /// top edges are asked to be on the screen (the `left` and `top` features). Windows with no
    /// opener are [`TabEvent::Open`]'s.
    OpenWindow {
        window: u64,
        popup: bool,
        size: (f32, f32),
        position: (Option<i32>, Option<i32>),
    },
    /// Sent as a view's ([`TabCommand::AttachView`]): its window closed (`window.close()`, or the
    /// page it is a view of went), and the browser closes the tab.
    CloseWindow,
    /// Sent as a view's: script asked to move or resize its window (`moveTo()`, `resizeTo()` and
    /// the like) to this rectangle on the screen: left, top, width, height (device-independent
    /// px). The browser does it for a popup window only.
    WindowRect(i32, i32, i32, i32),
}

/// The largest viewport side a [`TabEvent::OpenWindow`] may ask for (CSS px).
pub const MAX_WINDOW_SIDE: f32 = 100_000.0;

/// Where a tab's events go. The browser half implements it over its channel and the window's wake; it is
/// what a pipe to another process replaces. The raster thread, the content filter's and the audio's
/// notifications use the same sink.
pub trait EventSink: Send + Sync {
    /// Delivers `event` of tab `id` and wakes whoever reads them.
    fn send(&self, id: TabId, event: TabEvent);
}

/// A sink that only queues (nothing to wake): for tests and for runs without a window.
impl EventSink for snowlark_ipc::Sender<(TabId, TabEvent)> {
    fn send(&self, id: TabId, event: TabEvent) {
        let _ = snowlark_ipc::Sender::send(self, (id, event));
    }
}

/// What the window needs to start a tab.
#[derive(Clone)]
pub struct TabContext {
    pub gpu: Arc<Gpu>,
    /// The window's video frames (the compositor draws them).
    pub videos: Option<Arc<snowlark_paint::video::VideoStore>>,
    pub net: NetClient,
    pub compositor: Sender<CompInput>,
    /// With a GPU process: where renderers' frames are drawn ([`crate::raster_thread::GpuRaster`]).
    pub gpu_raster: Option<Arc<dyn crate::raster_thread::GpuRaster>>,
    pub events: Arc<dyn EventSink>,
    pub clipboard: Arc<dyn Clipboard>,
    pub internal: Arc<dyn InternalPages>,
    /// The tab's content filter (cosmetic filters and scriptlets of its documents).
    pub content_filter: Option<Arc<dyn snowlark_page::ContentFilter>>,
    /// The tab's media players (whether it makes sound, its mute).
    pub audio: Option<Arc<snowlark_media::AudioGroup>>,
    /// The output device AudioContexts play on (none: a null sink). Message-shaped: a renderer
    /// process reaches the browser's through its pipe.
    pub audio_out: Option<Arc<dyn snowlark_audio::realtime::AudioOutputs>>,
    /// The browser's BroadcastChannel registry (shared by its tabs).
    pub broadcast: snowlark_page::BroadcastHub,
    pub web_storage: Arc<dyn snowlark_page::web_storage::WebStorage>,
    /// The browser's IndexedDB backend (shared by its tabs).
    pub idb: snowlark_page::IdbBackend,
    pub caches: snowlark_page::CacheBackend,
    /// The browser's key systems (none: pages are offered Clear Key only). Message-shaped: a
    /// renderer process reaches the browser's through its pipe.
    pub cdm: Option<Arc<dyn snowlark_page::media::cdm::Cdm>>,
}

/// What a tab's renderer runs on: a [`TabContext`] in the browser's process, or what a renderer
/// process makes of its pipe ([`crate::renderer_main`]). The two differ only in their [`Surface`].
pub(crate) struct RendererContext {
    pub surface: Surface,
    pub videos: Option<Arc<snowlark_paint::video::VideoStore>>,
    pub net: NetClient,
    pub events: Arc<dyn EventSink>,
    pub clipboard: Arc<dyn Clipboard>,
    pub internal: Arc<dyn InternalPages>,
    pub content_filter: Option<Arc<dyn snowlark_page::ContentFilter>>,
    pub audio: Option<Arc<snowlark_media::AudioGroup>>,
    pub audio_out: Option<Arc<dyn snowlark_audio::realtime::AudioOutputs>>,
    pub broadcast: snowlark_page::BroadcastHub,
    pub web_storage: Arc<dyn snowlark_page::web_storage::WebStorage>,
    pub idb: snowlark_page::IdbBackend,
    pub caches: snowlark_page::CacheBackend,
    pub cdm: Option<Arc<dyn snowlark_page::media::cdm::Cdm>>,
}

impl From<TabContext> for RendererContext {
    fn from(c: TabContext) -> RendererContext {
        RendererContext {
            surface: Surface::Local {
                gpu: c.gpu,
                compositor: c.compositor,
            },
            videos: c.videos,
            net: c.net,
            events: c.events,
            clipboard: c.clipboard,
            internal: c.internal,
            content_filter: c.content_filter,
            audio: c.audio,
            audio_out: c.audio_out,
            broadcast: c.broadcast,
            web_storage: c.web_storage,
            idb: c.idb,
            caches: c.caches,
            cdm: c.cdm,
        }
    }
}

/// Where a tab's frames are drawn and its messages to the compositor go.
pub(crate) enum Surface {
    /// In this process: the tab's raster thread draws with `gpu` and sends to `compositor`.
    Local {
        gpu: Arc<Gpu>,
        compositor: Sender<CompInput>,
    },
    /// In the browser, at the other end of a renderer process's pipe: the encode thread sends its
    /// messages up through `raster`, and messages to the compositor go up `port`.
    Remote {
        port: crate::services::RendererPort,
        raster: Arc<crate::raster_thread::RemoteRaster>,
        /// The seams of the views ([`Surface::view_raster`]).
        views: crate::raster_thread::ViewSeams,
    },
}

impl Surface {
    fn compositor(&self, message: CompMsg) {
        match self {
            Surface::Local { compositor, .. } => {
                let _ = compositor.send(message.into());
            }
            Surface::Remote { port, .. } => {
                // (a browser that is gone ends this process soon)
                let _ = port.send(&crate::services::Up::Tab(
                    crate::renderer_process::TabUp::Comp(message),
                ));
            }
        }
    }

    fn raster(&self, id: TabId) -> std::io::Result<crate::raster_thread::RasterThread> {
        match self {
            Surface::Local { gpu, compositor } => {
                crate::raster_thread::RasterThread::start(id, gpu.clone(), compositor.clone())
            }
            Surface::Remote { raster, .. } => {
                crate::raster_thread::RasterThread::remote(id, raster.clone())
            }
        }
    }

    /// The raster thread of view tab `id` ([`TabCommand::AttachView`]): a raster seam of its own,
    /// beside the tab's.
    fn view_raster(&self, id: TabId) -> std::io::Result<crate::raster_thread::RasterThread> {
        match self {
            Surface::Local { .. } => self.raster(id),
            Surface::Remote { port, views, .. } => {
                // (its link to the GPU process may have come first, with a seam made for it)
                let seam = views
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(id)
                    .or_insert_with(|| {
                        crate::raster_thread::RemoteRaster::of(port.clone(), Some(id))
                    })
                    .clone();
                crate::raster_thread::RasterThread::remote(id, seam)
            }
        }
    }

    /// View tab `id` is gone: its seam too.
    fn forget_view(&self, id: TabId) {
        if let Surface::Remote { views, .. } = self {
            views.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        }
    }
}

/// Starts a tab's thread, which takes commands from `rx` (`own` is the other end) until the host
/// closes the tab.
pub fn spawn_renderer(
    id: TabId,
    context: TabContext,
    own: snowlark_ipc::Sender<TabCommand>,
    rx: Receiver<TabCommand>,
    device: Device,
    factory: DriverFactory,
    live: Arc<Liveness>,
) -> JoinHandle<()> {
    spawn(id, context.into(), own, rx, device, factory, live)
}

/// Which tab a tab's thread is, with its viewport and where it marks its passes.
pub(crate) type Assignment = (TabId, Device, Arc<Liveness>);

/// When a tab's thread learns which tab it is: at once, or once a renderer process is given a tab
/// (it may be a spare: `docs/process-isolation.md` §11, "Spare process").
pub(crate) enum Assign {
    Now(Assignment),
    /// The text engine is made and V8 initialized meanwhile; the thread ends if the sender goes
    /// without a tab.
    Later(Receiver<Begin>),
}

/// What a renderer's tab thread hears before its page.
pub(crate) enum Begin {
    /// The renderer is a spare: it warms up further while it waits ([`warm_up`]).
    WarmUp,
    /// Its tab.
    Tab(Assignment),
}

/// [`spawn_renderer`], on either [`Surface`].
pub(crate) fn spawn(
    id: TabId,
    context: RendererContext,
    own: snowlark_ipc::Sender<TabCommand>,
    rx: Receiver<TabCommand>,
    device: Device,
    factory: DriverFactory,
    live: Arc<Liveness>,
) -> JoinHandle<()> {
    spawn_assigned(context, own, rx, factory, Assign::Now((id, device, live)))
}

/// [`spawn`], for a tab that is assigned now or later.
pub(crate) fn spawn_assigned(
    context: RendererContext,
    own: snowlark_ipc::Sender<TabCommand>,
    rx: Receiver<TabCommand>,
    factory: DriverFactory,
    assign: Assign,
) -> JoinHandle<()> {
    let name = match &assign {
        Assign::Now((id, ..)) => format!("snowlark tab {id}"),
        Assign::Later(_) => "snowlark tab".into(),
    };
    std::thread::Builder::new()
        .name(name)
        // (style, layout and painting recurse over the box tree: deep documents need room)
        .stack_size(snowlark_page::PAGE_THREAD_STACK)
        .spawn(move || {
            let events = context.events.clone();
            let mut renderer: Option<TabRenderer> = None;
            // (0 until a spare is given its tab)
            let mut id = match &assign {
                Assign::Now((id, ..)) => *id,
                Assign::Later(_) => 0,
            };
            let r = catch_unwind(AssertUnwindSafe(|| {
                run(&mut renderer, &mut id, context, own, rx, factory, assign)
            }));
            if let Err(e) = r {
                // A tab failure: the window shows the crashed tab's page and trusts nothing more
                // from this tab; the other tabs go on.
                let message = snowlark_js::guard::panic_message(&*e);
                eprintln!("[tab {id}] crashed: {message}");
                // (the views of the page's windows go with it)
                let views = renderer.as_ref().map(|r| r.attached.clone());
                for view in views.unwrap_or_default() {
                    events.send(view, TabEvent::Crashed(message.clone()));
                }
                events.send(id, TabEvent::Crashed(message));
                // The crashed page is dropped here, after the unwinding: a panic in a destructor
                // (of state the first panic left half-changed) is then an ordinary panic that is
                // caught, where during the unwinding it would abort the process.
                let _ = catch_unwind(AssertUnwindSafe(move || drop(renderer)));
            }
        })
        .expect("tab thread")
}

/// The band of the document a tab rasterizes for a viewport scrolled to `scroll`: three viewports
/// tall, within the document, with its top on a device pixel. It reaches further the way the view
/// last moved (`direction`: `1.0` down, `-1.0` up, `0.0` not known; see [`scroll_direction`]): half
/// a viewport behind the view and a viewport and a half ahead of it, and a viewport each side before
/// the view has moved. Pages whose content only shows right when painted for the scroll position
/// (`pinned`) get exactly the viewport (they are repainted for each scroll position).
///
/// `skew` (CSS px, from [`ScrollHistory::skew`]) reaches further ahead: where the view will be
/// soon at the speed it scrolls, so a fast scroll does not outrun the band before the next one is
/// painted. A wheel notch moves the same CSS px at every display scale, while a viewport holds
/// fewer CSS px the larger the scale, so a band of viewports alone lasts less of a scroll there.
pub fn band(
    scroll: Point,
    viewport: Size,
    content: Size,
    scale: f32,
    pinned: bool,
    direction: f32,
    skew: f32,
) -> (f32, f32) {
    let snap = |y: f32| (y * scale).floor() / scale;
    if pinned {
        return (snap(scroll.y), viewport.height);
    }
    // (viewports of the band above the view's top; the view and the rest are below it)
    let above = if direction > 0.0 {
        0.5
    } else if direction < 0.0 {
        1.5
    } else {
        1.0
    };
    let (skew_up, skew_down) = if direction > 0.0 {
        (0.0, skew)
    } else if direction < 0.0 {
        (skew, 0.0)
    } else {
        (0.0, 0.0)
    };
    let top = snap((scroll.y - above * viewport.height - skew_up).max(0.0));
    let bottom = (scroll.y + (3.0 - above) * viewport.height + skew_down)
        .min(content.height.max(viewport.height));
    (top, (bottom - top).max(viewport.height))
}

/// Whether a band still serves a viewport scrolled to `scroll`: the view lies inside it with a
/// quarter viewport to spare behind it and three quarters ahead of it in the way it scrolls
/// (`direction`, as for [`band`]), plus where the view will be soon (`skew`, as for [`band`]), so
/// the next band is painted while that much is left to show (except at the document's ends).
pub fn band_serves(
    band: (f32, f32),
    scroll: Point,
    viewport: Size,
    content: Size,
    direction: f32,
    skew: f32,
) -> bool {
    let (top, height) = band;
    let bottom = top + height;
    let behind = viewport.height / 4.0;
    let ahead = viewport.height * 0.75 + skew;
    let (margin_top, margin) = if direction > 0.0 {
        (behind, ahead)
    } else if direction < 0.0 {
        (ahead, behind)
    } else {
        (behind, behind)
    };
    let need_top = if top <= 0.0 {
        scroll.y >= 0.0
    } else {
        scroll.y >= top + margin_top
    };
    let doc_bottom = content.height.max(viewport.height);
    let need_bottom = if bottom >= doc_bottom - 0.5 {
        scroll.y + viewport.height <= bottom + 0.5
    } else {
        scroll.y + viewport.height <= bottom - margin
    };
    need_top && need_bottom
}

/// How far ahead of the view a band reaches for the speed it scrolls: the distance covered in this
/// long at that speed (the time to paint and raster the next band, with room to spare).
const SKEW_TIME: f32 = 0.2;
/// The most a band reaches ahead for the speed, in device pixels.
const SKEW_LIMIT: f32 = 2000.0;
/// How far back [`ScrollHistory`] measures the speed.
const SKEW_HISTORY: std::time::Duration = std::time::Duration::from_millis(100);
/// After this long with no new place heard the view is taken to have stopped. Longer than the
/// history: the paint after a rendering update (which can run long) keeps the reach of the paint
/// the scroll asked for just before it.
const SKEW_STOPPED: std::time::Duration = std::time::Duration::from_millis(500);

/// Where the compositor said the viewport was, lately: the speed the view scrolls at, for how far
/// ahead of it a band reaches ([`band`]).
#[derive(Default)]
pub struct ScrollHistory {
    /// When each place was heard and its y, oldest first: one older than [`SKEW_HISTORY`] at most.
    at: std::collections::VecDeque<(Instant, f32)>,
}

impl ScrollHistory {
    /// The viewport was heard to be at `y` at `now`.
    pub fn push(&mut self, now: Instant, y: f32) {
        self.at.push_back((now, y));
        // (the oldest is kept while the next is still within the history: a speed measured over a
        // tab that was busy spans the time it was)
        while self.at.len() > 2 && now.saturating_duration_since(self.at[1].0) >= SKEW_HISTORY {
            self.at.pop_front();
        }
    }

    /// A jump (a new document, the page's own scroll): no speed until the view moves again.
    pub fn clear(&mut self) {
        self.at.clear();
    }

    /// How far ahead of the view, the way it scrolls (`direction`, as for [`band`]), it will be
    /// [`SKEW_TIME`] from `now` at the speed it moved lately, at most [`SKEW_LIMIT`] device pixels
    /// at `scale`: CSS px, `0.0` when it is not moving that way.
    pub fn skew(&self, now: Instant, direction: f32, scale: f32) -> f32 {
        let (Some(&(t0, y0)), Some(&(t1, y1))) = (self.at.front(), self.at.back()) else {
            return 0.0;
        };
        // (nothing heard lately: the view stopped)
        if now.saturating_duration_since(t1) >= SKEW_STOPPED {
            return 0.0;
        }
        let dt = t1.saturating_duration_since(t0).as_secs_f32();
        if dt <= 0.0 || direction == 0.0 {
            return 0.0;
        }
        let ahead = (y1 - y0) * direction / dt * SKEW_TIME;
        ahead.clamp(0.0, SKEW_LIMIT / scale.max(0.01))
    }
}

/// The way a view scrolls (`1.0` down, `-1.0` up, `0.0` not known yet) once it moved to `y`, and the
/// new anchor: `anchor` is the furthest point reached the current way, and the way turns only after
/// `threshold` of travel back from it, so a wheel's or touchpad's jitter does not turn it.
pub fn scroll_direction(direction: f32, anchor: f32, y: f32, threshold: f32) -> (f32, f32) {
    if (direction > 0.0 && y >= anchor) || (direction < 0.0 && y <= anchor) {
        return (direction, y);
    }
    if (y - anchor).abs() >= threshold {
        return ((y - anchor).signum(), y);
    }
    (direction, anchor)
}

/// A scroll the page made of its viewport that the compositor has not shown yet: its document's
/// generation, its serial ([`crate::comp::PageScroll::serial`]) and when the page made it.
type UnshownScroll = (u64, u64, Instant);

/// How long the tab waits for the compositor to show the page's scroll before it takes the
/// compositor's reports again, whatever they name: past the compositor's own wait for the frame
/// ([`crate::comp::SHOWN_WITH_FRAME_WAIT`]), so a scroll lost on its way cannot stop them for
/// good.
const UNSHOWN_SCROLL_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a report of where the viewport is ([`TabCommand::Scrolled`]), naming the last of the
/// page's scrolls the compositor showed (`shown`), is from before the page's own last scroll
/// (`unshown`): it is of a frame painted for the place before that scroll (the user's wheel or a
/// smooth scroll moving it meanwhile), and taking it would scroll the page back until the
/// scroll's frame shows. The page's position is the one it scrolled to until then.
fn report_predates(unshown: UnshownScroll, shown: crate::comp::ShownScroll, now: Instant) -> bool {
    let (generation, serial, since) = unshown;
    now.saturating_duration_since(since) < UNSHOWN_SCROLL_WAIT
        && (shown.generation, shown.serial) < (generation, serial)
}

/// A digest of what a frame shows: its list's content (`list_hash`), its geometry (`numbers`: the
/// band, the viewport, the document's size...), the document's generation and the rest (`other`).
fn frame_signature(list_hash: u64, numbers: &[f32], generation: u64, other: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    list_hash.hash(&mut h);
    for n in numbers {
        n.to_bits().hash(&mut h);
    }
    generation.hash(&mut h);
    other.hash(&mut h);
    h.finish()
}

/// How far the raster of a pinned list (painted for exactly the viewport at `top`, see [`band`])
/// reaches past the viewport: CSS px above the view's top, and the frame's height. The list is
/// repainted at each scroll position, but until the next paint lands the compositor scrolls the
/// last frame, so the frame reaches the way the view scrolls (`direction`, as for [`band`]): what
/// comes into view meanwhile shows at once, and only the pinned content is off until then. Within
/// the document, and within what the renderer paints around the viewport (a viewport and a half
/// above it to three and a half below).
pub fn pinned_reach(
    top: f32,
    viewport: Size,
    content: Size,
    scale: f32,
    direction: f32,
) -> (f32, f32) {
    // (viewports above the view and below it)
    let (up, down) = if direction > 0.0 {
        (0.25, 1.5)
    } else if direction < 0.0 {
        (1.0, 0.5)
    } else {
        (0.5, 1.0)
    };
    let above = ((up * viewport.height).min(top.max(0.0)) * scale).floor() / scale;
    let bottom = (top + (1.0 + down) * viewport.height).min(content.height.max(viewport.height));
    (above, (bottom - (top - above)).max(viewport.height))
}

/// A favicon's pixels: a bitmap format WIC decodes (ICO, PNG, ...) or an SVG image (rendered at
/// 32 px, enough for the strip's 16 DIPs at 200%).
pub fn decode_icon(bytes: &[u8]) -> Option<DecodedImage> {
    if let Ok(img) = snowlark_paint::image::decode(bytes) {
        return Some(img);
    }
    let svg = snowlark_render::svg::SvgImage::parse(bytes).ok()?;
    svg.rasterize(
        (16.0, 16.0),
        (32, 32),
        snowlark_paint::ImageRendering::Smooth,
        None,
    )
}

/// What the window was last told of the document shown ([`TabEvent::DocumentUrl`]).
struct ToldDocument {
    generation: u64,
    url: Url,
    /// The latest [`TabCommand::Navigate`] when the document arrived.
    navigation: u64,
}

struct Favicon {
    url: Url,
    stream: ResponseStream,
    body: Vec<u8>,
    ok: bool,
}

struct TabRenderer {
    id: TabId,
    ctx: RendererContext,
    driver: Box<dyn TabDriver>,
    /// The tab's raster thread (none when it could not start).
    raster: Option<crate::raster_thread::RasterThread>,
    /// The text engine the driver's display lists refer to (the raster thread shares it).
    text: Arc<TextEngine>,
    device: Device,
    scroll: Point,
    band: Option<(f32, f32, f32)>,
    generation: u64,
    restore: Option<Point>,
    needs_paint: bool,
    /// The last paint found content that only shows right when painted for the scroll position
    /// (then the band is exactly the viewport, painted again as it scrolls).
    pinned: bool,
    sent: TabState,
    /// [`TabState::navigations`].
    navigations: u64,
    /// The document the last [`TabEvent::DocumentUrl`] was about.
    document: Option<ToldDocument>,
    icon: Option<Url>,
    favicon: Option<Favicon>,
    wake: Arc<dyn Fn() + Send + Sync>,
    /// A frame went to the compositor.
    painted: bool,
    /// The window shows the tab ([`TabCommand::Shown`]); a hidden tab paints nothing.
    shown: bool,
    /// The way the view scrolls and the furthest point that way ([`scroll_direction`]).
    scroll_dir: f32,
    dir_anchor: f32,
    /// Where the compositor said the viewport was, lately (the speed it scrolls).
    scroll_history: ScrollHistory,
    /// The view scrolled out of the band: paint before the page runs again.
    scroll_paint: bool,
    /// The page scrolled its viewport there and the compositor has not been told: the scroll goes
    /// in the frame painted for it, or after a paint that showed nothing new
    /// ([`TabRenderer::tell_page_scroll`]).
    page_scroll: Option<crate::comp::PageScroll>,
    /// How many times the page scrolled its viewport: the last one's serial.
    page_scrolls: u64,
    /// The page's last scroll, until the compositor reports a place with it shown
    /// ([`report_predates`]).
    unshown_scroll: Option<UnshownScroll>,
    /// How many frames went to the raster thread: the last one's number
    /// ([`crate::comp::FrameData::number`]).
    frames: u64,
    /// The compositor was last told the page asks for begin frames.
    /// The tabs the compositor was last told the page asks for begin frames as: its own, or, once
    /// its own tab closed, its views' (whose window's compositor then drives the page).
    begin_frames_to: Vec<TabId>,
    /// What the last frame sent to the compositor showed ([`frame_signature`]): a paint that would
    /// show the same is not rasterized or sent.
    last_frame: Option<u64>,
    /// The next paint is sent whatever it shows ([`TabCommand::Repaint`]): a new raster side has
    /// none of the tab's frames.
    repaint: bool,
    /// `SNOWLARK_TAB_STATS=1`: a line per batch of input with the time it took.
    stats: bool,
    /// With `stats`: the last reason printed for a pinned list.
    pin_why: Option<String>,
    big_list: usize,
    /// Tells the window when a pass of the loop starts and ends ([`Liveness`]).
    live: Arc<Liveness>,
    /// The views of the page's windows the browser shows as tabs of their own
    /// ([`TabCommand::AttachView`]), and their tabs (kept apart, for a crash to name them all).
    views: Vec<View>,
    attached: Vec<TabId>,
    /// A view's fields are the renderer's now ([`TabRenderer::with_view`]).
    serving_view: bool,
    /// What the page was last told of being shown: while it or any of its views is.
    page_shown: bool,
    /// The browser closed the page's own tab while views stayed open: the page goes on for them
    /// (its own top-level traversable destroyed), and the thread ends with the last of them.
    own_closed: bool,
}

/// A view of a window the page hosts ([`TabCommand::AttachView`]).
struct View {
    window: snowlark_page::HostedWindow,
    fields: ViewFields,
}

/// The fields of a [`TabRenderer`] that are a shown document's: a view keeps its own, which trade
/// places with the renderer's around each call that serves it ([`TabRenderer::with_view`]), so the
/// renderer's commands, publishing and painting serve the view as they are.
struct ViewFields {
    id: TabId,
    raster: Option<crate::raster_thread::RasterThread>,
    device: Device,
    scroll: Point,
    band: Option<(f32, f32, f32)>,
    generation: u64,
    restore: Option<Point>,
    needs_paint: bool,
    pinned: bool,
    sent: TabState,
    navigations: u64,
    document: Option<ToldDocument>,
    icon: Option<Url>,
    favicon: Option<Favicon>,
    painted: bool,
    shown: bool,
    scroll_dir: f32,
    dir_anchor: f32,
    scroll_history: ScrollHistory,
    scroll_paint: bool,
    page_scroll: Option<crate::comp::PageScroll>,
    page_scrolls: u64,
    unshown_scroll: Option<UnshownScroll>,
    frames: u64,
    last_frame: Option<u64>,
    repaint: bool,
}

impl ViewFields {
    fn new(id: TabId, raster: Option<crate::raster_thread::RasterThread>, device: Device) -> Self {
        ViewFields {
            id,
            raster,
            device,
            scroll: Point::ZERO,
            band: None,
            generation: 0,
            restore: None,
            needs_paint: true,
            pinned: false,
            sent: TabState::default(),
            navigations: 0,
            document: None,
            icon: None,
            favicon: None,
            painted: false,
            // (the browser shows a new view's tab in front: it says otherwise if not)
            shown: true,
            scroll_dir: 0.0,
            dir_anchor: 0.0,
            scroll_history: ScrollHistory::default(),
            scroll_paint: false,
            page_scroll: None,
            page_scrolls: 0,
            unshown_scroll: None,
            frames: 0,
            last_frame: None,
            repaint: false,
        }
    }

    /// Trades places with the renderer's own fields.
    fn swap(&mut self, t: &mut TabRenderer) {
        use std::mem::swap;
        swap(&mut self.id, &mut t.id);
        swap(&mut self.raster, &mut t.raster);
        swap(&mut self.device, &mut t.device);
        swap(&mut self.scroll, &mut t.scroll);
        swap(&mut self.band, &mut t.band);
        swap(&mut self.generation, &mut t.generation);
        swap(&mut self.restore, &mut t.restore);
        swap(&mut self.needs_paint, &mut t.needs_paint);
        swap(&mut self.pinned, &mut t.pinned);
        swap(&mut self.sent, &mut t.sent);
        swap(&mut self.navigations, &mut t.navigations);
        swap(&mut self.document, &mut t.document);
        swap(&mut self.icon, &mut t.icon);
        swap(&mut self.favicon, &mut t.favicon);
        swap(&mut self.painted, &mut t.painted);
        swap(&mut self.shown, &mut t.shown);
        swap(&mut self.scroll_dir, &mut t.scroll_dir);
        swap(&mut self.dir_anchor, &mut t.dir_anchor);
        swap(&mut self.scroll_history, &mut t.scroll_history);
        swap(&mut self.scroll_paint, &mut t.scroll_paint);
        swap(&mut self.page_scroll, &mut t.page_scroll);
        swap(&mut self.page_scrolls, &mut t.page_scrolls);
        swap(&mut self.unshown_scroll, &mut t.unshown_scroll);
        swap(&mut self.frames, &mut t.frames);
        swap(&mut self.last_frame, &mut t.last_frame);
        swap(&mut self.repaint, &mut t.repaint);
    }
}

/// Whether a command is something the user did to the tab (a hung tab is one that does not answer
/// these).
pub fn is_input(c: &TabCommand) -> bool {
    if let TabCommand::View { command, .. } = c {
        return is_input(command);
    }
    matches!(
        c,
        TabCommand::Navigate { .. }
            | TabCommand::Commit(_)
            | TabCommand::GoInDocument { .. }
            | TabCommand::Pointer(_)
            | TabCommand::Drag(_)
            | TabCommand::Key(_)
            | TabCommand::ScrollContainer(..)
            | TabCommand::SnapScroll(..)
            | TabCommand::SelectChosen(..)
            | TabCommand::Ime(_)
    )
}

/// What a spare renderer does before it has a tab: a page of its own, made and dropped, with no
/// service (it asks the browser nothing) and no script. What the first page of a process pays once
/// (libraries, the engine's tables, the first isolate) is then paid. Not with a crash switch set:
/// its one firing is for the tab's page.
fn warm_up(text: &Arc<TextEngine>) {
    if ["SNOWLARK_TEST_CRASH", "SNOWLARK_TEST_PANIC"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return;
    }
    let page = snowlark_page::Page::new(snowlark_page::PageOptions {
        render: Some(snowlark_page::RenderSetup {
            net: None,
            text: text.clone(),
            device_pixel_ratio: 1.0,
            scrollbar_width: 0.0,
            trace: false,
            dark: false,
        }),
        ..Default::default()
    });
    drop(page);
    // What the system plays (the decoders installed and the GPU's decoder profiles, about 180 ms
    // in a process): a page's first `canPlayType()` or `isTypeSupported()` then finds it, as a
    // renderer has its decoder facts before its page asks.
    let _ = snowlark_media::support();
}

/// The tab's thread: makes its [`TabRenderer`] in `slot` (where the caller finds it to drop after a
/// panic) and handles commands until the window closes the tab.
fn run(
    slot: &mut Option<TabRenderer>,
    id_slot: &mut TabId,
    ctx: RendererContext,
    own: snowlark_ipc::Sender<TabCommand>,
    rx: Receiver<TabCommand>,
    factory: DriverFactory,
    assign: Assign,
) {
    let began = Instant::now();
    let stats = std::env::var("SNOWLARK_TAB_STATS").is_ok_and(|v| v == "1");
    // A renderer process makes this thread's canvas device now, on a helper thread, while it
    // starts or waits as a spare: the process's first Direct3D device costs about 150 ms (the
    // driver's start in a process) and the window's process has the compositor's, so a canvas
    // asks a renderer for it far sooner than its markup could lead it (Chromium's renderer
    // establishes its GPU channel at startup, `RenderThreadImpl::Init`, "so its ready when needed").
    // It stays for the process's life: once the last device goes, the driver stops within a second.
    if matches!(assign, Assign::Later(_)) {
        snowlark_render::canvas::Backend::from_env().prepare();
    }
    // What needs no tab: the text engine (DirectWrite's factory and the system's fonts) and V8.
    let text = Arc::new(TextEngine::new().expect("DirectWrite"));
    snowlark_js::init();
    let (id, device, live) = match assign {
        Assign::Now(a) => a,
        Assign::Later(start) => {
            let mut next = start.recv();
            let spare = matches!(next, Ok(Begin::WarmUp));
            let mut warmed = began.elapsed();
            if spare {
                warm_up(&text);
                warmed = began.elapsed();
                next = start.recv();
            }
            match next {
                Ok(Begin::Tab(a)) => {
                    if stats {
                        let what = if spare { "a spare" } else { "started for it" };
                        eprintln!(
                            "[tab {}] its renderer ({what}) was ready {:.1} ms after its thread \
                             started, {:.1} ms before the tab",
                            a.0,
                            warmed.as_secs_f64() * 1000.0,
                            (began.elapsed() - warmed).as_secs_f64() * 1000.0
                        );
                    }
                    a
                }
                // (a spare that was never given a tab)
                Ok(Begin::WarmUp) | Err(_) => return,
            }
        }
    };
    // A sandboxed renderer locks itself down before its page: what needs the system (V8, the text
    // engine's configuration, the media libraries) is made by now.
    #[cfg(target_os = "linux")]
    if snowlark_ipc::sandboxed() {
        let _ = snowlark_media::support();
        // (the system's font files, each mapped once: faces are made from the mappings)
        let _ = text.font_database();
        if !snowlark_sandbox::in_namespaces() {
            eprintln!("[tab {id}] no namespaces for the sandbox here: the system call filter only");
        }
        if let Err(e) = snowlark_sandbox::lock_down() {
            eprintln!("[tab {id}] the sandbox cannot lock this renderer down: {e}");
            std::process::exit(1);
        }
    }
    // The page is made once the tab is known, not before: it asks the browser (the user agent,
    // which decides what its `about:blank` realm installs), and only the tab's host answers.
    let shared_text = text.clone();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = own.send(TabCommand::Wake);
    });
    let driver = factory(DriverContext {
        videos: ctx.videos.clone(),
        net: ctx.net.clone(),
        text,
        wake: wake.clone(),
        clipboard: ctx.clipboard.clone(),
        internal: ctx.internal.clone(),
        content_filter: ctx.content_filter.clone(),
        audio: ctx.audio.clone(),
        audio_out: ctx.audio_out.clone(),
        broadcast: ctx.broadcast.clone(),
        web_storage: ctx.web_storage.clone(),
        idb: ctx.idb.clone(),
        caches: ctx.caches.clone(),
        cdm: ctx.cdm.clone(),
    });
    *id_slot = id;
    if let Some(terminator) = driver.terminator() {
        live.set_terminator(terminator);
    }
    if stats {
        eprintln!(
            "[tab {id}] driver ready {:.1} ms after the tab was started",
            live.age().as_secs_f64() * 1000.0
        );
    }
    let raster = match ctx.surface.raster(id) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("[tab {id}] no raster thread: {e}");
            None
        }
    };
    let t = slot.insert(TabRenderer {
        id,
        ctx,
        driver,
        raster,
        text: shared_text,
        device,
        scroll: Point::ZERO,
        band: None,
        generation: 0,
        restore: None,
        needs_paint: false,
        pinned: false,
        sent: TabState::default(),
        navigations: 0,
        document: None,
        icon: None,
        favicon: None,
        wake,
        painted: false,
        shown: true,
        scroll_dir: 0.0,
        dir_anchor: 0.0,
        scroll_history: ScrollHistory::default(),
        scroll_paint: false,
        page_scroll: None,
        page_scrolls: 0,
        unshown_scroll: None,
        frames: 0,
        begin_frames_to: Vec::new(),
        last_frame: None,
        repaint: false,
        stats,
        pin_why: None,
        big_list: 0,
        live,
        views: Vec::new(),
        attached: Vec::new(),
        serving_view: false,
        page_shown: true,
        own_closed: false,
    });
    t.driver.set_device(device);
    // (engine code that panicked under V8 is a failure of the tab: raised here, where it unwinds)
    snowlark_js::guard::resume();
    let mut next: Option<Instant> = None;
    // (a wait for the next frame or a short timer is precise only on a finer timer: as Chromium,
    // the thread asks for one while its next wake-up needs it; see `timer_resolution`)
    let mut high_res = snowlark_paint::timer_resolution::HighResWaits::new();
    loop {
        t.live.end();
        let wait = next.map(|at| at.saturating_duration_since(Instant::now()));
        high_res.set(wait.is_some_and(snowlark_paint::timer_resolution::wait_needs_high_res));
        let first = match wait {
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
        };
        let mut commands: Vec<TabCommand> = first.into_iter().collect();
        commands.extend(rx.try_iter());
        let received = commands.len();
        t.live.begin(Instant::now(), commands.iter().any(is_input));
        let moves = commands
            .iter()
            .filter(|c| matches!(c, TabCommand::Pointer(Pointer::Move(_))))
            .count();
        coalesce_moves(&mut commands);
        let t0 = Instant::now();
        let handled = commands.len();
        for c in commands {
            let go_on = match c {
                TabCommand::View { tab, command } => {
                    t.view_command(tab, *command);
                    true
                }
                TabCommand::AttachView {
                    window,
                    tab,
                    device,
                } => {
                    t.attach_view(window, tab, device);
                    true
                }
                c => t.command(c),
            };
            if !go_on {
                return;
            }
            snowlark_js::guard::resume();
        }
        if t.own_closed && t.views.is_empty() {
            // (the browser closed the last window the closed tab's page hosted)
            return;
        }
        t.tell_page_shown();
        // The view scrolled out of its band: the next band is painted now, from the layout the page
        // has, so the page's scripts and rendering update (a slice of up to 12 ms) don't hold it up.
        let mut painted = None;
        if std::mem::take(&mut t.scroll_paint) && t.shown {
            painted = t.paint();
            snowlark_js::guard::resume();
        }
        let t1 = Instant::now();
        let update = t.driver.update();
        snowlark_js::guard::resume();
        // (a pointer move the page held for its rendering update was dispatched there)
        if let Some(r) = t.driver.take_held_pointer() {
            t.event(TabEvent::Hover {
                cursor: r.cursor,
                link: r.link,
                title: r.title,
                press: None,
            });
        }
        let t2 = Instant::now();
        next = update.next;
        if update.repaint {
            t.needs_paint = true;
        }
        t.after_input();
        if let Some(to) = update.scroll_to {
            t.page_scrolled(to, None);
        }
        if let Some(by) = update.scroll_by {
            t.page_scrolled(Point::ZERO, Some(by));
        }
        t.poll_favicon();
        t.publish();
        if t.needs_paint && t.shown {
            painted = t.paint();
        }
        t.tell_page_scroll();
        snowlark_js::guard::resume();
        t.window_changes();
        if let Some(at) = t.serve_views() {
            next = Some(next.map_or(at, |n| n.min(at)));
        }
        if t.own_closed && t.views.is_empty() {
            // (the last window the closed tab's page hosted went)
            return;
        }
        t.ask_for_begin_frames();
        if t.stats && moves > 0 {
            let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1000.0;
            let (list, _) = painted.unwrap_or_default();
            // (the raster runs on the tab's raster thread, which prints its own line)
            eprintln!(
                "[tab {}] input {received} (moves {moves}, handled {handled}): commands {:.2} ms, update {:.2} ms, display list {list:.2} ms, total {:.2} ms",
                t.id,
                ms(t0, t1),
                ms(t1, t2),
                ms(t0, Instant::now()),
            );
        }
    }
}

/// Coalesces pointer moves (UI Events leaves the rate of `mousemove` to the user agent; Pointer
/// Events 3 calls the moves merged into one "coalesced events"): of moves with no other input
/// between them (wake-ups don't count), only the last is handled. A fast mouse
/// sends hundreds a second, more than a page can hit-test and select for one at a time.
fn coalesce_moves(commands: &mut Vec<TabCommand>) {
    let is_move = |c: &TabCommand| matches!(c, TabCommand::Pointer(Pointer::Move(_)));
    let neutral = |c: &TabCommand| matches!(c, TabCommand::Wake);
    // (from the back: a move is dropped when a later move follows with only neutral commands
    // between)
    let mut later_move = false;
    let mut keep = vec![true; commands.len()];
    for (i, c) in commands.iter().enumerate().rev() {
        if is_move(c) {
            keep[i] = !later_move;
            later_move = true;
        } else if !neutral(c) {
            later_move = false;
        }
    }
    let mut i = 0;
    commands.retain(|_| {
        i += 1;
        keep[i - 1]
    });
}

impl TabRenderer {
    fn event(&self, e: TabEvent) {
        self.ctx.events.send(self.id, e);
    }

    /// Handles a command; false to stop the thread.
    fn command(&mut self, c: TabCommand) -> bool {
        match c {
            TabCommand::Close => {
                if self.views.is_empty() {
                    return false;
                }
                // The page's windows outlive its own tab: the page goes on running for them, as a
                // renderer process lives on while any of its views does. The tab's own document
                // goes, so the windows see `opener` null.
                if !self.own_closed {
                    self.own_closed = true;
                    self.shown = false;
                    self.driver.close_own_page();
                }
            }
            TabCommand::Navigate {
                url,
                restore,
                navigation,
                reload,
            } => {
                self.navigations = navigation;
                self.restore = restore;
                if reload {
                    self.driver.reload(url);
                } else {
                    self.driver.navigate(url);
                }
            }
            TabCommand::Commit(commit) => {
                self.navigations = commit.navigation;
                self.restore = commit.restore;
                self.driver.commit(*commit);
            }
            TabCommand::GoInDocument {
                index, navigation, ..
            } => {
                self.navigations = navigation;
                self.driver.go_in_document(index);
            }
            TabCommand::Stop => self.driver.stop(),
            TabCommand::UseBeginFrames => self.driver.use_begin_frames(),
            TabCommand::BeginFrame(args) => self.driver.begin_frame(args.to_page()),
            TabCommand::Memory => {
                let d = self.driver.memory();
                let images = self.driver.image_store().bytes();
                let tiles = self.band.map_or(0.0, |(_, height, scale)| {
                    f64::from(self.device.viewport.width * scale) * f64::from(height * scale) * 4.0
                });
                self.event(TabEvent::Memory(MemoryUse {
                    js: d.js,
                    dom: d.dom,
                    images,
                    tiles: tiles as u64,
                    received: d.received,
                }));
            }
            TabCommand::Device(d) => {
                if d != self.device {
                    self.device = d;
                    self.driver.set_device(d);
                    self.needs_paint = true;
                }
            }
            TabCommand::Scrolled(p, shown) => {
                if let Some(unshown) = self.unshown_scroll {
                    if report_predates(unshown, shown, Instant::now()) {
                        return true;
                    }
                    self.unshown_scroll = None;
                }
                self.scroll = p;
                self.driver.set_scroll(p);
                (self.scroll_dir, self.dir_anchor) = scroll_direction(
                    self.scroll_dir,
                    self.dir_anchor,
                    p.y,
                    self.device.viewport.height / 8.0,
                );
                let now = Instant::now();
                self.scroll_history.push(now, p.y);
                let info = self.driver.info();
                match self.band {
                    Some((top, height, _)) => {
                        let skew =
                            self.scroll_history
                                .skew(now, self.scroll_dir, self.device.scale);
                        let serves = band_serves(
                            (top, height),
                            p,
                            self.device.viewport,
                            info.content,
                            self.scroll_dir,
                            skew,
                        );
                        let pinned_moved =
                            self.pinned && (top - p.y).abs() >= 0.5 / self.device.scale;
                        if !serves || pinned_moved {
                            self.needs_paint = true;
                            self.scroll_paint = true;
                        }
                    }
                    None => self.needs_paint = true,
                }
            }
            TabCommand::Key(k) => {
                let r = self.driver.key(k);
                if r.repaint {
                    self.needs_paint = true;
                }
                if let Some(s) = r.scroll {
                    // (a viewport that snaps goes where the key's direction chooses: CSS Scroll
                    // Snap 1 §6.2)
                    if let KeyScroll::By(delta) = s {
                        if self.driver.info().snap_viewport {
                            if self.driver.snap_scroll(None, delta, true) {
                                self.needs_paint = true;
                            }
                            return true;
                        }
                    }
                    let info = self.driver.info();
                    let msg = match s {
                        KeyScroll::By(delta) => CompMsg::ScrollBy {
                            tab: self.id,
                            delta,
                            smooth: Some(ScrollKind::Keyboard),
                            input: None,
                        },
                        KeyScroll::Top => CompMsg::ScrollTo {
                            tab: self.id,
                            to: Point::new(self.scroll.x, 0.0),
                            smooth: true,
                            with: None,
                        },
                        KeyScroll::Bottom => CompMsg::ScrollTo {
                            tab: self.id,
                            to: Point::new(
                                self.scroll.x,
                                (info.content.height - self.device.viewport.height).max(0.0),
                            ),
                            smooth: true,
                            with: None,
                        },
                    };
                    self.ctx.surface.compositor(msg);
                }
            }
            TabCommand::Drag(d) => {
                let op = self.driver.drag(d);
                self.needs_paint = true;
                self.event(TabEvent::DragOperation(op));
            }
            TabCommand::Pointer(p) => {
                let r = self.driver.pointer(p);
                if r.repaint {
                    self.needs_paint = true;
                }
                if !r.held && !matches!(p, Pointer::Leave | Pointer::Wheel(..)) {
                    self.event(TabEvent::Hover {
                        cursor: r.cursor,
                        link: r.link.clone(),
                        title: r.title.clone(),
                        press: r.press,
                    });
                }
                if let Some((url, disposition)) = r.open {
                    match disposition {
                        Disposition::CurrentTab => {
                            // (a driver whose navigations begin in the browser asks it instead:
                            // `TabEvent::BeginNavigation`, and the entry is the commit's)
                            if !self.driver.browser_navigates() {
                                self.event(TabEvent::Committed {
                                    url: url.clone(),
                                    scroll: self.scroll,
                                });
                            }
                            self.restore = None;
                            self.driver.navigate(url);
                        }
                        d => self.event(TabEvent::Open(url, d)),
                    }
                }
            }
            TabCommand::SelectChosen(select, index) => {
                self.driver.select_popup_chosen(select, index);
            }
            TabCommand::Ime(e) => self.driver.ime(e),
            TabCommand::Devtools(c) => self.driver.devtools(c),
            TabCommand::Find(c) => self.driver.find(c),
            TabCommand::ClearSiteData(site) => self.driver.clear_site_data(site.as_deref()),
            TabCommand::ContextMenu(at) => self.driver.context_menu_at(at),
            TabCommand::Fullscreen(on) => self.driver.fullscreen(on),
            TabCommand::FillPassword(fill) => self.driver.fill_password(&fill),
            TabCommand::ColorScheme(dark) => {
                self.driver.set_color_scheme(dark);
                self.needs_paint = true;
            }
            TabCommand::Shown(shown) => {
                // (the page hears of it once the pass's commands are in: `tell_page_shown`)
                self.shown = shown;
                if shown {
                    // (a hidden tab paints nothing: the window gets a current frame now)
                    self.needs_paint = true;
                }
            }
            TabCommand::ScrollContainer(node, delta) => {
                if self.driver.scroll_container(node, delta) {
                    self.needs_paint = true;
                }
            }
            TabCommand::SnapScroll(node, delta) => {
                if self.driver.snap_scroll(node, delta, false) {
                    self.needs_paint = true;
                } else if node.is_none() && !self.driver.info().snap_viewport {
                    // (the page stopped snapping since the window asked: the compositor scrolls)
                    self.ctx.surface.compositor(CompMsg::ScrollBy {
                        tab: self.id,
                        delta,
                        smooth: Some(ScrollKind::Wheel),
                        input: None,
                    });
                }
            }
            TabCommand::Wake => {}
            TabCommand::Repaint => {
                // (a hidden tab paints once it is shown)
                self.repaint = true;
                self.needs_paint = true;
            }
            // (the run loop's: `attach_view`, `view_command`)
            TabCommand::AttachView { .. } | TabCommand::View { .. } => {}
        }
        true
    }

    /// Tells the page whether it is shown: while its own tab or any view's is.
    fn tell_page_shown(&mut self) {
        let shown = self.shown || self.views.iter().any(|v| v.fields.shown);
        if shown != self.page_shown {
            self.page_shown = shown;
            self.driver.set_shown(shown);
        }
    }

    /// Runs `f` with view `i`'s fields in the renderer's place and the driver addressing its
    /// window; `None` (and nothing run) when the window is gone.
    fn with_view<R>(&mut self, i: usize, f: impl FnOnce(&mut TabRenderer) -> R) -> Option<R> {
        let mut views = std::mem::take(&mut self.views);
        let r = if self.driver.select_view(Some(views[i].window)) {
            views[i].fields.swap(self);
            self.serving_view = true;
            let r = f(self);
            self.serving_view = false;
            views[i].fields.swap(self);
            self.driver.select_view(None);
            Some(r)
        } else {
            None
        };
        self.views = views;
        r
    }

    /// Removes view `i`; its window has closed or closes (`close`: the driver closes it).
    fn remove_view(&mut self, i: usize, close: bool) -> TabId {
        let v = self.views.remove(i);
        self.attached.retain(|&t| t != v.fields.id);
        self.ctx.surface.forget_view(v.fields.id);
        if close {
            self.driver.close_view(v.window);
        }
        v.fields.id
    }

    /// [`TabCommand::AttachView`]: tab `tab` shows the page's window `window` from now on.
    fn attach_view(&mut self, window: u64, tab: TabId, device: Device) {
        let Ok(window) = snowlark_page::HostedWindow::try_from(window) else {
            return;
        };
        if tab == self.id || self.attached.contains(&tab) {
            return;
        }
        let raster = match self.ctx.surface.view_raster(tab) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("[tab {tab}] no raster thread for the view: {e}");
                None
            }
        };
        self.views.push(View {
            window,
            fields: ViewFields::new(tab, raster, device),
        });
        self.attached.push(tab);
        let i = self.views.len() - 1;
        let attached = self.with_view(i, |t| t.driver.set_device(device));
        if attached.is_none() {
            // (it closed before the browser showed it)
            self.remove_view(i, false);
            self.ctx.events.send(tab, TabEvent::CloseWindow);
        }
        self.tell_page_shown();
    }

    /// [`TabCommand::View`]: a command of view tab `tab`.
    fn view_command(&mut self, tab: TabId, command: TabCommand) {
        let Some(i) = self.views.iter().position(|v| v.fields.id == tab) else {
            return;
        };
        match command {
            // The browser closed the view's tab: its window closes.
            TabCommand::Close => {
                self.remove_view(i, true);
            }
            // What is the page's own, or the browser's own tab's: the view has none of it.
            TabCommand::Commit(_)
            | TabCommand::GoInDocument { .. }
            | TabCommand::Stop
            | TabCommand::Devtools(_)
            | TabCommand::Fullscreen(_)
            | TabCommand::FillPassword(_)
            | TabCommand::ClearSiteData(_)
            | TabCommand::ColorScheme(_)
            | TabCommand::Wake
            | TabCommand::AttachView { .. }
            | TabCommand::View { .. } => {}
            // (the page's rendering opportunities, from the view's window once the page's own tab
            // closed: `ask_for_begin_frames`)
            TabCommand::UseBeginFrames => self.driver.use_begin_frames(),
            TabCommand::BeginFrame(args) => self.driver.begin_frame(args.to_page()),
            command => {
                let served = self.with_view(i, |t| {
                    t.command(command);
                    snowlark_js::guard::resume();
                    // (what the input asked for is the view's: a select's list, a context menu)
                    t.after_input();
                });
                if served.is_none() {
                    self.ctx.events.send(tab, TabEvent::CloseWindow);
                    self.remove_view(i, false);
                }
                self.tell_page_shown();
            }
        }
    }

    /// The windows the page opened (the browser is asked to show each) and closed since the last
    /// pass.
    fn window_changes(&mut self) {
        for change in self.driver.take_window_changes() {
            match change {
                snowlark_page::WindowChange::Opened {
                    window,
                    popup,
                    size,
                    position,
                } => {
                    // (the browser shows no window for a tab it closed: the window closes)
                    if self.own_closed {
                        self.driver.close_view(window);
                        continue;
                    }
                    self.event(TabEvent::OpenWindow {
                        window: window as u64,
                        popup,
                        size,
                        position,
                    })
                }
                snowlark_page::WindowChange::Rect { window, rect } => {
                    if let Some(v) = self.views.iter().find(|v| v.window == window) {
                        let (x, y, w, h) = rect;
                        self.ctx
                            .events
                            .send(v.fields.id, TabEvent::WindowRect(x, y, w, h));
                    }
                }
                snowlark_page::WindowChange::Closed(window) => {
                    if let Some(i) = self.views.iter().position(|v| v.window == window) {
                        let tab = self.remove_view(i, true);
                        self.ctx.events.send(tab, TabEvent::CloseWindow);
                    }
                }
            }
        }
    }

    /// Each view's part of the pass, after the page's: what changed in its document, told to the
    /// window, and painted while its tab shows.
    /// Returns when a view's work goes on (its find box's search).
    fn serve_views(&mut self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut i = 0;
        while i < self.views.len() {
            let served = self.with_view(i, |t| {
                let update = t.driver.view_update();
                if let Some(at) = update.next {
                    next = Some(next.map_or(at, |n| n.min(at)));
                }
                if update.repaint {
                    t.needs_paint = true;
                }
                t.after_input();
                if let Some(to) = update.scroll_to {
                    t.page_scrolled(to, None);
                }
                if let Some(by) = update.scroll_by {
                    t.page_scrolled(Point::ZERO, Some(by));
                }
                t.poll_favicon();
                t.publish();
                let scroll_paint = std::mem::take(&mut t.scroll_paint);
                if (t.needs_paint || scroll_paint) && t.shown {
                    t.paint();
                }
                t.tell_page_scroll();
                snowlark_js::guard::resume();
            });
            if served.is_none() {
                // (its window went without the page saying so: with the document it was in)
                let tab = self.remove_view(i, false);
                self.ctx.events.send(tab, TabEvent::CloseWindow);
                continue;
            }
            i += 1;
        }
        next
    }

    /// What input made the document ask for: new tabs, new session history entries.
    /// Tells the compositor when the page starts or stops asking for begin frames, on a change only
    /// (cc subscribes to its begin frame source while `BeginFrameNeeded`; the frame sink tells viz
    /// with `SetNeedsBeginFrame`).
    fn ask_for_begin_frames(&mut self) {
        let want: Vec<TabId> = if !self.driver.needs_begin_frames() {
            Vec::new()
        } else if self.own_closed {
            self.views.iter().map(|v| v.fields.id).collect()
        } else {
            vec![self.id]
        };
        if want == self.begin_frames_to {
            return;
        }
        for &tab in &self.begin_frames_to {
            if !want.contains(&tab) {
                self.ctx
                    .surface
                    .compositor(CompMsg::NeedsBeginFrames { tab, needs: false });
            }
        }
        for &tab in &want {
            if !self.begin_frames_to.contains(&tab) {
                self.ctx
                    .surface
                    .compositor(CompMsg::NeedsBeginFrames { tab, needs: true });
            }
        }
        self.begin_frames_to = want;
    }

    fn after_input(&mut self) {
        for (url, d) in self.driver.take_new_tabs() {
            self.event(TabEvent::Open(url, d));
        }
        for url in self.driver.take_committed() {
            self.event(TabEvent::Committed {
                url,
                scroll: self.scroll,
            });
        }
        if let Some(p) = self.driver.take_select_popup() {
            self.event(TabEvent::SelectPopup(Box::new(p)));
        }
        if let Some(c) = self.driver.take_context_menu() {
            self.event(TabEvent::ContextMenu(Box::new(c)));
        }
        // (a view's find box searches its own document)
        if let Some(s) = self.driver.take_find_status() {
            self.event(TabEvent::Find(s));
        }
        if self.serving_view {
            // (the rest is the page's own top-level document's, or its tab's: the page's pass
            // tells it as the page's)
            return;
        }
        // (a form's submission is reported before the navigation it begins: the browser takes
        // the login as the document that submitted it, before any response can commit)
        for e in self.driver.take_password_events() {
            self.event(TabEvent::Passwords(e));
        }
        for begin in self.driver.take_begun_navigations() {
            self.event(TabEvent::BeginNavigation(Box::new(begin)));
        }
        for change in self.driver.take_same_document_changes() {
            self.event(TabEvent::SameDocument {
                change,
                navigation: self.navigations,
            });
        }
        for steps in self.driver.take_traversals() {
            self.event(TabEvent::Traverse(steps));
        }
        for on in self.driver.take_fullscreen_asks() {
            self.event(TabEvent::Fullscreen(on));
        }
        for e in self.driver.take_devtools() {
            self.event(TabEvent::Devtools(Box::new(e)));
        }
        for r in self.driver.take_internal_requests() {
            self.event(TabEvent::Internal(r));
        }
    }

    /// Tells the window what changed.
    fn publish(&mut self) {
        let info = self.driver.info();
        // A new document, or the same one at another URL (`pushState`, `replaceState`, a fragment
        // navigation, a same-document traversal): the session history entry takes its URL, as
        // Chromium's browser hears every same-document commit
        // (`RenderFrameHostImpl::DidCommitSameDocumentNavigation`) and its NavigationEntry takes
        // the URL (`NavigationControllerImpl::RendererDidNavigateToNewEntry`, `...ToExistingEntry`).
        // A document belongs to the navigation it arrived under: one still shown while a later
        // navigation loads (Chromium's last committed entry, under a pending one) speaks for its
        // own entry, which the window then ignores.
        if let Some(url) = &info.url {
            let (told, navigation) = match &self.document {
                Some(d) if d.generation == info.generation => (&d.url == url, d.navigation),
                _ => (false, self.navigations),
            };
            if !told {
                self.event(TabEvent::DocumentUrl {
                    url: url.clone(),
                    navigation,
                });
                self.document = Some(ToldDocument {
                    generation: info.generation,
                    url: url.clone(),
                    navigation,
                });
            }
        }
        let info = self.driver.info();
        let state = TabState {
            url: info.url.as_ref().map(|u| u.to_string()).unwrap_or_default(),
            title: info.title.clone(),
            loading: info.loading,
            scrollers: info.scrollers.clone(),
            snap_viewport: info.snap_viewport,
            error_page: info.error_page,
            navigations: self.navigations,
        };
        if state != self.sent {
            self.sent = state.clone();
            self.event(TabEvent::State(state));
        }
        if info.icon != self.icon {
            self.icon = info.icon.clone();
            self.favicon = None;
            if let Some(url) = self.icon.clone() {
                let doc = info.url.clone().unwrap_or_else(|| url.clone());
                let req = Request::subresource(
                    url.clone(),
                    Destination::Image,
                    Arc::new(Client::for_url(&doc)),
                );
                let wake = self.wake.clone();
                let stream = self
                    .ctx
                    .net
                    .fetch_with_notify(req, Arc::new(move || wake()));
                self.favicon = Some(Favicon {
                    url,
                    stream,
                    body: Vec::new(),
                    ok: false,
                });
            }
        }
    }

    fn poll_favicon(&mut self) {
        let Some(f) = &mut self.favicon else {
            return;
        };
        let mut done = false;
        while let Some(e) = f.stream.try_next() {
            match e {
                FetchEvent::Head(h) => f.ok = h.ok(),
                FetchEvent::Body(b) => {
                    if f.body.len() < (1 << 20) {
                        f.body.extend_from_slice(&b)
                    }
                }
                FetchEvent::Done | FetchEvent::Error(_) => {
                    done = true;
                    break;
                }
                FetchEvent::Violation(_) => {}
            }
        }
        if done {
            let f = self.favicon.take().unwrap();
            if f.ok && !f.body.is_empty() {
                match decode_icon(&f.body) {
                    Some(img) => {
                        self.event(TabEvent::Favicon(Box::new(IconBitmap::new(&img)), f.body))
                    }
                    None => {
                        if std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1") {
                            eprintln!("[tab {}] favicon {}: not an image", self.id, f.url);
                        }
                    }
                }
            }
        }
    }

    /// The page scrolled its viewport to `to` (CSSOM View "scroll the viewport", by script or a
    /// fragment): the next frame is painted for that place, and the compositor, which holds the
    /// viewport's position, is told with it ([`TabRenderer::tell_page_scroll`]). Until the
    /// compositor says where the viewport is, the tab takes it to be there.
    ///
    /// `by`: scroll anchoring moved it by this much (CSS Scroll Anchoring 1), from where the tab
    /// takes it to be (`to` is ignored); the compositor moves its view by as much from where it is.
    fn page_scrolled(&mut self, to: Point, by: Option<Point>) {
        let to = match by {
            Some(d) => Point::new(self.scroll.x + d.x, self.scroll.y + d.y),
            None => to,
        };
        self.page_scrolls += 1;
        let scroll = crate::comp::PageScroll {
            to,
            serial: self.page_scrolls,
            by,
        };
        // (one not told yet goes with it)
        self.page_scroll = Some(match self.page_scroll {
            Some(earlier) => crate::comp::PageScroll::merge(earlier, scroll),
            None => scroll,
        });
        self.unshown_scroll = Some((
            self.driver.info().generation,
            self.page_scrolls,
            Instant::now(),
        ));
        if self.generation == self.driver.info().generation {
            (self.scroll_dir, self.dir_anchor) = scroll_direction(
                self.scroll_dir,
                self.dir_anchor,
                to.y,
                self.device.viewport.height / 8.0,
            );
            self.scroll = to;
        }
        // (a jump, not a speed)
        self.scroll_history.clear();
        // (the band follows the viewport)
        self.needs_paint = true;
    }

    /// Tells the compositor of a scroll of the page's ([`TabRenderer::page_scrolled`]) that no
    /// frame took with it (the paint for it showed nothing new, or the tab is hidden): it shows
    /// with the last frame painted, which has what the page shows there. The scroll of a document
    /// none of whose frames was painted yet waits for its first, and goes in it.
    fn tell_page_scroll(&mut self) {
        if self.page_scroll.is_none() || self.generation != self.driver.info().generation {
            return;
        }
        let Some(scroll) = self.page_scroll.take() else {
            return;
        };
        self.ctx.surface.compositor(CompMsg::ScrollTo {
            tab: self.id,
            to: scroll.to,
            smooth: false,
            with: Some(crate::comp::FrameRef {
                frame: self.frames,
                generation: self.generation,
                serial: scroll.serial,
            }),
        });
    }

    /// Paints and rasterizes the band around the viewport and hands it to the compositor, with
    /// the fixed and sticky layers the compositor moves on its own as it scrolls. Returns the time
    /// the display list and the raster took (ms).
    fn paint(&mut self) -> Option<(f64, f64)> {
        self.needs_paint = false;
        self.raster.as_ref()?;
        let list_start = Instant::now();
        let info = self.driver.info();
        let generation = info.generation;
        let content = info.content;
        let info_scrollbar = info.scrollbar;
        let color_scheme = info.color_scheme;
        let scrollbar_colors = info.scrollbar_colors;
        let background = info.background;
        let page_background = info.page_background;
        let new_doc = generation != self.generation;
        if new_doc {
            self.pinned = false;
        }
        // A new document starts at the top (or where history restores it), or where it scrolled
        // itself since.
        let scroll = if new_doc {
            let scrolled = self.page_scroll.map(|s| s.to);
            scrolled.or(self.restore).unwrap_or(Point::ZERO)
        } else {
            self.scroll
        };
        if new_doc {
            // (a new document has not been scrolled yet)
            self.scroll_dir = 0.0;
            self.dir_anchor = scroll.y;
            self.scroll_history.clear();
        }
        let skew = self
            .scroll_history
            .skew(Instant::now(), self.scroll_dir, self.device.scale);
        let scale = self.device.scale;
        let viewport = self.device.viewport;
        // (a list found to be pinned is painted again for exactly the viewport, and back)
        let mut attempt = 0;
        let (top, height, origin, split, list_hash, animations) = loop {
            let pinned = self.pinned;
            let (top, height) = band(
                scroll,
                viewport,
                content,
                scale,
                pinned,
                self.scroll_dir,
                skew,
            );
            let origin = Point::new(if pinned { scroll.x } else { 0.0 }, top);
            let mut list = self.driver.paint(origin)?;
            // (what crosses to the raster side stays within a pipe frame: boxes whose animations
            // do not fit are drawn as painted)
            snowlark_paint::layer_animation::fit_budget(&mut list.animations);
            let split = list.split_layers();
            attempt += 1;
            if self.stats {
                let why = split.pin_why.as_ref().map(|w| list.describe_pin(w));
                if why != self.pin_why {
                    if let Some(w) = &why {
                        eprintln!("[tab {}] pinned: {w}", self.id);
                    }
                    self.pin_why = why;
                }
                if list.items.len() > 100_000 && list.items.len() > self.big_list * 11 / 10 {
                    self.big_list = list.items.len();
                    let mut kinds: std::collections::HashMap<String, usize> = Default::default();
                    for item in &list.items {
                        let d = format!("{item:?}");
                        let k = d.split([' ', '(', '{']).next().unwrap_or("").to_owned();
                        *kinds.entry(k).or_default() += 1;
                    }
                    let mut kinds: Vec<_> = kinds.into_iter().collect();
                    kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
                    kinds.truncate(8);
                    eprintln!(
                        "[tab {}] list of {} items: {kinds:?}",
                        self.id,
                        list.items.len()
                    );
                }
            }
            self.pinned = split.pinned;
            if split.pinned != pinned && attempt < 2 {
                continue;
            }
            let mut hashes = Vec::new();
            let images = self.driver.images();
            list.content_hashes(&|id| images.version(id), &mut hashes);
            // (the compositor's animations are part of what the frame shows)
            if !list.animations.is_empty() {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                format!("{:?}", list.animations).hash(&mut h);
                hashes.push(h.finish());
            }
            let list_hash = hashes.iter().fold(hashes.len() as u64, |h, &v| {
                (h.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95)
            });
            break (top, height, origin, split, list_hash, list.animations);
        };
        let pinned = self.pinned;
        let width = if pinned {
            viewport.width
        } else {
            content.width.max(viewport.width).min(viewport.width * 2.0)
        };
        // A pinned list is rasterized past the viewport too ([`pinned_reach`]): the frame's pixel
        // (0, 0) is then above the list's.
        let (above, frame_height) = if pinned {
            pinned_reach(top, viewport, content, scale, self.scroll_dir)
        } else {
            (0.0, height)
        };
        let base_at = (0, -((above * scale).round() as i32));
        let view = Viewport {
            width: (width * scale).ceil().max(1.0) as u32,
            height: (frame_height * scale).ceil().max(1.0) as u32,
            scale,
            background,
        };
        let list_ms = list_start.elapsed().as_secs_f64() * 1000.0;
        // A rendering update that changed nothing the frame shows (an animation out of view, a
        // style change that paints the same) needs no raster and no new frame.
        let signature = frame_signature(
            list_hash,
            &[
                origin.x,
                origin.y,
                top,
                above,
                frame_height,
                width,
                scale,
                viewport.width,
                viewport.height,
                content.width,
                content.height,
                info_scrollbar,
            ],
            generation,
            &format!("{color_scheme:?} {scrollbar_colors:?} {background:?} {page_background:?}"),
        );
        if !new_doc && !self.repaint && self.last_frame == Some(signature) {
            if std::env::var("SNOWLARK_GPU_STATS").is_ok_and(|v| v == "1") {
                eprintln!("[tab {}] unchanged: not rastered", self.id);
            }
            return Some((list_ms, 0.0));
        }
        let Some(raster) = &self.raster else {
            return None;
        };
        let mut pieces = split.layers.into_iter().peekable();
        let base = match pieces.peek() {
            Some(l) if l.kind.is_none() && l.state.0.is_empty() => pieces.next().unwrap().list,
            _ => DisplayList::new(),
        };
        let layers: Vec<SplitLayer> = pieces.collect();
        self.band = Some((top, height, scale));
        // (the `SNOWLARK_TEST_PANIC=tab` testing switch: a panic on the tab's thread, outside any
        // engine call, once a page has shown)
        if self.painted && snowlark_js::guard::test_panic(snowlark_js::guard::TestPanic::Tab) {
            panic!("SNOWLARK_TEST_PANIC=tab: a panic on the tab's thread");
        }
        // (`SNOWLARK_TEST_CRASH=stack|av|abort|hang`: a hard failure of this thread at the same place)
        if self.painted {
            if let Some(kind) = snowlark_js::guard::test_crash() {
                snowlark_js::guard::crash(kind);
            }
        }
        if !self.painted && self.stats {
            eprintln!(
                "[tab {}] first frame sent {:.1} ms after the tab was started",
                self.id,
                self.live.age().as_secs_f64() * 1000.0
            );
        }
        self.painted = true;
        // (a repaint for a new raster side names no place: the compositor made again was given
        // the tab's place by the browser, moved by the wheel turned meanwhile, which is newer than
        // the tab's own, `Scrolled` before the restart)
        let restore = if new_doc { self.restore.take() } else { None };
        self.repaint = false;
        self.generation = generation;
        self.last_frame = Some(signature);
        self.frames += 1;
        if new_doc {
            // (where the compositor places a new document's first frame: the frames that follow
            // are painted for that place until it says where the viewport is)
            self.scroll = scroll;
        }
        // The encode and raster threads draw it and hand it to the compositor; the page goes on
        // meanwhile.
        raster.post(crate::raster_thread::PaintedFrame {
            base,
            layers,
            view,
            viewport,
            base_at,
            text: self.text.clone(),
            images: self.driver.image_store(),
            meta: crate::raster_thread::FrameMeta {
                origin,
                scale,
                content,
                scrollbar: info_scrollbar,
                color_scheme,
                scrollbar_colors,
                // (what the compositor shows where the frame has no tile)
                background: page_background,
                generation,
                number: self.frames,
                restore,
                // (the page's scroll this frame was painted for shows with it)
                scroll: self.page_scroll.take(),
            },
            band: (top - above, top - above + frame_height),
            pinned,
            animations,
        });
        Some((list_ms, 0.0))
    }
}

/// The device pixel rectangle (x, y, width, height) of a layer's raster in its list's device
/// coordinates: what its items draw, within the band `view` (whose pixel (0, 0) is the list's
/// `base_at`) for content that scrolls with the document, within the viewport for a fixed layer, near its container for a sticky one. `None`
/// when nothing shows. `backdrop`: the box (CSS px) of the backdrop filter the layer starts with
/// ([`DisplayList::take_leading_backdrop`]), which shows there though the list draws nothing.
pub(crate) fn layer_region(
    list: &DisplayList,
    kind: Option<LayerKind>,
    view: &Viewport,
    viewport: Size,
    base_at: (i32, i32),
    backdrop: Option<Rect>,
) -> Option<(i32, i32, u32, u32)> {
    let scale = view.scale;
    let mut drawn = list.drawn_bounds(&Matrix::scale(scale, scale));
    if let Some(b) = backdrop {
        let b = Rect::new(b.x * scale, b.y * scale, b.width * scale, b.height * scale);
        drawn = if drawn.is_empty() { b } else { drawn.union(&b) };
    }
    let limit = match kind {
        None => Rect::new(
            base_at.0 as f32,
            base_at.1 as f32,
            view.width as f32,
            view.height as f32,
        ),
        Some(LayerKind::Fixed) => {
            Rect::new(0.0, 0.0, viewport.width * scale, viewport.height * scale)
        }
        Some(LayerKind::Sticky(c)) => {
            // (its shadows and overflow may reach a little past its container)
            let reach = viewport.height.max(viewport.width) * scale / 4.0;
            let r = c.container.union(&c.rect);
            Rect::new(r.x * scale, r.y * scale, r.width * scale, r.height * scale)
                .inflate(reach, reach)
        }
        Some(LayerKind::Pinned) => return None,
        // (in its own coordinates, where the band does not apply; cut to the size below)
        Some(LayerKind::Composited(_)) => drawn,
    };
    let r = drawn.intersect(&limit);
    if r.is_empty() {
        return None;
    }
    let x0 = r.x.floor();
    let y0 = r.y.floor();
    // (no layer is larger than a few viewports: a huge sticky container is cut)
    let max = 4.0 * (viewport.width.max(viewport.height) * scale).max(1.0);
    let w = ((r.x + r.width).ceil() - x0).min(max);
    let h = ((r.y + r.height).ceil() - y0).min(max);
    Some((x0 as i32, y0 as i32, w.max(1.0) as u32, h.max(1.0) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    const VIEW: Size = Size::new(1000.0, 800.0);

    /// [`super::band`] of a view that is not moving fast (no skew).
    fn band(
        scroll: Point,
        viewport: Size,
        content: Size,
        scale: f32,
        pinned: bool,
        direction: f32,
    ) -> (f32, f32) {
        super::band(scroll, viewport, content, scale, pinned, direction, 0.0)
    }

    /// [`super::band_serves`] of a view that is not moving fast (no skew).
    fn band_serves(
        band: (f32, f32),
        scroll: Point,
        viewport: Size,
        content: Size,
        direction: f32,
    ) -> bool {
        super::band_serves(band, scroll, viewport, content, direction, 0.0)
    }

    /// Chrome's skewport: a band reaches ahead by where the view will be soon at its speed.
    #[test]
    fn scroll_history_measures_the_speed_ahead() {
        let t = Instant::now();
        let ms = |n: u64| t + std::time::Duration::from_millis(n);
        let mut h = ScrollHistory::default();
        assert_eq!(h.skew(t, 1.0, 1.0), 0.0);
        // 3000 px/s down: 600 px in 0.2 s.
        for i in 0..=10u64 {
            h.push(ms(i * 10), i as f32 * 30.0);
        }
        let skew = h.skew(ms(100), 1.0, 1.0);
        assert!((skew - 600.0).abs() < 1.0, "{skew}");
        // Only ahead: none the other way.
        assert_eq!(h.skew(ms(100), -1.0, 1.0), 0.0);
        assert_eq!(h.skew(ms(100), 0.0, 1.0), 0.0);
        // At most 2000 device pixels.
        let mut fast = ScrollHistory::default();
        fast.push(t, 0.0);
        fast.push(ms(10), 1000.0);
        assert_eq!(fast.skew(ms(10), 1.0, 1.5), 2000.0 / 1.5);
        // Still the speed after a long rendering update; a view that stopped has none.
        assert!((h.skew(ms(300), 1.0, 1.0) - 600.0).abs() < 1.0);
        assert_eq!(h.skew(ms(700), 1.0, 1.0), 0.0);
        // A tab busy for 300 ms then hearing a batch: the speed spans the wait.
        let mut busy = ScrollHistory::default();
        busy.push(t, 0.0);
        for y in [890.0, 895.0, 900.0] {
            busy.push(ms(300), y);
        }
        let skew = busy.skew(ms(300), 1.0, 1.0);
        assert!((skew - 600.0).abs() < 1.0, "{skew}");
        // A jump is no speed.
        busy.clear();
        assert_eq!(busy.skew(ms(300), 1.0, 1.0), 0.0);
    }

    /// A fast scroll gets a band reaching further ahead, asked for earlier: at 150% a viewport is
    /// two thirds of the CSS px it is at 100%, and the wheel moves the same CSS px.
    #[test]
    fn fast_scrolls_reach_further_ahead() {
        let content = Size::new(1000.0, 10_000.0);
        let at = Point::new(0.0, 3000.0);
        assert_eq!(
            super::band(at, VIEW, content, 1.5, false, 1.0, 600.0),
            (2600.0, 3000.0)
        );
        assert_eq!(
            super::band(at, VIEW, content, 1.5, false, -1.0, 600.0),
            (1200.0, 3000.0)
        );
        // Not past the document.
        assert_eq!(
            super::band(
                Point::new(0.0, 9000.0),
                VIEW,
                content,
                1.0,
                false,
                1.0,
                600.0
            ),
            (8600.0, 1400.0)
        );
        // A band made for no speed is replaced sooner at speed: 600 px of view left ahead is
        // under the 600 px the view covers before the next band shows, plus the usual margin.
        let b = band(at, VIEW, content, 1.0, false, 1.0);
        assert!(band_serves(b, Point::new(0.0, 3500.0), VIEW, content, 1.0));
        assert!(!super::band_serves(
            b,
            Point::new(0.0, 3500.0),
            VIEW,
            content,
            1.0,
            600.0
        ));
        // A band made at speed serves the view it was made for, at that speed.
        let b = super::band(at, VIEW, content, 1.0, false, 1.0, 600.0);
        assert!(super::band_serves(b, at, VIEW, content, 1.0, 600.0));
        assert!(super::band_serves(
            b,
            Point::new(0.0, 3600.0),
            VIEW,
            content,
            1.0,
            600.0
        ));
    }

    #[test]
    fn icons_in_bitmap_and_svg_formats() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 18 18"><rect width="18" height="18" fill="#f60"/></svg>"##;
        let img = decode_icon(svg).unwrap();
        assert_eq!((img.width(), img.height()), (32, 32));
        assert!(decode_icon(b"not an icon").is_none());
    }

    #[test]
    fn pointer_moves_coalesce_up_to_other_input() {
        let mv = |x: f32| TabCommand::Pointer(Pointer::Move(Point::new(x, 0.0)));
        let mut c = vec![
            mv(1.0),
            mv(2.0),
            TabCommand::Wake,
            mv(3.0),
            TabCommand::Pointer(Pointer::Leave),
            mv(4.0),
            TabCommand::Scrolled(Point::ZERO, Default::default()),
            mv(5.0),
            mv(6.0),
        ];
        coalesce_moves(&mut c);
        let kept: Vec<String> = c
            .iter()
            .map(|c| match c {
                TabCommand::Pointer(Pointer::Move(p)) => format!("move {}", p.x),
                TabCommand::Pointer(Pointer::Leave) => "leave".into(),
                TabCommand::Wake => "wake".into(),
                TabCommand::Scrolled(..) => "scrolled".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            kept,
            ["wake", "move 3", "leave", "move 4", "scrolled", "move 6"]
        );
    }

    #[test]
    fn bands_cover_a_viewport_each_side() {
        let content = Size::new(1000.0, 10_000.0);
        assert_eq!(
            band(Point::ZERO, VIEW, content, 1.0, false, 0.0),
            (0.0, 1600.0)
        );
        assert_eq!(
            band(Point::new(0.0, 3000.0), VIEW, content, 1.0, false, 0.0),
            (2200.0, 2400.0)
        );
        // At the bottom the band ends with the document.
        assert_eq!(
            band(Point::new(0.0, 9200.0), VIEW, content, 1.0, false, 0.0),
            (8400.0, 1600.0)
        );
        // A short document: one viewport.
        assert_eq!(
            band(Point::ZERO, VIEW, Size::new(1000.0, 300.0), 1.0, false, 0.0),
            (0.0, 800.0)
        );
        // Device pixel aligned top at 150%.
        let (top, _) = band(Point::new(0.0, 1000.3), VIEW, content, 1.5, false, 0.0);
        assert_eq!((top * 1.5).fract(), 0.0);
        // Fixed boxes: exactly the viewport.
        assert_eq!(
            band(Point::new(0.0, 3000.0), VIEW, content, 1.0, true, 0.0),
            (3000.0, 800.0)
        );
    }

    #[test]
    fn bands_are_replaced_near_their_edges() {
        let content = Size::new(1000.0, 10_000.0);
        let b = band(Point::new(0.0, 3000.0), VIEW, content, 1.0, false, 0.0);
        assert!(band_serves(b, Point::new(0.0, 3000.0), VIEW, content, 0.0));
        assert!(band_serves(b, Point::new(0.0, 3300.0), VIEW, content, 0.0));
        assert!(!band_serves(b, Point::new(0.0, 3700.0), VIEW, content, 0.0));
        assert!(!band_serves(b, Point::new(0.0, 2300.0), VIEW, content, 0.0));
        // At the top of the document the band serves all the way up.
        let b = band(Point::ZERO, VIEW, content, 1.0, false, 0.0);
        assert!(band_serves(b, Point::ZERO, VIEW, content, 0.0));
        assert!(band_serves(b, Point::new(0.0, 600.0), VIEW, content, 0.0));
        assert!(!band_serves(b, Point::new(0.0, 700.0), VIEW, content, 0.0));
        // And at the bottom all the way down.
        let b = band(Point::new(0.0, 9200.0), VIEW, content, 1.0, false, 0.0);
        assert!(band_serves(b, Point::new(0.0, 9200.0), VIEW, content, 0.0));
    }

    #[test]
    fn bands_reach_ahead_the_way_the_view_scrolls() {
        let content = Size::new(1000.0, 10_000.0);
        let at = Point::new(0.0, 3000.0);
        // Down: half a viewport above, a viewport and a half below the view; up: the reverse.
        assert_eq!(band(at, VIEW, content, 1.0, false, 1.0), (2600.0, 2400.0));
        assert_eq!(band(at, VIEW, content, 1.0, false, -1.0), (1800.0, 2400.0));
        // Near the top of the document going up, the band starts there.
        assert_eq!(
            band(Point::new(0.0, 500.0), VIEW, content, 1.0, false, -1.0),
            (0.0, 1700.0)
        );
        // Pinned content ignores the direction.
        assert_eq!(band(at, VIEW, content, 1.0, true, 1.0), (3000.0, 800.0));
    }

    #[test]
    fn bands_are_replaced_with_most_of_a_viewport_left_ahead() {
        let content = Size::new(1000.0, 10_000.0);
        // Scrolling down: the band below the view lasts 600 px of travel, with 600 px still left.
        let b = band(Point::new(0.0, 3000.0), VIEW, content, 1.0, false, 1.0);
        assert!(band_serves(b, Point::new(0.0, 3000.0), VIEW, content, 1.0));
        assert!(band_serves(b, Point::new(0.0, 3600.0), VIEW, content, 1.0));
        assert!(!band_serves(b, Point::new(0.0, 3601.0), VIEW, content, 1.0));
        // Turning back up asks for a band that reaches up.
        assert!(!band_serves(
            b,
            Point::new(0.0, 2900.0),
            VIEW,
            content,
            -1.0
        ));
        // Scrolling up, mirrored.
        let b = band(Point::new(0.0, 3000.0), VIEW, content, 1.0, false, -1.0);
        assert!(band_serves(b, Point::new(0.0, 2400.0), VIEW, content, -1.0));
        assert!(!band_serves(
            b,
            Point::new(0.0, 2399.0),
            VIEW,
            content,
            -1.0
        ));
        // A band made for each way serves where it was made.
        for dir in [-1.0, 0.0, 1.0] {
            for y in [0.0, 500.0, 3000.0, 9200.0] {
                let p = Point::new(0.0, y);
                assert!(band_serves(
                    band(p, VIEW, content, 1.0, false, dir),
                    p,
                    VIEW,
                    content,
                    dir
                ));
            }
        }
    }

    #[test]
    fn pinned_frames_reach_past_the_viewport_the_way_it_scrolls() {
        let content = Size::new(1000.0, 10_000.0);
        // (above the view's top, frame height)
        assert_eq!(
            pinned_reach(3000.0, VIEW, content, 1.0, 1.0),
            (200.0, 2200.0)
        );
        assert_eq!(
            pinned_reach(3000.0, VIEW, content, 1.0, -1.0),
            (800.0, 2000.0)
        );
        assert_eq!(
            pinned_reach(3000.0, VIEW, content, 1.0, 0.0),
            (400.0, 2000.0)
        );
        // Not above the document's top, nor past its end.
        assert_eq!(
            pinned_reach(100.0, VIEW, content, 1.0, -1.0),
            (100.0, 1300.0)
        );
        assert_eq!(
            pinned_reach(9200.0, VIEW, content, 1.0, 1.0),
            (200.0, 1000.0)
        );
        // A short document: at least the viewport.
        assert_eq!(
            pinned_reach(0.0, VIEW, Size::new(1000.0, 300.0), 1.0, 1.0),
            (0.0, 800.0)
        );
        // Whole device pixels above, at 150%.
        let (above, _) = pinned_reach(1000.0, Size::new(1000.0, 777.0), content, 1.5, 1.0);
        assert_eq!((above * 1.5).fract(), 0.0);
        // Within what the renderer paints around the viewport: from 1.5 viewports above it to 3.5
        // below its top.
        for dir in [-1.0, 0.0, 1.0] {
            let (above, height) = pinned_reach(5000.0, VIEW, content, 1.0, dir);
            assert!(above <= 1.5 * VIEW.height && height - above <= 3.5 * VIEW.height);
        }
    }

    /// A report of where the viewport is, naming the last of the page's scrolls the compositor
    /// showed, is dropped while it names one from before the page's own last scroll (of its
    /// document, or of an earlier document), and taken once it names that scroll or a later
    /// document, or once the wait for the scroll has run out.
    #[test]
    fn reports_from_before_the_pages_last_scroll_are_dropped() {
        use crate::comp::ShownScroll;
        let shown = |generation, serial| ShownScroll { generation, serial };
        let at = Instant::now();
        let unshown = (4, 7, at);
        assert!(report_predates(unshown, shown(4, 6), at));
        assert!(report_predates(unshown, shown(4, 0), at));
        assert!(report_predates(unshown, shown(3, 9), at));
        assert!(!report_predates(unshown, shown(4, 7), at));
        assert!(!report_predates(unshown, shown(4, 8), at));
        assert!(!report_predates(unshown, shown(5, 0), at));
        let later = at + UNSHOWN_SCROLL_WAIT;
        assert!(report_predates(
            unshown,
            shown(4, 6),
            later - std::time::Duration::from_millis(1)
        ));
        assert!(!report_predates(unshown, shown(4, 6), later));
    }

    #[test]
    fn scroll_direction_turns_after_a_threshold() {
        // Not known until the view moved the threshold.
        assert_eq!(scroll_direction(0.0, 0.0, 50.0, 100.0), (0.0, 0.0));
        assert_eq!(scroll_direction(0.0, 0.0, 120.0, 100.0), (1.0, 120.0));
        // Going on the same way moves the anchor along.
        assert_eq!(scroll_direction(1.0, 120.0, 400.0, 100.0), (1.0, 400.0));
        // Jitter back does not turn it.
        assert_eq!(scroll_direction(1.0, 400.0, 340.0, 100.0), (1.0, 400.0));
        // A real turn does.
        assert_eq!(scroll_direction(1.0, 400.0, 300.0, 100.0), (-1.0, 300.0));
        assert_eq!(scroll_direction(-1.0, 300.0, 200.0, 100.0), (-1.0, 200.0));
        assert_eq!(scroll_direction(-1.0, 200.0, 310.0, 100.0), (1.0, 310.0));
    }
}
