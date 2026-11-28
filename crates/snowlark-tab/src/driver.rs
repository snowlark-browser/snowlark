// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a tab's thread drives: a [`TabDriver`] owns a document's lifetime (fetching, parsing, style,
//! layout, painting to display lists, hit testing). The tab thread ([`crate::tab`]) owns one, feeds it
//! input and viewport changes and rasterizes what it paints; the session history is the window's
//! ([`crate::tab_host::TabHost`]).
//!
//! [`crate::renderer_driver::RendererDriver`] implements it over `snowlark_render::Renderer` without
//! scripts. The page crate's `Page` (scripts, the event loop, DOM events) implements the same trait
//! and replaces it through a [`DriverFactory`]: a scripted page answers [`TabDriver::pointer`] by
//! dispatching DOM events and returns the default action (a link to follow), asks for repaints from
//! its rendering steps, and says when its next timer is due with [`Update::next`].

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use snowlark_dom::NodeId;
pub use snowlark_layout::style::{ColorScheme, ScrollbarColors};
use snowlark_net::{NetClient, Url};
use snowlark_paint::input::{Cursor, Modifiers, MouseButton};
use snowlark_paint::{Color, DisplayList, ImageStore, Point, Rect, Size, TextEngine};

/// The system clipboard, as the window process shares it (text only).
pub trait Clipboard: Send + Sync {
    fn text(&self) -> Option<String>;
    fn set_text(&self, text: &str);
    /// The clipboard's markup (`text/html`), if it has some.
    fn html(&self) -> Option<String> {
        None
    }
    /// Puts text and, with it, markup on the clipboard.
    fn set_text_and_html(&self, text: &str, html: Option<&str>) {
        let _ = html;
        self.set_text(text);
    }
}

/// A clipboard in memory (tests, headless drivers).
#[derive(Default)]
pub struct MemoryClipboard(std::sync::Mutex<(Option<String>, Option<String>)>);

impl Clipboard for MemoryClipboard {
    fn text(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).0.clone()
    }
    fn set_text(&self, text: &str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = (Some(text.to_owned()), None);
    }
    fn html(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).1.clone()
    }
    fn set_text_and_html(&self, text: &str, html: Option<&str>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) =
            (Some(text.to_owned()), html.map(str::to_owned));
    }
}

/// What a tab asks the browser about its own pages (`snowlark:`, `about:` and `view-source:` addresses),
/// in plain data: the browser half owns their content. It is the request of [`InternalPages`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InternalQuery {
    /// The page at a `snowlark:` or `about:` address: where it is shown and its HTML.
    Page { url: String },
    /// Which kind of the browser's own pages an address is.
    Kind { url: String },
    /// What a `view-source:` address shows: the address the browser fetches for it.
    ViewSourceTarget { url: String },
    /// The page that shows a fetched source: the `view-source:` address, the response's headers
    /// and body, and whether the body was cut at [`MAX_SOURCE`].
    ViewSource {
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        cut: bool,
    },
}

/// A kind of page of the browser's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InternalKind {
    Settings,
    About,
    Credits,
    /// The history page.
    History,
    /// The bookmark manager.
    Bookmarks,
    /// The downloads page.
    Downloads,
    /// The saved passwords page.
    Passwords,
    /// The certificate viewer.
    Certificate,
    /// Not one of those (an address the browser has no page at, or another scheme).
    Other,
}

/// The answer to an [`InternalQuery`], of the same shape as the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InternalAnswer {
    /// `shown` is the address the page is shown at (the canonical one: a settings form's submission
    /// shows the settings page itself); `html` is `None` for an address that names no page.
    Page {
        shown: String,
        html: Option<String>,
    },
    Kind(InternalKind),
    ViewSourceTarget(Option<String>),
    ViewSource {
        html: String,
    },
}

/// The most bytes of a source `view-source:` shows (the rest is cut, with a note).
pub const MAX_SOURCE: usize = 8 << 20;

