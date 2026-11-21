// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a tab hands the compositor and what the window and tabs tell it: the frame and its layers,
//! the plain messages ([`CompMsg`]) and the compositor thread's channel input ([`CompInput`]).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use snowlark_paint::video::VideoStore;
use snowlark_paint::window::WindowHandle;
use snowlark_paint::{
    Color, DisplayList, LayerKind, LayerState, Point, Resource, Size, Ticks, TiledFrame, VideoId,
};

use crate::driver::{ColorScheme, ScrollbarColors};
use crate::tab::{TabCommand, TabId};
use crate::video::VideoQuad;

/// A layer of a tab's frame, drawn over the frame's content in order.
pub struct LayerFrame {
    pub frame: TiledFrame,
    /// Where the layer's pixel (0, 0) is, in device pixels: in the frame's coordinates for
    /// content that scrolls with the document and for a sticky layer (as painted), in the
    /// viewport's for a fixed layer.
    pub at: (i32, i32),
    /// `None` for content that scrolls with the document; else [`LayerKind::Fixed`],
    /// [`LayerKind::Sticky`] or [`LayerKind::Composited`] (which scrolls with the document too).
    pub kind: Option<LayerKind>,
    /// The clips, transforms and opacities the layer is drawn through (empty: as painted; then
    /// `at` places its pixels). A composited layer is rastered in its own coordinates, with its
    /// pixel (0, 0) at its device pixel `at` there.
    pub state: LayerState,
    /// Videos in the layer (device pixels of the layer's list, as `at`).
    pub videos: Vec<VideoQuad>,
    /// The backdrop filter the layer starts with (CSS px of the layer's list), which the
    /// compositor applies to what it has drawn under the layer, where the layer is now.
    pub backdrop: Option<snowlark_paint::LayerBackdrop>,
}

/// A tab's rasterized frame and what it shows.
pub struct FrameData {
    pub frame: TiledFrame,
    /// Layers drawn over `frame`, in paint order.
    pub layers: Vec<LayerFrame>,
    /// The CSS px point of the document at the frame's top-left corner.
    pub origin: Point,
    /// Device pixels per CSS px the frame was rasterized at.
    pub scale: f32,
    pub viewport: Size,
    pub content: Size,
    /// The width (CSS px) of the classic vertical scroll bar shown at the viewport's right edge
    /// (0: none): the scrollport is the viewport without it.
    pub scrollbar: f32,
    /// The root element's used color scheme, which the scroll bar's automatic colors follow.
    pub color_scheme: ColorScheme,
    /// The root's `scrollbar-color` (thumb, track), overriding them.
    pub scrollbar_colors: Option<ScrollbarColors>,
    /// What shows where the frame has no tile (a part scrolled into view before its raster): the
    /// canvas background's color over the canvas surface. Opaque.
    pub background: Color,
    /// The document's generation (a new one resets the scroll position).
    pub generation: u64,
    /// The frame's number among its tab's, counted from 1 in the order they were painted: what a
    /// scroll that shows with a frame waits for ([`CompMsg::ScrollTo`]).
    pub number: u64,
    /// The scroll position to restore for a new document (back/forward, reload).
    pub restore: Option<Point>,
    /// The page scrolled its viewport there and painted this frame for it: the compositor shows
    /// the frame there.
    pub scroll: Option<PageScroll>,
    /// Videos of the list (in its device pixels, whose (0, 0) is at `origin`).
    pub videos: Vec<VideoQuad>,
    /// The frame's pixel (0, 0) in the list's device pixels: (0, 0), or above the list's for a
    /// pinned list rasterized past the viewport. Layers are placed from the list's pixel (0, 0).
    pub base_at: (i32, i32),
    /// Where the frames go back to be reused when the compositor replaces this one.
    pub recycler: crate::raster_thread::Recycler,
    /// The transform and opacity animations of its composited layers that the compositor samples at
    /// each display frame ([`snowlark_paint::layer_animation`]).
    pub animations: Vec<snowlark_paint::layer_animation::LayerAnimation>,
}

impl FrameData {
    /// The compositor shows another frame: this one's tiles (and its videos' patches, and its
    /// layers') go back to the raster thread that drew them.
    pub fn recycle(self) {
        let FrameData {
            frame,
            videos,
            layers,
            recycler,
            ..
        } = self;
        recycler.recycle(frame);
        for (p, _) in videos.into_iter().filter_map(|q| q.patch) {
            recycler.recycle(p);
        }
        for l in layers {
            recycler.recycle(l.frame);
            for (p, _) in l.videos.into_iter().filter_map(|q| q.patch) {
                recycler.recycle(p);
            }
        }
    }
}

