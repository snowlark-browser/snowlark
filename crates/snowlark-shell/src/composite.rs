// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compositor thread: it owns the window's [`Compositor`] (the DirectComposition layers), every
//! tab's latest tiled frame and every tab's viewport scroll position, and presents frames.
//!
//! Scrolling happens here, not on the tab's thread (as Chrome's compositor thread scrolls): the
//! window sends wheel and key scrolls ([`CompMsg::ScrollBy`]), the animation advances once per
//! display refresh, and each frame re-composes the tiles the tab already rasterized (a band of
//! about three viewports around the scroll position) at the new offset. The tab hears where the
//! viewport is ([`TabCommand::Scrolled`]) and rasterizes a new band when the view nears an edge of
//! the old one. Nothing here waits for a tab.
//!
//! A scroll the page makes itself (script, a fragment) is another matter: the page paints a frame
//! for the new place, often with new content, and the scroll shows with that frame. It comes in
//! the frame ([`FrameData::scroll`]), or, when the page had nothing new to paint for it, as a
//! message that names the frame it shows with ([`CompMsg::ScrollTo`]'s `with`) and waits for it.
//! Until the frame comes, the frame on screen stays where it is and goes on being presented, so
//! no frame shows the old content at the new place or the new content at the old one; the wait
//! is bounded ([`SHOWN_WITH_FRAME_WAIT`]).
//!
//! Fixed and sticky content comes as layers of its own ([`LayerFrame`]), placed at each frame for
//! the scroll position: a fixed layer stays in the viewport, a sticky one moves with the document
//! and is shifted per its constraints. So the page shows them right while the compositor scrolls
//! ahead of the tab's paint.
//!
//! Frame pacing: idle, the thread sleeps on its channel and presents only when something changed.
//! While the page moves it waits on the swap chain's frame-latency object and presents every
//! refresh, and keeps doing so for a moment after the motion ends, so a variable-refresh display
//! stays at its maximum rate instead of following our frame times.
//!
//! Videos come as quads of the frame or of a layer ([`crate::video::VideoQuad`]): the compositor
//! draws each video's latest GPU frame there, then what the page paints over it, and composes
//! again whenever a video it shows has a new frame ([`CompMsg::VideoFrame`]).
//!
//! `SNOWLARK_GPU_STATS=1` prints each frame's timings to stderr and a summary after each scroll. A
//! frame presented with rows of the page area that no tile of it covers says so (`UNCOVERED`), and
//! so does a page's scroll that waited for its frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use snowlark_paint::compositor::WindowSurface;
use snowlark_paint::window::WindowHandle;
use snowlark_paint::{
    Color, Compositor, DisplayList, DisplayListBuilder, FontTable, Gpu, ImageStore, ImageTable,
    LayerKind, LayerState, Point, Radii, Rect, Resource, Size, Sources, StateOp, TextEngine, Ticks,
};
use snowlark_tab::raster_thread::Sent;
use snowlark_tab::watchdog::PassMark;

use crate::begin_frames::{BeginFrameSource, VSyncThread};
use crate::driver::{ColorScheme, ScrollbarColors};
use crate::scroll::{ScrollKind, Scrollbar, SmoothScroll};
use crate::tab::{TabCommand, TabId};
use crate::video::{Picture, VideoQuad};
use snowlark_paint::compositor::Underlay;
use snowlark_paint::video::VideoStore;
pub use snowlark_tab::comp::{
    AutoTarget, CompEvent, CompInput, CompMsg, DevtoolsFrame, FrameData, FrameRef, Handle,
    LayerFrame, WindowId, SHOWN_WITH_FRAME_WAIT,
};

/// The compositors of the browser's windows, by window: where a tab shown in another window has
/// its frames sent ([`CompMsg::Forward`]). Each compositor is in it while it runs.
pub type Windows = Arc<Mutex<HashMap<WindowId, Sender<CompInput>>>>;

/// How long frames keep going out at the display's rate after scrolling stops.
const KEEP_PRESENTING: Duration = Duration::from_millis(250);

/// The viewport scroll bar's width (DIPs) and how far it is from the edge.
pub const SCROLLBAR_WIDTH: f32 = 17.0;

/// Where a layer's pixel (0, 0) goes in the content area (device pixels) when the frame's own
/// pixel (0, 0) goes to `base` and the viewport is scrolled to `scroll`.
pub fn layer_position(
    layer: &LayerFrame,
    data: &FrameData,
    base: (i32, i32),
    scroll: Point,
) -> (i32, i32) {
    let (x, y) = layer.at;
    match layer.kind {
        None | Some(LayerKind::Pinned) | Some(LayerKind::Composited(_)) => (base.0 + x, base.1 + y),
        Some(LayerKind::Fixed) => (x, y),
        Some(LayerKind::Sticky(c)) => {
            // The list was painted for the viewport at the frame's origin; the shift is the
            // constraint's at the scroll position less the one painted in.
            let (w, h) = (data.viewport.width, data.viewport.height);
            let painted = c.offset(Rect::new(0.0, 0.0, w, h));
            let now = c.offset(Rect::new(
                scroll.x - data.origin.x,
                scroll.y - data.origin.y,
                w,
                h,
            ));
            let s = data.scale;
            (
                base.0 + x + ((now.x - painted.x) * s).round() as i32,
                base.1 + y + ((now.y - painted.y) * s).round() as i32,
            )
        }
    }
}

/// A sticky piece drawn through the clips around it (its state: the clips of content that scrolls
/// with the document, in the list's CSS px), moved `shift` device pixels from where it was painted:
/// the state to draw its tiles through (the clips, then the move, at `scale`), and the device rect
/// its videos show within (the clips' bounds with the list's pixel (0, 0) at `base`).
pub fn moved_through(
    state: &LayerState,
    shift: (i32, i32),
    scale: f32,
    base: (i32, i32),
) -> (LayerState, Rect) {
    let mut within = Rect::infinite();
    for op in &state.0 {
        if let StateOp::Clip(r) | StateOp::ClipRounded(r, _) = op {
            let d = Rect::new(
                r.x * scale + base.0 as f32,
                r.y * scale + base.1 as f32,
                r.width * scale,
                r.height * scale,
            );
            within = within.intersect(&d);
        }
    }
    let mut moved = state.clone();
    if shift != (0, 0) && scale > 0.0 {
        moved.0.push(StateOp::Transform([
            1.0,
            0.0,
            0.0,
            1.0,
            shift.0 as f32 / scale,
            shift.1 as f32 / scale,
        ]));
    }
    (moved, within)
}

/// How many device rows of a page area `area_height` high show none of `frame`'s content when the
/// viewport is scrolled to `scroll`, though the document has content there: the frame was painted
/// for another part of the document. 0 for a frame that covers what shows.
pub fn uncovered_rows(frame: &FrameData, scroll: Point, area_height: u32) -> u32 {
    let s = frame.scale;
    let dy = (frame.origin.y * s).round() as i64 - (scroll.y * s).round() as i64;
    let top = dy + i64::from(frame.base_at.1);
    let bottom = top + i64::from(frame.frame.height);
    // (the rows of the area the document reaches)
    let document = ((frame.content.height - scroll.y) * s).round() as i64;
    let need = document.clamp(0, i64::from(area_height));
    let covered = (bottom.min(need) - top.max(0)).max(0);
    (need - covered) as u32
}

/// What the window can read about a tab's viewport scroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScrollView {
    pub scroll: Point,
    pub target: Point,
    pub max: Point,
    pub viewport: Size,
    pub content: Size,
    /// The width (DIPs) of the classic vertical scroll bar at the viewport's right edge (0: none
    /// shows).
    pub bar: f32,
}

pub type SharedScroll = Arc<Mutex<HashMap<TabId, ScrollView>>>;

/// The window's handle to the compositor thread.
pub struct CompositorThread {
    tx: Sender<CompInput>,
    /// The compositor's thread, or (in the GPU process) the thread that passes its input on.
    thread: Option<JoinHandle<()>>,
    gpu_raster: Option<Arc<dyn snowlark_tab::raster_thread::GpuRaster>>,
    scroll: SharedScroll,
    /// What the compositor's tables have of the window's text engine and image store.
    ui: Mutex<UiSent>,
    /// What the compositor tells the window ([`CompEvent`]), and the last [`CompMsg::Ack`]'s number.
    events: Mutex<Receiver<CompEvent>>,
    acked: AtomicU64,
    windows: Windows,
    window: WindowId,
    /// The window's visual tree, when the compositor presents through a composition surface
    /// ([`CompMsg::Surface`]). Kept until the compositor thread has gone.
    surface: Option<WindowSurface>,
}

/// The window's fonts and images, and what of them the compositor has been sent.
struct UiSent {
    text: Arc<TextEngine>,
    images: Arc<ImageStore>,
    sent: Sent,
}

impl UiSent {
    /// Gives `out` what the compositor's tables need for the window's lists in `input`, then
    /// `input`.
    fn send(&mut self, input: CompInput, out: &mut dyn FnMut(CompInput)) {
        let UiSent { text, images, sent } = self;
        let lists = ui_lists(&input).into_iter().flatten();
        for r in sent.resources(&"window", text, images, lists) {
            if let Some(r) = CompInput::resource(r) {
                out(r);
            }
        }
        out(input);
    }
}

/// The window's own lists in `input` (shaped with its text engine, showing its images).
fn ui_lists(input: &CompInput) -> [Option<&DisplayList>; 2] {
    let msg = match input {
        CompInput::Msg(m) => m,
        _ => return [None, None],
    };
    match msg {
        CompMsg::Resize {
            chrome, overlay, ..
        } => [chrome.as_ref().map(|(l, _)| l), overlay.as_ref()],
        CompMsg::Chrome(list, _) | CompMsg::Overlay(list) => [Some(list), None],
        CompMsg::DevtoolsWindow(Some(f)) => [Some(&f.list), None],
        _ => [None, None],
    }
}

impl CompositorThread {
    /// The compositor of `window`. `text` and `images` are what the window's own lists (the chrome,
    /// the overlay, the undocked developer tools') are shaped with and show: the compositor gets
    /// their fonts and images as messages, ahead of the lists, into tables of its own. It is in
    /// `windows` while it runs, and finds the other windows' compositors there. With `surfaces`
    /// the window keeps a visual tree with no device of its own and the compositor presents one
    /// swap chain into it, the chrome and the content together ([`CompMsg::Surface`]; the GPU
    /// process's path, `docs/process-isolation.md` M7); else the compositor binds a visual tree of
    /// its own to the window.
    ///
    /// With `gpu_process` (a link to the GPU process that has not fallen back) the compositor runs
    /// there: the window keeps its visual tree with a surface, and this end passes everything on
    /// up the GPU process's pipe ([`crate::gpu_compositor`]). A window whose surface cannot be made
    /// keeps its compositor here.
    pub fn start(
        gpu: Arc<Gpu>,
        window: WindowHandle,
        text: Arc<TextEngine>,
        images: Arc<ImageStore>,
        windows: &Windows,
        gpu_process: Option<crate::gpu_process::GpuLink>,
    ) -> CompositorThread {
        let (tx, rx) = channel();
        let id = WindowId::from(window);
        let link = gpu_process.filter(|l| l.status() != crate::gpu_process::GpuStatus::FellBack);
        let surface = link.is_some().then(|| attach(&tx, window)).flatten();
        let link = link.filter(|_| surface.is_some());
        windows.lock().unwrap().insert(id, tx.clone());
        let (events_tx, events) = snowlark_ipc::channel();
        let scroll: SharedScroll = Arc::default();
        let thread = match &link {
            Some(link) => crate::gpu_compositor::start_proxy(
                link.clone(),
                id,
                rx,
                events_tx,
                scroll.clone(),
                crate::gpu_compositor::InBrowser {
                    gpu,
                    window,
                    windows: windows.clone(),
                },
            ),
            None => {
                let out = local_out(scroll.clone(), events_tx);
                spawn(
                    gpu,
                    window,
                    out,
                    windows.clone(),
                    tx.clone(),
                    rx,
                    PassMark::default(),
                )
            }
        };
        CompositorThread {
            tx,
            thread: Some(thread),
            gpu_raster: link.map(|l| crate::gpu_compositor::window_raster(l, id)),
            scroll,
            ui: Mutex::new(UiSent {
                text,
                images,
                sent: Sent::default(),
            }),
            events: Mutex::new(events),
            acked: AtomicU64::new(0),
            windows: windows.clone(),
            window: id,
            surface,
        }
    }