/// The browser's own pages, answered by the browser half from its state (the settings).
pub trait InternalPages: Send + Sync {
    fn request(&self, request: InternalQuery) -> InternalAnswer;

    /// Which kind of page `url` is.
    fn kind(&self, url: &Url) -> InternalKind {
        match self.request(InternalQuery::Kind {
            url: url.as_str().to_owned(),
        }) {
            InternalAnswer::Kind(k) => k,
            _ => InternalKind::Other,
        }
    }
}

/// No pages of the browser's own (tests, headless drivers).
pub struct NoInternalPages;

impl InternalPages for NoInternalPages {
    fn request(&self, request: InternalQuery) -> InternalAnswer {
        match request {
            InternalQuery::Page { url } => InternalAnswer::Page {
                shown: url,
                html: None,
            },
            InternalQuery::Kind { .. } => InternalAnswer::Kind(InternalKind::Other),
            InternalQuery::ViewSourceTarget { .. } => InternalAnswer::ViewSourceTarget(None),
            InternalQuery::ViewSource { .. } => InternalAnswer::ViewSource {
                html: String::new(),
            },
        }
    }
}

pub fn no_internal_pages() -> Arc<dyn InternalPages> {
    Arc::new(NoInternalPages)
}

pub use snowlark_page::find::FindStatus;

/// What the find box asks of a tab's page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FindCommand {
    /// The box opened: search for the page's selected text if it has one, else for `previous`.
    Open {
        previous: String,
    },
    /// The text changed.
    Query(String),
    /// The next match, or the previous one.
    Next,
    Previous,
    /// The box closed; with `select` the active match is left selected in the page.
    Close {
        select: bool,
    },
}

/// What a context menu is about, and where (viewport CSS px).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextInfo {
    pub point: Point,
    pub target: snowlark_page::embedder::ContextTarget,
    /// Opened from the keyboard (Shift+F10, the menu key).
    pub keyboard: bool,
}

/// What a page of the browser's own asked the window for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InternalRequest {
    /// The settings page's form, submitted (its query).
    Settings(String),
    /// The history page's form, submitted (its query): visits to remove.
    History(String),
    /// A change the bookmark manager asks for (its query).
    Bookmarks(String),
    /// What the downloads page asks for (its query): open a file, show it in its folder, copy its
    /// link, remove it from the list, cancel or retry it, clear the list, undo a removal.
    Downloads(String),
    /// What the passwords page asks for (its query): reveal or delete a login, take a site off the
    /// Never list.
    Passwords(String),
}

/// What a driver gets when its tab starts.
#[derive(Clone)]
pub struct DriverContext {
    pub net: NetClient,
    /// The window's video frames: media elements' players decode into them.
    pub videos: Option<std::sync::Arc<snowlark_paint::video::VideoStore>>,
    /// The tab's text engine (web fonts register here).
    pub text: Arc<TextEngine>,
    /// Wakes the tab's thread (call it from any thread when there is work: a fetch made progress, a
    /// timer is due); the thread then calls [`TabDriver::update`].
    pub wake: Arc<dyn Fn() + Send + Sync>,
    pub clipboard: Arc<dyn Clipboard>,
    pub internal: Arc<dyn InternalPages>,
    /// The tab's content filter, for its page.
    pub content_filter: Option<Arc<dyn snowlark_page::ContentFilter>>,
    /// The tab's media players: whether it makes sound, its mute (none: the page's media is not
    /// counted, as in tests).
    pub audio: Option<Arc<snowlark_media::AudioGroup>>,
    /// Where the page's AudioContexts play (none: a null sink).
    pub audio_out: Option<Arc<dyn snowlark_audio::realtime::AudioOutputs>>,
    /// The browser's BroadcastChannel registry (its pages' channels of one origin reach each other).
    pub broadcast: snowlark_page::BroadcastHub,
    pub web_storage: Arc<dyn snowlark_page::web_storage::WebStorage>,
    /// The browser's IndexedDB backend (its pages' databases, persisted in the profile).
    pub idb: snowlark_page::IdbBackend,
    /// The browser's Cache API storage (its pages' caches, persisted in the profile).
    pub caches: snowlark_page::CacheBackend,
    pub service_workers: Option<snowlark_page::ServiceWorkerBackend>,
    /// The browser's key systems (none: Clear Key only).
    pub cdm: Option<Arc<dyn snowlark_page::media::cdm::Cdm>>,
    /// Where the page's WebGL runs (a renderer's link to the GPU process); `None`: this process's own.
    pub webgl: Option<Arc<dyn snowlark_page::WebGlProvider>>,
}