/// The frame a page's scroll shows with, as a message names it ([`CompMsg::ScrollTo`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRef {
    /// The frame's number ([`FrameData::number`]).
    pub frame: u64,
    /// The generation of the document it shows ([`FrameData::generation`]): the scroll is that
    /// document's, and is dropped when another document's frame comes instead.
    pub generation: u64,
    /// The scroll's place among its page's ([`PageScroll::serial`]).
    pub serial: u64,
}

/// A scroll a page made of its own viewport (by script, to a fragment), which shows with the frame
/// painted for it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageScroll {
    pub to: Point,
    /// Counted from 1 in the order the page scrolled: a scroll is shown only while none after it
    /// has been (one comes in its frame, another as a message, by different ways).
    pub serial: u64,
}

/// The last of a page's scrolls ([`PageScroll`]) the compositor has shown, as its reports of where
/// the viewport is name it ([`crate::tab::TabCommand::Scrolled`]): a report that names one from
/// before the page's own last scroll is of a frame painted for the place before that scroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShownScroll {
    /// The generation of the document whose frame the compositor shows
    /// ([`FrameData::generation`]).
    pub generation: u64,
    /// The serial of that document's last scroll shown ([`PageScroll::serial`]), 0 for none.
    pub serial: u64,
}

/// How long a compositor waits for the frame a page's scroll shows with ([`CompMsg::ScrollTo`])
/// before it scrolls the frame it has: a frame can be lost on its way (a list refused, a raster side
/// that ended), and the scroll must not be lost with it.
pub const SHOWN_WITH_FRAME_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// What an autoscroll scrolls ([`CompMsg::AutoScroll`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoTarget {
    /// The tab's viewport (the compositor scrolls it as it does for the wheel).
    Viewport,
    /// A scroll container of the page, in the top document or a frame (the tab scrolls it, as for
    /// the wheel).
    Container(snowlark_dom::NodeId),
}

/// A begin frame: the display's vsync, which the compositor sends a tab that asks for them
/// ([`CompMsg::NeedsBeginFrames`]) to start a rendering opportunity (Chromium's `viz::BeginFrameArgs`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeginFrameArgs {
    /// The vsync's time (`frame_time`); the next is due an `interval` later (`deadline`).
    pub frame_time: Ticks,
    /// The display's refresh interval.
    pub interval: std::time::Duration,
    /// For the vsync that had passed when the tab began to ask (`BeginFrameArgs::MISSED`).
    pub missed: bool,
}

impl BeginFrameArgs {
    /// The page's begin frame: the same times as instants of this process.
    pub fn to_page(self) -> snowlark_page::BeginFrame {
        let now = std::time::Instant::now();
        let frame_time = now - Ticks::now().since(self.frame_time);
        snowlark_page::BeginFrame {
            frame_time,
            deadline: frame_time + self.interval,
            interval: self.interval,
            missed: self.missed,
        }
    }
}