    /// Whether windows show what the compositor presents through a surface of theirs.
    pub fn surfaces(&self) -> bool {
        self.surface.is_some()
    }

    /// With the compositor in the GPU process: its raster threads, for the window's renderers.
    pub fn gpu_raster(&self) -> Option<Arc<dyn snowlark_tab::raster_thread::GpuRaster>> {
        self.gpu_raster.clone()
    }

    /// For another window the compositor draws (the undocked developer tools'): its visual tree
    /// when windows show the compositor's surfaces, which the caller keeps while the window lives.
    pub fn attach(&self, window: WindowHandle) -> Option<WindowSurface> {
        self.surface.as_ref()?;
        attach(&self.tx, window)
    }

    /// Sends `msg` and waits, at most `timeout`, until a frame showing it is on its way to the
    /// screen; false when it is not by then or the compositor has gone.
    pub fn send_and_wait(&self, msg: CompMsg, timeout: Duration) -> bool {
        let seq = self.acked.fetch_add(1, Ordering::Relaxed) + 1;
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        // (what came before this wait is of earlier ones: a GPU process that ended before it
        // never had this message, which goes to the next one)
        while events.try_recv().is_ok() {}
        self.send(msg);
        self.send(CompMsg::Ack(seq));
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left) {
                // (an answer to a wait that timed out before comes first)
                Ok(CompEvent::Presented(n)) if n >= seq => return true,
                Ok(CompEvent::Presented(_)) => {}
                // (the GPU process ended: nothing will be presented for it)
                Ok(CompEvent::Lost) | Err(_) => return false,
            }
        }
    }

    pub fn sender(&self) -> Sender<CompInput> {
        self.tx.clone()
    }

    /// Sends `msg`, after what the compositor's tables need for the window's lists in it.
    pub fn send(&self, msg: impl Into<CompInput>) {
        let input = msg.into();
        if ui_lists(&input).iter().all(Option::is_none) {
            let _ = self.tx.send(input);
            return;
        }
        // (held across both, so the resources stay ahead of their list)
        let mut ui = self.ui.lock().unwrap_or_else(|e| e.into_inner());
        ui.send(input, &mut |i| {
            let _ = self.tx.send(i);
        });
    }

    /// The scroll positions, for a tab's host.
    pub fn scroll_table(&self) -> SharedScroll {
        self.scroll.clone()
    }

    pub fn scroll_view(&self, tab: TabId) -> ScrollView {
        self.scroll
            .lock()
            .unwrap()
            .get(&tab)
            .copied()
            .unwrap_or_default()
    }
}

/// Where a compositor tells its window of scroll positions and presented frames: the window's
/// tables in the same process, or the GPU process's pipe.
pub(crate) trait WindowOut: Send {
    /// Tab `tab`'s viewport scroll (`None`: the tab has gone).
    fn scroll(&self, tab: TabId, view: Option<ScrollView>);
    /// The frame [`CompMsg::Ack`] `seq` asked for is on its way to the screen.
    fn presented(&self, seq: u64);
    /// The window's compositor could not be made, for the reason `why` (already in this
    /// process's log).
    fn no_compositor(&self, _why: &str) {}
}

struct LocalOut {
    scroll: SharedScroll,
    events: snowlark_ipc::Sender<CompEvent>,
}

/// Where a compositor in the browser's process tells its window: straight into the window's
/// tables.
pub(crate) fn local_out(
    scroll: SharedScroll,
    events: snowlark_ipc::Sender<CompEvent>,
) -> Box<dyn WindowOut> {
    Box::new(LocalOut { scroll, events })
}

impl WindowOut for LocalOut {
    fn scroll(&self, tab: TabId, view: Option<ScrollView>) {
        let mut table = self.scroll.lock().unwrap();
        match view {
            Some(v) => table.insert(tab, v),
            None => table.remove(&tab),
        };
    }

    fn presented(&self, seq: u64) {
        let _ = self.events.send(CompEvent::Presented(seq));
    }
}

/// Runs the compositor of `window` on a thread of its own, on `gpu`: it takes what comes on `rx`
/// (`tx` is its own sender, for its vsyncs), tells the window through `out`, and finds the other
/// windows' compositors in `windows`. Its passes are marked in `mark` (the GPU process's
/// watchdog).
pub(crate) fn spawn(
    gpu: Arc<Gpu>,
    window: WindowHandle,
    out: Box<dyn WindowOut>,
    windows: Windows,
    tx: Sender<CompInput>,
    rx: Receiver<CompInput>,
    mark: PassMark,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("snowlark compositor".into())
        .spawn(move || {
            // What the screen waits for runs before the machine's other work: at normal
            // priority, a busy machine delays its wake-up by several frames.
            if std::env::var("SNOWLARK_COMPOSITOR_PRIORITY").map_or(true, |v| v != "normal") {
                snowlark_paint::window::raise_thread_priority();
            }
            let fonts = match FontTable::new() {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[gpu] no font table: {e}");
                    return;
                }
            };
            let mut state = State::new(gpu, window, fonts, out, windows);
            // (vsyncs reach the compositor as Chromium's reach viz's thread:
            // `ExternalBeginFrameSourceWin::OnVSync` posts them to it)
            state.vsync = Some(VSyncThread::start(move |time, interval| {
                let _ = tx.send(CompInput::VSync(time, interval));
            }));
            state.run(rx, &mark);
        })
        .expect("compositor thread")
}

/// Makes `window`'s visual tree with a composition surface and sends it to the compositor at `tx`;
/// `None` when that fails (the compositor then binds a tree of its own).
fn attach(tx: &Sender<CompInput>, window: WindowHandle) -> Option<WindowSurface> {
    match WindowSurface::new(window) {
        Ok(surface) => {
            let _ = tx.send(CompInput::tree(WindowId::from(window), surface.clone()));
            Some(surface)
        }
        Err(e) => {
            eprintln!("[gpu] no composition surface for the window: {e}");
            None
        }
    }
}

impl Drop for CompositorThread {
    fn drop(&mut self) {
        self.windows.lock().unwrap().remove(&self.window);
        let _ = self.tx.send(CompMsg::Quit.into());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Layer {
    tab: snowlark_ipc::Sender<TabCommand>,
    /// The tab asks for begin frames ([`CompMsg::NeedsBeginFrames`]).
    needs_begin_frames: bool,
    frame: Option<Box<FrameData>>,
    scroll: SmoothScroll,
    reported: Option<Point>,
    generation: u64,
    restore: Option<Point>,
    /// The page's scroll that waits for its frame.
    held: Option<Held>,
    /// The serial of the last of the page's scrolls shown ([`snowlark_tab::comp::PageScroll`]): one
    /// from before it that comes later (a message a frame overtook) is not shown.
    page_scroll: u64,
}

/// A page's scroll waiting for the frame it shows with ([`CompMsg::ScrollTo`]'s `with`).
#[derive(Clone, Copy, Debug)]
struct Held {
    to: Point,
    with: FrameRef,
    since: Instant,
}

impl Held {
    /// Whether `frame` is one the scroll shows with: the frame painted for it, or a later one.
    fn shows_with(&self, frame: &FrameData) -> bool {
        frame.number >= self.with.frame
    }
}

/// An autoscroll in progress ([`CompMsg::AutoScroll`]).
#[derive(Clone, Copy)]
struct Auto {
    tab: TabId,
    target: AutoTarget,
    velocity: Point,
    /// The distance not yet scrolled: less than a pixel.
    carry: Point,
}

/// What an autoscroll moves in one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoStep {
    /// The whole pixels to scroll now.
    pub delta: Point,
    /// What is left, to add to the next frame's.
    pub carry: Point,
}

/// The distance `velocity` (px a second) covers in `dt` plus `carry`, in whole pixels, with the rest
/// carried: slow scrolling still moves, a pixel at a time, instead of rounding away.
pub fn autoscroll_step(velocity: Point, dt: Duration, carry: Point) -> AutoStep {
    let t = dt.as_secs_f32();
    let (x, y) = (carry.x + velocity.x * t, carry.y + velocity.y * t);
    let whole = |v: f32| v.trunc();
    AutoStep {
        delta: Point::new(whole(x), whole(y)),
        carry: Point::new(x - whole(x), y - whole(y)),
    }
}

#[derive(Default)]
struct Stats {
    enabled: bool,
    frames: u64,
    last_present: Option<Instant>,
    intervals: Vec<f64>,
    draws: Vec<f64>,
    latencies: Vec<f64>,
    waits: Vec<f64>,
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

impl Stats {
    fn summary(&mut self) {
        if !self.enabled || self.intervals.len() < 2 {
            self.intervals.clear();
            self.draws.clear();
            self.latencies.clear();
            self.waits.clear();
            return;
        }
        let n = self.intervals.len();
        let (i50, i95, imax) = (
            percentile(&mut self.intervals, 0.5),
            percentile(&mut self.intervals, 0.95),
            percentile(&mut self.intervals, 1.0),
        );
        let (d50, dmax) = (
            percentile(&mut self.draws, 0.5),
            percentile(&mut self.draws, 1.0),
        );
        let w50 = percentile(&mut self.waits, 0.5);
        let lat = if self.latencies.is_empty() {
            String::new()
        } else {
            let k = self.latencies.len();
            format!(
                "; input→present ({k} inputs) p50 {:.2} p95 {:.2} max {:.2} ms",
                percentile(&mut self.latencies, 0.5),
                percentile(&mut self.latencies, 0.95),
                percentile(&mut self.latencies, 1.0)
            )
        };
        eprintln!(
            "[gpu] scroll: {n} frames, interval p50 {i50:.2} p95 {i95:.2} max {imax:.2} ms, compose p50 {d50:.2} max {dmax:.2} ms, frame wait p50 {w50:.2} ms{lat}"
        );
        self.intervals.clear();
        self.draws.clear();
        self.latencies.clear();
        self.waits.clear();
    }
}

struct State {
    gpu: Arc<Gpu>,
    window: WindowHandle,
    /// The fonts and images of the window's own lists ([`CompInput::Resource`]).
    fonts: FontTable,
    images: ImageTable,
    /// Where the window hears of scroll positions and presented frames.
    out: Box<dyn WindowOut>,
    comp: Option<Compositor>,
    size: (u32, u32, u32),
    dpi_scale: f32,
    chrome: Option<(DisplayList, Color)>,
    overlay: DisplayList,
    layers: HashMap<TabId, Layer>,
    active: Option<TabId>,
    dirty: bool,
    keep_until: Option<Instant>,
    last_tick: Option<Instant>,
    /// The autoscroll in progress.
    auto: Option<Auto>,
    pending_input: Option<Ticks>,
    scrollbar_active: bool,
    page_area: Option<(u32, u32, u32)>,
    dark: bool,
    stats: Stats,
    failed: bool,
    chrome_dirty: bool,
    /// Visual tree changes waiting for a commit (a resize).
    uncommitted: bool,
    /// The [`CompMsg::Ack`]s waiting for their frame, and when each arrived.
    acks: Vec<(u64, Instant)>,
    videos: Option<Arc<VideoStore>>,
    /// The browser's protected players' pictures ([`CompInput::Protected`]).
    protected: Option<Arc<dyn snowlark_tab::comp::ProtectedPictures>>,
    /// The begin frames for tabs ([`CompMsg::NeedsBeginFrames`]), and the thread that waits for
    /// the vsyncs that start them.
    begin_frames: BeginFrameSource,
    vsync: Option<VSyncThread>,
    /// The undocked developer tools' window: its own visual tree, and the frame to draw next.
    devtools: Option<Compositor>,
    devtools_frame: Option<Box<DevtoolsFrame>>,
    /// Tabs shown in other windows, and those windows ([`CompMsg::Forward`]).
    forward: HashMap<TabId, WindowId>,
    windows: Windows,
    /// The windows' composition surfaces ([`CompMsg::Surface`]): a window with one is presented
    /// into it, with the chrome in the same swap chain.
    surfaces: HashMap<WindowId, snowlark_tab::comp::OsHandle>,
}

impl State {
    fn new(
        gpu: Arc<Gpu>,
        window: WindowHandle,
        fonts: FontTable,
        out: Box<dyn WindowOut>,
        windows: Windows,
    ) -> State {
        State {
            gpu,
            window,
            fonts,
            images: ImageTable::keeping(),
            out,
            comp: None,
            size: (0, 0, 0),
            dpi_scale: 1.0,
            chrome: None,
            overlay: DisplayList::new(),
            layers: HashMap::new(),
            active: None,
            dirty: false,
            keep_until: None,
            last_tick: None,
            auto: None,
            pending_input: None,
            scrollbar_active: false,
            page_area: None,
            dark: false,
            stats: Stats {
                enabled: std::env::var("SNOWLARK_GPU_STATS").is_ok_and(|v| v == "1"),
                ..Default::default()
            },
            failed: false,
            chrome_dirty: false,
            uncommitted: false,
            acks: Vec::new(),
            videos: None,
            protected: None,
            begin_frames: BeginFrameSource::default(),
            vsync: None,
            devtools: None,
            devtools_frame: None,
            forward: HashMap::new(),
            windows,
            surfaces: HashMap::new(),
        }
    }

