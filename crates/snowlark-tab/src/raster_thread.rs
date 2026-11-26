// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tab's raster thread and its encode thread. The tab's thread builds each frame's display list
//! (it needs the document) and hands the rest over as a [`PaintedFrame`], by move. The encode thread
//! turns the lists into bytes, a [`RasterJob`], which is all the raster thread sees of them: it
//! decodes and validates them, rasterizes the base and the layers into tiles, makes the video quads
//! and sends the frame to the compositor. The page's event loop does not wait for any of it, so its
//! next task, timer or rendering update runs while the frame is drawn (ARCHITECTURE.md "Threads": one
//! raster thread per tab; `docs/process-isolation.md` "One frame": the lists cross as bytes even in
//! one process, because the raster thread will be in another).
//!
//! Each thread keeps only the newest frame: when the page makes frames faster than they can be
//! drawn, a frame not started yet is replaced by the next one, never queued behind it (the scroll
//! position a new document restores goes over to the frame that replaces it).

use std::collections::hash_map::{Entry, HashMap};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use snowlark_ipc::Bulk;
use snowlark_paint::layer_animation::LayerAnimation;
use snowlark_paint::{
    Color, DisplayList, FontId, FontTable, Gpu, ImageId, ImageStore, ImageTable, Item, LayerKind,
    LayerState, ListError, Point, Refused, Resource, Size, Sources, SplitLayer, TextEngine,
    TileRasterizer, TiledFrame, VideoId, Viewport, MAX_LIST_BYTES, MAX_TILES, TILE_SIZE,
};

use crate::comp::{CompInput, FrameData, LayerFrame, PageScroll};
use crate::raster_link::{cross, Taken, ToRaster};
use crate::renderer_process::{RasterUp, TabDown, TabUp};
use crate::services::{BrowserPort, Down, RendererPort, Up, CALL_WAIT};
use crate::tab::TabId;
use crate::video::Picture;
use crate::watchdog::PassMark;

/// A frame the page painted: its lists, how to rasterize them, and what the compositor gets with it.
pub(crate) struct PaintedFrame {
    /// The list of the document's own content, and the lists of its layers (fixed, sticky...) in
    /// paint order.
    pub base: DisplayList,
    pub layers: Vec<SplitLayer>,
    pub view: Viewport,
    pub viewport: Size,
    pub base_at: (i32, i32),
    pub text: Arc<TextEngine>,
    pub images: Arc<ImageStore>,
    /// The frame's metadata ([`FrameData`] without the raster's output).
    pub meta: FrameMeta,
    /// For `SNOWLARK_GPU_STATS`: the band (CSS px) and whether the list was pinned.
    pub band: (f32, f32),
    pub pinned: bool,
    /// The animations of its composited boxes that the compositor runs
    /// ([`snowlark_paint::DisplayList::animations`]).
    pub animations: Vec<LayerAnimation>,
}

/// A frame as the raster thread gets it: [`PaintedFrame`] with its lists encoded. It crosses as
/// a message ([`crate::raster_link`]): the head, and the lists beside it.
#[derive(Debug, PartialEq)]
pub(crate) struct RasterJob {
    pub head: JobHead,
    /// The lists one after the other ([`DisplayList::encode`]): the base and the layers.
    pub lists: Vec<u8>,
}

/// All of a [`RasterJob`] but its lists.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct JobHead {
    /// How many bytes of the lists are the base's.
    pub base_len: usize,
    /// Each layer's kind, the state it is drawn through and how many bytes it takes, in paint
    /// order.
    pub layers: Vec<(Option<LayerKind>, LayerState, usize)>,
    pub view: Viewport,
    pub viewport: Size,
    pub base_at: (i32, i32),
    pub meta: FrameMeta,
    pub band: (f32, f32),
    pub pinned: bool,
    /// For `SNOWLARK_GPU_STATS`: how long encoding took (ms).
    pub encode_ms: f64,
    /// See [`PaintedFrame::animations`].
    pub animations: Vec<LayerAnimation>,
}

impl RasterJob {
    /// The job `head` and `lists` make, if the raster thread can take it: within the caps, the
    /// lists exactly as long as the head says, and every number one it can draw with. The lists
    /// themselves are decoded and validated when they are drawn.
    pub(crate) fn join(head: JobHead, lists: Vec<u8>) -> Result<RasterJob, Refused> {
        let view = &head.view;
        let tiles = view.width.div_ceil(TILE_SIZE) as u64 * view.height.div_ceil(TILE_SIZE) as u64;
        if lists.len() > MAX_LIST_BYTES || tiles > MAX_TILES {
            return Err(Refused("a frame larger than a raster side takes"));
        }
        let total = head
            .layers
            .iter()
            .try_fold(head.base_len, |sum, (_, _, len)| sum.checked_add(*len));
        if total != Some(lists.len()) {
            return Err(Refused("lists not as long as the job says"));
        }
        if head
            .layers
            .iter()
            .flat_map(|(k, _, _)| k)
            .any(|k| !k.is_valid())
        {
            return Err(Refused("a layer's kind"));
        }
        if head.layers.iter().any(|(_, state, _)| !state.is_valid()) {
            return Err(Refused("a layer's state"));
        }
        if head.animations.len() > snowlark_paint::layer_animation::MAX_ANIMATIONS
            || head.animations.iter().any(|a| !a.is_valid())
        {
            return Err(Refused("an animation a compositor cannot sample"));
        }
        let point = |p: &Point| p.x.is_finite() && p.y.is_finite();
        let size = |s: &Size| {
            s.width >= 0.0 && s.height >= 0.0 && s.width.is_finite() && s.height.is_finite()
        };
        let m = &head.meta;
        let finite = view.scale.is_finite()
            && view.scale > 0.0
            && size(&head.viewport)
            && head.band.0.is_finite()
            && head.band.1.is_finite()
            && head.encode_ms.is_finite()
            && point(&m.origin)
            && m.scale.is_finite()
            && size(&m.content)
            && m.scrollbar.is_finite()
            && m.restore.as_ref().is_none_or(point)
            && m.scroll.as_ref().is_none_or(|s| point(&s.to));
        if !finite {
            return Err(Refused("a number a frame cannot have"));
        }
        Ok(RasterJob { head, lists })
    }
}

/// What [`FrameData`] carries besides the rasterized tiles, layers and videos.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "MetaWire", from = "MetaWire")]
pub(crate) struct FrameMeta {
    pub origin: Point,
    pub scale: f32,
    pub content: Size,
    pub scrollbar: f32,
    pub color_scheme: crate::driver::ColorScheme,
    pub scrollbar_colors: Option<crate::driver::ScrollbarColors>,
    pub background: Color,
    pub generation: u64,
    /// See [`FrameData::number`].
    pub number: u64,
    pub restore: Option<Point>,
    /// See [`FrameData::scroll`].
    pub scroll: Option<PageScroll>,
}

/// [`FrameMeta`] on the wire (its color scheme and scroll bar colors are layout's types).
#[derive(Serialize, Deserialize)]
struct MetaWire {
    origin: Point,
    scale: f32,
    content: Size,
    scrollbar: f32,
    dark: bool,
    /// The thumb's and the track's.
    scrollbar_colors: Option<(Color, Color)>,
    background: Color,
    generation: u64,
    number: u64,
    restore: Option<Point>,
    scroll: Option<PageScroll>,
}

impl From<FrameMeta> for MetaWire {
    fn from(m: FrameMeta) -> MetaWire {
        MetaWire {
            origin: m.origin,
            scale: m.scale,
            content: m.content,
            scrollbar: m.scrollbar,
            dark: m.color_scheme == crate::driver::ColorScheme::Dark,
            scrollbar_colors: m.scrollbar_colors.map(|c| (c.thumb, c.track)),
            background: m.background,
            generation: m.generation,
            number: m.number,
            restore: m.restore,
            scroll: m.scroll,
        }
    }
}

impl From<MetaWire> for FrameMeta {
    fn from(m: MetaWire) -> FrameMeta {
        use crate::driver::{ColorScheme, ScrollbarColors};
        FrameMeta {
            origin: m.origin,
            scale: m.scale,
            content: m.content,
            scrollbar: m.scrollbar,
            color_scheme: if m.dark {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            },
            scrollbar_colors: m
                .scrollbar_colors
                .map(|(thumb, track)| ScrollbarColors { thumb, track }),
            background: m.background,
            generation: m.generation,
            number: m.number,
            restore: m.restore,
            scroll: m.scroll,
        }
    }
}

impl FrameMeta {
    /// Takes over what `old`, the frame this one replaces before it was drawn, still had to do: a new
    /// document's scroll restoration, and the page's scroll that was to show with it.
    fn replaces(&mut self, old: FrameMeta) {
        if old.generation != self.generation {
            return;
        }
        if self.restore.is_none() {
            self.restore = old.restore;
        }
        self.scroll = match (self.scroll, old.scroll) {
            (Some(new), Some(old)) => Some(PageScroll::merge(new, old)),
            (new, old) => new.or(old),
        };
    }
}

/// The raster thread's mailbox.
#[derive(Default)]
pub struct Inbox {
    job: Option<Box<RasterJob>>,
    recycled: Vec<TiledFrame>,
    /// What its tables need before `job`, in order. Unlike a job, none of it may be lost: a job
    /// replaced before it was drawn leaves what it brought here.
    resources: Vec<Resource>,
    /// Buffers of lists it has decoded, for the encode thread to fill again.
    spare: Vec<Vec<u8>>,
    quit: bool,
}

/// The encode thread's mailbox.
#[derive(Default)]
struct Painted {
    frame: Option<Box<PaintedFrame>>,
    quit: bool,
}