/// Makes a tab's driver on the tab's thread (a driver need not be `Send`).
pub type DriverFactory = Box<dyn FnOnce(DriverContext) -> Box<dyn TabDriver> + Send>;

/// The viewport the document is laid out in.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// CSS px: the whole content area (`innerWidth`, `innerHeight`), a classic scroll bar included.
    pub viewport: Size,
    /// Device pixels per CSS px: the monitor's scale times the page zoom.
    pub scale: f32,
    /// The width in CSS px of the classic vertical scroll bar the window shows at the viewport's
    /// right edge when the document overflows it (layout leaves it out of the initial containing
    /// block); 0 for none.
    pub scrollbar: f32,
    /// Where the viewport and the window sit on the screen (`screenX`, `outerWidth`, a mouse
    /// event's screen coordinates).
    pub screen: snowlark_page::ScreenPlacement,
}

impl Default for Device {
    fn default() -> Self {
        Device {
            viewport: Size::new(1280.0, 720.0),
            scale: 1.0,
            scrollbar: 0.0,
            screen: snowlark_page::ScreenPlacement::default(),
        }
    }
}

/// How a link is to be opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    CurrentTab,
    NewBackgroundTab,
    NewForegroundTab,
}

/// Pointer input over the page, at a viewport point in CSS px.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Pointer {
    Move(Point),
    /// A press, with the click count (2 for the second press of a double-click).
    Down(Point, MouseButton, Modifiers, u32),
    Up(Point, MouseButton, Modifiers),
    /// A wheel turn by `delta` CSS px (the compositor scrolls; the page gets a `wheel` event).
    Wheel(Point, Point, Modifiers),
    Leave,
}

/// A drag from another program over the page (the window's drop target), in viewport CSS px.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DragInput {
    /// It entered the page with `data` (the files without their contents).
    Enter {
        at: Point,
        data: Box<snowlark_page::dnd::ExternalDrag>,
        mods: Modifiers,
    },
    Over {
        at: Point,
        mods: Modifiers,
    },
    /// It left the page, or was canceled.
    Leave,
    /// It was dropped, with `data` (the files with their contents, read by the browser).
    Drop {
        at: Point,
        data: Box<snowlark_page::dnd::ExternalDrag>,
        mods: Modifiers,
    },
}

/// Keyboard input for the page.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum KeyKind {
    Down {
        repeat: bool,
    },
    Up,
    /// A character typed (after the keyboard layout).
    Char(char),
}

/// A key: its Windows virtual-key code, scan code (`0x100` for extended keys), the character it
/// types and the modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyInput {
    pub kind: KeyKind,
    pub key: u16,
    pub scan: u16,
    pub text: Option<char>,
    pub mods: Modifiers,
}

/// A viewport scroll a key asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyScroll {
    By(Point),
    Top,
    Bottom,
}

/// What a key did.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KeyResult {
    pub scroll: Option<KeyScroll>,
    pub repaint: bool,
}

/// One arrow-key step (CSS px).
pub const LINE_STEP: f32 = 40.0;

/// Page Up/Down and Space: most of the viewport, keeping a little of the previous screen in view.
pub fn page_step(viewport: f32) -> f32 {
    (viewport * 0.875).max(viewport - 80.0).max(LINE_STEP)
}