    fn moving(&self, now: Instant) -> bool {
        let animating = self
            .active
            .and_then(|t| self.layers.get(&t))
            .is_some_and(|l| l.scroll.is_animating());
        animating
            || self.auto.is_some()
            || self.keep_until.is_some_and(|t| now < t)
            || self.layers_animating()
    }

    /// Whether the active tab's frame has a compositor animation whose value still changes: frames
    /// go on at the display's rate while one does (`cc::AnimationHost::TickAnimations`).
    fn layers_animating(&self) -> bool {
        let Some(f) = self
            .active
            .and_then(|t| self.layers.get(&t))
            .and_then(|l| l.frame.as_ref())
        else {
            return false;
        };
        if f.animations.is_empty() {
            return false;
        }
        let now = snowlark_paint::Ticks::now();
        f.animations.iter().any(|a| a.running_at(a.time_at(now)))
    }

    /// The compositor's loop: each turn is a pass (`mark`), but the wait for input.
    fn run(&mut self, rx: Receiver<CompInput>, mark: &PassMark) {
        loop {
            mark.begin();
            let now = Instant::now();
            if !self.dirty
                && !self.chrome_dirty
                && self.devtools_frame.is_none()
                && !self.moving(now)
            {
                if self.keep_until.take().is_some() {
                    self.stats.summary();
                    self.last_tick = None;
                }
                mark.end();
                // (a page's scroll that waits for its frame waits so long at most)
                let input = match self.wait_ends() {
                    None => rx.recv().map(Some).map_err(|_| ()),
                    Some(at) => match rx.recv_timeout(at.saturating_duration_since(now)) {
                        Ok(m) => Ok(Some(m)),
                        Err(RecvTimeoutError::Timeout) => Ok(None),
                        Err(RecvTimeoutError::Disconnected) => Err(()),
                    },
                };
                mark.begin();
                match input {
                    Ok(Some(m)) => {
                        if !self.handle(m) {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(()) => return,
                }
            }
            while let Ok(m) = rx.try_recv() {
                if !self.handle(m) {
                    return;
                }
            }
            self.end_waits(Instant::now());
            if self.needs_frame() {
                if let Some(comp) = &self.comp {
                    let w0 = Instant::now();
                    comp.wait_for_frame(Duration::from_millis(100));
                    let waited = w0.elapsed();
                    // (the latest input wins: messages that came while waiting for the screen)
                    while let Ok(m) = rx.try_recv() {
                        if !self.handle(m) {
                            return;
                        }
                    }
                    self.frame(waited);
                } else {
                    self.dirty = false;
                }
            }
            // (chrome-only changes need no content frame)
            self.flush_chrome();
            self.draw_devtools();
            for (seq, since) in self.acks.drain(..) {
                if self.stats.enabled {
                    eprintln!(
                        "[gpu] resize {}x{}: new frame presented {:.2} ms after WM_SIZE",
                        self.size.0,
                        self.size.1,
                        since.elapsed().as_secs_f64() * 1000.0
                    );
                }
                self.out.presented(seq);
            }
        }
    }

    /// Whether a content frame is due: something changed or moves, or (in one swap chain) the
    /// chrome changed, which shows only with a frame of the content.
    fn needs_frame(&mut self) -> bool {
        if self.chrome_dirty && self.comp.as_ref().is_some_and(|c| c.chrome_in_frame()) {
            self.dirty = true;
        }
        self.dirty || self.moving(Instant::now())
    }

    /// Draws the chrome when it changed and commits the visual tree (with a resize's changes).
    fn flush_chrome(&mut self) {
        if self.chrome_dirty {
            self.chrome_dirty = false;
            self.uncommitted = false;
            self.draw_chrome();
        }
        if self.uncommitted {
            self.uncommitted = false;
            if let Some(c) = &self.comp {
                let _ = c.commit();
            }
        }
    }

    /// Where a tab's input goes when the tab shows in another window ([`CompMsg::Forward`]): that
    /// window's compositor, while it runs.
    fn forwarded(&self, input: &CompInput) -> Option<Sender<CompInput>> {
        if self.forward.is_empty() {
            return None;
        }
        let tab = match input {
            CompInput::Frame(id, _) => *id,
            CompInput::Msg(
                CompMsg::ScrollTo { tab, .. }
                | CompMsg::NeedsBeginFrames { tab, .. }
                | CompMsg::AutoScroll { tab, .. },
            ) => *tab,
            _ => return None,
        };
        let window = self.forward.get(&tab)?;
        self.windows.lock().unwrap().get(window).cloned()
    }

    /// Handles what arrived; false to stop.
    fn handle(&mut self, input: CompInput) -> bool {
        if let Some(to) = self.forwarded(&input) {
            let _ = to.send(input);
            return true;
        }
        match input {
            CompInput::Msg(m) => return self.message(m),
            CompInput::Handles(m, handles) => return self.with_handles(m, handles),
            CompInput::Frame(id, data) => {
                if let Some(layer) = self.layers.get_mut(&id) {
                    if data.generation != layer.generation {
                        layer.generation = data.generation;
                        // (a layer's first frame with no place of its own goes where the layer was
                        // asked to scroll before it: a frame painted before its tab heard of a
                        // compositor made again is not the top of the page)
                        let asked = layer.frame.is_none().then_some(layer.restore).flatten();
                        layer.restore = data.restore.or(asked);
                        layer.scroll = SmoothScroll::default();
                        layer.reported = None;
                        // (a message of the document before is dropped as that document's; its
                        // serials need not be this one's)
                        layer.page_scroll = 0;
                    }
                    let port = Size::new(
                        (data.viewport.width - data.scrollbar).max(0.0),
                        data.viewport.height,
                    );
                    layer.scroll.set_extent(data.content, port);
                    if let Some(r) = layer.restore {
                        layer.scroll.scroll_to(r, None);
                        let p = layer.scroll.position();
                        if (p.y - r.y).abs() < 1.0 && (p.x - r.x).abs() < 1.0 {
                            layer.restore = None;
                        }
                    }
                    // The page's scroll shows with this frame, in the frame's own scroll range:
                    // the one the frame was painted for, or one that waited for it, whichever
                    // the page made last. Another document's frame drops one that waited: its
                    // document went.
                    // (both unshown: as one, the later after the earlier, so that a scroll
                    // anchoring adjustment is not lost to a scroll before it)
                    let mut scroll = data.scroll.filter(|s| s.serial > layer.page_scroll);
                    if let Some(held) = layer.held.filter(|h| h.shows_with(&data)) {
                        layer.held = None;
                        if held.with.generation == data.generation
                            && held.with.serial > layer.page_scroll
                        {
                            let h = snowlark_tab::comp::PageScroll::to(held.to, held.with.serial);
                            scroll = Some(match scroll {
                                Some(s) => snowlark_tab::comp::PageScroll::merge(s, h),
                                None => h,
                            });
                        }
                        if self.stats.enabled {
                            eprintln!(
                                "[gpu] tab {id}: the page's scroll to {:.0} held {:.2} ms for frame {}, shown with frame {}",
                                held.to.y,
                                held.since.elapsed().as_secs_f64() * 1000.0,
                                held.with.frame,
                                data.number
                            );
                        }
                    }
                    if let Some(scroll) = scroll.filter(|s| s.serial > layer.page_scroll) {
                        layer.page_scroll = scroll.serial;
                        let moved = match scroll.by {
                            // A scroll anchoring adjustment moves the view from where it is (the
                            // user may have scrolled since the page heard); a scroll restoration
                            // still under way goes on to its place instead.
                            Some(by) if layer.restore.is_none() => layer.scroll.shift(by),
                            Some(_) => false,
                            None => {
                                layer.restore = None;
                                layer.scroll.scroll_to(scroll.to, None)
                            }
                        };
                        if moved {
                            self.keep_until = Some(Instant::now() + KEEP_PRESENTING);
                        }
                        // (the tab hears where the viewport is, moved or not: it took it to be
                        // where the page scrolled)
                        layer.reported = None;
                    }
                    if let Some(old) = layer.frame.replace(data) {
                        old.recycle();
                    }
                    if self.active == Some(id) {
                        self.dirty = true;
                    }
                    self.publish(id);
                }
            }
            CompInput::Resource(r) => self.resource(r),
            CompInput::Protected(p) => self.protected = Some(p),
            CompInput::VSync(time, interval) => {
                let args = self.begin_frames.on_vsync(time, interval);
                for layer in self.layers.values().filter(|l| l.needs_begin_frames) {
                    let _ = layer.tab.send(TabCommand::BeginFrame(args));
                }
            }
        }
        true
    }

    /// Handles a message that came with handles, each named in it by its index; false to stop. A
    /// handle that is not there, or not of the kind the message says, leaves the message undone.
    fn with_handles(&mut self, msg: CompMsg, handles: Vec<Handle>) -> bool {
        let mut handles: Vec<Option<Handle>> = handles.into_iter().map(Some).collect();
        let mut take = |i: u32| handles.get_mut(i as usize).and_then(Option::take);
        match msg {
            CompMsg::Bind { tab, pipe } => match take(pipe) {
                Some(Handle::Pipe(tx)) => self.bind(tab, tx),
                _ => eprintln!("[gpu] tab {tab}: a surface with no pipe"),
            },
            CompMsg::Videos { store } => match take(store) {
                Some(Handle::Videos(store)) => self.videos = Some(store),
                _ => eprintln!("[gpu] video frames with no store"),
            },
            CompMsg::Surface { window, handle } => match take(handle).and_then(|h| match h {
                Handle::Tree(tree) => tree
                    .handle()
                    .map_err(|e| eprintln!("[gpu] a window's surface has no handle: {e}"))
                    .ok(),
                Handle::Surface(h) => Some(h),
                _ => None,
            }) {
                Some(h) => {
                    // (a compositor made for the window before goes: the next is made on it)
                    if WindowId::from(self.window) == window {
                        self.comp = None;
                        self.failed = false;
                    } else if self.devtools.as_ref().map(|c| WindowId::from(c.window()))
                        == Some(window)
                    {
                        self.devtools = None;
                    }
                    self.surfaces.insert(window, h);
                    self.ensure();
                    self.chrome_dirty = true;
                    self.dirty = true;
                }
                _ => eprintln!("[gpu] a window's surface with no handle"),
            },
            m => return self.message(m),
        }
        true
    }

    /// Tab `id`'s surface, whose commands go to `tx`.
    fn bind(&mut self, id: TabId, tx: snowlark_ipc::Sender<TabCommand>) {
        // (its page's rendering opportunities follow the display: the begin frames below)
        let _ = tx.send(TabCommand::UseBeginFrames);
        self.layers.insert(
            id,
            Layer {
                tab: tx,
                needs_begin_frames: false,
                frame: None,
                scroll: SmoothScroll::default(),
                reported: None,
                generation: 0,
                restore: None,
                held: None,
                page_scroll: 0,
            },
        );
    }

    /// Puts `resource` in the table it is for. What is refused stays unresolved: the items that need
    /// it are skipped.
    fn resource(&mut self, resource: Resource) {
        match resource {
            Resource::FontAdd { id, source } => {
                if let Err(e) = self.fonts.add(id, &source) {
                    eprintln!("[gpu] font {}: {e}", id.0);
                }
            }
            Resource::ImageAdd {
                id,
                frame,
                version,
                width,
                height,
                pixels,
            } => {
                if self
                    .images
                    .add(id, frame, version, (width, height), &pixels)
                    .is_none()
                {
                    eprintln!("[gpu] image {}: {width}x{height} refused", id.0);
                }
            }
            Resource::ImageRemove(id) => self.images.remove(id),
            Resource::ImagesClear => self.images.clear(),
        }
    }

    /// The vsync thread waits while some tab asks for begin frames (`ExternalBeginFrameSource`
    /// adds itself as an observer of `VSyncThreadWin` while it has observers).
    fn observe_vsync(&mut self) {
        let observing = self.layers.values().any(|l| l.needs_begin_frames);
        if let Some(v) = &self.vsync {
            v.set_observing(observing);
        }
    }

    /// Handles a message; false to stop.
    fn message(&mut self, msg: CompMsg) -> bool {
        match msg {
            CompMsg::Quit => return false,
            CompMsg::Ack(seq) => self.acks.push((seq, Instant::now())),
            CompMsg::Bind { tab, .. } => eprintln!("[gpu] tab {tab}: a surface with no pipe"),
            CompMsg::Videos { .. } => eprintln!("[gpu] video frames with no store"),
            CompMsg::Surface { .. } => eprintln!("[gpu] a window's surface with no handle"),
            CompMsg::Forward {
                tab,
                window: Some(w),
            } => {
                self.forward.insert(tab, w);
            }
            CompMsg::Forward { tab, window: None } => {
                self.forward.remove(&tab);
            }
            CompMsg::DevtoolsWindow(Some(f)) => self.devtools_frame = Some(f),
            CompMsg::DevtoolsWindow(None) => {
                if let Some(c) = self.devtools.take() {
                    self.surfaces.remove(&WindowId::from(c.window()));
                }
                self.devtools_frame = None;
            }
            CompMsg::Resize {
                width,
                height,
                chrome_height,
                dpi_scale,
                chrome,
                overlay,
            } => {
                self.size = (width, height, chrome_height);
                self.dpi_scale = dpi_scale;
                if let Some(c) = chrome {
                    self.chrome = Some(c);
                }
                if let Some(o) = overlay {
                    self.overlay = o;
                }
                self.ensure();
                if let Some(c) = &mut self.comp {
                    if let Err(e) = c.resize(width, height, chrome_height) {
                        eprintln!("[gpu] resize: {e}");
                    }
                }
                self.uncommitted = true;
                self.chrome_dirty = true;
                self.dirty = true;
            }
            CompMsg::Chrome(list, bg) => {
                self.chrome = Some((list, bg));
                self.chrome_dirty = true;
            }
            CompMsg::Overlay(list) => {
                self.overlay = list;
                self.dirty = true;
            }
            CompMsg::NeedsBeginFrames { tab, needs } => {
                if let Some(layer) = self.layers.get_mut(&tab) {
                    if needs && !layer.needs_begin_frames {
                        // (`ExternalBeginFrameSource::AddObserver` sends a missed begin frame)
                        let args = self.begin_frames.missed(snowlark_paint::Ticks::now());
                        let _ = layer.tab.send(TabCommand::BeginFrame(args));
                    }
                    layer.needs_begin_frames = needs;
                }
                self.observe_vsync();
            }
            CompMsg::RemoveTab(id) => {
                self.layers.remove(&id);
                self.observe_vsync();
                self.out.scroll(id, None);
                if self.active == Some(id) {
                    self.active = None;
                }
                self.dirty = true;
            }
            CompMsg::Activate(id) => {
                self.active = Some(id);
                self.dirty = true;
            }
            CompMsg::ScrollBy {
                tab,
                delta,
                smooth,
                input,
            } => {
                // (a tab with no frame here yet has nothing to scroll: its extent and position are
                // the tab's, as after a GPU process started again, and saying 0 would move it; a
                // place it was given for its first frame moves, so the wheel turned meanwhile
                // counts, clamped when the frame comes)
                if let Some(at) = self
                    .layers
                    .get_mut(&tab)
                    .filter(|l| l.frame.is_none())
                    .and_then(|l| l.restore.as_mut())
                {
                    *at = Point::new((at.x + delta.x).max(0.0), (at.y + delta.y).max(0.0));
                }
                if let Some(layer) = self.layers.get_mut(&tab).filter(|l| l.frame.is_some()) {
                    layer.restore = None;
                    if layer.scroll.scroll_by(delta, smooth) {
                        if input.is_some() && self.pending_input.is_none() {
                            self.pending_input = input;
                        }
                        self.keep_until = Some(Instant::now() + KEEP_PRESENTING);
                        self.dirty = true;
                    }
                    self.publish(tab);
                }
            }
            CompMsg::AutoScroll {
                tab,
                target,
                velocity,
            } => match velocity {
                Some(v) if self.layers.get(&tab).is_some_and(|l| l.frame.is_some()) => {
                    // (a new velocity keeps what is left over from the last frame)
                    let carry = self
                        .auto
                        .filter(|a| a.tab == tab && a.target == target)
                        .map_or(Point::ZERO, |a| a.carry);
                    self.auto = Some(Auto {
                        tab,
                        target,
                        velocity: v,
                        carry,
                    });
                    self.keep_until = Some(Instant::now() + KEEP_PRESENTING);
                    self.dirty = true;
                }
                _ => self.auto = None,
            },
            CompMsg::ScrollTo {
                tab,
                to,
                smooth,
                with,
            } => {
                let Some(layer) = self.layers.get_mut(&tab) else {
                    return true;
                };
                // The page's own scroll shows with the frame that has what the page shows there:
                // it waits while the frame here is an earlier one, or another document's.
                let here = |with: &FrameRef| {
                    layer
                        .frame
                        .as_ref()
                        .is_some_and(|f| f.number >= with.frame && f.generation == with.generation)
                };
                match with {
                    Some(with) if !here(&with) => {
                        layer.held = Some(Held {
                            to,
                            with,
                            since: Instant::now(),
                        });
                    }
                    // (a later scroll of the page's came in a frame that is here already)
                    Some(with) if with.serial <= layer.page_scroll => {}
                    Some(with) => {
                        // (it replaces one that still waited)
                        layer.held = None;
                        layer.page_scroll = with.serial;
                        self.scroll_to(tab, to, smooth);
                    }
                    None => self.scroll_to(tab, to, smooth),
                }
            }
            CompMsg::ScrollbarActive(on) => {
                if self.scrollbar_active != on {
                    self.scrollbar_active = on;
                    self.dirty = true;
                }
            }
            CompMsg::Dark(dark) => {
                self.dark = dark;
                self.dirty = true;
            }
            CompMsg::VideoFrame(id) => {
                // (a renderer's video: its newest frame becomes the one drawn)
                if let Some(videos) = &self.videos {
                    videos.latch(id);
                }
                let shown = self
                    .active
                    .and_then(|t| self.layers.get(&t))
                    .and_then(|l| l.frame.as_ref())
                    .is_some_and(|f| {
                        let id = Picture::Frames(id);
                        f.videos.iter().any(|q| q.picture == id)
                            || f.layers
                                .iter()
                                .any(|l| l.videos.iter().any(|q| q.picture == id))
                    });
                if shown {
                    self.dirty = true;
                }
            }
            CompMsg::PageArea(a) => {
                if self.page_area != a {
                    self.page_area = a;
                    self.dirty = true;
                }
            }
        }
        true
    }

    /// Scrolls tab `tab`'s viewport to `to` now, in the scroll range of the frame it has.
    fn scroll_to(&mut self, tab: TabId, to: Point, smooth: bool) {
        // (before the tab's first frame it is where that frame is placed, and the tab is told
        // nothing until then; a compositor made again gets each tab's position so, G5)
        if let Some(layer) = self.layers.get_mut(&tab).filter(|l| l.frame.is_none()) {
            layer.restore = Some(to);
        }
        if let Some(layer) = self.layers.get_mut(&tab).filter(|l| l.frame.is_some()) {
            layer.restore = None;
            if layer
                .scroll
                .scroll_to(to, smooth.then_some(ScrollKind::Keyboard))
            {
                self.keep_until = Some(Instant::now() + KEEP_PRESENTING);
                self.dirty = true;
            }
            self.publish(tab);
        }
    }

    /// When the first of the pages' scrolls that wait for their frames has waited long enough.
    fn wait_ends(&self) -> Option<Instant> {
        self.layers
            .values()
            .filter_map(|l| l.held.map(|h| h.since + SHOWN_WITH_FRAME_WAIT))
            .min()
    }

    /// Ends the waits that are over at `now`: a frame that has not come by then was lost on its
    /// way, and its page's scroll shows with the frame on screen (if that is its document's).
    fn end_waits(&mut self, now: Instant) {
        let over = |h: &Held| now.saturating_duration_since(h.since) >= SHOWN_WITH_FRAME_WAIT;
        let due: Vec<TabId> = self
            .layers
            .iter()
            .filter(|(_, l)| l.held.as_ref().is_some_and(over))
            .map(|(id, _)| *id)
            .collect();
        for tab in due {
            let Some(layer) = self.layers.get_mut(&tab) else {
                continue;
            };
            let Some(held) = layer.held.take() else {
                continue;
            };
            let same_document = layer
                .frame
                .as_ref()
                .is_none_or(|f| f.generation == held.with.generation);
            let newest = held.with.serial > layer.page_scroll;
            if self.stats.enabled {
                eprintln!(
                    "[gpu] tab {tab}: the page's scroll to {:.0} held for frame {}, which did not come",
                    held.to.y, held.with.frame
                );
            }
            // (the tab hears where the viewport is, whatever became of the scroll)
            layer.reported = None;
            if same_document && newest {
                layer.page_scroll = held.with.serial;
                self.scroll_to(tab, held.to, false);
            } else {
                self.publish(tab);
            }
        }
    }

    fn ensure(&mut self) {
        if self.comp.is_some() || self.failed {
            return;
        }
        let (w, h, ch) = self.size;
        if w == 0 || h == 0 {
            return;
        }
        match self.compositor_for(self.window, w, h, ch) {
            Ok(c) => {
                if self.stats.enabled {
                    eprintln!(
                        "[gpu] compositor on {} ({:?}), display {:.0} Hz",
                        self.gpu.adapter(),
                        self.gpu.driver(),
                        snowlark_paint::window::display_refresh_hz().unwrap_or(0.0)
                    );
                }
                self.comp = Some(c);
            }
            Err(e) => {
                eprintln!("[gpu] no compositor: {e}");
                self.out.no_compositor(&e.to_string());
                self.failed = true;
            }
        }
    }

    /// A compositor for `window`: on its composition surface when it has one, else with a visual
    /// tree of its own.
    fn compositor_for(
        &self,
        window: WindowHandle,
        w: u32,
        h: u32,
        chrome: u32,
    ) -> Result<Compositor, String> {
        let gpu = self.gpu.clone();
        match self.surfaces.get(&WindowId::from(window)) {
            Some(surface) => {
                let surface = surface.try_clone().map_err(|e| e.to_string())?;
                Compositor::on_surface(gpu, &self.fonts, window, surface, w, h, chrome)
                    .map_err(|e| e.to_string())
            }
            None => {
                Compositor::new(gpu, &self.fonts, window, w, h, chrome).map_err(|e| e.to_string())
            }
        }
    }

    /// Draws the latest frame of the undocked developer tools' window (its visual tree is made for
    /// the window on the first, and made again for a new window).
    fn draw_devtools(&mut self) {
        let Some(f) = self.devtools_frame.take() else {
            return;
        };
        let (w, h) = (f.width.max(1), f.height.max(1));
        if self
            .devtools
            .as_ref()
            .is_some_and(|c| c.window() != f.window.handle())
        {
            self.devtools = None;
        }
        if self.devtools.is_none() {
            match self.compositor_for(f.window.handle(), w, h, 0) {
                Ok(c) => self.devtools = Some(c),
                Err(e) => {
                    eprintln!("[gpu] developer tools window: {e}");
                    return;
                }
            }
        }
        let Some(c) = &mut self.devtools else {
            return;
        };
        if c.content_size() != (w, h) {
            if let Err(e) = c.resize(w, h, 0) {
                eprintln!("[gpu] developer tools window: {e}");
                return;
            }
        }
        c.wait_for_frame(Duration::from_millis(50));
        let sources = Sources {
            text: &self.fonts,
            images: &self.images,
        };
        let r = c.draw_content(|pass| {
            pass.clear(f.background);
            pass.list(&f.list, sources, f.dpi_scale, 0.0, 0.0);
        });
        if let Err(e) = r
            .and_then(|()| c.commit())
            .and_then(|()| c.present().map(|_| ()))
        {
            eprintln!("[gpu] developer tools window: {e}");
        }
    }

    fn draw_chrome(&mut self) {
        let (Some(c), Some((list, bg))) = (&mut self.comp, &self.chrome) else {
            return;
        };
        let sources = Sources {
            text: &self.fonts,
            images: &self.images,
        };
        if let Err(e) = c.draw_chrome(list, sources, self.dpi_scale, *bg) {
            eprintln!("[gpu] chrome: {e}");
        }
    }

    /// Publishes a tab's scroll state to the window and its tab.
    fn publish(&mut self, id: TabId) {
        let Some(layer) = self.layers.get_mut(&id) else {
            return;
        };
        let (viewport, content, bar) = layer
            .frame
            .as_ref()
            .map(|f| {
                (
                    f.viewport,
                    f.content,
                    f.scrollbar * f.scale / self.dpi_scale,
                )
            })
            .unwrap_or_default();
        let view = ScrollView {
            scroll: layer.scroll.position(),
            target: layer.scroll.target(),
            max: layer.scroll.max(),
            viewport,
            content,
            bar,
        };
        self.out.scroll(id, Some(view));
        // (while the page's own scroll waits for its frame the page is elsewhere already: it
        // hears where the viewport is once that shows)
        if layer.held.is_some() {
            return;
        }
        let p = layer.scroll.position();
        if layer.reported != Some(p) {
            layer.reported = Some(p);
            // (naming the last of the page's scrolls shown: a report from before the page's own
            // last scroll is of the frame painted for the place before it)
            let shown = snowlark_tab::comp::ShownScroll {
                generation: layer.generation,
                serial: layer.page_scroll,
            };
            let _ = layer.tab.send(TabCommand::Scrolled(p, shown));
        }
    }

    /// One display frame of autoscroll: the distance `velocity` covers in `dt` (what is under a
    /// whole pixel waits for a later frame), through the paths the wheel takes.
    fn autoscroll(&mut self, dt: Duration) {
        let Some(mut a) = self.auto else {
            return;
        };
        if self.active != Some(a.tab) {
            // (the tab is not shown: nothing scrolls it)
            self.auto = None;
            return;
        }
        let step = autoscroll_step(a.velocity, dt, a.carry);
        a.carry = step.carry;
        if let Some(layer) = self.layers.get_mut(&a.tab) {
            if step.delta != Point::ZERO {
                match a.target {
                    AutoTarget::Viewport => {
                        layer.restore = None;
                        layer.scroll.scroll_by(step.delta, None);
                    }
                    AutoTarget::Container(node) => {
                        let _ = layer
                            .tab
                            .send(TabCommand::ScrollContainer(node, step.delta));
                    }
                }
            }
            // (frames go on while it runs: one a refresh, at the display's rate)
            self.keep_until = Some(Instant::now() + KEEP_PRESENTING);
        }
        self.auto = Some(a);
    }

    fn frame(&mut self, waited: Duration) {
        let now = Instant::now();
        // Advance the active tab's scroll animation by the time since the last frame.
        let dt = self
            .last_tick
            .map(|t| now - t)
            .unwrap_or(Duration::from_micros(5556))
            .min(Duration::from_millis(50));
        self.last_tick = Some(now);
        self.autoscroll(dt);
        // (the display's refresh interval: a new scroll animation's first frame is one)
        let interval = self
            .begin_frames
            .missed(snowlark_paint::Ticks::now())
            .interval;
        if let Some(id) = self.active {
            if let Some(layer) = self.layers.get_mut(&id) {
                if layer.scroll.tick(dt, interval) {
                    self.keep_until = Some(now + KEEP_PRESENTING);
                }
            }
            self.publish(id);
        }
        if self.comp.as_ref().is_some_and(|c| c.chrome_in_frame()) {
            // (the chrome is drawn into this frame: its bitmap first)
            self.flush_chrome();
        }
        let Some(comp) = &mut self.comp else {
            return;
        };
        let t0 = Instant::now();
        let layer = self.active.and_then(|id| self.layers.get(&id));
        let dpi = self.dpi_scale;
        let (cw, chh) = comp.content_size();
        // (the page's part of the content area)
        let (cw, chh, ox) = match self.page_area {
            Some((w, h, x)) => (w.min(cw), h.min(chh), x as i32),
            None => (cw, chh, 0),
        };
        let scrollbar = layer.and_then(|l| {
            let f = l.frame.as_ref()?;
            if f.scrollbar <= 0.0 {
                return None;
            }
            scrollbar_list(
                f.viewport,
                f.content,
                l.scroll.position(),
                Size::new(cw as f32 / dpi, chh as f32 / dpi),
                self.scrollbar_active,
                &ScrollbarLook {
                    width: f.scrollbar * f.scale / dpi,
                    color_scheme: f.color_scheme,
                    colors: f.scrollbar_colors,
                },
            )
        });
        // (`SNOWLARK_GPU_STATS`: the page frame's size in device pixels against the area it is drawn
        // into; they differ while the page has not drawn at a new window size yet)
        let sizes = layer.and_then(|l| l.frame.as_ref()).map(|f| {
            let page = (
                (f.viewport.width * f.scale).round() as i32,
                (f.viewport.height * f.scale).round() as i32,
            );
            (page, (cw as i32, chh as i32))
        });
        // (`SNOWLARK_GPU_STATS`: the rows of the page area this frame leaves blank)
        let uncovered = layer
            .and_then(|l| Some((l.frame.as_ref()?, l.scroll.position())))
            .map_or((0, 0.0), |(f, at)| (uncovered_rows(f, at, chh), at.y));
        let text = &self.fonts;
        let images = &self.images;
        let overlay = &self.overlay;
        let videos = self.videos.clone();
        // (protected players' pictures show under the content, through holes in it)
        let protected = self
            .protected
            .clone()
            .filter(|_| layer.is_some_and(shows_protected))
            .filter(|_| comp.allow_underlays());
        let mut under = Under {
            pictures: protected.as_deref(),
            shown: Vec::new(),
            placed: Vec::new(),
        };
        let r = comp.draw_content(|pass| {
            let bg = layer
                .and_then(|l| l.frame.as_ref())
                .map_or(Color::WHITE, |f| f.background);
            pass.clear(bg);
            if let Some(l) = layer {
                if let Some(f) = &l.frame {
                    let s = l.scroll.position();
                    let dx =
                        (f.origin.x * f.scale).round() as i32 - (s.x * f.scale).round() as i32 + ox;
                    let dy = (f.origin.y * f.scale).round() as i32 - (s.y * f.scale).round() as i32;
                    let clip = Rect::new(ox as f32, 0.0, cw as f32, chh as f32);
                    pass.tiles(&f.frame, dx + f.base_at.0, dy + f.base_at.1, clip);
                    draw_videos(
                        pass,
                        videos.as_deref(),
                        &mut under,
                        &f.videos,
                        (dx, dy),
                        clip,
                    );
                    let now = snowlark_paint::Ticks::now();
                    for layer in &f.layers {
                        if !layer.state.0.is_empty()
                            && matches!(layer.kind, Some(LayerKind::Sticky(_)))
                        {
                            // (a sticky piece through the clips around it, which stay with the
                            // document: the piece moves within them)
                            let (x, y) = layer_position(layer, f, (dx, dy), s);
                            let shift = (x - dx - layer.at.0, y - dy - layer.at.1);
                            let (moved, within) =
                                moved_through(&layer.state, shift, f.scale, (dx, dy));
                            pass.tiles_through(
                                &layer.frame,
                                layer.at,
                                &moved,
                                f.scale,
                                (dx, dy),
                                clip,
                            );
                            draw_videos(
                                pass,
                                videos.as_deref(),
                                &mut under,
                                &layer.videos,
                                (x - layer.at.0, y - layer.at.1),
                                clip.intersect(&within),
                            );
                            continue;
                        }
                        if !layer.state.0.is_empty() {
                            // (a composited piece: through its clips, transforms and opacity, as
                            // its box's animations have them now; it holds no video)
                            let sampled = snowlark_paint::layer_animation::sampled_state(
                                &layer.state,
                                &f.animations,
                                now,
                            );
                            pass.tiles_through(
                                &layer.frame,
                                layer.at,
                                sampled.as_ref().unwrap_or(&layer.state),
                                f.scale,
                                (dx, dy),
                                clip,
                            );
                            continue;
                        }
                        let (x, y) = layer_position(layer, f, (dx, dy), s);
                        if let Some(b) = &layer.backdrop {
                            // (where the layer list's pixel (0, 0) is now)
                            pass.backdrop(b, f.scale, (x - layer.at.0, y - layer.at.1), clip);
                        }
                        pass.tiles(&layer.frame, x, y, clip);
                        // (the quads are in the layer list's device pixels; its pixel (0, 0) is
                        // at `at` of them)
                        let off = (x - layer.at.0, y - layer.at.1);
                        draw_videos(
                            pass,
                            videos.as_deref(),
                            &mut under,
                            &layer.videos,
                            off,
                            clip,
                        );
                    }
                }
            }
            let sources = Sources { text, images };
            if let Some(list) = &scrollbar {
                pass.list(list, sources, dpi, ox as f32, 0.0);
            }
            if !overlay.is_empty() {
                pass.list(overlay, sources, dpi, 0.0, 0.0);
            }
        });
        let drawn = t0.elapsed();
        if let Err(e) = r {
            eprintln!("[gpu] frame: {e}");
            self.dirty = false;
            return;
        }
        // The chrome (and a resize's visual changes) go out with this frame.
        self.flush_chrome();
        let Some(comp) = &mut self.comp else {
            return;
        };
        let shown = std::mem::take(&mut under.shown);
        let placed = std::mem::take(&mut under.placed);
        if let Err(e) = comp.set_underlays(&shown) {
            eprintln!("[gpu] protected video: {e}");
        }
        if let Some(p) = &protected {
            let (_, client) = comp.window().screen_placement();
            let top = self.size.2 as i32;
            for (player, dest) in &placed {
                p.place(
                    *player,
                    client.0 + dest.x.round() as i32,
                    client.1 + top + dest.y.round() as i32,
                    dest.width.round() as i32,
                    dest.height.round() as i32,
                );
            }
        }
        let present = match comp.present() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[gpu] present: {e}");
                Duration::ZERO
            }
        };
        let done = Instant::now();
        self.dirty = false;
        if self.stats.enabled {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            let interval = self.stats.last_present.map(|t| ms(done - t));
            self.stats.last_present = Some(done);
            self.stats.frames += 1;
            let latency = self.pending_input.take().map(|t| ms(t.elapsed()));
            let moving = self.moving(done) || self.keep_until.is_some();
            if moving {
                if let Some(i) = interval {
                    if i < 100.0 {
                        self.stats.intervals.push(i);
                    }
                }
                self.stats.draws.push(ms(drawn));
                self.stats.waits.push(ms(waited));
                if let Some(l) = latency {
                    self.stats.latencies.push(l);
                }
            }
            // (the active tab's scroll position this frame shows)
            let scroll = self
                .active
                .and_then(|id| self.layers.get(&id))
                .map(|l| l.scroll.position());
            eprintln!(
                "[gpu] frame {} interval {} wait {:.2} compose {:.2} present {:.2} ms{}{}{}{}{}",
                self.stats.frames,
                interval.map_or("-".into(), |i| format!("{i:.2}")),
                ms(waited),
                ms(drawn),
                ms(present),
                scroll.map_or(String::new(), |p| format!(" scroll {:.1},{:.1}", p.x, p.y)),
                latency.map_or(String::new(), |l| format!(" input→present {l:.2} ms")),
                sizes.map_or(String::new(), |((pw, ph), (aw, ah))| format!(
                    " page {pw}x{ph} area {aw}x{ah}"
                )),
                if uncovered.0 > 0 {
                    let (rows, at) = uncovered;
                    format!(" UNCOVERED {rows} rows at scroll {at:.0}")
                } else {
                    String::new()
                },
                match self.comp.as_ref().map_or(0, |c| c.underlays()) {
                    0 => String::new(),
                    n => format!(" protected {n}"),
                }
            );
        } else {
            self.pending_input = None;
        }
    }
}

/// The protected players' pictures of a frame: where they come from (none: the compositor cannot
/// show them, and their videos stay black), and the ones it shows under the content.
struct Under<'a> {
    pictures: Option<&'a dyn snowlark_tab::comp::ProtectedPictures>,
    shown: Vec<Underlay>,
    /// Where each protected video is (device pixels of the content area), with a picture or
    /// not yet: its engine protects its output there and draws at that size.
    placed: Vec<(u64, Rect)>,
}

/// Whether a tab's frame has a protected player's picture.
fn shows_protected(layer: &Layer) -> bool {
    let protected = |q: &VideoQuad| matches!(q.picture, Picture::Protected(_));
    layer.frame.as_ref().is_some_and(|f| {
        f.videos.iter().any(protected) || f.layers.iter().any(|l| l.videos.iter().any(protected))
    })
}

/// Draws video quads (device pixels moved by `off`) within `clip`: each video's latest frame (a
/// protected player's picture: a hole in the content over it, `under`), then what the page paints
/// over it.
fn draw_videos(
    pass: &mut snowlark_paint::ContentPass<'_>,
    store: Option<&VideoStore>,
    under: &mut Under<'_>,
    quads: &[VideoQuad],
    off: (i32, i32),
    clip: Rect,
) {
    for q in quads {
        let (dx, dy) = (off.0 as f32, off.1 as f32);
        let visible = q.clip.translate(dx, dy).intersect(&clip);
        if visible.is_empty() {
            continue;
        }
        let dest = q.dest.translate(dx, dy);
        match q.picture {
            Picture::Frames(video) => {
                if let Some(front) = store.and_then(|s| s.front_of(video, q.transparent)) {
                    // (a canvas's frame is bottom row first, as OpenGL stores it)
                    pass.video_frame(&front, dest, visible, q.transparent);
                }
            }
            Picture::Protected(player) => {
                under.placed.push((player, dest));
                let surface = under.pictures.and_then(|p| p.surface(player));
                if let Some(surface) = surface {
                    let shown = dest.intersect(&visible);
                    pass.hole(shown);
                    under.shown.push(Underlay {
                        key: player,
                        surface,
                        dest,
                        clip: shown,
                    });
                }
            }
        }
        if let Some((patch, at)) = &q.patch {
            pass.tiles(patch, at.0 + off.0, at.1 + off.1, visible);
        }
    }
}

/// How the viewport's scroll bar looks: the engine's decision, from the root element's style.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarLook {
    /// The gutter's width (DIPs): [`SCROLLBAR_WIDTH`], or less for `scrollbar-width: thin`.
    pub width: f32,
    /// The root's used color scheme (CSS Color Adjust 1 §2.4): light or dark automatic colors.
    pub color_scheme: ColorScheme,
    /// The root's `scrollbar-color` (CSS Scrollbars 1 §2): thumb and track.
    pub colors: Option<ScrollbarColors>,
}