/// Where the compositor hands back the frames it no longer shows, straight to the raster thread
/// that drew them (their tiles are reused; the tab's thread is not involved). Cloned into every
/// frame the thread sends.
#[derive(Clone)]
pub struct Recycler(Arc<(Mutex<Inbox>, Condvar)>);

impl Recycler {
    /// Gives `frame` back to the raster thread, or drops it when the thread has ended.
    pub fn recycle(&self, frame: TiledFrame) {
        let (lock, ready) = &*self.0;
        let mut inbox = lock.lock().unwrap_or_else(|e| e.into_inner());
        if inbox.quit {
            return;
        }
        inbox.recycled.push(frame);
        ready.notify_one();
    }

    /// A recycler of no raster thread (frames made by hand, for tests): it keeps what it gets
    /// until it is dropped.
    pub fn detached() -> Recycler {
        Recycler(Arc::default())
    }

    /// How many frames wait to be reused.
    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.0 .0.lock().unwrap().recycled.len()
    }
}

/// The tab's handle to its encode thread and, in one process, its raster thread. Dropping it ends
/// them (after the frame they are working on, without waiting for it).
pub(crate) struct RasterThread {
    painted: Arc<(Mutex<Painted>, Condvar)>,
    link: Link,
}

/// Where the encode thread's messages go.
#[derive(Clone)]
enum Link {
    /// The raster thread is in this process: its mailbox.
    Local(Arc<(Mutex<Inbox>, Condvar)>),
    /// The raster thread is the browser's, at the other end of the renderer's pipe.
    Remote(Arc<RemoteRaster>),
    /// Tests: the GPU process's raster thread, fed each message as its wire form.
    #[cfg(test)]
    #[cfg_attr(not(windows), allow(dead_code))]
    Seam(Arc<SeamRaster>),
}

/// What the raster thread does with a buffer of lists it has decoded.
type TakenSink = Box<dyn Fn(Vec<u8>) + Send>;

impl RasterThread {
    /// Starts the raster thread of tab `id`, which sends its frames to `compositor`, and the encode
    /// thread that feeds it, both in this process.
    pub fn start(
        id: TabId,
        gpu: Arc<Gpu>,
        compositor: std::sync::mpsc::Sender<CompInput>,
    ) -> std::io::Result<RasterThread> {
        let inbox: Arc<(Mutex<Inbox>, Condvar)> = Arc::default();
        let spare = inbox.clone();
        let taken: TakenSink = Box::new(move |buffer| {
            if let Some(Taken(buffer)) = cross(Taken(buffer)) {
                let (lock, _) = &*spare;
                lock.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .spare
                    .push(buffer);
            }
        });
        start_raster(
            id,
            gpu,
            compositor,
            inbox.clone(),
            taken,
            None,
            None,
            PassMark::default(),
        )?;
        RasterThread::encoder(id, Link::Local(inbox))
    }

    /// Starts the encode thread of tab `id` in a renderer process: what it encodes goes up the pipe
    /// to the tab's raster thread in the browser ([`RasterHost`]).
    pub fn remote(id: TabId, remote: Arc<RemoteRaster>) -> std::io::Result<RasterThread> {
        RasterThread::encoder(id, Link::Remote(remote))
    }

    /// Tests: the encode thread of tab `id` feeding a raster thread as the GPU process runs one
    /// ([`SeamRaster`]), each message split into its head and bytes and checked on the way in.
    #[cfg(test)]
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn through_seam(
        id: TabId,
        gpu: Arc<Gpu>,
        compositor: std::sync::mpsc::Sender<CompInput>,
    ) -> std::io::Result<RasterThread> {
        let seam = SeamRaster::start(id, gpu, compositor, Arc::new(|| {}))
            .ok_or_else(|| std::io::Error::other("no raster thread"))?;
        RasterThread::encoder(id, Link::Seam(Arc::new(seam)))
    }

    fn encoder(id: TabId, link: Link) -> std::io::Result<RasterThread> {
        let threads = RasterThread {
            painted: Arc::default(),
            link,
        };
        let painted = threads.painted.clone();
        let link = threads.link.clone();
        std::thread::Builder::new()
            .name(format!("snowlark encode {id}"))
            .spawn(move || {
                let mut sent = Sent::default();
                loop {
                    // (across processes one job is in flight at a time: until the raster thread
                    // has taken it, newer frames replace each other here, as they would in its
                    // mailbox)
                    if let Link::Remote(remote) = &link {
                        if !remote.wait_taken() {
                            return;
                        }
                    }
                    let Some(frame) = next_painted(&painted) else {
                        return;
                    };
                    // (a new raster thread has none of what went to the last)
                    if let Link::Remote(remote) = &link {
                        if remote.take_restarted() {
                            sent = Sent::default();
                        }
                    }
                    match &link {
                        Link::Local(inbox) => {
                            let spare = {
                                let (lock, _) = &**inbox;
                                let mut inbox = lock.lock().unwrap_or_else(|e| e.into_inner());
                                inbox.spare.pop()
                            };
                            let (job, resources) =
                                encode(id, *frame, spare.unwrap_or_default(), &mut sent);
                            send(inbox, job, resources);
                        }
                        Link::Remote(remote) => {
                            let (job, resources) = encode(id, *frame, remote.spare(), &mut sent);
                            remote.send(id, job, resources);
                        }
                        #[cfg(test)]
                        Link::Seam(seam) => {
                            use crate::raster_link::Message;
                            let (job, resources) = encode(id, *frame, Vec::new(), &mut sent);
                            let messages = resources
                                .into_iter()
                                .map(ToRaster::Resource)
                                .chain([ToRaster::Job(Box::new(job))]);
                            for message in messages {
                                let (head, bytes) = message.split();
                                let message = SeamMsg {
                                    head,
                                    bytes: Bulk::Inline(bytes),
                                };
                                let opened = seam.receive(message, |b| match b {
                                    Bulk::Inline(bytes) => Ok(bytes),
                                    _ => Err(snowlark_ipc::Error::Refused),
                                });
                                if let Err(why) = opened {
                                    eprintln!("[tab {id}] the seam refused a message: {why}");
                                }
                            }
                        }
                    }
                }
            })?;
        Ok(threads)
    }

    /// Draws `frame` next, in place of a frame not encoded yet.
    pub fn post(&self, mut frame: PaintedFrame) {
        let (lock, ready) = &*self.painted;
        let mut painted = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = painted.frame.take() {
            frame.meta.replaces(old.meta);
        }
        painted.frame = Some(Box::new(frame));
        ready.notify_one();
    }
}

impl Drop for RasterThread {
    fn drop(&mut self) {
        match &self.link {
            Link::Local(inbox) => quit(inbox),
            Link::Remote(remote) => remote.quit(),
            #[cfg(test)]
            Link::Seam(_) => {}
        }
        let (lock, ready) = &*self.painted;
        lock.lock().unwrap_or_else(|e| e.into_inner()).quit = true;
        ready.notify_one();
    }
}

fn quit(inbox: &(Mutex<Inbox>, Condvar)) {
    let (lock, ready) = inbox;
    lock.lock().unwrap_or_else(|e| e.into_inner()).quit = true;
    ready.notify_one();
}

/// Starts the raster thread of tab `id`: it takes jobs from `inbox`, sends its frames to
/// `compositor` and gives each job's buffer to `taken` once it has decoded it.
/// `video_ids`: for a renderer process's tab, the window's id of each of its videos, and
/// `players` the browser's id of each of its protected players (none: its lists name none).
#[allow(clippy::too_many_arguments)]
fn start_raster(
    id: TabId,
    gpu: Arc<Gpu>,
    compositor: std::sync::mpsc::Sender<CompInput>,
    inbox: Arc<(Mutex<Inbox>, Condvar)>,
    taken: TakenSink,
    video_ids: Option<crate::services::video::VideoIds>,
    players: Option<crate::services::cdm::RendererPlayers>,
    mark: PassMark,
) -> std::io::Result<()> {
    let recycler = Recycler(inbox.clone());
    std::thread::Builder::new()
        .name(format!("snowlark raster {id}"))
        .spawn(move || {
            let mut raster = match TileRasterizer::new(gpu) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[tab {id}] no rasterizer: {e}");
                    return;
                }
            };
            let stats = std::env::var("SNOWLARK_GPU_STATS").is_ok_and(|v| v == "1")
                || std::env::var("SNOWLARK_TAB_STATS").is_ok_and(|v| v == "1");
            let mut fonts = match FontTable::new() {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[tab {id}] no font table: {e}");
                    return;
                }
            };
            let mut images = ImageTable::new();
            // (kept between frames, so decoding reuses their memory)
            let mut lists = Vec::new();
            loop {
                let (job, resources, recycled) = {
                    let (lock, ready) = &*inbox;
                    let mut inbox = lock.lock().unwrap_or_else(|e| e.into_inner());
                    mark.end();
                    while inbox.job.is_none() && inbox.recycled.is_empty() && !inbox.quit {
                        inbox = ready.wait(inbox).unwrap_or_else(|e| e.into_inner());
                    }
                    mark.begin();
                    if inbox.quit {
                        // (frames handed back are released before the rasterizer, whose drop
                        // has Direct3D destroy what was released)
                        drop(std::mem::take(&mut inbox.recycled));
                        return;
                    }
                    (
                        inbox.job.take(),
                        std::mem::take(&mut inbox.resources),
                        std::mem::take(&mut inbox.recycled),
                    )
                };
                for frame in recycled {
                    raster.recycle(frame);
                }
                for resource in resources {
                    apply(id, resource, &mut fonts, &mut images, &mut raster);
                }
                let Some(mut job) = job else { continue };
                let t = Instant::now();
                let decoded = decode(&job, &mut lists);
                let decode_ms = t.elapsed().as_secs_f64() * 1000.0;
                let bytes = job.lists.len();
                taken(std::mem::take(&mut job.lists));
                match decoded {
                    Ok(()) => {
                        let cost = stats.then_some((bytes, decode_ms));
                        let sources = Sources {
                            text: &fonts,
                            images: &images,
                        };
                        let job = *job;
                        draw(
                            id,
                            &mut raster,
                            job,
                            &mut lists,
                            sources,
                            &compositor,
                            &recycler,
                            cost,
                            video_ids.as_deref(),
                            players.as_deref(),
                        );
                    }
                    Err(e) => eprintln!("[tab {id}] raster: {e}"),
                }
            }
        })?;
    Ok(())
}