/// The viewport scroll of a scrolling key (arrows, Page Up/Down, Space, Home, End) that nothing in
/// the page took.
pub fn key_scroll(key: u16, mods: Modifiers, viewport: Size) -> Option<KeyScroll> {
    use snowlark_paint::input::keys;
    let page = page_step(viewport.height);
    let by = |x: f32, y: f32| Some(KeyScroll::By(Point::new(x, y)));
    match key {
        keys::DOWN => by(0.0, LINE_STEP),
        keys::UP => by(0.0, -LINE_STEP),
        keys::RIGHT if !mods.alt => by(LINE_STEP, 0.0),
        keys::LEFT if !mods.alt => by(-LINE_STEP, 0.0),
        keys::PAGE_DOWN => by(0.0, page),
        keys::PAGE_UP => by(0.0, -page),
        keys::SPACE if mods.shift => by(0.0, -page),
        keys::SPACE => by(0.0, page),
        keys::HOME if !mods.alt => Some(KeyScroll::Top),
        keys::END => Some(KeyScroll::Bottom),
        _ => None,
    }
}

/// What pointer input did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PointerResult {
    pub cursor: Cursor,
    /// The link under the pointer (for the status line).
    pub link: Option<Url>,
    /// A link was activated.
    pub open: Option<(Url, Disposition)>,
    /// The `title` tooltip of what is under the pointer.
    pub title: Option<String>,
    pub repaint: bool,
    /// For a press: what the press found ([`PressOutcome`]).
    pub press: Option<PressOutcome>,
    /// A move the page holds for its next rendering update: nothing is known yet (the result comes
    /// from [`Driver::take_held_pointer`]).
    pub held: bool,
}

/// What a press found, for the embedder's own default actions (autoscroll on a middle press).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressOutcome {
    /// The page canceled the `mousedown`.
    pub canceled: bool,
    /// The press is on a link, a form control or an editable element.
    pub blocked: bool,
}

/// A scroll container the user can scroll (not the viewport), in document coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scroller {
    pub node: NodeId,
    /// Its padding box.
    pub rect: Rect,
    pub scroll: Point,
    pub max: Point,
}

impl Scroller {
    pub fn can_scroll(&self, delta: Point) -> bool {
        (delta.y < 0.0 && self.scroll.y > 0.0)
            || (delta.y > 0.0 && self.scroll.y < self.max.y)
            || (delta.x < 0.0 && self.scroll.x > 0.0)
            || (delta.x > 0.0 && self.scroll.x < self.max.x)
    }
}

/// What the shell shows about the document.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentInfo {
    /// The URL being loaded, or the document's.
    pub url: Option<Url>,
    pub title: String,
    pub loading: bool,
    /// The favicon to fetch.
    pub icon: Option<Url>,
    /// The scrollable size (CSS px).
    pub content: Size,
    /// The width (CSS px) of the classic vertical scroll bar the last layout showed (0: none).
    pub scrollbar: f32,
    /// The root element's used color scheme (CSS Color Adjust 1 §2.4): the viewport's scroll bar
    /// follows it.
    pub color_scheme: ColorScheme,
    /// The root's `scrollbar-color` (CSS Scrollbars 1 §2; `None`: `auto`), for the viewport's bar.
    pub scrollbar_colors: Option<ScrollbarColors>,
    /// What a list is painted over: the canvas surface, white or dark by the root's used color
    /// scheme.
    pub background: Color,
    /// What shows where the page is not drawn (yet), a part scrolled into view before its raster:
    /// the canvas background's color over the canvas surface.
    pub page_background: Color,
    /// Whether anything is `position: fixed` or `sticky` (such pages are repainted at each scroll
    /// position instead of being moved by the compositor).
    pub fixed: bool,
    pub scrollers: Vec<Scroller>,
    /// The viewport snaps (the root element's `scroll-snap-type` is not `none`): the page, not the
    /// compositor, chooses where the wheel and the scrolling keys take it.
    pub snap_viewport: bool,
    /// Bumped for each new document (the compositor resets the scroll position then).
    pub generation: u64,
    /// The document is an error page for `url` (never shown as secure).
    pub error_page: bool,
}