/// The track, its left edge line, the thumb and the thumb while hovered or dragged.
fn scrollbar_palette(look: &ScrollbarLook) -> (Color, Color, Color, Color) {
    if let Some(c) = look.colors {
        // `scrollbar-color`: the author's thumb and track; the edge is the track's.
        return (c.track, c.track, c.thumb, c.thumb);
    }
    match look.color_scheme {
        ColorScheme::Dark => (
            Color::hex(0x2B2B2B),
            Color::hex(0x3A3A3A),
            Color::hex(0x6B6B6B),
            Color::hex(0x9E9E9E),
        ),
        ColorScheme::Light => (
            Color::hex(0xF1F1F1),
            Color::hex(0xE4E4E4),
            Color::hex(0xC1C1C1),
            Color::hex(0x8F8F8F),
        ),
    }
}

/// The viewport's classic vertical scroll bar (DIPs, content-area coordinates): a track over the
/// gutter at the right edge (which layout leaves out of the page) and a rounded thumb in it, darker
/// while hovered or dragged, colored as `look` says.
fn scrollbar_list(
    viewport: Size,
    content: Size,
    scroll: Point,
    area: Size,
    active: bool,
    look: &ScrollbarLook,
) -> Option<DisplayList> {
    let track = (2.0, area.height - 4.0);
    let bar = Scrollbar::new(viewport.height, content.height, scroll.y, track.0, track.1)?;
    let mut b = DisplayListBuilder::new();
    let w = look.width;
    let gutter = Rect::new(area.width - w, 0.0, w, area.height);
    let (track_color, edge, thumb, thumb_active) = scrollbar_palette(look);
    b.fill_rect(gutter, track_color);
    b.fill_rect(Rect::new(gutter.x, 0.0, 1.0, area.height), edge);
    // (9 and 11 DIPs in the 17 DIP gutter: the same proportions in a thin one)
    let width = w * if active { 11.0 } else { 9.0 } / SCROLLBAR_WIDTH;
    let x = gutter.x + (w - width) / 2.0 + 0.5;
    let rect = Rect::new(x, bar.thumb_start, width, bar.thumb_len);
    b.fill_rounded_rect(
        rect,
        Radii::uniform(width / 2.0),
        if active { thumb_active } else { thumb },
    );
    Some(b.build())
}