/// A renderer's raster seams of view tabs ([`crate::tab::TabCommand::AttachView`]), by tab: where
/// its pipe's reader hands each view's [`TabDown::Taken`].
pub(crate) type ViewSeams = Arc<Mutex<std::collections::HashMap<TabId, Arc<RemoteRaster>>>>;

/// What the GPU process tells a renderer on a raster seam's link (`docs/process-isolation.md`, M7
/// stage 4, steps 2 and 3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SeamDown {
    /// The raster thread has taken the job in flight (its lists are decoded).
    Taken,
    /// What the window's compositor tells the seam's tab: only a compositor's commands
    /// ([`compositor_command`]); the renderer drops any other.
    Tab(crate::tab::TabCommand),
}

/// Whether `command` is one a compositor sends a tab: the scrolls it made (at finite positions) and
/// its begin frames. All a GPU process may tell a tab.
pub fn compositor_command(command: &crate::tab::TabCommand) -> bool {
    use crate::tab::TabCommand;
    let finite = |p: &Point| p.x.is_finite() && p.y.is_finite();
    match command {
        TabCommand::Scrolled(p, _) | TabCommand::ScrollContainer(_, p) => finite(p),
        TabCommand::BeginFrame(_) | TabCommand::UseBeginFrames => true,
        _ => false,
    }
}

/// A renderer's end of its raster seam's link to the GPU process: it sends the seam's messages and
/// hears [`SeamDown`].
pub type SeamClient = snowlark_ipc::ClientEnd<SeamMsg, SeamDown>;
/// The GPU process's end of a renderer's raster seam link.
pub type SeamServer = snowlark_ipc::HostEnd<SeamDown, SeamMsg>;

/// The ring a seam's link has from the renderer to the GPU process. A message past it streams
/// through as the GPU process reads (`snowlark_ipc::link`); the other way carries only small messages.
pub const SEAM_RING: u64 = 8 << 20;
/// How long either end of a seam's link waits for the other's hello.
pub const SEAM_CONNECT_WAIT: Duration = Duration::from_secs(5);
/// How long the GPU process waits for a message's bytes to come on a seam's link once its head has:
/// a renderer that stops halfway for longer broke the rule.
pub const SEAM_BYTES_WAIT: Duration = Duration::from_secs(10);

/// The renderer's end of the raster seam across processes: it sends what the encode thread makes up
/// the pipe and hears [`Taken`] come back down. One job is in flight at a time.
///
/// With a GPU process the seam also gets a link to it ([`TabDown::RasterLink`]): from then on the
/// messages go there directly, and the job's `Taken` comes back on it. The link may come at any
/// time: a job is sent only once the last one was taken, so whatever went by the browser before is
/// in the raster thread's mailbox ahead of what the link brings.
pub(crate) struct RemoteRaster {
    port: RendererPort,
    /// The view tab whose seam it is (`None`: the tab's own).
    view: Option<TabId>,
    flight: Mutex<Flight>,
    ready: Condvar,
    /// The link to the GPU process, while it holds.
    link: Mutex<Option<SeamClient>>,
    /// The raster thread is new ([`TabDown::RasterNew`]): the encode thread forgets what it sent.
    restarted: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Flight {
    /// A job went up and the raster thread has not taken it yet.
    waiting: bool,
    quit: bool,
    /// The last job's buffer: its bytes were copied into the message, so it is free at once.
    spare: Option<Vec<u8>>,
}

impl RemoteRaster {
    pub fn new(port: RendererPort) -> Arc<RemoteRaster> {
        RemoteRaster::of(port, None)
    }

    /// The seam of view tab `view` (the browser has a raster thread for it:
    /// [`crate::renderer_process::RendererProcess::attach_view`]).
    pub fn of(port: RendererPort, view: Option<TabId>) -> Arc<RemoteRaster> {
        Arc::new(RemoteRaster {
            port,
            view,
            flight: Mutex::default(),
            ready: Condvar::new(),
            link: Mutex::default(),
            restarted: Default::default(),
        })
    }

    /// The seam's raster thread is new and has none of what was sent ([`TabDown::RasterNew`]): the
    /// next frame is encoded with every font and image it needs, and a job sent to the last one is
    /// not waited for (its answer may never come).
    pub fn restarted(&self) {
        self.restarted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.taken();
    }

    fn take_restarted(&self) -> bool {
        self.restarted
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Joins the link the browser made for this seam to the GPU process ([`TabDown::RasterLink`]),
    /// on a thread of its own, which then hears the link until it ends: a compositor's command for
    /// the seam's tab goes to `command`, and any other is dropped. Until it is joined, and after it
    /// ends, the seam's messages go by the browser, and so do the compositor's.
    pub fn join_link(
        self: &Arc<RemoteRaster>,
        end: snowlark_ipc::LinkEnd,
        command: impl Fn(crate::tab::TabCommand) + Send + 'static,
    ) {
        let seam = self.clone();
        let spawned = std::thread::Builder::new()
            .name("snowlark seam link".into())
            .spawn(move || {
                let link =
                    match snowlark_ipc::connect_link::<SeamMsg, SeamDown, snowlark_ipc::FromHost>(
                        end,
                        SEAM_CONNECT_WAIT,
                    ) {
                        Ok(link) => link,
                        Err(e) => {
                            eprintln!("[raster] no link to the GPU process: {e}");
                            return;
                        }
                    };
                *seam.link.lock().unwrap_or_else(|e| e.into_inner()) = Some(link.clone());
                while let Ok(received) = link.recv() {
                    match received {
                        snowlark_ipc::Received::Message(SeamDown::Taken) => seam.taken(),
                        snowlark_ipc::Received::Message(SeamDown::Tab(c)) => {
                            if compositor_command(&c) {
                                command(c);
                            } else {
                                eprintln!(
                                    "[raster] the GPU process sent a command no compositor \
                                         sends: dropped"
                                );
                            }
                        }
                        snowlark_ipc::Received::Call(..) => {}
                    }
                }
                seam.unlink();
            });
        if let Err(e) = spawned {
            eprintln!("[raster] no thread for the link to the GPU process: {e}");
        }
    }

    /// The link ended: the messages go by the browser again, and a job that went on the link is
    /// not waited for (its answer will not come).
    fn unlink(&self) {
        if let Some(link) = self.link.lock().unwrap_or_else(|e| e.into_inner()).take() {
            link.close();
        }
        self.taken();
    }

    /// Whether the seam's messages go on a link now (tests).
    #[cfg(test)]
    pub(crate) fn linked(&self) -> bool {
        self.link
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// The browser's raster thread has taken the job in flight.
    pub fn taken(&self) {
        self.flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting = false;
        self.ready.notify_all();
    }

    /// Waits until no job is in flight: false when the tab has gone.
    fn wait_taken(&self) -> bool {
        let mut flight = self.flight.lock().unwrap_or_else(|e| e.into_inner());
        while flight.waiting && !flight.quit {
            flight = self.ready.wait(flight).unwrap_or_else(|e| e.into_inner());
        }
        !flight.quit
    }

    fn quit(&self) {
        self.flight.lock().unwrap_or_else(|e| e.into_inner()).quit = true;
        self.ready.notify_all();
    }

    fn spare(&self) -> Vec<u8> {
        let mut flight = self.flight.lock().unwrap_or_else(|e| e.into_inner());
        flight.spare.take().unwrap_or_default()
    }

    /// Sends what `job` needs and then `job`. What cannot be sent (the browser is gone, or a section
    /// was refused) is left out, as a refused message would be.
    fn send(&self, id: TabId, job: RasterJob, resources: Vec<Resource>) {
        use crate::raster_link::Message;
        // (the way is chosen once a job, when none is in flight: the type's comment says why)
        let link = self.link.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(link) = link {
            return self.send_linked(id, &link, job, resources);
        }
        for resource in resources {
            let (head, bytes) = ToRaster::Resource(resource).split();
            match self.port.bulk(bytes, CALL_WAIT) {
                Ok(bytes) => self.up(RasterUp {
                    view: self.view,
                    head,
                    bytes,
                }),
                Err(e) => eprintln!("[tab {id}] a resource does not go up: {e}"),
            }
        }
        let (head, lists) = ToRaster::Job(Box::new(job)).split();
        let bytes = match bulk_of(&self.port, &lists) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("[tab {id}] a frame does not go up: {e}");
                return;
            }
        };
        {
            let mut flight = self.flight.lock().unwrap_or_else(|e| e.into_inner());
            flight.spare = Some(lists);
            // (before it is sent: the answer may come before this thread runs again)
            flight.waiting = true;
        }
        if self
            .port
            .send(&Up::Tab(TabUp::Raster(RasterUp {
                view: self.view,
                head,
                bytes,
            })))
            .is_err()
        {
            self.taken();
        }
    }