/// What the window and the tabs tell the compositor: plain data, which can cross a process
/// boundary. What cannot is [`CompInput`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CompMsg {
    /// Middle-button autoscroll: while `velocity` (CSS px a second) is `Some`, every display frame
    /// scrolls `target` of the tab by what that is in the time since the last frame, through the
    /// same paths as the wheel (so scroll events, sticky layers and lazy loading follow); `None`
    /// ends it.
    AutoScroll {
        tab: TabId,
        target: AutoTarget,
        velocity: Option<Point>,
    },
    /// The client area (device px), the chrome's height (device px) and the DIP scale, with the
    /// chrome and overlay for the new size.
    Resize {
        width: u32,
        height: u32,
        chrome_height: u32,
        dpi_scale: f32,
        chrome: Option<(DisplayList, Color)>,
        overlay: Option<DisplayList>,
    },
    /// The chrome's display list (DIPs) and its background.
    Chrome(DisplayList, Color),
    /// Drawn over the page (DIPs, content-area coordinates): the status bubble, suggestions.
    Overlay(DisplayList),
    RemoveTab(TabId),
    Activate(TabId),
    ScrollBy {
        tab: TabId,
        delta: Point,
        smooth: bool,
        input: Option<Ticks>,
    },
    /// `with`: the page scrolled its viewport itself, and what it shows there is in the frame named
    /// (a scroll that needed a new frame comes in that frame instead: [`FrameData::scroll`]). The
    /// scroll shows with that frame or a later one, not before: until it comes, the frame on screen
    /// stays where it is ([`SHOWN_WITH_FRAME_WAIT`] at most).
    ScrollTo {
        tab: TabId,
        to: Point,
        smooth: bool,
        with: Option<FrameRef>,
    },
    /// The viewport scroll bar is hovered or dragged (drawn wider).
    ScrollbarActive(bool),
    /// The page's part of the content area (device px from its top-left corner; `None`: all of
    /// it). The rest is the developer tools' panel, drawn in the overlay.
    /// The page's part of the content area in device px: width, height and the left edge (the
    /// developer tools docked at the left push the page right). `None`: all of it.
    PageArea(Option<(u32, u32, u32)>),
    Dark(bool),
    /// A video has a new frame to show.
    VideoFrame(VideoId),
    /// The tab wants a begin frame at each vsync, or no more (Chromium's
    /// `CompositorFrameSinkSupport::SetNeedsBeginFrame`).
    NeedsBeginFrames {
        tab: TabId,
        needs: bool,
    },
    /// Asks for [`CompEvent::Presented`] with this number once a frame showing every message before
    /// this one is on its way to the screen (the window waits for it inside WM_SIZE, so DWM never
    /// shows the window at a size its layers don't have yet).
    Ack(u64),
    /// Tab `tab`'s surface: the message's handle `pipe` ([`Handle::Pipe`]) is where the tab hears of
    /// its viewport's scroll position and its begin frames ([`CompInput::bind`]).
    Bind {
        tab: TabId,
        pipe: u32,
    },
    /// The window's video frames: the message's handle `store` ([`Handle::Videos`],
    /// [`CompInput::videos`]).
    Videos {
        store: u32,
    },
    /// Window `window` shows what the compositor presents into the composition surface that is the
    /// message's handle `handle` ([`Handle::Surface`], [`CompInput::surface`]): one swap chain with
    /// the chrome and the content, which the window's process binds to its visual tree. A window
    /// with no surface gets a visual tree of the compositor's own, which only works in the
    /// window's process. Only the browser sends it.
    Surface {
        window: WindowId,
        handle: u32,
    },
    /// Tab `tab` shows in window `window`, whose compositor this one sends what the tab's renderer
    /// sends for it here (its frames, scrolls and begin frame asks); `None` stops. Only the browser
    /// sends it: a popup window's tab is served by its opener's renderer, which sends to the
    /// opener's window.
    Forward {
        tab: TabId,
        window: Option<WindowId>,
    },
    /// What the undocked developer tools' window shows (`None`: the window goes). Only the browser
    /// sends it: no renderer draws into that window.
    DevtoolsWindow(Option<Box<DevtoolsFrame>>),
    Quit,
}

/// A window as messages name it: its handle's value, which is the same in every process of the
/// desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub i64);

impl From<WindowHandle> for WindowId {
    fn from(w: WindowHandle) -> WindowId {
        WindowId(w.raw() as i64)
    }
}

impl WindowId {
    pub fn handle(self) -> WindowHandle {
        WindowHandle::from_raw(self.0 as isize)
    }
}

/// What a message to the compositor carries beside its bytes, named in it by index, as a pipe
/// carries handles beside a message ([`CompInput::Handles`]). Across processes each is a handle
/// the browser brokers into the compositor's process; in one, the object itself moves.
pub enum Handle {
    /// A tab's command pipe.
    Pipe(snowlark_ipc::Sender<TabCommand>),
    /// The video frames' textures.
    Videos(Arc<VideoStore>),
    /// A window's composition surface (`snowlark_paint::compositor::WindowSurface::handle`).
    Surface(std::os::windows::io::OwnedHandle),
    /// A window's composition surface as the window's process has it, which can give a new
    /// surface ([`snowlark_paint::compositor::WindowSurface::renew`]): for a compositor in the
    /// window's process, or for what passes it on to the GPU process. Never crosses a process.
    Tree(snowlark_paint::compositor::WindowSurface),
}