/// The viewport scroll bar's geometry for hit testing (DIPs, content-area coordinates).
pub fn viewport_scrollbar(view: &ScrollView, area: Size) -> Option<Scrollbar> {
    if view.bar <= 0.0 {
        return None;
    }
    Scrollbar::new(
        view.viewport.height,
        view.content.height,
        view.scroll.y,
        2.0,
        area.height - 4.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_paint::TiledFrame;

    /// A list with a run of the system's UI font and image `image`, and its engine and store.
    fn ui_list(text: &TextEngine, image: snowlark_paint::ImageId) -> DisplayList {
        use snowlark_paint::display_list::{GlyphOffset, ImageRendering};
        use snowlark_paint::text::{FontQuery, FontStretch, FontStyle};
        let fonts = text
            .font_list(&FontQuery {
                families: &["Segoe UI"],
                weight: 400,
                style: FontStyle::Normal,
                stretch: FontStretch::NORMAL,
            })
            .unwrap();
        let font = text.primary_font(fonts);
        let mut b = DisplayListBuilder::new();
        b.text(
            &[10],
            &[7.0],
            &[GlyphOffset::default()],
            Point::new(1.0, 10.0),
            font,
            12.0,
            Color::BLACK,
            0,
        );
        let dest = Rect::new(0.0, 0.0, 4.0, 4.0);
        b.image(image, dest, None, 1.0, ImageRendering::Smooth);
        b.build()
    }

    /// What the compositor gets for `input`: the resources' kinds ("font", "image"), then "list".
    fn sent(ui: &mut UiSent, input: CompInput) -> Vec<&'static str> {
        let mut got = Vec::new();
        ui.send(input, &mut |i| {
            got.push(match i {
                CompInput::Resource(Resource::FontAdd { .. }) => "font",
                CompInput::Resource(Resource::ImageAdd { .. }) => "image",
                CompInput::Resource(_) => "other",
                _ => "list",
            })
        });
        got
    }

    /// A compositor of a window that does not exist (`raw`): it draws nothing and handles all else.
    fn windowless(raw: isize, windows: &Windows) -> CompositorThread {
        let gpu = Gpu::new().unwrap();
        let text = Arc::new(TextEngine::new().unwrap());
        let images = Arc::new(ImageStore::new());
        CompositorThread::start(
            gpu,
            WindowHandle::from_raw(raw),
            text,
            images,
            windows,
            None,
        )
    }

    /// The commands tab `tab` hears through `rx` within a moment, by name.
    fn heard(rx: &Receiver<TabCommand>) -> Vec<&'static str> {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_millis(300);
        while let Ok(c) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            got.push(match c {
                TabCommand::UseBeginFrames => "use begin frames",
                TabCommand::BeginFrame(_) => "begin frame",
                _ => "other",
            });
        }
        got
    }

    /// Begin frames, one or more (the display's vsyncs follow the first).
    fn only_begin_frames(heard: &[&str]) -> bool {
        !heard.is_empty() && heard.iter().all(|c| *c == "begin frame")
    }

    #[test]
    fn a_surface_binds_to_the_pipe_its_message_names() {
        let windows = Windows::default();
        let comp = windowless(0, &windows);
        let (pipe, rx) = snowlark_ipc::channel();
        comp.send(CompInput::bind(5, pipe));
        assert_eq!(heard(&rx), ["use begin frames"]);
        comp.send(CompMsg::NeedsBeginFrames {
            tab: 5,
            needs: true,
        });
        assert!(only_begin_frames(&heard(&rx)));
        // (a message that names a handle it does not carry binds nothing)
        let (other, other_rx) = snowlark_ipc::channel::<TabCommand>();
        comp.send(CompMsg::Bind { tab: 6, pipe: 0 });
        comp.send(CompInput::Handles(
            CompMsg::Bind { tab: 6, pipe: 1 },
            Vec::new(),
        ));
        let videos = snowlark_paint::video::VideoStore::new(None);
        let wrong = vec![Handle::Videos(videos)];
        comp.send(CompInput::Handles(CompMsg::Bind { tab: 6, pipe: 0 }, wrong));
        comp.send(CompMsg::NeedsBeginFrames {
            tab: 6,
            needs: true,
        });
        assert!(heard(&other_rx).is_empty());
        drop(other);
    }

    /// A tab with no frame here yet (as each is in a GPU process started again) is not scrolled:
    /// the compositor knows neither its extent nor its position, and telling it 0 moved the page.
    #[test]
    fn a_tab_with_no_frame_yet_is_not_scrolled() {
        let windows = Windows::default();
        let comp = windowless(0, &windows);
        let (pipe, rx) = snowlark_ipc::channel();
        comp.send(CompInput::bind(5, pipe));
        assert_eq!(heard(&rx), ["use begin frames"]);
        comp.send(CompMsg::ScrollBy {
            tab: 5,
            delta: Point::new(0.0, 100.0),
            smooth: None,
            input: None,
        });
        comp.send(CompMsg::ScrollTo {
            tab: 5,
            to: Point::new(0.0, 300.0),
            smooth: false,
            with: None,
        });
        assert!(heard(&rx).is_empty());
    }

    #[test]
    fn a_tab_shown_in_another_window_is_sent_there_by_its_id() {
        let windows = Windows::default();
        let (opener, popup) = (windowless(1, &windows), windowless(2, &windows));
        let (pipe, rx) = snowlark_ipc::channel();
        popup.send(CompInput::bind(5, pipe));
        assert_eq!(heard(&rx), ["use begin frames"]);
        let to = WindowId::from(WindowHandle::from_raw(2));
        opener.send(CompMsg::Forward {
            tab: 5,
            window: Some(to),
        });
        opener.send(CompMsg::NeedsBeginFrames {
            tab: 5,
            needs: true,
        });
        assert!(only_begin_frames(&heard(&rx)));
        opener.send(CompMsg::NeedsBeginFrames {
            tab: 5,
            needs: false,
        });
        heard(&rx);
        opener.send(CompMsg::Forward {
            tab: 5,
            window: None,
        });
        opener.send(CompMsg::NeedsBeginFrames {
            tab: 5,
            needs: true,
        });
        assert!(heard(&rx).is_empty(), "no longer forwarded");
        // (a window whose compositor has gone is no longer one to send to)
        drop(popup);
        assert!(!windows.lock().unwrap().contains_key(&to));
        assert_eq!(windows.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_wait_ends_with_the_frame_and_at_once_when_the_compositor_has_gone() {
        // (no window: no frame is drawn, and the answers still come in order)
        let windows = Windows::default();
        let comp = windowless(0, &windows);
        let wait = Duration::from_secs(10);
        let t = Instant::now();
        assert!(comp.send_and_wait(CompMsg::Dark(true), wait));
        assert!(comp.send_and_wait(CompMsg::Dark(false), wait));
        assert!(t.elapsed() < wait);
        comp.send(CompMsg::Quit);
        let t = Instant::now();
        assert!(!comp.send_and_wait(CompMsg::Dark(true), wait));
        assert!(
            t.elapsed() < wait / 2,
            "the wait ends as the compositor goes"
        );
    }

    #[test]
    fn the_windows_lists_send_their_fonts_and_images_ahead_of_them() {
        use snowlark_paint::image::{DecodedImage, Frame};
        let text = Arc::new(TextEngine::new().unwrap());
        let images = Arc::new(ImageStore::new());
        let id = images.add(DecodedImage::still(Frame {
            width: 2,
            height: 2,
            pixels: vec![9; 16],
            delay_ms: 0,
        }));
        let list = ui_list(&text, id);
        let mut ui = UiSent {
            text,
            images,
            sent: Sent::default(),
        };
        let chrome = CompMsg::Chrome(list.clone(), Color::WHITE);
        assert_eq!(
            sent(&mut ui, chrome.clone().into()),
            ["font", "image", "list"]
        );
        // (the compositor's tables keep them: the same list again needs nothing)
        assert_eq!(sent(&mut ui, chrome.into()), ["list"]);
        // (each of the window's lists: a resize's, the overlay, the developer tools')
        let mut fresh = UiSent {
            sent: Sent::default(),
            ..ui
        };
        let resize = CompMsg::Resize {
            width: 10,
            height: 10,
            chrome_height: 2,
            dpi_scale: 1.0,
            chrome: None,
            overlay: Some(list.clone()),
        };
        assert_eq!(sent(&mut fresh, resize.into()), ["font", "image", "list"]);
        fresh.sent = Sent::default();
        let devtools = CompMsg::DevtoolsWindow(Some(Box::new(DevtoolsFrame {
            window: WindowId(0),
            width: 1,
            height: 1,
            dpi_scale: 1.0,
            list,
            background: Color::WHITE,
        })));
        assert_eq!(sent(&mut fresh, devtools.into()), ["font", "image", "list"]);
        // (what holds none of the window's lists goes alone)
        assert_eq!(sent(&mut fresh, CompMsg::Dark(true).into()), ["list"]);
    }

    /// A compositor's state for a hidden window, and that window.
    #[cfg(windows)]
    fn state_of(window: &snowlark_paint::window::HiddenWindow) -> State {
        let (events, _) = snowlark_ipc::channel();
        State::new(
            Gpu::new().unwrap(),
            window.handle(),
            FontTable::new().unwrap(),
            Box::new(LocalOut {
                scroll: SharedScroll::default(),
                events,
            }),
            Windows::default(),
        )
    }

    #[cfg(windows)]
    fn resize() -> CompInput {
        CompMsg::Resize {
            width: 200,
            height: 150,
            chrome_height: 40,
            dpi_scale: 1.0,
            chrome: Some((DisplayList::new(), Color::WHITE)),
            overlay: None,
        }
        .into()
    }

    #[cfg(windows)]
    #[test]
    fn a_window_with_a_surface_gets_one_chain_whose_chrome_changes_draw_frames() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let surface = WindowSurface::new(window.handle()).unwrap();
        let mut s = state_of(&window);
        let id = WindowId::from(window.handle());
        assert!(s.handle(CompInput::surface(id, surface.handle().unwrap())));
        assert!(s.handle(resize()));
        assert!(s.comp.as_ref().unwrap().chrome_in_frame());
        s.frame(Duration::ZERO);
        assert!(!s.needs_frame());
        // A change of the chrome alone is drawn with a frame of the content.
        s.handle(CompMsg::Chrome(DisplayList::new(), Color::BLACK).into());
        assert!(s.needs_frame());
        s.frame(Duration::ZERO);
        assert!(!s.chrome_dirty && !s.needs_frame());

        // With no surface, the compositor binds a tree of its own, whose chrome needs no frame.
        let other = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&other);
        s.handle(resize());
        assert!(!s.comp.as_ref().unwrap().chrome_in_frame());
        s.frame(Duration::ZERO);
        s.handle(CompMsg::Chrome(DisplayList::new(), Color::BLACK).into());
        assert!(!s.needs_frame());
    }

    #[test]
    fn percentiles() {
        let mut v = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&mut v, 0.5), 3.0);
        assert_eq!(percentile(&mut v, 1.0), 5.0);
        assert_eq!(percentile(&mut [], 0.5), 0.0);
    }

    #[test]
    fn scrollbar_only_when_scrollable() {
        let area = Size::new(1000.0, 800.0);
        let look = ScrollbarLook {
            width: SCROLLBAR_WIDTH,
            color_scheme: ColorScheme::Light,
            colors: None,
        };
        assert!(scrollbar_list(
            Size::new(1000.0, 800.0),
            Size::new(1000.0, 800.0),
            Point::ZERO,
            area,
            false,
            &look,
        )
        .is_none());
        let list = scrollbar_list(
            Size::new(1000.0, 800.0),
            Size::new(1000.0, 8000.0),
            Point::ZERO,
            area,
            true,
            &look,
        )
        .unwrap();
        assert_eq!(list.items.len(), 3);
    }

    #[test]
    fn scrollbar_colors_follow_the_root_style() {
        use snowlark_paint::Item;
        let draw = |look: ScrollbarLook| {
            let list = scrollbar_list(
                Size::new(1000.0, 800.0),
                Size::new(1000.0, 8000.0),
                Point::ZERO,
                Size::new(1000.0, 800.0),
                false,
                &look,
            )
            .unwrap();
            let track = match list.items[0] {
                Item::FillRect { rect, color } => (rect, color),
                _ => panic!("track first"),
            };
            let thumb = match list.items[2] {
                Item::FillRoundedRect { rect, color, .. } => (rect, color),
                _ => panic!("thumb last"),
            };
            (track, thumb)
        };
        let light = ScrollbarLook {
            width: SCROLLBAR_WIDTH,
            color_scheme: ColorScheme::Light,
            colors: None,
        };
        // The used color scheme decides the automatic colors: a light thumb on a dark track.
        let (track, thumb) = draw(ScrollbarLook {
            color_scheme: ColorScheme::Dark,
            ..light
        });
        assert_eq!(track.1, Color::hex(0x2B2B2B));
        assert_eq!(thumb.1, Color::hex(0x6B6B6B));
        let (track, thumb) = draw(light);
        assert_eq!(track.1, Color::hex(0xF1F1F1));
        assert_eq!(thumb.1, Color::hex(0xC1C1C1));
        // `scrollbar-color: red blue` overrides both, in either scheme.
        let red = Color::rgb(255, 0, 0);
        let blue = Color::rgb(0, 0, 255);
        let (track, thumb) = draw(ScrollbarLook {
            color_scheme: ColorScheme::Dark,
            colors: Some(ScrollbarColors {
                thumb: red,
                track: blue,
            }),
            ..light
        });
        assert_eq!((track.1, thumb.1), (blue, red));
        // `thin`: a narrower gutter and thumb.
        let (track, thumb) = draw(ScrollbarLook {
            width: 11.0,
            ..light
        });
        assert_eq!(track.0.width, 11.0);
        assert!(thumb.0.width < 9.0);
    }

    /// An empty frame of a 200 px wide page `height` px tall in a 200 × 110 viewport, of document
    /// generation `generation`, that places itself nowhere.
    fn frame_data(height: f32, generation: u64) -> FrameData {
        FrameData {
            frame: TiledFrame {
                width: 0,
                height: 0,
                tiles: Vec::new(),
            },
            layers: Vec::new(),
            origin: Point::ZERO,
            scale: 1.0,
            viewport: Size::new(200.0, 110.0),
            content: Size::new(200.0, height),
            scrollbar: 0.0,
            color_scheme: ColorScheme::Light,
            scrollbar_colors: None,
            background: Color::WHITE,
            generation,
            number: 0,
            restore: None,
            scroll: None,
            videos: Vec::new(),
            base_at: (0, 0),
            recycler: snowlark_tab::raster_thread::Recycler::detached(),
            animations: Vec::new(),
        }
    }

    /// G5: a compositor made again is told where each tab was before the tab's first frame there.
    /// Nothing is scrolled or told to the tab until that frame, which is placed there (moved by the
    /// wheel turned meanwhile) though it carries no place of its own (one painted before the tab
    /// heard of the new compositor).
    #[cfg(windows)]
    #[test]
    fn a_scroll_asked_before_a_tabs_first_frame_places_that_frame() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        let _ = told.try_recv(); // (`UseBeginFrames`)
        assert!(s.handle(
            CompMsg::ScrollTo {
                tab: 3,
                to: Point::new(0.0, 900.0),
                smooth: false,
                with: None,
            }
            .into()
        ));
        assert!(s.handle(
            CompMsg::ScrollBy {
                tab: 3,
                delta: Point::new(0.0, -100.0),
                smooth: None,
                input: None,
            }
            .into()
        ));
        assert!(
            told.try_recv().is_err(),
            "the tab is told nothing before its frame"
        );
        assert!(s.handle(CompInput::Frame(3, Box::new(frame_data(3000.0, 5)))));
        let at = Point::new(0.0, 800.0);
        assert_eq!(s.layers[&3].scroll.position(), at);
        assert!(matches!(told.try_recv(), Ok(TabCommand::Scrolled(p, _)) if p == at));
        // A new document's first frame starts at its top, as before.
        assert!(s.handle(CompInput::Frame(3, Box::new(frame_data(3000.0, 6)))));
        assert_eq!(s.layers[&3].scroll.position(), Point::ZERO);
    }

    /// Tab 3's frame `number`: [`frame_data`] of a page `height` px tall, of document `generation`.
    #[cfg(windows)]
    fn numbered(number: u64, height: f32, generation: u64) -> CompInput {
        let mut data = frame_data(height, generation);
        data.number = number;
        CompInput::Frame(3, Box::new(data))
    }

    /// Tab 3's page scrolled its viewport to `y`, its scroll number `serial`, and what it shows
    /// there is in its frame `frame`.
    #[cfg(windows)]
    fn page_scroll(y: f32, serial: u64, frame: u64, generation: u64) -> CompInput {
        CompMsg::ScrollTo {
            tab: 3,
            to: Point::new(0.0, y),
            smooth: false,
            with: Some(FrameRef {
                frame,
                generation,
                serial,
            }),
        }
        .into()
    }

    /// [`numbered`], painted for the page's scroll number `serial` to `y`.
    #[cfg(windows)]
    fn scrolled(number: u64, height: f32, generation: u64, y: f32, serial: u64) -> CompInput {
        let mut data = frame_data(height, generation);
        data.number = number;
        data.scroll = Some(snowlark_tab::comp::PageScroll::to(
            Point::new(0.0, y),
            serial,
        ));
        CompInput::Frame(3, Box::new(data))
    }

    /// A frame the page painted for a scroll of its own brings the scroll: it shows there
    /// from its first presentation, in its own scroll range, and the tab is told. A scroll from
    /// before it that comes later as a message is not shown.
    #[cfg(windows)]
    #[test]
    fn a_frame_painted_for_a_pages_scroll_shows_there() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        // On screen: a document 500 px tall in the 110 px viewport, at its top.
        assert!(s.handle(numbered(1, 500.0, 5)));
        // The page put in taller content and scrolled to 2500 (its second scroll).
        assert!(s.handle(scrolled(2, 3000.0, 5, 2500.0, 2)));
        assert_eq!(s.layers[&3].scroll.position().y, 2500.0);
        assert_eq!(scrolls(&told), [0.0, 2500.0]);
        // Its first scroll, which needed no frame of its own, comes after: the page has moved on.
        assert!(s.handle(page_scroll(100.0, 1, 1, 5)));
        assert_eq!(s.layers[&3].scroll.position().y, 2500.0);
        // The frames that follow leave the viewport where it is, and so does the same scroll in
        // a frame that replaced its own before it was drawn.
        assert!(s.handle(numbered(3, 3000.0, 5)));
        assert!(s.handle(scrolled(4, 3000.0, 5, 2500.0, 2)));
        assert!(s.handle(
            CompMsg::ScrollBy {
                tab: 3,
                delta: Point::new(0.0, 40.0),
                smooth: None,
                input: None,
            }
            .into()
        ));
        assert!(s.handle(scrolled(5, 3000.0, 5, 2500.0, 2)));
        assert_eq!(s.layers[&3].scroll.position().y, 2540.0);
        assert_eq!(scrolls(&told), [2540.0]);
    }

    /// A frame painted after scroll anchoring moved the page's content (CSS Scroll Anchoring 1)
    /// moves the view by as much from where it is: what the wheel scrolled since the page last
    /// heard is kept, not undone by the page's idea of the place. One that waited for a frame
    /// before it goes with it.
    #[cfg(windows)]
    #[test]
    fn a_scroll_anchoring_adjustment_moves_the_view_from_where_it_is() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        assert!(s.handle(numbered(1, 5000.0, 5)));
        assert!(s.handle(
            CompMsg::ScrollBy {
                tab: 3,
                delta: Point::new(0.0, 700.0),
                smooth: None,
                input: None,
            }
            .into()
        ));
        let adjusted = |number: u64, to: f32, by: f32, serial: u64| {
            let mut data = frame_data(5000.0, 5);
            data.number = number;
            data.scroll = Some(snowlark_tab::comp::PageScroll {
                to: Point::new(0.0, to),
                serial,
                by: Some(Point::new(0.0, by)),
            });
            CompInput::Frame(3, Box::new(data))
        };
        // (the page had heard of 600 only: its own idea of the place is 600 + 200)
        assert!(s.handle(adjusted(2, 800.0, 200.0, 1)));
        assert_eq!(s.layers[&3].scroll.position().y, 900.0);
        // A scroll to a place that waited for frame 4, then frame 4 with an adjustment after it:
        // the place the page has after both.
        assert!(s.handle(page_scroll(300.0, 2, 4, 5)));
        assert_eq!(s.layers[&3].scroll.position().y, 900.0);
        assert!(s.handle(adjusted(4, 350.0, 50.0, 3)));
        assert_eq!(s.layers[&3].scroll.position().y, 350.0);
        let _ = scrolls(&told);
    }

    /// The compositor's reports of where the viewport is name the last of the page's scrolls it
    /// has shown, so the tab can tell one of a frame from before its own last scroll: none while
    /// that scroll is on its way, its serial once its frame shows, none again for another
    /// document.
    #[cfg(windows)]
    #[test]
    fn reports_name_the_last_of_the_pages_scrolls_shown() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        let reports = |told: &Receiver<TabCommand>| {
            let mut all = Vec::new();
            while let Ok(command) = told.try_recv() {
                if let TabCommand::Scrolled(p, shown) = command {
                    all.push((p.y, shown.generation, shown.serial));
                }
            }
            all
        };
        let wheel = |s: &mut State| {
            assert!(s.handle(
                CompMsg::ScrollBy {
                    tab: 3,
                    delta: Point::new(0.0, 40.0),
                    smooth: None,
                    input: None,
                }
                .into()
            ));
        };
        assert!(s.handle(numbered(1, 3000.0, 5)));
        // The wheel turns while the page's first scroll is on its way.
        wheel(&mut s);
        assert_eq!(reports(&told), [(0.0, 5, 0), (40.0, 5, 0)]);
        assert!(s.handle(scrolled(2, 3000.0, 5, 900.0, 1)));
        wheel(&mut s);
        assert_eq!(reports(&told), [(900.0, 5, 1), (940.0, 5, 1)]);
        // A scroll shown as a message is named too.
        assert!(s.handle(page_scroll(1200.0, 2, 2, 5)));
        assert_eq!(reports(&told), [(1200.0, 5, 2)]);
        // Another document's frame has shown none of its page's scrolls.
        assert!(s.handle(numbered(3, 3000.0, 6)));
        assert_eq!(reports(&told).last().map(|r| (r.1, r.2)), Some((6, 0)));
    }

    /// Of a scroll that waits for a frame and the scroll that frame was painted for, the one the
    /// page made last shows.
    #[cfg(windows)]
    #[test]
    fn the_pages_last_scroll_shows_of_a_frames_own_and_one_that_waited() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        assert!(s.handle(numbered(1, 3000.0, 5)));
        // Scroll 2 was painted into frame 2; scroll 3 after it needed no new frame.
        assert!(s.handle(page_scroll(700.0, 3, 2, 5)));
        assert!(s.handle(scrolled(2, 3000.0, 5, 600.0, 2)));
        assert_eq!(s.layers[&3].scroll.position().y, 700.0);
        // Scroll 4 waits for frame 3, which was replaced by frame 4, painted for scroll 5.
        assert!(s.handle(page_scroll(800.0, 4, 3, 5)));
        assert!(s.handle(scrolled(4, 3000.0, 5, 900.0, 5)));
        assert_eq!(s.layers[&3].scroll.position().y, 900.0);
        assert!(s.wait_ends().is_none());
        assert_eq!(scrolls(&told), [0.0, 700.0, 900.0]);
    }

    /// Where the tab was told its viewport is, since the last call.
    #[cfg(windows)]
    fn scrolls(told: &Receiver<TabCommand>) -> Vec<f32> {
        let mut at = Vec::new();
        while let Ok(command) = told.try_recv() {
            if let TabCommand::Scrolled(p, _) = command {
                at.push(p.y);
            }
        }
        at
    }

    /// A scroll the page makes itself shows with the frame the page painted for it, not
    /// before: until that frame comes the one on screen stays where it is (and still scrolls by
    /// the wheel), and the scroll lands in the new frame's scroll range, not the old one's.
    #[cfg(windows)]
    #[test]
    fn a_pages_scroll_waits_for_the_frame_painted_for_it() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        // On screen: a document 500 px tall in the 110 px viewport, at its top.
        assert!(s.handle(numbered(1, 500.0, 5)));
        assert_eq!(scrolls(&told), [0.0]);
        // The page scrolled to 2500, which is in its frame 2 (taller content, painted before).
        assert!(s.handle(page_scroll(2500.0, 1, 2, 5)));
        assert_eq!(
            s.layers[&3].scroll.position(),
            Point::ZERO,
            "frame 1 stays where it is"
        );
        assert!(s.wait_ends().is_some());
        assert!(s.handle(
            CompMsg::ScrollBy {
                tab: 3,
                delta: Point::new(0.0, 40.0),
                smooth: None,
                input: None,
            }
            .into()
        ));
        assert_eq!(s.layers[&3].scroll.position().y, 40.0);
        assert!(
            scrolls(&told).is_empty(),
            "the page is at 2500 already: it is not told of the old frame's place"
        );
        assert!(s.handle(numbered(2, 3000.0, 5)));
        assert_eq!(s.layers[&3].scroll.position().y, 2500.0);
        assert_eq!(scrolls(&told), [2500.0]);
        assert!(s.wait_ends().is_none());
    }

    /// A page's scroll whose frame is here already (the paint for it showed nothing new, or the
    /// frame came first) shows at once.
    #[cfg(windows)]
    #[test]
    fn a_pages_scroll_shows_at_once_with_a_frame_already_here() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        assert!(s.handle(numbered(4, 3000.0, 5)));
        assert!(s.handle(page_scroll(900.0, 1, 4, 5)));
        assert_eq!(s.layers[&3].scroll.position().y, 900.0);
        assert!(s.handle(page_scroll(700.0, 2, 3, 5)));
        assert_eq!(s.layers[&3].scroll.position().y, 700.0);
        assert_eq!(scrolls(&told), [0.0, 900.0, 700.0]);
        assert!(s.wait_ends().is_none());
    }

    /// The wait is bounded: a frame that does not come was lost on its way, and the scroll shows
    /// with the frame on screen.
    #[cfg(windows)]
    #[test]
    fn a_pages_scroll_whose_frame_is_lost_shows_after_the_wait() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        assert!(s.handle(numbered(1, 3000.0, 5)));
        assert!(s.handle(page_scroll(900.0, 1, 2, 5)));
        let asked = Instant::now();
        s.end_waits(asked);
        assert_eq!(s.layers[&3].scroll.position(), Point::ZERO);
        let ends = s.wait_ends().expect("a wait");
        assert!(ends <= asked + SHOWN_WITH_FRAME_WAIT);
        s.end_waits(ends);
        assert_eq!(s.layers[&3].scroll.position().y, 900.0);
        assert_eq!(scrolls(&told), [0.0, 900.0]);
        assert!(s.wait_ends().is_none());
    }

    /// A page's scroll is its document's: when another document's frame comes instead of the one
    /// it waited for, it is dropped, and the new document shows where its own frame says.
    #[cfg(windows)]
    #[test]
    fn another_documents_frame_drops_a_pages_scroll_that_waited() {
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let mut s = state_of(&window);
        let (tab, told) = snowlark_ipc::channel();
        assert!(s.handle(CompInput::bind(3, tab)));
        assert!(s.handle(numbered(1, 3000.0, 5)));
        assert!(s.handle(page_scroll(900.0, 1, 2, 5)));
        assert!(s.handle(numbered(2, 3000.0, 6)));
        assert_eq!(s.layers[&3].scroll.position(), Point::ZERO);
        assert!(s.wait_ends().is_none());
        // The same when the wait ends on another document's earlier frame.
        assert!(s.handle(page_scroll(900.0, 2, 9, 5)));
        s.end_waits(Instant::now() + SHOWN_WITH_FRAME_WAIT);
        assert_eq!(s.layers[&3].scroll.position(), Point::ZERO);
        assert!(s.wait_ends().is_none());
        assert_eq!(scrolls(&told), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn uncovered_rows_are_those_the_document_has_and_the_frame_does_not() {
        // A frame of document rows 1000..1300 of a 3000 px page, in a 110 px high area.
        let mut data = frame_data(3000.0, 1);
        data.origin = Point::new(0.0, 1000.0);
        data.frame.height = 300;
        assert_eq!(uncovered_rows(&data, Point::new(0.0, 1000.0), 110), 0);
        assert_eq!(uncovered_rows(&data, Point::new(0.0, 1190.0), 110), 0);
        assert_eq!(uncovered_rows(&data, Point::new(0.0, 1250.0), 110), 60);
        assert_eq!(uncovered_rows(&data, Point::new(0.0, 950.0), 110), 50);
        assert_eq!(uncovered_rows(&data, Point::ZERO, 110), 110);
        // A frame rasterized from above its list's first row covers from there.
        data.base_at = (0, -100);
        data.frame.height = 400;
        assert_eq!(uncovered_rows(&data, Point::new(0.0, 950.0), 110), 0);
        // A document shorter than the area leaves the rest to the canvas: nothing is missing.
        let mut short = frame_data(50.0, 1);
        short.frame.height = 50;
        assert_eq!(uncovered_rows(&short, Point::ZERO, 110), 0);
        // At 2 device px per CSS px the rows are device rows.
        let mut dense = frame_data(3000.0, 1);
        dense.scale = 2.0;
        dense.frame.height = 400;
        assert_eq!(uncovered_rows(&dense, Point::new(0.0, 150.0), 220), 120);
    }

    #[test]
    fn layers_are_placed_for_the_scroll_position() {
        use snowlark_paint::StickyConstraint;
        let empty = || TiledFrame {
            width: 0,
            height: 0,
            tiles: Vec::new(),
        };
        // A frame painted with its top at document y 100, at 2 device px per CSS px.
        let data = FrameData {
            frame: empty(),
            layers: Vec::new(),
            origin: Point::new(0.0, 100.0),
            scale: 2.0,
            viewport: Size::new(400.0, 300.0),
            content: Size::new(400.0, 3000.0),
            scrollbar: 0.0,
            color_scheme: ColorScheme::Light,
            scrollbar_colors: None,
            background: Color::WHITE,
            generation: 1,
            number: 0,
            restore: None,
            scroll: None,
            videos: Vec::new(),
            base_at: (0, 0),
            recycler: snowlark_tab::raster_thread::Recycler::detached(),
            animations: Vec::new(),
        };
        let layer = |kind, at| LayerFrame {
            frame: empty(),
            at,
            kind,
            state: Default::default(),
            videos: Vec::new(),
            backdrop: None,
        };
        // Scrolled to 150: the frame's top is 50 CSS px (100 device px) above the viewport.
        let scroll = Point::new(0.0, 150.0);
        let base = (0, -100);
        assert_eq!(
            layer_position(&layer(None, (0, 40)), &data, base, scroll),
            (0, -60)
        );
        assert_eq!(
            layer_position(&layer(Some(LayerKind::Fixed), (0, 0)), &data, base, scroll),
            (0, 0)
        );
        // A sticky box (top: 0) at list y 20, unshifted when painted: stuck at the viewport's top
        // once scrolled past it.
        let sticky = StickyConstraint {
            rect: Rect::new(0.0, 20.0, 400.0, 10.0),
            container: Rect::new(0.0, 0.0, 400.0, 1000.0),
            insets: [Some(0.0), None, None, None],
        };
        let l = layer(Some(LayerKind::Sticky(sticky)), (0, 40));
        let (_, y) = layer_position(&l, &data, base, scroll);
        assert_eq!(y, 0);
        // Not reached yet: it scrolls with the document.
        let (_, y) = layer_position(&l, &data, (0, 0), Point::new(0.0, 100.0));
        assert_eq!(y, 40);
    }

    /// A sticky piece drawn through the clips around it: the clips stay with the document (at the
    /// base offset), the piece's tiles move by its shift, and its videos show within the clips.
    #[test]
    fn a_sticky_piece_moves_within_its_clips() {
        let state = LayerState(vec![
            StateOp::Clip(Rect::new(0.0, 100.0, 400.0, 600.0)),
            StateOp::ClipRounded(Rect::new(10.0, 0.0, 300.0, 2000.0), [4.0; 8]),
        ]);
        let (moved, within) = moved_through(&state, (0, 30), 1.5, (5, -100));
        assert_eq!(moved.0[..2], state.0[..]);
        assert_eq!(
            moved.0[2],
            StateOp::Transform([1.0, 0.0, 0.0, 1.0, 0.0, 20.0])
        );
        assert_eq!(within, Rect::new(20.0, 50.0, 450.0, 900.0));
        // (not moved: drawn as painted)
        let (same, _) = moved_through(&state, (0, 0), 1.5, (5, -100));
        assert_eq!(same, state);
    }
}