    /// [`RemoteRaster::send`] on the link to the GPU process: bytes past a frame go in its ring, so
    /// no section is asked of the browser. A message that cannot be sent ends the link (the GPU
    /// process is gone, or broke it): it is left out, and the next job goes by the browser.
    fn send_linked(&self, id: TabId, link: &SeamClient, job: RasterJob, resources: Vec<Resource>) {
        use crate::raster_link::Message;
        let gone = |e: snowlark_ipc::Error| {
            eprintln!("[tab {id}] the link to the GPU process ended: {e}");
            self.unlink();
        };
        for resource in resources {
            let (head, bytes) = ToRaster::Resource(resource).split();
            if let Err(e) = link.send_bulk(&bytes, |bytes| SeamMsg { head, bytes }, CALL_WAIT) {
                return gone(e);
            }
        }
        let (head, lists) = ToRaster::Job(Box::new(job)).split();
        // (before it is sent: the answer may come before this thread runs again)
        self.flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting = true;
        let sent = link.send_bulk(&lists, |bytes| SeamMsg { head, bytes }, CALL_WAIT);
        self.flight.lock().unwrap_or_else(|e| e.into_inner()).spare = Some(lists);
        if let Err(e) = sent {
            gone(e);
        }
    }

    fn up(&self, message: RasterUp) {
        // (a browser that is gone ends this process soon)
        let _ = self.port.send(&Up::Tab(TabUp::Raster(message)));
    }
}

/// `bytes` as bulk for the browser, leaving the buffer with the caller to fill again.
fn bulk_of(port: &RendererPort, bytes: &[u8]) -> Result<Bulk, snowlark_ipc::Error> {
    if bytes.len() <= snowlark_ipc::INLINE_MAX {
        return Ok(Bulk::Inline(bytes.to_vec()));
    }
    let section = port.alloc_shared(bytes.len() as u64, CALL_WAIT)?;
    if !section.write(0, bytes) {
        return Err(snowlark_ipc::Error::Refused);
    }
    Ok(Bulk::Shared {
        id: section.hand_over(),
        len: bytes.len() as u64,
    })
}

/// The browser's end of a renderer's raster seam: the tab's raster thread, fed what comes up the
/// pipe. Every message is joined to its bytes by the checks of [`crate::raster_link`] first, and one
/// that fails them kills the renderer. Dropping it ends the raster thread.
///
/// With a GPU process the raster thread is that process's ([`SeamRaster`]): what comes up is passed
/// on to it unread ([`GpuRaster`]), checked there, and a message that fails the checks kills the
/// renderer all the same.
pub(crate) struct RasterHost {
    port: BrowserPort,
    /// The view tab whose seam it is (`None`: the renderer's tab's).
    view: Option<TabId>,
    raster: HostRaster,
}

/// Where a renderer's raster seams draw: the window's device and compositor, or the GPU process.
#[derive(Clone)]
pub(crate) struct RasterEnds {
    pub gpu: Arc<Gpu>,
    pub compositor: std::sync::mpsc::Sender<CompInput>,
    pub gpu_raster: Option<Arc<dyn GpuRaster>>,
}

/// Whose videos a seam's lists name: those of tab `renderer`'s renderer, with their ids in this
/// process's store (`ids`, for a raster thread here; the GPU process maps them itself).
#[derive(Clone)]
pub(crate) struct SeamVideos {
    pub renderer: TabId,
    pub ids: crate::services::video::VideoIds,
    /// The browser's id of each of the renderer's protected players.
    pub players: crate::services::cdm::RendererPlayers,
}

enum HostRaster {
    /// The raster thread is this process's; `None` when it could not start: jobs are taken and
    /// dropped.
    Local(Option<SeamRaster>),
    /// The GPU process's, for tab `.1`.
    Gpu(Arc<dyn GpuRaster>, TabId),
}

impl RasterHost {
    /// The raster thread of tab `id`, whose frames go to the compositor as `id`'s: the renderer's
    /// tab, or (`view`) a view tab of it. With `remote`, the GPU process's. Its lists name `videos`.
    pub fn start(
        id: TabId,
        view: bool,
        ends: &RasterEnds,
        port: BrowserPort,
        videos: SeamVideos,
    ) -> RasterHost {
        let view = view.then_some(id);
        let down = port.clone();
        let taken = move || {
            // (the bytes stay here: the renderer kept its own buffer)
            let _ = down.send(&Down::Tab(TabDown::Taken { view }));
        };
        let raster = match &ends.gpu_raster {
            Some(remote) => {
                let bad = port.clone();
                // A GPU process started again has a new raster thread for the seam: the renderer
                // is told, sends everything again, and gets a link to that process.
                let restarted = {
                    let (gpu, renderer) = (Arc::downgrade(remote), port.clone());
                    move || {
                        let Some(gpu) = gpu.upgrade() else { return };
                        let _ = renderer.send(&Down::Tab(TabDown::RasterNew { view }));
                        link_seam(gpu, id, view, renderer.clone());
                    }
                };
                remote.start(
                    id,
                    videos.renderer,
                    Box::new(taken),
                    Box::new(move |why: String| bad.bad_message(&format!("raster: {why}"))),
                    Box::new(restarted),
                );
                link_seam(remote.clone(), id, view, port.clone());
                HostRaster::Gpu(remote.clone(), id)
            }
            None => HostRaster::Local(SeamRaster::start_with(
                id,
                ends.gpu.clone(),
                ends.compositor.clone(),
                Arc::new(taken),
                Some(videos.ids),
                Some(videos.players),
                PassMark::default(),
            )),
        };
        RasterHost { port, view, raster }
    }

    /// Takes one raster message of the renderer's (of this seam's tab).
    pub fn receive(&self, message: RasterUp) -> Result<(), snowlark_ipc::Error> {
        let RasterUp { head, bytes, .. } = message;
        let message = SeamMsg { head, bytes };
        match &self.raster {
            HostRaster::Gpu(remote, id) => remote.send(*id, &self.port, message),
            HostRaster::Local(raster) => {
                let view = self.view;
                let refused = |why: String| {
                    let why = format!("raster: {why}");
                    self.port.bad_message(&why);
                    snowlark_ipc::Error::BadMessage(why)
                };
                let bytes = self.port.open_bulk(message.bytes)?;
                let message = ToRaster::join_head(message.head, bytes).map_err(refused)?;
                match raster {
                    Some(raster) => raster.take(message),
                    None => {
                        if let ToRaster::Job(_) = message {
                            let _ = self.port.send(&Down::Tab(TabDown::Taken { view }));
                        }
                    }
                }
                Ok(())
            }
        }
    }
}

/// Gives tab `id`'s seam (or view `view`'s) a link to the GPU process once one is ready: a thread
/// waits for it, so a tab starting with the browser does not.
fn link_seam(gpu: Arc<dyn GpuRaster>, id: TabId, view: Option<TabId>, renderer: BrowserPort) {
    let spawned = std::thread::Builder::new()
        .name(format!("snowlark raster link {id}"))
        .spawn(move || {
            if let Some(end) = gpu.link(id, &renderer) {
                // (a renderer that is gone has its end closed with its handle table)
                let _ = renderer.send(&Down::Tab(TabDown::RasterLink { view, end }));
            }
        });
    if let Err(e) = spawned {
        eprintln!("[tab {id}] no link to the GPU process: {e}");
    }
}

impl Drop for RasterHost {
    fn drop(&mut self) {
        if let HostRaster::Gpu(remote, id) = &self.raster {
            remote.end(*id);
        }
    }
}

/// One message of a renderer's raster seam on its way through the browser: its head, and its bytes
/// as the renderer sent them or, on to the GPU process, as the browser passed them on unread
/// ([`SeamMsg::pass`]). Window resources for the GPU process's compositor travel so too
/// ([`SeamMsg::resource`]).
#[derive(Debug, Serialize, Deserialize)]
pub struct SeamMsg {
    head: crate::raster_link::ToRasterHead,
    bytes: Bulk,
}

/// A seam's message is checked when it is joined to its bytes ([`SeamRaster::receive`]): nothing
/// in it alone can be wrong.
impl snowlark_ipc::Validate for SeamMsg {
    fn validate(
        self,
        _: &snowlark_ipc::ProcessPolicy,
    ) -> Result<SeamMsg, snowlark_ipc::BadMessage> {
        Ok(self)
    }
}

impl SeamMsg {
    /// The message with its bytes handed from the renderer at `from` on to the child at `to`,
    /// unread: [`snowlark_ipc::Connection::pass_bulk`]'s rules, one broken killing the renderer.
    pub fn pass<O: snowlark_ipc::Wire, I: snowlark_ipc::Wire>(
        self,
        from: &BrowserPort,
        to: &snowlark_ipc::BrowserEnd<O, I>,
    ) -> Result<SeamMsg, snowlark_ipc::Error> {
        Ok(SeamMsg {
            head: self.head,
            bytes: from.pass_bulk(self.bytes, to)?,
        })
    }

    /// Sends it on a seam's link, its bytes (inline here, however many) in the link's ring when they
    /// are past a frame.
    pub fn send_on(self, link: &SeamClient, timeout: Duration) -> Result<(), snowlark_ipc::Error> {
        let Bulk::Inline(bytes) = self.bytes else {
            return Err(snowlark_ipc::Error::Refused);
        };
        let head = self.head;
        link.send_bulk(&bytes, |bytes| SeamMsg { head, bytes }, timeout)
    }