impl Default for DocumentInfo {
    fn default() -> Self {
        DocumentInfo {
            url: None,
            title: String::new(),
            loading: false,
            icon: None,
            content: Size::ZERO,
            scrollbar: 0.0,
            color_scheme: ColorScheme::Light,
            scrollbar_colors: None,
            background: Color::WHITE,
            page_background: Color::WHITE,
            fixed: false,
            scrollers: Vec::new(),
            snap_viewport: false,
            generation: 0,
            error_page: false,
        }
    }
}

/// What [`TabDriver::update`] found.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Update {
    /// The document looks different: paint again.
    pub repaint: bool,
    /// When to be updated again without being woken (a scripted page's next timer).
    pub next: Option<Instant>,
    /// Script scrolled the viewport here (CSS px): the compositor moves to it.
    pub scroll_to: Option<Point>,
    /// Scroll anchoring moved the viewport by this much (CSS px; CSS Scroll Anchoring 1): the
    /// compositor moves by as much from where it is. Never set with `scroll_to`.
    pub scroll_by: Option<Point>,
}

/// A tab's document, driven by its tab's thread. See the module documentation.
/// What a [`TabDriver`]'s document takes ([`TabDriver::memory`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriverMemory {
    pub js: u64,
    pub dom: u64,
    /// The body bytes the document's fetches have taken so far (not memory: the task manager's
    /// Network rate is its growth).
    pub received: u64,
}

pub trait TabDriver {
    /// Starts loading `url` (a fetch for http(s), or a built-in page). Returns at once.
    fn navigate(&mut self, url: Url);

    /// The windows the page opened (with an opener) or closed by script since the last call:
    /// each is a view of the same page, which the browser shows as a tab of its own.
    fn take_window_changes(&mut self) -> Vec<snowlark_page::WindowChange> {
        Vec::new()
    }

    /// Makes the calls that follow address the view of the hosted window `view` (its document,
    /// viewport, scrolling, input and paint) until `select_view(None)` puts the page's own
    /// top-level document back. False when the window is gone (or the driver has no views).
    fn select_view(&mut self, _view: Option<snowlark_page::HostedWindow>) -> bool {
        false
    }

    /// A view's part of a pass, after the page's [`TabDriver::update`] (which runs the page for
    /// every view): what changed in the view's document. Called with the view selected.
    fn view_update(&mut self) -> Update {
        Update::default()
    }

    /// The user closed the view of a hosted window: the window is closed.
    fn close_view(&mut self, _view: snowlark_page::HostedWindow) {}

    /// The browser closed the page's own tab while views of its windows stay open: the page's own
    /// top-level traversable is destroyed (its windows see `opener` null), and the page goes on
    /// running for them.
    fn close_own_page(&mut self) {}

    /// Loads the document at `url` again, as it is now: a driver with an HTTP cache validates what
    /// it holds.
    fn reload(&mut self, url: Url) {
        self.navigate(url);
    }

    /// Stops a load in progress.
    fn stop(&mut self);

    fn set_device(&mut self, device: Device);

    /// Where the compositor has scrolled the viewport (CSS px).
    fn set_scroll(&mut self, scroll: Point);

    /// Applies what arrived (network, tasks) and lays out when due.
    fn update(&mut self) -> Update;

    /// The page's rendering opportunities come from begin frames from now on
    /// ([`TabDriver::begin_frame`]). A driver without a page has none.
    fn use_begin_frames(&mut self) {}

    /// The display's vsync: a rendering opportunity, which the next [`TabDriver::update`] takes.
    fn begin_frame(&mut self, _args: snowlark_page::BeginFrame) {}

    /// Whether the page asks for begin frames.
    fn needs_begin_frames(&self) -> bool {
        false
    }

    /// The display list of the viewport scrolled to `origin` (which may lie above the viewport's
    /// scroll position: the tab rasterizes a band around it). `None` when there is no document.
    fn paint(&mut self, origin: Point) -> Option<DisplayList>;