/// What the compositor tells the window: plain data, as [`CompMsg`] is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CompEvent {
    /// The frame that [`CompMsg::Ack`] with this number asked for is on its way to the screen.
    Presented(u64),
    /// The compositor's process ended (the GPU process): no wait will be answered.
    Lost,
}

/// What the compositor thread's channel carries: the plain messages, messages with handles, the
/// window's lists' resources, and what only ever comes from inside the compositor's own process
/// (tiles on its device, its vsyncs).
pub enum CompInput {
    Msg(CompMsg),
    /// A message and the handles it names by index ([`Handle`]).
    Handles(CompMsg, Vec<Handle>),
    Frame(TabId, Box<FrameData>),
    /// The display's vsync, from the compositor's vsync thread: its time and the refresh interval
    /// (`VSyncObserver::OnVSync`).
    VSync(Ticks, std::time::Duration),
    /// What the compositor's own tables need for the window's lists (the chrome, the overlay, the
    /// undocked developer tools'), sent ahead of the list that needs it, as a renderer's raster seam
    /// sends its fonts and images ([`CompInput::resource`]).
    Resource(Resource),
    /// The pictures of the browser's protected players, for the window's protected videos
    /// ([`crate::video::Picture::Protected`]). Never crosses a process.
    Protected(Arc<dyn ProtectedPictures>),
}

/// The pictures of the browser's protected players (its key systems' media engines), which a
/// compositor in the browser's process shows for a [`crate::video::Picture::Protected`] quad under
/// the content.
pub trait ProtectedPictures: Send + Sync {
    /// Player `player`'s composition surface, once its engine has a picture.
    fn surface(&self, player: u64) -> Option<Arc<std::os::windows::io::OwnedHandle>>;
    /// Player `player`'s picture shows at this rectangle of the screen (device pixels): where its
    /// output is protected, and the size its engine draws at.
    fn place(&self, player: u64, x: i32, y: i32, width: i32, height: i32);
}

impl CompInput {
    /// Binds tab `tab`'s surface to its command pipe `pipe` ([`CompMsg::Bind`]).
    pub fn bind(tab: TabId, pipe: snowlark_ipc::Sender<TabCommand>) -> CompInput {
        let msg = snowlark_ipc::loopback::through(CompMsg::Bind { tab, pipe: 0 });
        CompInput::Handles(msg, vec![Handle::Pipe(pipe)])
    }

    /// The window's video frames are `store`'s ([`CompMsg::Videos`]).
    pub fn videos(store: Arc<VideoStore>) -> CompInput {
        let msg = snowlark_ipc::loopback::through(CompMsg::Videos { store: 0 });
        CompInput::Handles(msg, vec![Handle::Videos(store)])
    }

    /// Window `window` shows the composition surface `surface` ([`CompMsg::Surface`]).
    pub fn surface(window: WindowId, surface: std::os::windows::io::OwnedHandle) -> CompInput {
        let msg = snowlark_ipc::loopback::through(CompMsg::Surface { window, handle: 0 });
        CompInput::Handles(msg, vec![Handle::Surface(surface)])
    }

    /// Window `window` shows the composition surface of `tree`, a visual tree of the window's
    /// process ([`CompMsg::Surface`], [`Handle::Tree`]).
    pub fn tree(window: WindowId, tree: snowlark_paint::compositor::WindowSurface) -> CompInput {
        let msg = snowlark_ipc::loopback::through(CompMsg::Surface { window, handle: 0 });
        CompInput::Handles(msg, vec![Handle::Tree(tree)])
    }

    /// `resource` as the compositor gets it: with `SNOWLARK_IPC=loopback`, its head through the wire
    /// format and joined to its bytes again with every check; `None` when refused on the way.
    pub fn resource(resource: Resource) -> Option<CompInput> {
        use crate::raster_link::{cross, ToRaster};
        match cross(ToRaster::Resource(resource)) {
            Some(ToRaster::Resource(r)) => Some(CompInput::Resource(r)),
            _ => None,
        }
    }
}

/// A frame of the undocked developer tools' window: the whole client area, one display list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DevtoolsFrame {
    pub window: WindowId,
    /// The client area in device px.
    pub width: u32,
    pub height: u32,
    pub dpi_scale: f32,
    /// In DIPs from the client area's top-left corner.
    pub list: DisplayList,
    pub background: Color,
}

impl From<CompMsg> for CompInput {
    fn from(m: CompMsg) -> CompInput {
        CompInput::Msg(snowlark_ipc::loopback::through(m))
    }
}