    /// Tests: a frame of an empty page, generation `generation`, its bytes inline (over the cap
    /// with `pad` bytes of nothing after the lists, which the checks refuse).
    #[doc(hidden)]
    pub fn blank_frame(generation: u64, pad: usize) -> SeamMsg {
        SeamMsg::frame_of(generation, &DisplayList::new(), pad)
    }

    /// Tests: [`SeamMsg::blank_frame`] of a page whose list is `list`, on an 8 × 8 viewport.
    #[doc(hidden)]
    pub fn frame_of(generation: u64, list: &DisplayList, pad: usize) -> SeamMsg {
        use crate::raster_link::Message;
        let mut lists = Vec::new();
        list.encode(&mut lists);
        let base_len = lists.len();
        lists.resize(base_len + pad, 0);
        let meta = FrameMeta {
            origin: Point::ZERO,
            scale: 1.0,
            content: Size::ZERO,
            scrollbar: 0.0,
            color_scheme: crate::driver::ColorScheme::Light,
            scrollbar_colors: None,
            background: Color::WHITE,
            generation,
            number: generation,
            restore: None,
            scroll: None,
        };
        let head = JobHead {
            base_len,
            layers: Vec::new(),
            view: Viewport {
                width: 8,
                height: 8,
                scale: 1.0,
                background: Color::WHITE,
            },
            viewport: Size::new(8.0, 8.0),
            base_at: (0, 0),
            meta,
            band: (0.0, 0.0),
            pinned: false,
            encode_ms: 0.0,
            animations: Vec::new(),
        };
        let (head, bytes) = ToRaster::Job(Box::new(RasterJob { head, lists })).split();
        SeamMsg {
            head,
            bytes: Bulk::Inline(bytes),
        }
    }

    /// Whether it is a frame (whose sender waits for it to be taken).
    pub fn is_job(&self) -> bool {
        matches!(self.head, crate::raster_link::ToRasterHead::Job(_))
    }

    /// Tests: for a frame, the place it names for its view (`FrameMeta::restore`).
    #[cfg(test)]
    pub(crate) fn restore(&self) -> Option<Option<Point>> {
        match &self.head {
            crate::raster_link::ToRasterHead::Job(head) => Some(head.meta.restore),
            _ => None,
        }
    }

    /// `resource` as a message, its bytes packed by `pack`.
    pub fn resource(
        resource: Resource,
        pack: impl FnOnce(Vec<u8>) -> Result<Bulk, snowlark_ipc::Error>,
    ) -> Result<SeamMsg, snowlark_ipc::Error> {
        use crate::raster_link::Message;
        let (head, bytes) = ToRaster::Resource(resource).split();
        Ok(SeamMsg {
            head,
            bytes: pack(bytes)?,
        })
    }

    /// The resource a message of [`SeamMsg::resource`] makes, its bytes opened by `open`, checked.
    pub fn open_resource(
        self,
        open: impl FnOnce(Bulk) -> Result<Vec<u8>, snowlark_ipc::Error>,
    ) -> Result<Resource, String> {
        let bytes = open(self.bytes).map_err(|e| e.to_string())?;
        match ToRaster::join_head(self.head, bytes)? {
            ToRaster::Resource(r) => Ok(r),
            ToRaster::Job(_) => Err("a frame where a resource was due".into()),
        }
    }
}

/// The browser's end of the GPU process's raster threads, for a window's tabs: what a renderer's
/// [`RasterHost`] hands on (`docs/process-isolation.md`, M7 stage 3).
pub trait GpuRaster: Send + Sync {
    /// Starts tab `tab`'s raster thread in the GPU process, its frames going to this window's
    /// compositor there and its lists naming the videos of tab `videos`'s renderer (the tab's own,
    /// or a view's renderer's). `taken` runs when it has decoded a job's lists, `refused` with why
    /// when a message failed its checks there, and `restarted` when a GPU process started again
    /// has a new raster thread for it (G5 step 1), which has nothing the renderer sent the last.
    fn start(
        &self,
        tab: TabId,
        videos: TabId,
        taken: Box<dyn Fn() + Send + Sync>,
        refused: Box<dyn Fn(String) + Send + Sync>,
        restarted: Box<dyn Fn() + Send + Sync>,
    );
    /// Where the videos of tab `tab`'s renderer are made and drawn: the GPU process's store (G4
    /// step 4). What it refuses there ends the renderer through `tab`'s `refused`.
    fn videos(&self, tab: TabId) -> Arc<dyn crate::services::video::VideoBackend>;
    /// Hands on one message of tab `tab`'s seam from the renderer at `from` ([`SeamMsg::pass`]).
    fn send(
        &self,
        tab: TabId,
        from: &BrowserPort,
        message: SeamMsg,
    ) -> Result<(), snowlark_ipc::Error>;
    /// Ends tab `tab`'s raster thread.
    fn end(&self, tab: TabId);
    /// Makes a link between the renderer at `renderer` and the GPU process for tab `tab`'s seam,
    /// once one is ready (a while at most), and gives the GPU process its end, bound to `tab`:
    /// the renderer's end, for it to join ([`TabDown::RasterLink`]). `None` with no GPU process.
    fn link(&self, tab: TabId, renderer: &BrowserPort) -> Option<snowlark_ipc::LinkEnd>;
}

/// A raster thread fed checked messages, with nothing between: the browser's for a renderer in
/// one process with it, the GPU process's for any renderer ([`GpuRaster`]). Dropping it ends the
/// thread.
pub struct SeamRaster {
    inbox: Arc<(Mutex<Inbox>, Condvar)>,
    taken: Arc<dyn Fn() + Send + Sync>,
}

impl SeamRaster {
    /// The raster thread of tab `id` on `gpu`, which sends its frames to `compositor` and runs
    /// `taken` for each job once it has decoded its lists; `None` when it could not start.
    pub fn start(
        id: TabId,
        gpu: Arc<Gpu>,
        compositor: std::sync::mpsc::Sender<CompInput>,
        taken: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<SeamRaster> {
        SeamRaster::start_with(id, gpu, compositor, taken, None, None, PassMark::default())
    }

    /// [`SeamRaster::start`] with the thread's passes marked in `mark` (the GPU process's
    /// watchdog), and the videos its lists name mapped by `video_ids` to the store's.
    pub fn start_marked(
        id: TabId,
        gpu: Arc<Gpu>,
        compositor: std::sync::mpsc::Sender<CompInput>,
        taken: Arc<dyn Fn() + Send + Sync>,
        video_ids: Option<crate::services::video::VideoIds>,
        mark: PassMark,
    ) -> Option<SeamRaster> {
        // (a GPU process shows no protected player: their pictures are the browser's)
        SeamRaster::start_with(id, gpu, compositor, taken, video_ids, None, mark)
    }

    fn start_with(
        id: TabId,
        gpu: Arc<Gpu>,
        compositor: std::sync::mpsc::Sender<CompInput>,
        taken: Arc<dyn Fn() + Send + Sync>,
        video_ids: Option<crate::services::video::VideoIds>,
        players: Option<crate::services::cdm::RendererPlayers>,
        mark: PassMark,
    ) -> Option<SeamRaster> {
        let inbox: Arc<(Mutex<Inbox>, Condvar)> = Arc::default();
        let sink = taken.clone();
        let sink: TakenSink = Box::new(move |_| sink());
        match start_raster(
            id,
            gpu,
            compositor,
            inbox.clone(),
            sink,
            video_ids,
            players,
            mark,
        ) {
            Ok(()) => Some(SeamRaster { inbox, taken }),
            Err(e) => {
                eprintln!("[tab {id}] no raster thread: {e}");
                None
            }
        }
    }

    /// Takes one message of the seam, its bytes opened by `open` and checked: `Err` with why when
    /// it fails the checks (its sender broke a rule).
    pub fn receive(
        &self,
        message: SeamMsg,
        open: impl FnOnce(Bulk) -> Result<Vec<u8>, snowlark_ipc::Error>,
    ) -> Result<(), String> {
        let bytes = open(message.bytes).map_err(|e| e.to_string())?;
        self.take(ToRaster::join_head(message.head, bytes)?);
        Ok(())
    }

    fn take(&self, message: ToRaster) {
        match message {
            ToRaster::Resource(r) => publish_resources(&self.inbox, vec![r]),
            ToRaster::Job(job) => {
                publish(&self.inbox, *job, Vec::new());
                // A job the raster thread had not started is replaced: it is taken too (the
                // renderer sends one at a time, so only one that breaks that gets here).
                let (lock, _) = &*self.inbox;
                let replaced =
                    std::mem::take(&mut lock.lock().unwrap_or_else(|e| e.into_inner()).spare);
                for _ in replaced {
                    (self.taken)();
                }
            }
        }
    }
}

impl Drop for SeamRaster {
    fn drop(&mut self) {
        quit(&self.inbox);
    }
}

/// Waits for the next frame to encode; `None` when the tab has gone.
fn next_painted(painted: &(Mutex<Painted>, Condvar)) -> Option<Box<PaintedFrame>> {
    let (lock, ready) = painted;
    let mut painted = lock.lock().unwrap_or_else(|e| e.into_inner());
    while painted.frame.is_none() && !painted.quit {
        painted = ready.wait(painted).unwrap_or_else(|e| e.into_inner());
    }
    if painted.quit {
        return None;
    }
    painted.frame.take()
}

/// What the encode thread has sent the raster thread's tables, which it mirrors. The window's own
/// lists (the chrome, the overlay, the undocked developer tools) are sent the compositor's tables
/// the same way ([`Sent::resources`]).
#[derive(Default)]
pub struct Sent {
    /// The engine the fonts are of: another's ids mean other faces.
    text: Option<Arc<TextEngine>>,
    /// By [`FontId`]: 1 + the font's revision when it was sent, 0 when it wasn't.
    fonts: Vec<u32>,
    /// The fonts of the frame at hand, and by [`FontId`] whether they are among them.
    used: Vec<FontId>,
    seen: Vec<bool>,
    /// The store the images are of: another's ids and versions mean other pixels.
    store: Option<Arc<ImageStore>>,
    /// Each image the raster thread has, by the version it has of it.
    images: HashMap<ImageId, u64>,
}

impl Sent {
    /// What the tables need for `lists`, shaped with `text` and showing images of `images`, that
    /// they don't have: the fonts and images to add, and the images that are gone or changed.
    /// `who` names the sender in logs.
    pub fn resources<'a>(
        &mut self,
        who: &dyn std::fmt::Display,
        text: &Arc<TextEngine>,
        images: &Arc<ImageStore>,
        lists: impl Iterator<Item = &'a DisplayList> + Clone,
    ) -> Vec<Resource> {
        let mut out = Vec::new();
        self.fonts(who, text, lists.clone(), &mut out);
        self.images(images, lists, &mut out);
        out
    }