    /// The text engine and images the display lists refer to.
    fn text(&self) -> &TextEngine;
    fn images(&self) -> &ImageStore;
    /// The images, shared (the tab's raster thread reads them while the page goes on).
    fn image_store(&self) -> std::sync::Arc<ImageStore>;

    fn pointer(&mut self, input: Pointer) -> PointerResult;

    /// A drag from another program over the page; returns the drag operation the page chose
    /// (none: it does not take the drag, as a driver without a page).
    fn drag(&mut self, _input: DragInput) -> snowlark_page::dnd::DragOperation {
        snowlark_page::dnd::DragOperation::None
    }

    /// A drag the page started for the window's drag loop to carry on.
    fn take_drag_out(&mut self) -> Option<snowlark_page::dnd::DragOut> {
        None
    }

    /// The window's drag loop of the page's drag ended with `operation`.
    fn drag_out_ended(&mut self, _operation: snowlark_page::dnd::DragOperation) {}

    /// What a pointer move held for the page's next rendering update found when it was dispatched
    /// there (a move's [`PointerResult`] is [`PointerResult::held`] until then).
    fn take_held_pointer(&mut self) -> Option<PointerResult> {
        None
    }

    /// Keyboard input while the page has the focus. The default takes no key but the scrolling
    /// ones.
    fn key(&mut self, input: KeyInput) -> KeyResult {
        match input.kind {
            KeyKind::Down { .. } if !input.mods.ctrl && !input.mods.alt => KeyResult {
                scroll: key_scroll(input.key, input.mods, self.viewport()),
                repaint: false,
            },
            _ => KeyResult::default(),
        }
    }

    /// What the document's own data takes (bytes): the JavaScript heap with what it keeps outside
    /// it, and the DOM. A driver that keeps none says zero.
    fn memory(&mut self) -> DriverMemory {
        DriverMemory::default()
    }

    /// How another thread stops the script this driver's page is running (a driver without scripts
    /// has none).
    fn terminator(&self) -> Option<snowlark_page::Terminator> {
        None
    }

    /// The viewport size (CSS px).
    fn viewport(&self) -> Size {
        Size::new(1280.0, 720.0)
    }

    /// New tabs the document asked for (a link with a target, Ctrl+click, a middle click).
    fn take_new_tabs(&mut self) -> Vec<(Url, Disposition)> {
        Vec::new()
    }

    /// Navigations the document started itself (a link, a form, `location`) that committed since the
    /// last call: new session history entries.
    fn take_committed(&mut self) -> Vec<Url> {
        Vec::new()
    }

    /// Whether the driver's top-level navigations begin in the browser (M5): it then asks for
    /// each ([`TabDriver::take_begun_navigations`]) and commits what the browser hands back
    /// ([`TabDriver::commit`]). A driver without a page loads by itself.
    fn browser_navigates(&self) -> bool {
        false
    }

    /// Commits a top-level navigation the browser began (`TabCommand::Commit`). The default
    /// shows the URL as loaded.
    fn commit(&mut self, commit: crate::tab::Commit) {
        let url = match &commit.params.response {
            snowlark_page::CommitResponse::Fetched { head } => {
                head.url().cloned().unwrap_or(commit.params.url)
            }
            _ => commit.params.url,
        };
        self.navigate(url);
    }

    /// The document's own session history entry `index` becomes current (the browser traversed
    /// to it).
    fn go_in_document(&mut self, _index: usize) {}

    /// The top-level navigations the page asked the browser to begin since the last call.
    fn take_begun_navigations(&mut self) -> Vec<snowlark_page::embedder::BeginNavigation> {
        Vec::new()
    }

    /// The same-document URL changes of the document since the last call.
    fn take_same_document_changes(&mut self) -> Vec<snowlark_page::embedder::SameDocumentChange> {
        Vec::new()
    }

    /// The session history traversals the page asked for since the last call (steps).
    fn take_traversals(&mut self) -> Vec<isize> {
        Vec::new()
    }