    /// Sends the images of `lists` that the raster thread doesn't have, and tells it of those that
    /// are gone or have changed, whether these lists show them or not (as a raster side that read
    /// the store itself would see: an image stays uploaded while the store holds those pixels).
    fn images<'a>(
        &mut self,
        store: &Arc<ImageStore>,
        lists: impl Iterator<Item = &'a DisplayList>,
        out: &mut Vec<Resource>,
    ) {
        if !self.store.as_ref().is_some_and(|s| Arc::ptr_eq(s, store)) {
            if self.store.replace(store.clone()).is_some() {
                out.push(Resource::ImagesClear);
            }
            self.images.clear();
        }
        self.images.retain(|id, version| {
            let same = store.version(*id) == *version;
            if !same {
                out.push(Resource::ImageRemove(*id));
            }
            same
        });
        for id in lists.flat_map(|l| l.images()) {
            let Entry::Vacant(entry) = self.images.entry(id) else {
                continue;
            };
            // (one the page has freed since it painted is left out, as a store it was read from
            // would not have it either)
            let Some(shown) = store.shown(id) else {
                continue;
            };
            let Some(image) = &shown.image else { continue };
            entry.insert(shown.version);
            // The one copy of the pixels on this side: it lives until the raster thread has
            // uploaded it, for a process boundary it will be the copy into a shared section.
            out.push(Resource::ImageAdd {
                id,
                frame: shown.frame,
                version: shown.version,
                width: shown.width,
                height: shown.height,
                pixels: image.frames[shown.frame].pixels.clone(),
            });
        }
    }

    /// Sends the fonts of `lists` that the raster thread doesn't have.
    fn fonts<'a>(
        &mut self,
        who: &dyn std::fmt::Display,
        text: &Arc<TextEngine>,
        lists: impl Iterator<Item = &'a DisplayList>,
        out: &mut Vec<Resource>,
    ) {
        if !self.text.as_ref().is_some_and(|t| Arc::ptr_eq(t, text)) {
            self.text = Some(text.clone());
            self.fonts.clear();
        }
        self.used.clear();
        self.seen.clear();
        for item in lists.flat_map(|l| &l.items) {
            if let Item::Text(run) = item {
                let index = run.font.0 as usize;
                if self.seen.len() <= index {
                    self.seen.resize(index + 1, false);
                }
                if !self.seen[index] {
                    self.seen[index] = true;
                    self.used.push(run.font);
                }
            }
        }
        for font in &self.used {
            let index = font.0 as usize;
            if self.fonts.len() <= index {
                self.fonts.resize(index + 1, 0);
            }
            let mark = text.font_revision(*font).map_or(0, |r| r + 1);
            if self.fonts[index] == mark {
                continue;
            }
            self.fonts[index] = mark;
            match text.font_source(*font) {
                Ok(source) => out.push(Resource::FontAdd { id: *font, source }),
                Err(e) => eprintln!("[{who}] font {}: {e}", font.0),
            }
        }
    }
}

/// Encodes the lists of `frame` into `lists` (a buffer to reuse), and says what the raster thread
/// needs for them that it doesn't have.
fn encode(
    id: TabId,
    frame: PaintedFrame,
    mut lists: Vec<u8>,
    sent: &mut Sent,
) -> (RasterJob, Vec<Resource>) {
    let t = Instant::now();
    let all = std::iter::once(&frame.base).chain(frame.layers.iter().map(|l| &l.list));
    let resources = sent.resources(&format_args!("tab {id}"), &frame.text, &frame.images, all);
    lists.clear();
    frame.base.encode(&mut lists);
    let base_len = lists.len();
    let mut layers = Vec::with_capacity(frame.layers.len());
    for layer in &frame.layers {
        let start = lists.len();
        layer.list.encode(&mut lists);
        layers.push((layer.kind, layer.state.clone(), lists.len() - start));
    }
    let head = JobHead {
        base_len,
        layers,
        view: frame.view,
        viewport: frame.viewport,
        base_at: frame.base_at,
        meta: frame.meta,
        band: frame.band,
        pinned: frame.pinned,
        encode_ms: t.elapsed().as_secs_f64() * 1000.0,
        animations: frame.animations,
    };
    (RasterJob { head, lists }, resources)
}

/// Sends the raster thread what `job` needs and then `job`, as messages ([`cross`]): what is
/// refused on the way is not sent.
fn send(inbox: &(Mutex<Inbox>, Condvar), job: RasterJob, resources: Vec<Resource>) {
    let mut crossed = Vec::with_capacity(resources.len());
    for resource in resources {
        if let Some(ToRaster::Resource(r)) = cross(ToRaster::Resource(resource)) {
            crossed.push(r);
        }
    }
    match cross(ToRaster::Job(Box::new(job))) {
        Some(ToRaster::Job(job)) => publish(inbox, *job, crossed),
        // (the tables still take what came before it, as they would in another process)
        _ => publish_resources(inbox, crossed),
    }
}

/// Gives the raster thread `job` to draw next, in place of a job it has not started, and what it
/// needs first.
fn publish(inbox: &(Mutex<Inbox>, Condvar), mut job: RasterJob, resources: Vec<Resource>) {
    let (lock, ready) = inbox;
    let mut inbox = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = inbox.job.take() {
        let old = *old;
        job.head.meta.replaces(old.head.meta);
        inbox.spare.push(old.lists);
    }
    queue(&mut inbox, resources);
    inbox.job = Some(Box::new(job));
    ready.notify_one();
}

/// Gives the raster thread `resources` with no job.
fn publish_resources(inbox: &(Mutex<Inbox>, Condvar), resources: Vec<Resource>) {
    if resources.is_empty() {
        return;
    }
    let (lock, ready) = inbox;
    let mut inbox = lock.lock().unwrap_or_else(|e| e.into_inner());
    queue(&mut inbox, resources);
    ready.notify_one();
}

fn queue(inbox: &mut Inbox, resources: Vec<Resource>) {
    for resource in resources {
        inbox.resources.retain(|r| !r.outdated_by(&resource));
        inbox.resources.push(resource);
    }
}

/// Puts `resource` in the table it is for. What is refused stays unresolved: the items that need
/// it are skipped.
fn apply(
    id: TabId,
    resource: Resource,
    fonts: &mut FontTable,
    images: &mut ImageTable,
    raster: &mut TileRasterizer,
) {
    match resource {
        Resource::FontAdd { id: font, source } => {
            if let Err(e) = fonts.add(font, &source) {
                eprintln!("[tab {id}] font {}: {e}", font.0);
            }
        }
        Resource::ImageAdd {
            id: image,
            frame,
            version,
            width,
            height,
            pixels,
        } => match images.add(image, frame, version, (width, height), &pixels) {
            Some(shown) => {
                if let Err(e) = raster.upload(&*fonts, image, shown, &pixels) {
                    eprintln!("[tab {id}] image {}: {e}", image.0);
                }
            }
            None => eprintln!("[tab {id}] image {}: {width}x{height} refused", image.0),
        },
        Resource::ImageRemove(image) => images.remove(image),
        Resource::ImagesClear => images.clear(),
    }
}

/// Decodes and validates the lists of `job` into `lists`, the base first.
pub(crate) fn decode(job: &RasterJob, lists: &mut Vec<DisplayList>) -> Result<(), ListError> {
    let head = &job.head;
    let (columns, rows) = (
        head.view.width.div_ceil(TILE_SIZE) as u64,
        head.view.height.div_ceil(TILE_SIZE) as u64,
    );
    if job.lists.len() > MAX_LIST_BYTES || columns * rows > MAX_TILES {
        return Err(ListError::TooLarge);
    }
    lists.resize_with(head.layers.len() + 1, DisplayList::new);
    let (base, mut rest) = job
        .lists
        .split_at_checked(head.base_len)
        .ok_or(ListError::Truncated)?;
    lists[0].decode_from(base)?;
    for ((_, _, len), list) in head.layers.iter().zip(&mut lists[1..]) {
        let (bytes, tail) = rest.split_at_checked(*len).ok_or(ListError::Truncated)?;
        rest = tail;
        list.decode_from(bytes)?;
    }
    Ok(())
}

/// Rasterizes `job`, whose decoded lists are `lists`, and sends the frame to the compositor. `cost`:
/// for `SNOWLARK_GPU_STATS`, the size of the encoded lists and the time decoding took (ms).
#[allow(clippy::too_many_arguments)]
fn draw(
    id: TabId,
    raster: &mut TileRasterizer,
    job: RasterJob,
    lists: &mut [DisplayList],
    sources: Sources,
    compositor: &std::sync::mpsc::Sender<CompInput>,
    recycler: &Recycler,
    cost: Option<(usize, f64)>,
    video_ids: Option<&(dyn Fn(VideoId) -> VideoId + Send + Sync)>,
    players: Option<&(dyn Fn(u64) -> Option<u64> + Send + Sync)>,
) {
    let t = Instant::now();
    let job = job.head;
    let view = job.view;
    let (base, pieces) = lists.split_first_mut().expect("the base list");
    let base = &*base;
    // (what a video's patch draws over its frame is left out of the tiles under it)
    let under = crate::video::under_videos(base, view.scale);
    let frame = match raster.raster_region(
        under.as_ref().unwrap_or(base),
        sources,
        &view,
        job.base_at,
        false,
    ) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[tab {id}] raster: {e}");
            return;
        }
    };
    let videos = crate::video::quads(raster, base, sources, &view, job.base_at, 0);
    let mut layers = Vec::new();
    // (each piece's tiles are kept for the next frame under a slot: a composited box's by its key,
    // the others' by their place; a slot met twice counts its occurrences)
    let mut keys: Vec<u64> = Vec::new();
    for (n, ((kind, state, _), list)) in job.layers.iter().zip(pieces).enumerate() {
        let kind = *kind;
        // (a fixed or sticky piece cut at a backdrop filter: the compositor applies the filter
        // to what is under the piece, and the piece is drawn without it)
        let backdrop = match (kind, state.0.is_empty()) {
            (Some(LayerKind::Fixed | LayerKind::Sticky(_)), true) => list.take_leading_backdrop(),
            _ => None,
        };
        let list = &*list;
        let key = match kind {
            Some(LayerKind::Composited(k)) => k,
            _ => u64::MAX - n as u64,
        };
        let seen = keys.iter().filter(|&&k| k == key).count() as u64;
        keys.push(key);
        let slot = key.rotate_left(17) ^ seen.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let Some(region) = crate::tab::layer_region(
            list,
            kind,
            &view,
            job.viewport,
            job.base_at,
            backdrop.as_ref().map(|b| b.rect),
        ) else {
            continue;
        };
        let layer_view = Viewport {
            width: region.2,
            height: region.3,
            scale: view.scale,
            background: Color::TRANSPARENT,
        };
        let under = crate::video::under_videos(list, view.scale);
        match raster.raster_piece(
            under.as_ref().unwrap_or(list),
            sources,
            &layer_view,
            (region.0, region.1),
            slot,
        ) {
            Ok(frame) => layers.push(LayerFrame {
                frame,
                at: (region.0, region.1),
                kind,
                state: state.clone(),
                videos: crate::video::quads(
                    raster,
                    list,
                    sources,
                    &layer_view,
                    (region.0, region.1),
                    slot,
                ),
                backdrop,
            }),
            Err(e) => eprintln!("[tab {id}] raster layer: {e}"),
        }
    }
    if let Some((bytes, decode_ms)) = cost {
        let s = raster.stats();
        let work = raster.work();
        eprintln!(
            "[tab {id}] raster {}x{} band {:.0}..{:.0}: {:.1} ms ({} tiles, {} painted, {} reused, {} layers{}, {} compositor animations; lists {} KB, encode {:.2} ms, decode {:.2} ms; prepare {:.2} ms, paint {:.2} ms, copy {:.2} ms, {} items drawn)",
            view.width,
            view.height,
            job.band.0,
            job.band.1,
            t.elapsed().as_secs_f64() * 1000.0,
            frame.tiles.len(),
            s.painted,
            s.reused,
            layers.len(),
            if job.pinned { ", pinned" } else { "" },
            job.animations.len(),
            bytes / 1024,
            job.encode_ms,
            decode_ms,
            work.prepare,
            work.paint,
            work.copy,
            work.items,
        );
    }
    let mut videos = videos;
    if let Some(map) = video_ids {
        // (a renderer's display list names its own videos and players: the compositor knows the
        // window's videos and the browser's players; a player not the renderer's shows nothing)
        let mut mapped = |q: &mut crate::video::VideoQuad| match q.picture {
            Picture::Frames(v) => {
                q.picture = Picture::Frames(map(v));
                true
            }
            Picture::Protected(p) => match players.and_then(|players| players(p)) {
                Some(b) => {
                    q.picture = Picture::Protected(b);
                    true
                }
                None => false,
            },
        };
        videos.retain_mut(&mut mapped);
        for l in &mut layers {
            l.videos.retain_mut(&mut mapped);
        }
    }
    let m = job.meta;
    let _ = compositor.send(CompInput::Frame(
        id,
        Box::new(FrameData {
            frame,
            layers,
            origin: m.origin,
            scale: m.scale,
            viewport: job.viewport,
            content: m.content,
            scrollbar: m.scrollbar,
            color_scheme: m.color_scheme,
            scrollbar_colors: m.scrollbar_colors,
            background: m.background,
            generation: m.generation,
            number: m.number,
            restore: m.restore,
            scroll: m.scroll,
            videos,
            base_at: job.base_at,
            recycler: recycler.clone(),
            animations: job.animations.clone(),
        }),
    ));
}

#[cfg(test)]
pub(crate) mod tests {
    use snowlark_paint::{
        DecodedImage, DisplayListBuilder, Frame, ImageRendering, Rect, TextEngine,
    };

    use super::*;

    pub(crate) fn meta(generation: u64, restore: Option<Point>) -> FrameMeta {
        FrameMeta {
            origin: Point::ZERO,
            scale: 1.0,
            content: Size::ZERO,
            scrollbar: 0.0,
            color_scheme: crate::driver::ColorScheme::Light,
            scrollbar_colors: None,
            background: Color::WHITE,
            generation,
            number: generation,
            restore,
            scroll: None,
        }
    }

    /// A raster thread with a mark (the GPU process's) is in a pass while it works on a job and in
    /// none once it waits for the next: the browser's hang watch reads it so.
    #[test]
    fn a_marked_raster_thread_is_in_a_pass_only_while_it_works() {
        use crate::watchdog::Passes;
        let passes = Arc::new(Passes::create().unwrap());
        let (tx, _rx) = std::sync::mpsc::channel();
        let (taken_tx, taken_rx) = std::sync::mpsc::channel();
        let taken_tx = Mutex::new(taken_tx);
        let raster = SeamRaster::start_marked(
            5,
            // (Linux has only the software device so far)
            Gpu::new()
                .or_else(|_| Gpu::with_driver(snowlark_paint::Driver::Warp))
                .unwrap(),
            tx,
            Arc::new(move || {
                let _ = taken_tx.lock().unwrap().send(());
            }),
            None,
            passes.mark(),
        )
        .unwrap();
        for generation in 1..=3 {
            raster.take(ToRaster::Job(Box::new(job(16, generation, None))));
            taken_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        }
        // (its passes counted, the last one ended once it waits again)
        let end = Instant::now() + std::time::Duration::from_secs(10);
        let idle = |c: &[u64]| c[0] >= 6 && c.iter().all(|c| c & 1 == 0);
        while !idle(&passes.read()) && Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(idle(&passes.read()), "{:?}", &passes.read()[..2]);
    }