    /// What the page asked the window for since the last call (the Fullscreen API): `true` to show
    /// it fullscreen, `false` to leave fullscreen.
    fn take_fullscreen_asks(&mut self) -> Vec<bool> {
        Vec::new()
    }

    /// The window shows the page fullscreen now (`true`), or no longer: its answer to an ask, or
    /// the user's own exit.
    fn fullscreen(&mut self, _on: bool) {}

    /// What the page reported for the browser's password manager since the last call.
    fn take_password_events(&mut self) -> Vec<snowlark_page::passwords::PasswordEvent> {
        Vec::new()
    }

    /// Fills a saved login the browser's password manager chose.
    fn fill_password(&mut self, _fill: &snowlark_page::passwords::PasswordFill) {}

    /// Scrolls a scroll container by `delta`; whether it moved.
    fn scroll_container(&mut self, node: NodeId, delta: Point) -> bool;

    /// A mouse wheel notch (`key`: an arrow or page key) scrolls a scroll container (`None`: the
    /// viewport) by `delta`, to the snap position it chooses when the box snaps (CSS Scroll Snap 1
    /// §6.2). Whether it moved; a driver that does not snap scrolls a container by `delta` and
    /// leaves the viewport to the compositor (false).
    fn snap_scroll(&mut self, node: Option<NodeId>, delta: Point, _key: bool) -> bool {
        node.is_some_and(|n| self.scroll_container(n, delta))
    }

    /// A drop-down select's list the document asked to show (the window draws it).
    fn take_select_popup(&mut self) -> Option<snowlark_page::popup::SelectPopup> {
        None
    }

    /// The user chose option `index` in the list of `select`.
    fn select_popup_chosen(&mut self, _select: NodeId, _index: usize) {}

    /// Text composed through an input method editor while the page has the focus.
    fn ime(&mut self, _event: snowlark_paint::input::ImeEvent) {}

    /// The context menu the page asked for (a right-click its `contextmenu` event didn't cancel).
    fn take_context_menu(&mut self) -> Option<ContextInfo> {
        None
    }

    /// The keyboard asks for a context menu, at `at` (viewport CSS px) or for the focused element.
    fn context_menu_at(&mut self, _at: Option<Point>) {}

    /// The user prefers a dark color scheme for pages (`prefers-color-scheme`), or not.
    fn set_color_scheme(&mut self, _dark: bool) {}

    /// The window shows the tab, or not (a background tab): a hidden page's animated images stop.
    fn set_shown(&mut self, _shown: bool) {}

    /// What the browser's own pages asked for since the last call.
    fn take_internal_requests(&mut self) -> Vec<InternalRequest> {
        Vec::new()
    }

    /// Clears the storage of `site` (a host; its registrable domain and subdomains), or of every
    /// site.
    fn clear_site_data(&mut self, _site: Option<&str>) {}

    /// A request of the find box (find in page). A page without scripts has none to search.
    fn find(&mut self, _command: FindCommand) {}

    /// The find box's counter, when it changed since the last call.
    fn take_find_status(&mut self) -> Option<FindStatus> {
        None
    }

    /// A request of the developer tools.
    fn devtools(&mut self, _command: crate::devtools::protocol::DevtoolsCommand) {}

    /// What the developer tools hear of the document since the last call.
    fn take_devtools(&mut self) -> Vec<crate::devtools::protocol::DevtoolsEvent> {
        Vec::new()
    }

    fn info(&self) -> &DocumentInfo;
}

/// The innermost scroller under the document point `p` that can scroll by `delta`.
pub fn scroller_at(scrollers: &[Scroller], p: Point, delta: Point) -> Option<&Scroller> {
    scrollers
        .iter()
        .filter(|s| s.rect.contains(p) && s.can_scroll(delta))
        .min_by(|a, b| {
            (a.rect.width * a.rect.height)
                .partial_cmp(&(b.rect.width * b.rect.height))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}