    /// G4 step 2, the renderer's end: once its link to the GPU process is joined, a seam sends
    /// resources and jobs there and nothing by the browser, a job's `Taken` comes back on the link,
    /// and when the link ends the seam goes back to the browser's way.
    #[test]
    fn a_seam_sends_on_its_link_and_by_the_browser_once_it_ends() {
        use snowlark_ipc::{FromClient, ProcessKind, ProcessPolicy, Received};
        const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
        let until = |what: &str, done: &dyn Fn() -> bool| {
            let end = Instant::now() + WAIT;
            while !done() {
                assert!(Instant::now() < end, "{what}");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let (browser, renderer) = snowlark_ipc::loopback::pair::<Down, Up>(1 << 20).unwrap();
        let (gpu, _gpu) = snowlark_ipc::loopback::pair::<(), ()>(0).unwrap();
        let seam = RemoteRaster::new(renderer);
        let [mine, theirs] = browser.link(&gpu, [SEAM_RING, 0]).unwrap();
        let (commands, heard) = std::sync::mpsc::channel();
        seam.join_link(mine, move |c| {
            let _ = commands.send(c);
        });
        let gpu =
            snowlark_ipc::connect_link::<SeamDown, SeamMsg, FromClient>(theirs, WAIT).unwrap();
        until("the link is joined", &|| seam.linked());
        seam.send(5, job(16, 1, None), vec![pixels(1, 1)]);
        let policy = ProcessPolicy {
            process: 0,
            kind: ProcessKind::Renderer,
            lock: Default::default(),
        };
        let next = || match gpu.recv_timeout(WAIT) {
            Ok(Received::Message(m)) => gpu.check(m, &policy).unwrap(),
            other => panic!("{:?}", other.err()),
        };
        let (image, frame) = (next(), next());
        assert!(!image.is_job() && frame.is_job());
        assert!(gpu.open_bulk(frame.bytes, WAIT).unwrap().len() == 16);
        // (nothing by the browser)
        assert!(matches!(
            browser.recv_timeout(std::time::Duration::from_millis(200)),
            Err(snowlark_ipc::Error::Timeout)
        ));
        let waiting = || seam.flight.lock().unwrap().waiting;
        assert!(waiting());
        gpu.send(&SeamDown::Taken).unwrap();
        until("the job is taken", &|| !waiting());
        // Step 3: the compositor's commands for the tab come on the link, and only those.
        use crate::tab::TabCommand;
        for command in [
            TabCommand::Close,
            TabCommand::Scrolled(Point::new(0.0, f32::NAN), Default::default()),
            TabCommand::UseBeginFrames,
        ] {
            gpu.send(&SeamDown::Tab(command)).unwrap();
        }
        assert_eq!(heard.recv_timeout(WAIT), Ok(TabCommand::UseBeginFrames));
        assert!(heard.try_recv().is_err());
        // The link ends: the next job goes by the browser.
        gpu.close();
        until("the link is gone", &|| !seam.linked());
        seam.send(5, job(16, 2, None), Vec::new());
        let up = match browser.recv_timeout(WAIT) {
            Ok(Received::Message(m)) => browser.check(m).unwrap(),
            other => panic!("{:?}", other.err()),
        };
        assert!(matches!(up, Up::Tab(TabUp::Raster(_))), "{up:?}");
    }

    pub(crate) fn job(bytes: usize, generation: u64, restore: Option<Point>) -> RasterJob {
        RasterJob {
            lists: vec![0; bytes],
            head: JobHead {
                base_len: bytes,
                layers: Vec::new(),
                view: Viewport {
                    width: 1,
                    height: 1,
                    scale: 1.0,
                    background: Color::WHITE,
                },
                viewport: Size::ZERO,
                base_at: (0, 0),
                meta: meta(generation, restore),
                band: (0.0, 0.0),
                pinned: false,
                encode_ms: 0.0,
                animations: Vec::new(),
            },
        }
    }

    fn pixels(id: u32, version: u64) -> Resource {
        Resource::ImageAdd {
            id: ImageId(id),
            frame: 0,
            version,
            width: 0,
            height: 0,
            pixels: Vec::new(),
        }
    }

    #[test]
    fn a_frame_past_the_caps_is_refused() {
        let mut lists = Vec::new();
        let mut large = job(0, 1, None);
        large.head.view.width = 512 * 64;
        large.head.view.height = 512 * 65;
        assert_eq!(decode(&large, &mut lists), Err(ListError::TooLarge));
        let bytes = job(MAX_LIST_BYTES + 1, 1, None);
        assert_eq!(decode(&bytes, &mut lists), Err(ListError::TooLarge));
    }

    #[test]
    fn lengths_past_the_lists_are_refused_not_a_panic() {
        let mut lists = Vec::new();
        let mut past = job(4, 1, None);
        past.head.base_len = 5;
        assert_eq!(decode(&past, &mut lists), Err(ListError::Truncated));
        let mut layer = job(4, 1, None);
        layer.head.base_len = 0;
        layer.head.layers.push((None, LayerState::default(), 5));
        assert_eq!(decode(&layer, &mut lists), Err(ListError::Truncated));
    }

    #[test]
    fn a_replaced_job_leaves_its_resources_and_its_restoration() {
        let inbox: (Mutex<Inbox>, Condvar) = Default::default();
        let at = Point::new(3.0, 4.0);
        publish(&inbox, job(5, 1, Some(at)), vec![pixels(1, 1)]);
        publish(&inbox, job(6, 1, None), vec![pixels(2, 2)]);
        let inbox = inbox.0.lock().unwrap();
        assert_eq!(inbox.resources, vec![pixels(1, 1), pixels(2, 2)]);
        let drawn = inbox.job.as_ref().unwrap();
        assert_eq!(drawn.lists.len(), 6);
        assert_eq!(drawn.head.meta.restore, Some(at));
        // (its buffer goes back to the encode thread)
        assert_eq!(inbox.spare.len(), 1);
    }

    /// The page's scroll a frame was painted for shows with the frame that replaces it before it
    /// was drawn, unless that one was painted for a scroll of its own or is another document's.
    #[test]
    fn a_replaced_job_leaves_the_pages_scroll_it_was_painted_for() {
        let scroll = |y: f32, serial: u64| Some(PageScroll::to(Point::new(0.0, y), serial));
        let scrolled = |bytes: usize, generation: u64, s: Option<PageScroll>| {
            let mut job = job(bytes, generation, None);
            job.head.meta.scroll = s;
            job
        };
        let inbox: (Mutex<Inbox>, Condvar) = Default::default();
        publish(&inbox, scrolled(5, 1, scroll(900.0, 1)), Vec::new());
        publish(&inbox, scrolled(6, 1, None), Vec::new());
        let taken = |inbox: &(Mutex<Inbox>, Condvar)| {
            let inbox = inbox.0.lock().unwrap();
            inbox.job.as_ref().unwrap().head.meta.scroll
        };
        assert_eq!(taken(&inbox), scroll(900.0, 1));
        publish(&inbox, scrolled(7, 1, scroll(1200.0, 2)), Vec::new());
        assert_eq!(taken(&inbox), scroll(1200.0, 2));
        publish(&inbox, scrolled(8, 2, None), Vec::new());
        assert_eq!(taken(&inbox), None);
    }

    #[test]
    fn another_documents_job_does_not_take_a_restoration() {
        let inbox: (Mutex<Inbox>, Condvar) = Default::default();
        publish(&inbox, job(5, 1, Some(Point::new(3.0, 4.0))), Vec::new());
        publish(&inbox, job(6, 2, None), Vec::new());
        let inbox = inbox.0.lock().unwrap();
        assert_eq!(inbox.job.as_ref().unwrap().head.meta.restore, None);
    }

    #[test]
    fn pixels_do_not_pile_up_behind_a_slow_raster_thread() {
        let inbox: (Mutex<Inbox>, Condvar) = Default::default();
        for version in 1..100 {
            // (as a canvas that draws every frame sends them)
            let again = vec![Resource::ImageRemove(ImageId(1)), pixels(1, version)];
            publish(&inbox, job(1, 1, None), again);
        }
        let inbox = inbox.0.lock().unwrap();
        let last = vec![Resource::ImageRemove(ImageId(1)), pixels(1, 99)];
        assert_eq!(inbox.resources, last);
    }

    fn frame_of(store: &Arc<ImageStore>, text: &Arc<TextEngine>, ids: &[ImageId]) -> PaintedFrame {
        let mut b = DisplayListBuilder::new();
        for id in ids {
            let dest = Rect::new(0.0, 0.0, 4.0, 4.0);
            b.image(*id, dest, None, 1.0, ImageRendering::Smooth);
        }
        PaintedFrame {
            base: b.build(),
            layers: Vec::new(),
            view: job(0, 1, None).head.view,
            viewport: Size::ZERO,
            base_at: (0, 0),
            text: text.clone(),
            images: store.clone(),
            meta: meta(1, None),
            band: (0.0, 0.0),
            pinned: false,
            animations: Vec::new(),
        }
    }

    fn image(seed: u8) -> DecodedImage {
        DecodedImage::still(Frame {
            width: 1,
            height: 1,
            pixels: vec![seed; 4],
            delay_ms: 0,
        })
    }

    /// (id, version) of each image sent and the ids taken away.
    fn sent_images(sent: &mut Sent, frame: &PaintedFrame) -> (Vec<(u32, u64)>, Vec<u32>, bool) {
        let mut out = Vec::new();
        let lists = std::iter::once(&frame.base).chain(frame.layers.iter().map(|l| &l.list));
        sent.images(&frame.images, lists, &mut out);
        let (mut added, mut removed, mut cleared) = (Vec::new(), Vec::new(), false);
        for r in out {
            match r {
                Resource::ImageAdd { id, version, .. } => added.push((id.0, version)),
                Resource::ImageRemove(id) => removed.push(id.0),
                Resource::ImagesClear => cleared = true,
                Resource::FontAdd { .. } => unreachable!(),
            }
        }
        (added, removed, cleared)
    }

    #[test]
    fn images_are_sent_once_and_again_when_they_change() {
        let text = Arc::new(TextEngine::new().unwrap());
        let store = Arc::new(ImageStore::new());
        let (a, b) = (store.add(image(1)), store.add(image(2)));
        let (va, vb) = (store.version(a), store.version(b));
        let mut sent = Sent::default();
        let both = sent_images(&mut sent, &frame_of(&store, &text, &[a, b, a]));
        assert_eq!(both, (vec![(a.0, va), (b.0, vb)], vec![], false));
        let again = sent_images(&mut sent, &frame_of(&store, &text, &[a, b]));
        assert_eq!(again, (vec![], vec![], false));
        // (scrolled out of the list, it stays: the store still holds those pixels)
        let without = sent_images(&mut sent, &frame_of(&store, &text, &[a]));
        assert_eq!(without, (vec![], vec![], false));
        let back = sent_images(&mut sent, &frame_of(&store, &text, &[a, b]));
        assert_eq!(back, (vec![], vec![], false));
        // A canvas that drew.
        store.replace(a, image(3));
        let va = store.version(a);
        let drew = sent_images(&mut sent, &frame_of(&store, &text, &[a, b]));
        assert_eq!(drew, (vec![(a.0, va)], vec![a.0], false));
        // Freed, whether a list shows it or not.
        store.remove(b);
        let freed = sent_images(&mut sent, &frame_of(&store, &text, &[a]));
        assert_eq!(freed, (vec![], vec![b.0], false));
        // A new document's store: the same ids are other pixels.
        let other = Arc::new(ImageStore::new());
        let c = other.add(image(4));
        assert_eq!(c, a);
        let vc = other.version(c);
        let new_store = sent_images(&mut sent, &frame_of(&other, &text, &[c]));
        assert_eq!(new_store, (vec![(c.0, vc)], vec![], true));
    }
}
