// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser window: the [`WindowHandler`] that owns the tabs, the chrome's state and the
//! history, routes input (to the chrome, to the compositor for scrolling, to tabs for the page) and
//! keeps the chrome's display list up to date. It runs on the window's thread and never waits for a
//! tab or the compositor.

mod autoscroll;
mod blocking;
mod bookmark_drag;
mod bookmark_manager_ui;
mod bookmarks_ui;
mod clear_data_ui;
mod close_ui;
mod commands;
mod context_menu;
mod devtools_menus;
mod devtools_ui;
mod devtools_window;
mod downloads_ui;
mod drop_files;
mod find;
mod fullscreen;
mod hang_ui;
mod history_ui;
pub mod incognito;
pub(crate) mod node_menu;
mod popup_window;
mod restore_ui;
pub(crate) mod settings_ui;
mod side_tabs_ui;
mod site_data;
mod tab_audio;
mod tab_card_ui;
mod task_manager_ui;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use snowlark_media::TabMuteControl;
use snowlark_net::{NetClient, Url};
use snowlark_paint::window::{
    self, keys, CaptionAction, CaptionButton, Cursor, Event, HitArea, Modifiers, MouseButton,
    Window, WindowHandler,
};
use snowlark_paint::{Gpu, ImageId, ImageStore, Point, Size, TextEngine};

use crate::address::{
    display_url, interpret, interpret_with, search_url_with, TextField, START_PAGE,
};
use crate::bookmarks::{BookmarkId, Bookmarks};
use crate::chrome::{self, AddressView, ChromeView, Fonts, Hit, Layout, OverlayView, Suggestion};
use crate::composite::{viewport_scrollbar, CompMsg, CompositorThread, SCROLLBAR_WIDTH};
use crate::crash::SadTab;
use crate::devtools::{DevTools, Dock};
use crate::dialog::Dialog;
use crate::driver::{scroller_at, Clipboard, Device, Disposition, KeyInput, KeyKind, Pointer};
use crate::find_bar::FindBar;
use crate::history::History;
use crate::menu::MenuStack;
use crate::page_driver::PageDriver;
use crate::popup::{MenuKey, SelectMenu};
use crate::profile::Profile;
use crate::restore::SavedTab;
use crate::scroll::{is_precise, wheel_pixels, ScrollKind};
use crate::settings::Settings;
use crate::tab::{TabCommand, TabContext, TabEvent, TabId, TabState};
use crate::tab_host::{ProcessModel, RemoteTab, Renderer, TabHost};
use crate::tabs::{Placement, TabList};
use crate::tooltip::{Tooltip, TOOLTIP_DELAY_MS};
use crate::widgets::{edit_address_key, KeyEdit};

pub use commands::Command;
pub(crate) use devtools_window::Route;

const TIMER_CARET: usize = 1;
const TIMER_SPINNER: usize = 2;
const TIMER_SAVE: usize = 3;
const TIMER_TOOLTIP: usize = 4;
/// A tab's speaker is due to go (the moment after its sound is over).
const TIMER_AUDIO: usize = 5;
/// The session file is brought up to date.
const TIMER_SESSION: usize = 6;
/// The tab hover card asks its tab for its memory again.
const TIMER_CARD: usize = 7;
/// The active tab is checked for a script that does not return.
const TIMER_HANG: usize = 8;
/// The task manager's refresh.
const TIMER_TASKS: usize = 9;
/// A bookmark drag rests on a folder: its drop-down opens.
const TIMER_FOLDER_DROP: usize = 11;

/// How long a resize waits for the compositor's frame of the new size.
const RESIZE_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// Page zoom steps (Chrome's).
const ZOOM_LEVELS: [f32; 17] = [
    0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0,
];

/// The display scale the window draws at, as dots per inch (96 is 100%): the monitor's, or the
/// `SNOWLARK_SCALE` testing switch's (a factor such as `1.5`), which draws the chrome and the pages as
/// on a monitor at that scale whatever the real one's is.
fn window_dpi(window: &Window) -> u32 {
    scale_override().unwrap_or_else(|| window.dpi())
}

/// Where a page's viewport and its window sit on the screen, for the page's `screenX`,
/// `outerWidth` and its mouse events' screen coordinates (CSSOM View §4.2, UI Events §5.3.1), in
/// device-independent px: from the window's rectangle and client origin in device pixels
/// (`placement`), the chrome's height in device pixels above the page, the display scale and
/// the page zoom. Without a window, the viewport's origin is 0 and the window is the viewport.
fn screen_placement(
    placement: Option<snowlark_paint::window::WindowPlacement>,
    page_px: (u32, u32),
    scale: f32,
    zoom: f32,
) -> snowlark_page::ScreenPlacement {
    let dip = |px: i32| (px as f32 / scale).round() as i32;
    match placement {
        Some(((left, top, width, height), (client_x, client_y))) => {
            snowlark_page::ScreenPlacement {
                origin: (
                    dip(client_x + page_px.0 as i32),
                    dip(client_y + page_px.1 as i32),
                ),
                outer: Some((dip(left), dip(top), dip(width), dip(height))),
                zoom,
            }
        }
        None => snowlark_page::ScreenPlacement {
            origin: (0, 0),
            outer: None,
            zoom,
        },
    }
}

/// The `SNOWLARK_SCALE` display scale as dots per inch (see [`window_dpi`]).
fn scale_override() -> Option<u32> {
    parse_scale(&std::env::var("SNOWLARK_SCALE").ok()?)
}

/// A display scale factor between 0.5 and 4 as dots per inch.
fn parse_scale(value: &str) -> Option<u32> {
    let scale = value.trim().parse::<f32>().ok()?;
    (0.5..=4.0)
        .contains(&scale)
        .then(|| (scale * 96.0).round() as u32)
}

/// The next zoom level up (`dir` 1) or down (−1) from `z`.
pub fn next_zoom(z: f32, dir: i32) -> f32 {
    if dir > 0 {
        ZOOM_LEVELS
            .iter()
            .copied()
            .find(|&l| l > z + 0.001)
            .unwrap_or(z)
    } else {
        ZOOM_LEVELS
            .iter()
            .rev()
            .copied()
            .find(|&l| l < z - 0.001)
            .unwrap_or(z)
    }
}

/// What a crashed tab does on its new thread.
enum Restart {
    Reload,
    Navigate(Url),
    /// Back or forward from the entry the crash left.
    Go(isize),
}

struct TabEntry {
    tab: TabHost,
    favicon: Option<ImageId>,
    link: Option<Url>,
    cursor: Cursor,
    zoom: f32,
    /// The tab's thread crashed: its page is this (drawn by the window), and nothing more from the
    /// thread is taken.
    crashed: Option<SadTab>,
    /// The favicon's file (for a bookmark of the page).
    icon_bytes: Option<Arc<Vec<u8>>>,
    /// What the developer tools show of this tab.
    dt: Box<DevTools>,
    /// The element the developer tools highlight in the page.
    highlight: Option<snowlark_page::devtools::Highlight>,
    /// The tab's content filter (what its pages had blocked), when blocking is on.
    filter: Option<Arc<snowlark_blocker::TabFilter>>,
    /// The tab's mute: its players' group, or the handle on a renderer's.
    audio: Arc<dyn snowlark_media::TabMuteControl>,
    /// The tab strip's speaker for the tab.
    indicator: crate::audio::AudioIndicator,
    /// Whether the tab is muted (it outlives the tab's thread: a crashed tab restarts muted).
    mute: crate::audio::TabMute,
    /// The tab's find box (Ctrl+F), while it is open.
    find: Option<FindBar>,
    /// A tab restored after an unexpected exit that has not loaded yet: its history, kept until the
    /// tab is first shown.
    lazy: Option<Box<SavedTab>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Page,
    Address,
    /// A field of the developer tools.
    Devtools,
    /// The find box.
    Find,
}

pub struct Browser {
    /// What the browser's windows share ([`incognito`]).
    app: std::rc::Rc<incognito::App>,
    /// The profile's storage: its network, storage, databases and history (an Incognito
    /// window's is shared with the other Incognito windows).
    partition: std::rc::Rc<crate::partition::Partition>,
    /// The count of open Incognito windows the Incognito button shows (an Incognito window's).
    incognito_shown: usize,
    /// Content blocking (`None` with `SNOWLARK_BLOCK=0`).
    blocker: Option<Arc<snowlark_blocker::ContentBlocker>>,
    comp: CompositorThread,
    /// The compositors of this window and its popup windows.
    windows: crate::composite::Windows,
    fonts: Fonts,
    images: Arc<ImageStore>,
    events: Receiver<(TabId, TabEvent)>,
    tab_ctx: TabContext,
    /// Where the tabs' renderers run (`--process-model`).
    process_model: ProcessModel,
    /// The code cache renderer processes are served (with `--process-model=tab`).
    code_cache: Option<Arc<dyn snowlark_js::code_cache::CodeCacheStore>>,
    /// The spare renderer new tabs take (with `--process-model=tab`).
    spares: Option<snowlark_tab::spare::SparePool>,
    /// The GPU process (`--gpu-process`, M7).
    gpu_process: Option<std::rc::Rc<crate::gpu_process::GpuHost>>,
    tabs: TabList<TabEntry>,
    next_id: TabId,
    /// The pages visited, shared with the history page.
    history: Arc<std::sync::Mutex<History>>,
    /// The profile's downloads, shared with the downloads page and the download threads.
    downloads: Arc<std::sync::Mutex<crate::downloads::DownloadHistory>>,
    profile: Profile,
    settings: Settings,
    bookmarks: Bookmarks,
    /// The counts of the settings' and the bookmarks' changes the regular windows shared that
    /// this one has ([`incognito::Shared`]), and the bookmarks' version it last shared.
    shared_seen: std::cell::Cell<(u64, u64)>,
    bookmarks_published: std::cell::Cell<u64>,
    /// Bookmark icons decoded into the image store (`None`: none, or not decodable).
    bookmark_icons: HashMap<BookmarkId, Option<ImageId>>,
    /// The bookmarks bar's item widths for a bookmarks version.
    bar_widths: RefCell<Option<(u64, Vec<f32>)>>,
    // The window.
    size: (u32, u32),
    dpi: u32,
    /// The window's rectangle and its client area's origin on the screen, in device pixels
    /// (`None` until the window has said: a window under test).
    placement_px: Option<snowlark_paint::window::WindowPlacement>,
    maximized: bool,
    window_active: bool,
    dark: bool,
    // The chrome.
    hover: Hit,
    pressed: Hit,
    caption_hover: Option<CaptionButton>,
    focus: Focus,
    address: TextField,
    address_scroll: f32,
    /// How far the side tab panel's list is scrolled (DIPs).
    side_scroll: f32,
    /// The active tab and tab count the side panel's list last scrolled to show.
    side_revealed: Option<(usize, usize)>,
    /// The side tab panel as drawn with the chrome, for the overlay.
    side_list: Option<snowlark_paint::DisplayList>,
    /// The side panel's edge is being dragged: the panel's width and the pointer's x at the press,
    /// and the width with titles under the pointer now (`None`: collapsed).
    side_resize: Option<(f32, f32, Option<f32>)>,
    /// A tab pressed in the side panel (a drag once the pointer moves).
    tab_drag: Option<side_tabs_ui::TabDrag>,
    address_drag: bool,
    caret_on: bool,
    suggestions: Vec<Suggestion>,
    selected: Option<usize>,
    hover_suggestion: Option<usize>,
    spinner: f32,
    spinning: bool,
    /// The recently closed tabs, newest last (the regular profile's windows share one list).
    closed: std::rc::Rc<RefCell<Vec<String>>>,
    // The page.
    mouse: Point,
    page_hover: bool,
    /// A drag from another program over the window.
    drag_in: Option<drop_files::IncomingDrag>,
    scrollbar_drag: Option<f32>,
    scrollbar_hover: bool,
    wheel_lines: u32,
    chrome_dirty: bool,
    overlay_dirty: bool,
    sent_cursor: Cursor,
    /// A select's open list, and whether the press that went to it is to be swallowed with its
    /// release.
    menu: Option<SelectMenu>,
    swallow_release: bool,
    /// The advisory information under the pointer (waiting for the hover delay), and its tooltip
    /// once shown.
    title: Option<String>,
    tooltip: Option<Tooltip>,
    /// The card of the tab the pointer rests on.
    tab_card: Option<crate::tab_card::TabCard>,
    /// The pointer is over a crashed tab's Reload button.
    sad_hover: bool,
    /// An open menu (the app menu, a context menu, a bookmark folder) and the chrome button it
    /// belongs to.
    popup: Option<(MenuStack<Command>, Hit)>,
    /// An open dialog and what it edits.
    dialog: Option<(Dialog<Command>, commands::DialogKind)>,
    /// The user chose to wait for this stuck tab (and pass of its loop) until this moment.
    hang_snooze: Option<(TabId, u64, std::time::Instant)>,
    /// A press on a bookmark (on the bar or in a folder's drop-down) that may be a drag.
    bm_drag: Option<bookmark_drag::BookmarkDrag>,
    /// The developer tools show (for the active tab), where, and how big (bottom, right).
    devtools_open: bool,
    dock: Dock,
    /// The panel's size on each side: bottom (height), right (width), left (width).
    dock_sizes: (f32, f32, f32),
    /// The developer tools' toolbar menu, open.
    dock_menu: Option<crate::devtools::dock_menu::DockMenu>,
    /// The tab whose page the tools follow (it records its network).
    attached: Option<TabId>,
    /// The panel's edge is being dragged (the pointer's offset from it).
    splitter_drag: Option<f32>,
    /// The pointer is over the panel.
    panel_hover: bool,
    /// The browser itself, for the handler of the tools' own window ([`Route`]).
    me: std::rc::Weak<RefCell<Browser>>,
    /// Wakes the main window, whose update draws what the tools' window changed.
    main_proxy: window::WindowProxy,
    /// The undocked tools' window, while it is open, and where it was when it last closed.
    dt_window: Option<devtools_window::DtWindow>,
    dt_placement: Option<snowlark_paint::placement::Placement>,
    /// The open menu (`popup`) is in the tools' window.
    dt_popup: bool,
    /// Commands chosen in the tools' window, run from the main window.
    deferred: Vec<Command>,
    /// The site information panel, when open.
    site_info: Option<crate::site_info::SiteInfo>,
    /// The content blocker's panel, when open.
    blocker_panel: Option<crate::blocker_panel::BlockerPanel>,
    /// The task manager, when open, with what its last refresh measured.
    task_manager: Option<(
        crate::task_manager::TaskManager,
        task_manager_ui::TaskSampler,
    )>,
    /// This window's id in the process's session file (`App::join_session`: the tabs, kept for
    /// a restore after an unexpected exit); `None` in an Incognito window.
    session: Option<u64>,
    /// Middle-button autoscroll in progress, and a middle press waiting for the page's answer to
    /// its `mousedown` ([`autoscroll`]).
    autoscroll: Option<autoscroll::Session>,
    autoscroll_pending: Option<autoscroll::PendingPress>,
    /// The window placement last written to the profile.
    placed: Option<snowlark_paint::placement::Placement>,
    /// The session of an unexpected exit, offered by `restore_bubble`.
    offer: Option<crate::restore::SessionFile>,
    restore_bubble: Option<crate::restore_bubble::RestoreBubble>,
    /// The browser's own fullscreen (F11) and a page's.
    fullscreen: fullscreen::FullscreenUi,
    /// What the browser's own pages show (shared with the tabs' threads).
    internal: Arc<std::sync::Mutex<settings_ui::InternalState>>,
    /// A message for the settings page (after clearing data).
    settings_notice: Option<String>,
    /// What was searched for last (a find box opens with it).
    find_last: String,
    /// The window the calls serve is a popup window (`popup_window`): its fields are traded in.
    popup_window: bool,
    /// The popup windows, and the one whose fields are the browser's now.
    popups: Vec<popup_window::PopupWindow>,
    current: Option<u64>,
    next_popup: u64,
    /// Tabs popup windows asked the main window to open.
    main_opens: Vec<(Url, Placement)>,
}

impl Browser {
    pub fn new(
        window: &Window,
        app: std::rc::Rc<incognito::App>,
        partition: std::rc::Rc<crate::partition::Partition>,
        start: Option<Url>,
        spares: Option<snowlark_tab::spare::SparePool>,
    ) -> Browser {
        let profile = &app.profile.clone();
        let process_model = app.model;
        let gpu_process = app.gpu_process.clone();
        let off_the_record = partition.is_off_the_record();
        if !off_the_record {
            profile.ensure();
        }
        let net: NetClient = partition.net.clone();
        // (with `single`, a page in this process opens the folder itself: snowlark-page's loader)
        let code_cache = (process_model == ProcessModel::Tab).then(|| partition.code_cache());
        let gpu = Gpu::new().expect("a Direct3D 11 device");
        let text = Arc::new(TextEngine::new().expect("DirectWrite"));
        let images = Arc::new(ImageStore::new());
        let windows = crate::composite::Windows::default();
        let comp = CompositorThread::start(
            gpu.clone(),
            window.handle(),
            text.clone(),
            images.clone(),
            &windows,
            gpu_process.as_ref().map(|h| h.link()),
        );
        // Video frames: decoded by Media Foundation into textures of the window's device, drawn by
        // the compositor whenever one has a new frame.
        let videos = snowlark_paint::video::VideoStore::new(Some(gpu.clone()));
        {
            let tx = comp.sender();
            videos.set_notify(Some(Arc::new(move |id| {
                let _ = tx.send(crate::composite::CompMsg::VideoFrame(id).into());
            })));
        }
        comp.send(crate::composite::CompInput::videos(videos.clone()));
        // (the profile's protected players' pictures, shown under its pages' videos)
        comp.send(crate::composite::CompInput::Protected(
            partition.cdm.pictures(),
        ));
        let (events_tx, events) = snowlark_ipc::channel();
        let settings = profile.settings();
        let dark = settings.theme.is_dark(window::system_uses_dark_theme());
        let internal = Arc::new(std::sync::Mutex::new(settings_ui::InternalState {
            settings: settings.clone(),
            dark,
            notice: None,
            bookmarks: Default::default(),
            bookmark_toast: None,
            // (the regular windows' one list; an Incognito window's files are kept, its list of
            // them only while it is open)
            downloads: app.downloads(off_the_record),
            off_the_record,
        }));
        // (content blocking: the windows' one, `App::new`)
        let blocker = app.blocker.clone();
        let history = partition.history.clone();
        let pages = settings_ui::internal_pages(
            internal.clone(),
            history.clone(),
            net.clone(),
            blocker.clone(),
        );
        let tab_ctx = TabContext {
            gpu,
            videos: Some(videos),
            net,
            compositor: comp.sender(),
            gpu_raster: comp.gpu_raster(),
            events: Arc::new(WindowSink {
                tx: events_tx,
                window: window.proxy(),
            }),
            clipboard: Arc::new(WindowClipboard(window.handle())),
            internal: pages,
            content_filter: None,
            // (each tab gets its own: Browser::audio_group)
            audio: None,
            // (AudioContexts play on the default output device, from any process model)
            audio_out: snowlark_media::audio_outputs(),
            // (the partition's: its tabs' channels, storage and databases)
            broadcast: partition.broadcast.clone(),
            web_storage: partition.local_storage.host(),
            idb: partition.idb.clone(),
            // (the partition's key systems: each origin's state in its store)
            cdm: Some(partition.cdm.clone() as Arc<dyn snowlark_page::media::cdm::Cdm>),
        };
        let mut bookmarks = Bookmarks::load(
            &profile.bookmarks_file(),
            profile.legacy_bookmarks().as_deref(),
        );
        if off_the_record {
            // (the regular window owns the file: an Incognito window shows the bookmarks as they
            // were when it opened, and keeps its own changes in memory)
            bookmarks = bookmarks.snapshot();
        }
        let downloads = {
            let mut st = internal.lock().unwrap_or_else(|e| e.into_inner());
            st.bookmarks = bookmarks.snapshot();
            st.downloads.clone()
        };
        window.set_dark_frame(dark || off_the_record);
        let dock = Dock::from_name(&settings.devtools_dock);
        // (what the regular windows shared before this one opened: it read the profile's files,
        // which hold it)
        let shared_seen = app.shared.borrow().seen();
        let closed = if off_the_record {
            std::rc::Rc::default()
        } else {
            app.closed.clone()
        };
        let bookmarks_published = bookmarks.version();
        let mut b = Browser {
            app,
            partition,
            incognito_shown: 0,
            blocker,
            comp,
            windows,
            fonts: Fonts::new(text),
            images,
            events,
            tab_ctx,
            process_model,
            code_cache,
            spares,
            gpu_process,
            tabs: TabList::default(),
            next_id: 1,
            history,
            downloads,
            profile: profile.clone(),
            settings,
            bookmarks,
            shared_seen: std::cell::Cell::new(shared_seen),
            bookmarks_published: std::cell::Cell::new(bookmarks_published),
            bookmark_icons: HashMap::new(),
            bar_widths: RefCell::new(None),
            size: window.client_size(),
            dpi: window_dpi(window),
            placement_px: Some(window.screen_placement()),
            maximized: window.is_maximized(),
            window_active: true,
            dark,
            hover: Hit::None,
            pressed: Hit::None,
            caption_hover: None,
            focus: Focus::Page,
            address: TextField::default(),
            address_scroll: 0.0,
            side_scroll: 0.0,
            side_revealed: None,
            side_list: None,
            side_resize: None,
            tab_drag: None,
            address_drag: false,
            caret_on: true,
            suggestions: Vec::new(),
            selected: None,
            hover_suggestion: None,
            spinner: 0.0,
            spinning: false,
            closed,
            mouse: Point::ZERO,
            page_hover: false,
            drag_in: None,
            scrollbar_drag: None,
            scrollbar_hover: false,
            wheel_lines: window::wheel_scroll_lines(),
            chrome_dirty: true,
            overlay_dirty: true,
            sent_cursor: Cursor::Arrow,
            menu: None,
            swallow_release: false,
            title: None,
            tooltip: None,
            tab_card: None,
            sad_hover: false,
            popup: None,
            dialog: None,
            hang_snooze: None,
            bm_drag: None,
            devtools_open: false,
            dock,
            dock_sizes: (320.0, 480.0, 480.0),
            dock_menu: None,
            attached: None,
            splitter_drag: None,
            panel_hover: false,
            me: std::rc::Weak::new(),
            main_proxy: window.proxy(),
            dt_window: None,
            dt_placement: None,
            dt_popup: false,
            deferred: Vec::new(),
            site_info: None,
            blocker_panel: None,
            task_manager: None,
            session: None,
            autoscroll: None,
            autoscroll_pending: None,
            placed: None,
            offer: None,
            restore_bubble: None,
            fullscreen: Default::default(),
            internal,
            settings_notice: None,
            find_last: String::new(),
            popup_window: false,
            popups: Vec::new(),
            current: None,
            next_popup: 1,
            main_opens: Vec::new(),
        };
        b.comp.send(CompMsg::Dark(b.chrome_dark()));
        b.apply_user_agent();
        // (every regular window keeps its part of the session file; only the first one offers a
        // restore)
        if !off_the_record {
            b.start_session();
        }
        let url = start.unwrap_or_else(|| b.startup_url());
        b.open_tab(url, Placement::End);
        if b.active_url() == START_PAGE {
            b.focus_address();
        }
        window.set_timer(TIMER_SAVE, 3000);
        window.set_timer(TIMER_SESSION, 1000);
        window.set_timer(TIMER_HANG, 1000);
        b
    }

    /// The active tab's find box, when it shows.
    fn find_box(&self) -> Option<&FindBar> {
        self.tabs
            .active()
            .filter(|e| e.crashed.is_none())
            .and_then(|e| e.find.as_ref())
    }

    fn home_url(&self) -> Url {
        self.settings
            .homepage
            .as_deref()
            .filter(|h| !h.trim().is_empty())
            .and_then(interpret)
            .unwrap_or_else(|| Url::parse(START_PAGE).unwrap())
    }

    /// What a new window opens.
    fn startup_url(&self) -> Url {
        self.settings
            .startup_address()
            .and_then(interpret)
            .unwrap_or_else(|| Url::parse(START_PAGE).unwrap())
    }

    /// The chrome's height (DIPs): the tab strip, the toolbar and the bookmarks bar when it shows.
    fn chrome_h(&self) -> f32 {
        // (fullscreen: no chrome at all)
        if self.fullscreen.on() {
            return 0.0;
        }
        if self.popup_window {
            return chrome::POPUP_BAR_HEIGHT;
        }
        let top = if self.side_tabs_on() {
            chrome::TOOLBAR_HEIGHT
        } else {
            chrome::CHROME_HEIGHT
        };
        top + if self.settings.bookmarks_bar {
            chrome::BOOKMARKS_BAR_HEIGHT
        } else {
            0.0
        }
    }

    fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    fn width_dips(&self) -> f32 {
        self.size.0 as f32 / self.scale()
    }

    fn chrome_px(&self) -> u32 {
        (self.chrome_h() * self.scale()).round() as u32
    }

    /// The content area in DIPs.
    fn content_dips(&self) -> Size {
        let s = self.scale();
        Size::new(
            self.size.0 as f32 / s,
            self.size.1.saturating_sub(self.chrome_px()) as f32 / s,
        )
    }

    fn device_for(&self, zoom: f32) -> Device {
        let page = self.page_dips();
        Device {
            viewport: Size::new((page.width / zoom).max(1.0), (page.height / zoom).max(1.0)),
            scale: self.scale() * zoom,
            // (the bar is as wide in DIPs at every zoom: narrower in the zoomed page's CSS px)
            scrollbar: SCROLLBAR_WIDTH / zoom,
            screen: screen_placement(
                self.placement_px,
                (
                    (self.page_rect().x * self.scale()).round() as u32,
                    self.chrome_px(),
                ),
                self.scale(),
                zoom,
            ),
        }
    }

    /// Every tab gets its device anew (the window moved: the viewports' places on the screen).
    fn send_devices(&mut self) {
        let devices: Vec<(usize, Device)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, e)| (i, self.device_for(e.zoom)))
            .collect();
        for (i, d) in devices {
            if let Some(e) = self.tabs.get(i) {
                e.tab.send(TabCommand::Device(d));
            }
        }
    }

    fn layout(&self) -> Layout {
        let zoom = self.active_zoom_label();
        let zw = if zoom.is_empty() {
            0.0
        } else {
            self.fonts.width(&zoom, 12.0) + 8.0
        };
        let widths = self.settings.bookmarks_bar.then(|| self.bar_item_widths());
        let audio = (0..self.tabs.len())
            .map(|i| self.tab_audio(i) != crate::audio::TabAudio::None)
            .collect();
        let (w, n) = (self.width_dips(), self.tabs.len());
        if self.popup_window {
            return Layout::popup(w).with_fullscreen(self.fullscreen.on());
        }
        let layout = if self.side_tabs_on() {
            Layout::with_side_tabs(w, n, zw, widths.as_deref(), &self.side_spec())
        } else {
            Layout::new(w, n, zw, widths.as_deref())
        };
        let layout = match self.incognito_label() {
            Some(label) => {
                layout.with_incognito(chrome::incognito_button_width(&self.fonts, &label))
            }
            None => layout,
        };
        layout
            .with_audio(audio)
            .with_maximized(self.maximized)
            .with_fullscreen(self.fullscreen.on())
    }

    fn active_zoom_label(&self) -> String {
        match self.tabs.active() {
            Some(e) if (e.zoom - 1.0).abs() > 0.001 => {
                chrome::zoom_label(Some((e.zoom * 100.0).round() as u32))
            }
            _ => String::new(),
        }
    }

    fn active_url(&self) -> String {
        self.tabs
            .active()
            .map(|e| e.tab.state.url.clone())
            .unwrap_or_default()
    }

    fn active_id(&self) -> Option<TabId> {
        self.tabs.active().map(|e| e.tab.id)
    }

    fn send_active(&self, c: TabCommand) {
        if let Some(e) = self.tabs.active().filter(|e| e.crashed.is_none()) {
            e.tab.send(c);
        }
    }

    /// The user's Stop of the active tab's load (a navigation the host has not committed too).
    fn stop_active(&mut self) {
        if let Some(e) = self.tabs.active_mut().filter(|e| e.crashed.is_none()) {
            e.tab.stop();
        }
        self.chrome_dirty = true;
    }

    /// The active tab's host, unless the tab crashed.
    fn active_host(&mut self) -> Option<&mut TabHost> {
        self.tabs
            .active_mut()
            .filter(|e| e.crashed.is_none())
            .map(|e| &mut e.tab)
    }

    /// Back (−1) or forward (+1) in the active tab's session history (from a crashed tab, on a new
    /// thread).
    fn go_active(&mut self, delta: isize) {
        if self.active_crashed() {
            if self.tabs.active().is_some_and(|e| e.tab.can_go(delta)) {
                self.restart_tab(self.tabs.active_index(), Restart::Go(delta));
            }
            return;
        }
        if let Some(tab) = self.active_host() {
            tab.go(delta);
        }
    }

    fn active_crashed(&self) -> bool {
        self.tabs.active().is_some_and(|e| e.crashed.is_some())
    }

    /// Starts a new thread for the crashed tab at `i`, which then does `how`; the tab keeps its
    /// place, zoom and icon, and its session history.
    fn restart_tab(&mut self, i: usize, how: Restart) {
        self.renew_renderer(i, |tab, id, ctx, device, renderer| {
            tab.respawn(id, ctx, device, renderer);
            match how {
                // (a tab that crashed before it had a history shows what it was asked to)
                Restart::Reload if tab.history_len() == 0 => {
                    if let Ok(url) = Url::parse(&tab.state.url) {
                        tab.navigate(url);
                    }
                }
                Restart::Reload => tab.reload(),
                Restart::Navigate(url) => tab.navigate(url),
                Restart::Go(delta) => tab.go(delta),
            }
        });
    }

    /// A navigation of the tab at `i` is to another site (M5 step 2, [`TabHost::needs_swap`]):
    /// it commits in a new renderer, started as after a crash, and the old one closes with its
    /// frames.
    fn swap_tab(&mut self, i: usize) {
        let Some(old) = self.tabs.get(i).map(|e| e.tab.id) else {
            return;
        };
        self.renew_renderer(i, |tab, id, ctx, device, renderer| {
            tab.swap_renderer(id, ctx, device, renderer);
        });
        self.comp.send(CompMsg::RemoveTab(old));
        // (the icon was the old site's page's; the new document reports its own)
        if let Some(e) = self.tabs.get_mut(i) {
            if let Some(f) = e.favicon.take() {
                self.images.remove(f);
            }
            e.icon_bytes = None;
        }
    }

    /// A new renderer for the tab at `i`, which `start` gives the host (with the tab's new id,
    /// context, device and renderer); the tab keeps its place, zoom and icon, and its session
    /// history. What the old renderer said about itself goes.
    fn renew_renderer(
        &mut self,
        i: usize,
        start: impl FnOnce(&mut TabHost, TabId, TabContext, Device, Renderer),
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let zoom = self.tabs.get(i).map_or(1.0, |e| e.zoom);
        let device = self.device_for(zoom);
        let (mut ctx, filter) = self.new_tab_context(id);
        let muted = self.tabs.get(i).is_some_and(|e| e.mute.muted);
        let renderer = self.renderer_for(id, &mut ctx, muted);
        let active = i == self.tabs.active_index();
        let dark = self.dark;
        let Some(e) = self.tabs.get_mut(i) else {
            return;
        };
        start(&mut e.tab, id, ctx, device, renderer);
        let audio = e.tab.audio();
        let audio = audio.unwrap_or_else(|| self.audio_group(id, muted) as Arc<dyn TabMuteControl>);
        let Some(e) = self.tabs.get_mut(i) else {
            return;
        };
        e.tab.send(TabCommand::ColorScheme(dark));
        e.tab.send(TabCommand::Shown(active));
        e.filter = filter;
        e.audio = audio;
        e.indicator = Default::default();
        e.crashed = None;
        // (the new thread has no search: the box goes)
        e.find = None;
        e.tab.state = TabState {
            url: e.tab.current_url().map(Url::to_string).unwrap_or_default(),
            loading: true,
            ..Default::default()
        };
        e.link = None;
        e.cursor = Cursor::Arrow;
        if active {
            self.comp.send(CompMsg::Activate(id));
            self.sad_hover = false;
            // (the new thread follows the tools if they show)
            self.attached = None;
            self.attach_devtools();
        }
        self.chrome_dirty = true;
        self.overlay_dirty = true;
    }

    /// What runs tab `id`'s renderer: a thread, with its players' group in `ctx`, or a process of its
    /// own (`--process-model=tab`), which keeps the players itself.
    fn renderer_for(&self, id: TabId, ctx: &mut TabContext, muted: bool) -> Renderer {
        match (self.process_model, &self.code_cache) {
            (ProcessModel::Tab, Some(code_cache)) => Renderer::Process(RemoteTab {
                launch: crate::tab_host::renderer_launch(Some(id), &self.profile.crash_dumps_dir()),
                code_cache: code_cache.clone(),
                muted,
                // (a tab a crash switch names starts its own renderer, which the switch reaches)
                spare: self
                    .spares
                    .as_ref()
                    .filter(|_| !crate::tab_host::crash_switch_names(id))
                    .and_then(|pool| pool.take()),
            }),
            _ => {
                ctx.audio = Some(self.audio_group(id, muted));
                Renderer::Thread(PageDriver::factory())
            }
        }
    }

    /// Reloads the active tab (a crashed one starts again).
    fn reload_active(&mut self) {
        if self.active_crashed() {
            self.restart_tab(self.tabs.active_index(), Restart::Reload);
        } else {
            if let Some(tab) = self.active_host() {
                tab.reload();
            }
        }
    }

    /// A tab on its own thread showing `url`; a restored one (`lazy`) shows its saved title and
    /// loads its history when first shown.
    fn make_tab(&mut self, url: Url, lazy: Option<SavedTab>) -> TabEntry {
        let zoom = lazy.as_ref().map_or(1.0, |t| t.zoom);
        let id = self.next_id;
        self.next_id += 1;
        let device = self.device_for(zoom);
        let (mut ctx, filter) = self.new_tab_context(id);
        let mute = match &lazy {
            Some(t) => crate::audio::TabMute {
                muted: t.muted,
                by_site: false,
            },
            None => self.initial_mute(url.as_str()),
        };
        let renderer = self.renderer_for(id, &mut ctx, mute.muted);
        let scroll = self.comp.scroll_table();
        let mut tab = if lazy.is_some() {
            TabHost::spawn_idle(id, ctx, scroll, device, renderer)
        } else {
            TabHost::spawn(id, ctx, scroll, device, url.clone(), renderer)
        };
        // (a renderer process that did not start has none: the tab shows as crashed)
        let audio = tab
            .audio()
            .unwrap_or_else(|| self.audio_group(id, mute.muted) as Arc<dyn TabMuteControl>);
        tab.send(TabCommand::ColorScheme(self.dark));
        tab.state = match &lazy {
            Some(t) => TabState {
                url: t.current().map(|e| e.url.clone()).unwrap_or_default(),
                title: t.current().map(|e| e.title.clone()).unwrap_or_default(),
                ..Default::default()
            },
            None => TabState {
                url: url.to_string(),
                loading: true,
                ..Default::default()
            },
        };
        TabEntry {
            tab,
            favicon: None,
            link: None,
            cursor: Cursor::Arrow,
            zoom,
            crashed: None,
            icon_bytes: None,
            dt: Box::default(),
            highlight: None,
            filter,
            audio,
            indicator: Default::default(),
            mute,
            find: None,
            lazy: lazy.map(Box::new),
        }
    }

    fn open_tab(&mut self, url: Url, placement: Placement) {
        if self.popup_window {
            self.open_in_main_window(url, placement);
            return;
        }
        let entry = self.make_tab(url, None);
        let before = self.active_id();
        self.tabs.add(entry, placement);
        if self.active_id() != before || before.is_none() {
            self.activated();
        } else {
            // (a background tab)
            self.send_shown();
        }
        self.chrome_dirty = true;
    }

    /// Tab `opener`'s page opened window `window` with an opener: a new tab in front, next to it,
    /// shows it as a view of that page ([`TabHost::view`]), in the opener's renderer.
    fn open_window(&mut self, opener: usize, window: u64) {
        let Some(o) = self.tabs.get(opener) else {
            return;
        };
        let id = self.next_id;
        self.next_id += 1;
        let zoom = o.zoom;
        let device = self.device_for(zoom);
        let scroll = self.comp.scroll_table();
        let tab = TabHost::view(id, &o.tab, window, &self.tab_ctx.compositor, scroll, device);
        let entry = self.view_entry(opener, tab);
        // (it opens from the active tab, as the user's activation it needs was there)
        if self.tabs.active_index() != opener {
            self.tabs.select(opener);
        }
        self.tabs.add(entry, Placement::Foreground);
        self.activated();
        self.chrome_dirty = true;
    }

    /// The window's entry for `tab`, a view of tab `opener`'s page.
    fn view_entry(&self, opener: usize, tab: TabHost) -> TabEntry {
        let o = self.tabs.get(opener).expect("the opener's tab");
        let (zoom, audio, mute) = (o.zoom, o.audio.clone(), o.mute);
        TabEntry {
            tab,
            favicon: None,
            link: None,
            cursor: Cursor::Arrow,
            zoom,
            crashed: None,
            icon_bytes: None,
            dt: Box::default(),
            highlight: None,
            // (what the window's documents load is blocked by the opener's filter, and counted
            // there)
            filter: None,
            audio,
            indicator: Default::default(),
            mute,
            find: None,
            lazy: None,
        }
    }

    /// Tells each tab whether the window shows it (the active one).
    fn send_shown(&self) {
        let active = self.tabs.active_index();
        for (i, e) in self.tabs.iter().enumerate() {
            e.tab.send(TabCommand::Shown(i == active));
        }
    }

    fn close_tab(&mut self, i: usize, window: &Window) {
        let was_active = i == self.tabs.active_index();
        // The undocked tools close with the page they inspect
        // (`DevToolsWindow::InspectedContentsClosing`).
        if self.dock == Dock::Undocked
            && self.attached.is_some()
            && self.tabs.get(i).map(|e| e.tab.id) == self.attached
        {
            self.close_devtools();
        }
        if let Some(e) = self.tabs.remove(i) {
            self.comp.send(CompMsg::RemoveTab(e.tab.id));
            if let Some(f) = e.favicon {
                self.images.remove(f);
            }
            if !e.tab.state.url.is_empty() && e.tab.state.url != START_PAGE {
                self.closed.borrow_mut().push(e.tab.state.url.clone());
            }
        }
        if self.tabs.is_empty() {
            window.close();
            return;
        }
        if was_active {
            self.activated();
        }
        self.chrome_dirty = true;
    }

    /// Closes a select's list, sending the option chosen (if any) to its tab.
    fn close_menu(&mut self, choice: Option<usize>) {
        if let Some(m) = self.menu.take() {
            if let Some(i) = choice {
                if let Some(e) = self.tabs.iter().find(|e| e.tab.id == m.tab) {
                    e.tab.send(TabCommand::SelectChosen(m.popup.select, i));
                }
            }
            self.overlay_dirty = true;
        }
    }

    /// Hides the tooltip (and forgets the title waiting to show) until the pointer reaches another.
    fn hide_tooltip(&mut self, window: &Window) {
        window.kill_timer(TIMER_TOOLTIP);
        window.kill_timer(TIMER_CARD);
        let tip = self.tooltip.take().is_some();
        let card = self.tab_card.take().is_some();
        if tip || card {
            self.overlay_dirty = true;
        }
    }

    /// A content-area DIP point of a window DIP point.
    fn content_point(&self, dip: Point) -> Point {
        Point::new(dip.x, dip.y - self.chrome_h())
    }

    /// The active tab changed.
    fn activated(&mut self) {
        self.end_autoscroll();
        self.load_lazy();
        self.close_menu(None);
        self.close_blocker_panel();
        self.attach_devtools();
        if let Some(id) = self.active_id() {
            self.comp.send(CompMsg::Activate(id));
        }
        self.send_shown();
        self.blur_address();
        if self.active_url() == START_PAGE {
            self.focus_address();
        }
        self.chrome_dirty = true;
        self.overlay_dirty = true;
    }

    fn sync_address(&mut self) {
        if self.focus != Focus::Address {
            let text = display_url(&self.active_url());
            if self.address.text() != text {
                self.address.set_text(&text);
            }
        }
    }

    fn focus_address(&mut self) {
        // (a popup window's location line is read-only)
        if self.popup_window {
            return;
        }
        self.focus = Focus::Address;
        let text = display_url(&self.active_url());
        self.address.set_text(&text);
        self.address.select_all();
        self.caret_on = true;
        self.suggestions.clear();
        self.selected = None;
        self.chrome_dirty = true;
        self.overlay_dirty = true;
    }

    fn blur_address(&mut self) {
        self.focus = Focus::Page;
        self.suggestions.clear();
        self.selected = None;
        self.address_drag = false;
        self.sync_address();
        self.chrome_dirty = true;
        self.overlay_dirty = true;
    }

    fn update_suggestions(&mut self) {
        let typed = self.address.typed_text().trim().to_owned();
        self.selected = None;
        self.suggestions.clear();
        if typed.is_empty() {
            self.overlay_dirty = true;
            return;
        }
        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        for e in history.suggest(&typed, 5) {
            self.suggestions.push(Suggestion {
                search: false,
                title: if e.title.is_empty() {
                    crate::history::stripped(&e.url).to_owned()
                } else {
                    e.title.clone()
                },
                url: e.url.clone(),
            });
        }
        self.suggestions.push(Suggestion {
            search: true,
            title: typed.clone(),
            url: format!("{} search", self.settings.engine().name),
        });
        self.overlay_dirty = true;
    }

    fn navigate_active(&mut self, url: Url) {
        if self.active_crashed() {
            let i = self.tabs.active_index();
            self.restart_tab(i, Restart::Navigate(url));
            self.blur_address();
            return;
        }
        if let Some(tab) = self.active_host() {
            tab.navigate(url.clone());
        }
        if let Some(e) = self.tabs.active_mut() {
            e.tab.state.url = url.to_string();
            e.tab.state.loading = true;
        }
        self.blur_address();
    }

    fn submit_address(&mut self) {
        let url = match self.selected.and_then(|i| self.suggestions.get(i)) {
            Some(s) if s.search => Some(search_url_with(self.settings.engine().prefix, &s.title)),
            Some(s) => Url::parse(&s.url).ok(),
            None => interpret_with(self.address.text(), self.settings.engine().prefix),
        };
        if let Some(url) = url {
            self.navigate_active(url);
        }
    }

    fn set_zoom(&mut self, zoom: f32) {
        let device = self.device_for(zoom);
        if let Some(e) = self.tabs.active_mut() {
            if (e.zoom - zoom).abs() < 0.001 {
                return;
            }
            e.zoom = zoom;
            e.tab.send(TabCommand::Device(device));
        }
        self.chrome_dirty = true;
    }

    /// The window has a new size: the compositor resizes its layers and presents a frame of the new
    /// size (old page tiles over the page background, the chrome laid out anew) before this returns,
    /// so DWM never shows the window at a size its content doesn't have yet (that showed as strips
    /// of whatever was behind the window while growing it). The tabs lay out again on their own
    /// time. The wait is bounded; the compositor never waits for a tab.
    fn resized(&mut self) {
        let s = self.scale();
        let area = self.content_dips();
        let chrome = self.chrome_list();
        let overlay = self.overlay_list();
        // (the wait ends early when the compositor has gone)
        self.comp.send_and_wait(
            CompMsg::Resize {
                width: self.size.0,
                height: self.size.1,
                chrome_height: self.chrome_px(),
                dpi_scale: s,
                chrome: Some(chrome),
                overlay: Some(overlay),
            },
            RESIZE_WAIT,
        );
        self.close_menu(None);
        self.close_popup();
        self.close_site_info();
        self.close_blocker_panel();
        self.dock_menu = None;
        if let Some(b) = &mut self.restore_bubble {
            b.place(area);
        }
        if let Some((m, _)) = &mut self.task_manager {
            m.place(area);
        }
        if let Some((d, _)) = &mut self.dialog {
            d.place(None, area, &self.fonts);
        }
        self.comp.send(CompMsg::PageArea(self.page_px()));
        let devices: Vec<(usize, Device)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, e)| (i, self.device_for(e.zoom)))
            .collect();
        for (i, d) in devices {
            if let Some(e) = self.tabs.get(i) {
                e.tab.send(TabCommand::Device(d));
            }
        }
        self.chrome_dirty = false;
        self.overlay_dirty = false;
        self.fullscreen_after_resize();
    }

    fn drain_tab_events(&mut self, window: &Window) {
        for (id, ev) in self.take_tab_events() {
            let Some(i) = self.tabs.position(|e| e.tab.id == id) else {
                continue;
            };
            // (nothing from a crashed tab's thread is trusted)
            if self.tabs.get(i).is_some_and(|e| e.crashed.is_some()) {
                continue;
            }
            let active = i == self.tabs.active_index();
            match ev {
                TabEvent::DragOperation(op) => {
                    if active {
                        self.drag_operation(window, op);
                    }
                }
                TabEvent::State(s) => {
                    let e = self.tabs.get_mut(i).unwrap();
                    if e.lazy.is_some() {
                        // (a restored tab that has not loaded: it shows what it was)
                        continue;
                    }
                    let url_changed = e.tab.state.url != s.url;
                    let title_changed = e.tab.state.title != s.title;
                    if url_changed {
                        let old_host = Url::parse(&e.tab.state.url)
                            .ok()
                            .and_then(|u| u.host_str().map(str::to_owned));
                        let new_host = Url::parse(&s.url)
                            .ok()
                            .and_then(|u| u.host_str().map(str::to_owned));
                        if old_host != new_host {
                            if let Some(f) = e.favicon.take() {
                                self.images.remove(f);
                            }
                            e.icon_bytes = None;
                        }
                    }
                    if self.partition.is_off_the_record() {
                        // (an Incognito window records no history)
                    } else if !s.loading && (url_changed || e.tab.state.loading) {
                        self.history
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .visit(&s.url, &s.title, url_changed || e.tab.state.loading);
                    } else if title_changed {
                        self.history
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .visit(&s.url, &s.title, false);
                    }
                    let url = url_changed.then(|| s.url.clone());
                    e.tab.page_state(s);
                    if let Some(url) = url {
                        if self.autoscroll.is_some_and(|s| s.tab == id) {
                            self.end_autoscroll();
                        }
                        self.tab_navigated_audio(i, &url);
                    }
                    if active {
                        self.sync_address();
                        self.overlay_dirty = true;
                        self.blocker_changed();
                    }
                    self.chrome_dirty = true;
                }
                TabEvent::Committed { url, scroll } => {
                    // (a page's fullscreen ends with its document)
                    if self.fullscreen.tab == Some(id) {
                        self.exit_tab_fullscreen(window);
                    }
                    self.tabs.get_mut(i).unwrap().tab.committed(url, scroll);
                    self.chrome_dirty = true;
                }
                TabEvent::DocumentUrl { url, navigation } => {
                    let tab = &mut self.tabs.get_mut(i).unwrap().tab;
                    tab.document_url(url, navigation);
                }
                // M5: navigation in the browser (`tab_host.rs`).
                TabEvent::NavigationReady(number) => {
                    let tab = &mut self.tabs.get_mut(i).unwrap().tab;
                    tab.navigation_ready(number);
                    if let Some(download) = tab.take_download() {
                        let tab_url = tab.state.url.clone();
                        self.save_download(window, download, tab_url);
                    } else if tab.needs_swap() {
                        self.swap_tab(i);
                    }
                    if active {
                        self.sync_address();
                    }
                    self.chrome_dirty = true;
                }
                TabEvent::BeginNavigation(begin) => {
                    let tab = &mut self.tabs.get_mut(i).unwrap().tab;
                    tab.begin_navigation(*begin);
                    if active {
                        self.sync_address();
                    }
                    self.chrome_dirty = true;
                }
                TabEvent::SameDocument { change, navigation } => {
                    let tab = &mut self.tabs.get_mut(i).unwrap().tab;
                    tab.same_document(change, navigation);
                    if active {
                        self.sync_address();
                    }
                    self.chrome_dirty = true;
                }
                TabEvent::Fullscreen(on) => self.tab_asks_fullscreen(window, id, on),
                TabEvent::Traverse(steps) => {
                    let tab = &mut self.tabs.get_mut(i).unwrap().tab;
                    tab.go(steps);
                    if active {
                        self.sync_address();
                    }
                    self.chrome_dirty = true;
                }
                TabEvent::Audible(audible) => self.tab_audible(window, i, audible),
                TabEvent::Memory(m) => {
                    self.task_manager_memory(id, &m);
                    self.tab_memory(id, m);
                }
                TabEvent::Blocked => {
                    if active {
                        self.blocker_changed();
                    }
                }
                TabEvent::Hover {
                    cursor,
                    link,
                    title,
                    press,
                } => {
                    if let Some(press) = press {
                        self.autoscroll_answer(window, id, press);
                    }
                    if active && title != self.title {
                        // A new title: the old tooltip goes, the new one waits for the delay.
                        self.title = title.clone();
                        self.hide_tooltip(window);
                        if title.is_some() && self.page_hover && self.menu.is_none() {
                            window.set_timer(TIMER_TOOLTIP, TOOLTIP_DELAY_MS);
                        }
                    }
                    let e = self.tabs.get_mut(i).unwrap();
                    e.cursor = cursor;
                    if e.link != link {
                        e.link = link;
                        if active {
                            self.overlay_dirty = true;
                        }
                    }
                    if active
                        && self.page_hover
                        && self.scrollbar_drag.is_none()
                        && !self.scrollbar_hover
                        && self.autoscroll.is_none()
                    {
                        window.set_cursor(cursor);
                        self.sent_cursor = cursor;
                    }
                }
                TabEvent::Open(url, d) => {
                    // (a web page doesn't open the browser's own pages)
                    let from_internal = self
                        .tabs
                        .get(i)
                        .is_some_and(|e| e.tab.state.url.starts_with("snowlark:"));
                    if matches!(url.scheme(), "snowlark" | "view-source") && !from_internal {
                        continue;
                    }
                    let placement = match d {
                        Disposition::NewForegroundTab => Placement::Foreground,
                        _ => Placement::Background,
                    };
                    self.open_tab(url, placement);
                }
                TabEvent::SelectPopup(p) => {
                    if active {
                        let zoom = self.tabs.get(i).map_or(1.0, |e| e.zoom);
                        let area = self.page_dips();
                        let mut menu = SelectMenu::new(id, *p, zoom, area, &self.fonts);
                        menu.rect.x += self.page_rect().x;
                        self.menu = Some(menu);
                        self.overlay_dirty = true;
                    }
                }
                TabEvent::Favicon(icon, bytes) => {
                    let id = self.images.add(icon.into_image());
                    let e = self.tabs.get_mut(i).unwrap();
                    e.icon_bytes = Some(Arc::new(bytes));
                    if let Some(old) = e.favicon.replace(id) {
                        self.images.remove(old);
                    }
                    self.chrome_dirty = true;
                }
                TabEvent::OpenWindow {
                    window,
                    popup,
                    size,
                    position,
                } => {
                    // (a popup window of its own; a window opened from a popup window is one
                    // too, as it has no tab strip to show it in)
                    if popup || self.popup_window {
                        self.open_popup_window(i, window, size, position);
                    } else {
                        self.open_window(i, window);
                    }
                }
                TabEvent::CloseWindow => {
                    // (only a view's window closes it: a page's own tab is the user's)
                    if self.tabs.get(i).is_some_and(|e| e.tab.is_view()) {
                        self.close_tab(i, window);
                    }
                }
                TabEvent::WindowRect(x, y, w, h) => {
                    if self.tabs.get(i).is_some_and(|e| e.tab.is_view()) {
                        self.move_popup_window(window, (x, y, w, h));
                    }
                }
                TabEvent::Devtools(ev) => self.devtools_event(i, *ev),
                TabEvent::Find(status) => self.find_status(i, status),
                TabEvent::ContextMenu(info) => self.show_context_menu(i, *info),
                TabEvent::Internal(crate::driver::InternalRequest::Settings(q)) => {
                    self.apply_settings(window, &q)
                }
                TabEvent::Internal(crate::driver::InternalRequest::History(q)) => {
                    self.apply_history(&q)
                }
                TabEvent::Internal(crate::driver::InternalRequest::Bookmarks(q)) => {
                    self.apply_bookmarks(&q)
                }
                TabEvent::Internal(crate::driver::InternalRequest::Downloads(q)) => {
                    self.apply_downloads(window, &q)
                }
                TabEvent::Crashed(message) => {
                    if self.autoscroll.is_some_and(|s| s.tab == id) {
                        self.end_autoscroll();
                    }
                    let e = self.tabs.get_mut(i).unwrap();
                    e.tab.crashed();
                    e.crashed = Some(SadTab { message });
                    // (its players went with it; the next thread hears of the tab's mute)
                    e.indicator = Default::default();
                    e.dt.reset();
                    e.highlight = None;
                    e.find = None;
                    // (what the tab said about itself goes: its scrollers, its link, its cursor)
                    e.tab.state.loading = false;
                    e.tab.state.scrollers.clear();
                    e.tab.state.error_page = true;
                    e.tab.state.title = "This tab crashed".into();
                    e.link = None;
                    e.cursor = Cursor::Arrow;
                    // The compositor drops the tab's frames.
                    self.comp.send(CompMsg::RemoveTab(id));
                    if self.menu.as_ref().is_some_and(|m| m.tab == id) {
                        self.close_menu(None);
                    }
                    if active {
                        if self.focus == Focus::Find {
                            self.focus = Focus::Page;
                        }
                        self.comp.send(CompMsg::Activate(id));
                        self.title = None;
                        self.hide_tooltip(window);
                        self.overlay_dirty = true;
                    }
                    self.chrome_dirty = true;
                }
            }
        }
    }

    fn update_spinner(&mut self, window: &Window) {
        let loading = self.tabs.iter().any(|e| e.tab.state.loading);
        if loading != self.spinning {
            self.spinning = loading;
            if loading {
                window.set_timer(TIMER_SPINNER, 33);
            } else {
                window.kill_timer(TIMER_SPINNER);
            }
        }
    }

    fn flush(&mut self, window: &Window) {
        self.fullscreen_sync(window);
        self.update_spinner(window);
        self.reveal_active_side_tab();
        if self.chrome_dirty {
            self.chrome_dirty = false;
            self.set_window_title(window);
            if !self.popup_window {
                self.sync_devtools_title();
            }
            let (list, bg) = self.chrome_list();
            self.comp.send(CompMsg::Chrome(list, bg));
        }
        if self.overlay_dirty {
            self.overlay_dirty = false;
            let list = self.overlay_list();
            self.comp.send(CompMsg::Overlay(list));
            self.draw_devtools_window();
        }
    }

    fn set_window_title(&self, window: &Window) {
        let title = self
            .tabs
            .active()
            .map(|e| e.tab.state.title.clone())
            .filter(|t| !t.is_empty());
        let app = if self.off_the_record() {
            "Snowlark (Incognito)"
        } else {
            "Snowlark"
        };
        window.set_title(&match title {
            Some(t) => format!("{t} - {app}"),
            None => app.into(),
        });
    }

    /// The chrome's display list and background for the current state and size.
    fn chrome_list(&mut self) -> (snowlark_paint::DisplayList, snowlark_paint::Color) {
        if self.settings.bookmarks_bar {
            self.ensure_bookmark_icons();
        }
        let layout = self.layout();
        // Keep the caret in view.
        let focused = self.focus == Focus::Address;
        if focused {
            let size = chrome::address_font_size();
            let cx = self
                .fonts
                .caret_x(self.address.text(), self.address.caret(), size);
            let room = layout.address_text.width;
            if cx - self.address_scroll > room {
                self.address_scroll = cx - room;
            }
            if cx - self.address_scroll < 0.0 {
                self.address_scroll = cx;
            }
        } else {
            self.address_scroll = 0.0;
        }
        let url = self.active_url();
        let error_page = self.tabs.active().is_some_and(|e| e.tab.state.error_page);
        let secure = if error_page {
            Some(false)
        } else if url.starts_with("https://") {
            Some(true)
        } else if url.starts_with("http://") || url.starts_with("file:") {
            Some(false)
        } else {
            None
        };
        let zoom = self
            .tabs
            .active()
            .filter(|e| (e.zoom - 1.0).abs() > 0.001)
            .map(|e| (e.zoom * 100.0).round() as u32);
        if self.off_the_record() {
            // (the count the Incognito button shows: a wake redraws it when it changes)
            self.incognito_shown = self.app.incognito_windows();
        }
        let active = self.tabs.active();
        let view = ChromeView {
            tabs: self
                .tabs
                .iter()
                .map(|e| chrome::TabView {
                    title: if e.tab.state.title.is_empty() {
                        if e.tab.state.url.starts_with("about:") {
                            ""
                        } else {
                            crate::history::stripped(&e.tab.state.url)
                        }
                    } else {
                        e.tab.state.title.as_str()
                    },
                    icon: e.favicon,
                    loading: e.tab.state.loading,
                    crashed: e.crashed.is_some(),
                    audio: e.indicator.view(e.mute.muted),
                })
                .collect(),
            active: self.tabs.active_index(),
            hover: self.hover,
            pressed: self.pressed,
            caption_hover: self.caption_hover,
            maximized: self.maximized,
            window_active: self.window_active,
            can_back: active.is_some_and(|e| e.tab.can_go(-1)),
            can_forward: active.is_some_and(|e| e.tab.can_go(1)),
            loading: active.is_some_and(|e| e.tab.state.loading),
            address: AddressView {
                field: &self.address,
                focused,
                caret_visible: self.caret_on,
                scroll: self.address_scroll,
                secure: if focused { None } else { secure },
                zoom,
            },
            spinner: self.spinner,
            theme: self.chrome_theme(),
            bar: if self.settings.bookmarks_bar {
                self.bar_views()
            } else {
                Vec::new()
            },
            starred: self.active_starred(),
            open: match (&self.popup, &self.dialog) {
                _ if self.site_info.is_some() => Hit::SiteInfo,
                _ if self.blocker_panel.is_some() => Hit::Shield,
                (Some((_, owner)), _) => *owner,
                (None, Some((_, commands::DialogKind::Bookmark { .. }))) => Hit::Star,
                _ => Hit::None,
            },
            bar_drop: self.bar_drop_view(),
            blocker: self.blocker_badge(),
            incognito: self.incognito_label(),
        };
        let list = chrome::build(&view, &layout, &self.fonts);
        let bg = view.theme.frame;
        let side = self.side_panel_list(&view, &layout);
        self.set_side_list(side);
        (list, bg)
    }

    /// The overlay's display list (status bubble, suggestions) for the current state and size.
    fn overlay_list(&self) -> snowlark_paint::DisplayList {
        let layout = self.layout();
        let status = self
            .tabs
            .active()
            .and_then(|e| e.link.as_ref())
            .filter(|_| self.page_hover)
            .map(|u| u.to_string());
        let view = OverlayView {
            status: status.as_deref(),
            suggestions: if self.focus == Focus::Address {
                &self.suggestions
            } else {
                &[]
            },
            selected: self.selected,
            hover: self.hover_suggestion,
            menu: self.menu.as_ref(),
            tooltip: self.tooltip.as_ref(),
        };
        let theme = self.chrome_theme();
        let area = self.page_dips();
        let overlay = chrome::build_overlay(&view, &layout, self.page_rect(), &self.fonts, &theme);
        let mut b = snowlark_paint::DisplayListBuilder::new();
        if let Some(side) = &self.side_list {
            b.append(side);
        }
        if let Some(sad) = self.tabs.active().and_then(|e| e.crashed.as_ref()) {
            // The crashed tab's page, under the rest of the overlay.
            // (right of a left-docked developer tools panel)
            b.push_transform(snowlark_paint::Matrix::translation(self.page_rect().x, 0.0));
            sad.paint(&mut b, area, &self.fonts, &theme, self.sad_hover);
            b.pop_transform();
        }
        self.paint_devtools(&mut b, &theme);
        b.append(&overlay);
        if let Some(s) = &self.site_info {
            s.paint(&mut b, &self.fonts, &theme);
        }
        if let Some(bar) = self.find_box() {
            bar.paint(
                &mut b,
                &self.fonts,
                &theme,
                area,
                self.focus == Focus::Find,
                self.caret_on,
            );
        }
        if let Some(p) = &self.blocker_panel {
            p.paint(&mut b, &self.fonts, &theme);
        }
        if let Some((m, _)) = &self.task_manager {
            m.paint(&mut b, &self.fonts, &theme);
        }
        self.paint_restore_bubble(&mut b, &theme);
        self.paint_fullscreen_bubble(&mut b);
        if let Some(c) = &self.tab_card {
            c.paint(&mut b, &self.fonts, &theme);
        }
        self.paint_autoscroll(&mut b);
        // (the tools' menus show in their own window when undocked)
        if let Some(m) = self
            .dock_menu
            .as_ref()
            .filter(|_| self.dock != Dock::Undocked)
        {
            m.paint(&mut b, &self.fonts, &theme);
        }
        if let Some((d, _)) = &self.dialog {
            d.paint(&mut b, &self.fonts, &theme, self.caret_on);
        }
        if let Some((m, _)) = self.popup.as_ref().filter(|_| !self.dt_popup) {
            m.paint(&mut b, &self.fonts, &theme);
        }
        b.build()
    }

    /// The viewport point in CSS px of a content-area DIP point.
    fn page_point(&self, dip: Point) -> Point {
        let zoom = self.tabs.active().map_or(1.0, |e| e.zoom);
        Point::new(
            (dip.x - self.page_rect().x) / zoom,
            (dip.y - self.chrome_h()) / zoom,
        )
    }

    fn in_scrollbar(&self, dip: Point) -> bool {
        let area = self.page_dips();
        let x = dip.x - self.page_rect().x;
        if dip.y < self.chrome_h()
            || dip.y >= self.chrome_h() + area.height
            || x < 0.0
            || x >= area.width
        {
            return false;
        }
        let Some(id) = self.active_id() else {
            return false;
        };
        let view = self.comp.scroll_view(id);
        x >= area.width - view.bar && viewport_scrollbar(&view, area).is_some()
    }

    fn suggestion_at(&self, dip: Point) -> Option<usize> {
        if self.focus != Focus::Address || self.suggestions.is_empty() {
            return None;
        }
        let r = chrome::suggestions_rect(&self.layout(), self.suggestions.len());
        let p = Point::new(dip.x, dip.y - self.chrome_h());
        if !r.contains(p) {
            return None;
        }
        let i = ((p.y - r.y - 4.0) / chrome::SUGGESTION_ROW).floor();
        (i >= 0.0 && (i as usize) < self.suggestions.len()).then_some(i as usize)
    }

    fn set_cursor(&mut self, window: &Window, c: Cursor) {
        if c != self.sent_cursor {
            window.set_cursor(c);
            self.sent_cursor = c;
        }
    }

    fn mouse_move(&mut self, window: &Window, dip: Point) {
        self.mouse = dip;
        self.autoscroll_moved(window, dip);
        if self.bm_drag.is_some() {
            self.bookmark_drag_move(window, dip);
            return;
        }
        if self.side_resize_move(dip) || self.side_drag_move(dip) {
            return;
        }
        let p = self.content_point(dip);
        if let Some((stack, _)) = self.popup.as_mut().filter(|_| !self.dt_popup) {
            if stack.contains(p) {
                let r = stack.hover(p);
                self.popup_result(window, r);
                self.set_cursor(window, Cursor::Arrow);
                return;
            }
        }
        if (self.dock != Dock::Undocked && self.dock_menu_move(p))
            || self.site_info_move(p)
            || self.blocker_panel_move(p)
            || self.restore_bubble_move(p)
            || self.task_manager_move(p)
        {
            self.set_cursor(window, Cursor::Arrow);
            return;
        }
        if let Some(hit) = self.find_move(p) {
            let c = if hit == crate::find_bar::FindHit::Field {
                Cursor::IBeam
            } else {
                Cursor::Arrow
            };
            self.set_cursor(window, c);
            if self.page_hover {
                self.page_hover = false;
                self.send_active(TabCommand::Pointer(Pointer::Leave));
            }
            return;
        }
        if let Some((d, _)) = &mut self.dialog {
            if d.dragging || d.rect.contains(p) {
                if d.mouse_move(p, &self.fonts) {
                    self.overlay_dirty = true;
                }
                let over_field = matches!(
                    d.hit(p, &self.fonts),
                    Some(crate::dialog::DialogHit::Field(i))
                        if matches!(d.fields.get(i), Some(crate::dialog::Field::Text { .. }))
                );
                let c = if over_field || d.dragging {
                    Cursor::IBeam
                } else {
                    Cursor::Arrow
                };
                self.set_cursor(window, c);
                return;
            }
        }
        if self.devtools_mouse_move(window, dip) {
            return;
        }
        if let Some(m) = &mut self.menu {
            if m.contains(p) {
                if m.hover(p) {
                    self.overlay_dirty = true;
                }
                if self.sent_cursor != Cursor::Arrow {
                    window.set_cursor(Cursor::Arrow);
                    self.sent_cursor = Cursor::Arrow;
                }
                return;
            }
        }
        if let Some(grab) = self.scrollbar_drag {
            self.drag_scrollbar(dip, grab);
            return;
        }
        if self.address_drag {
            let layout = self.layout();
            let x = dip.x - layout.address_text.x + self.address_scroll;
            let i = self
                .fonts
                .index_at(self.address.text(), x, chrome::address_font_size());
            self.address.set_caret(i, true);
            self.chrome_dirty = true;
            return;
        }
        let hs = self.suggestion_at(dip);
        if hs != self.hover_suggestion {
            self.hover_suggestion = hs;
            self.overlay_dirty = true;
        }
        let in_chrome = self.in_chrome(dip);
        let hit = if in_chrome {
            self.layout().hit(dip)
        } else {
            Hit::None
        };
        if hit != self.hover {
            // A tab: its hover card waits for the delay (moving within the tab keeps it).
            let (was, now) = (
                Browser::tab_card_target(self.hover),
                Browser::tab_card_target(hit),
            );
            if was != now {
                self.hide_tooltip(window);
                if now.is_some() {
                    window.set_timer(TIMER_TOOLTIP, TOOLTIP_DELAY_MS);
                }
            }
            // A chrome element with a tooltip (a tab's speaker): it waits for the delay.
            if self.chrome_tooltip(self.hover).is_some() || self.chrome_tooltip(hit).is_some() {
                self.hide_tooltip(window);
                if self.chrome_tooltip(hit).is_some() {
                    window.set_timer(TIMER_TOOLTIP, TOOLTIP_DELAY_MS);
                }
            }
            self.hover = hit;
            self.chrome_dirty = true;
        }
        let over_bar = !in_chrome && self.in_scrollbar(dip);
        if over_bar != self.scrollbar_hover {
            self.scrollbar_hover = over_bar;
            self.comp.send(CompMsg::ScrollbarActive(over_bar));
        }
        let over_page = !in_chrome && !over_bar && hs.is_none();
        if over_page && self.active_crashed() {
            let p = self.content_point(dip);
            let at = Point::new(p.x - self.page_rect().x, p.y);
            let over = SadTab::reload_button(self.page_dips()).contains(at);
            if over != self.sad_hover {
                self.sad_hover = over;
                self.overlay_dirty = true;
            }
            let c = if over { Cursor::Hand } else { Cursor::Arrow };
            if c != self.sent_cursor {
                window.set_cursor(c);
                self.sent_cursor = c;
            }
            return;
        }
        if over_page {
            self.page_hover = true;
            let p = self.page_point(dip);
            self.send_active(TabCommand::Pointer(Pointer::Move(p)));
            let c = self.tabs.active().map_or(Cursor::Arrow, |e| e.cursor);
            if c != self.sent_cursor {
                window.set_cursor(c);
                self.sent_cursor = c;
            }
        } else {
            if self.page_hover {
                self.page_hover = false;
                self.send_active(TabCommand::Pointer(Pointer::Leave));
                self.overlay_dirty = true;
                self.title = None;
                self.tooltip = None;
            }
            let c = if hit == Hit::Address {
                Cursor::IBeam
            } else if hit == Hit::SideEdge {
                Cursor::ResizeEW
            } else if hs.is_some() {
                Cursor::Hand
            } else {
                Cursor::Arrow
            };
            if c != self.sent_cursor {
                window.set_cursor(c);
                self.sent_cursor = c;
            }
        }
    }

    fn drag_scrollbar(&mut self, dip: Point, grab: f32) {
        let Some(id) = self.active_id() else {
            return;
        };
        let view = self.comp.scroll_view(id);
        let area = self.page_dips();
        if let Some(bar) = viewport_scrollbar(&view, area) {
            let y = bar.scroll_for_thumb(dip.y - self.chrome_h() - grab);
            self.comp.send(CompMsg::ScrollTo {
                tab: id,
                to: Point::new(view.scroll.x, y),
                smooth: false,
                with: None,
            });
        }
    }

    fn mouse_down(
        &mut self,
        window: &Window,
        dip: Point,
        button: MouseButton,
        mods: Modifiers,
        clicks: u32,
    ) {
        let cp = self.content_point(dip);
        if self.autoscroll_active() {
            // (any press ends autoscroll; one on the page is not the page's)
            self.stop_autoscroll(window);
            if !self.in_chrome(dip) {
                self.swallow_release = true;
                return;
            }
        }
        if self.dt_popup || self.dock == Dock::Undocked {
            // (a press here closes what is open in the tools' window, and goes on)
            if self.dt_popup {
                self.close_popup();
            }
            if self.dock_menu.take().is_some() {
                self.overlay_dirty = true;
            }
        }
        if button == MouseButton::Left && self.menu_bookmark_press(cp, dip) {
            // (a bookmark row acts on release, unless the press becomes a drag)
            window.capture_mouse();
            return;
        }
        if let Some((stack, _)) = &mut self.popup {
            // A press on a row acts; anywhere else it closes the menu (and is swallowed, so a press
            // on the button that opened it closes it).
            let r = stack.click(cp);
            self.swallow_release = true;
            self.popup_result(window, r);
            return;
        }
        if self.dock_menu_press(cp) {
            return;
        }
        if self.restore_bubble_press(cp) {
            return;
        }
        if self
            .task_manager
            .as_ref()
            .is_some_and(|(m, _)| m.rect.contains(cp))
        {
            // (it is drawn over the panels, which a press elsewhere closes)
            self.close_blocker_panel();
            self.close_site_info();
            self.task_manager_press(window, cp, clicks);
            return;
        }
        if self.blocker_panel.is_some() {
            let on_button = dip.y < self.chrome_h() && self.layout().hit(dip) == Hit::Shield;
            if self.blocker_panel_press(cp) {
                return;
            }
            if on_button {
                // (the press that would open it closes it)
                self.swallow_release = true;
                return;
            }
        }
        if self.site_info.is_some() {
            let on_button = dip.y < self.chrome_h() && self.layout().hit(dip) == Hit::SiteInfo;
            if self.site_info_press(window, cp) {
                return;
            }
            if on_button {
                // (the press that would open it closes it)
                self.swallow_release = true;
                return;
            }
        }
        if self.dialog.is_none() && self.find_press(cp, button, clicks) {
            return;
        }
        if let Some((d, _)) = &mut self.dialog {
            let inside = d.rect.contains(cp);
            let r = d.mouse_down(cp, &self.fonts);
            self.dialog_result(window, r);
            if inside {
                self.swallow_release = true;
                window.capture_mouse();
                return;
            }
            // (a press outside closes the dialog and goes on to what is there)
        }
        if self.devtools_mouse_down(window, dip, button, clicks) {
            return;
        }
        if let Some(m) = &self.menu {
            // A press on the list chooses; anywhere else it closes the list (and is swallowed).
            let p = self.content_point(dip);
            let inside = m.contains(p);
            let choice = m.click(p);
            self.swallow_release = true;
            match choice {
                Some(MenuKey::Close(c)) => self.close_menu(c),
                _ if !inside => self.close_menu(None),
                _ => {}
            }
            return;
        }
        match button {
            MouseButton::Back => {
                self.go_active(-1);
                return;
            }
            MouseButton::Forward => {
                self.go_active(1);
                return;
            }
            _ => {}
        }
        if let Some(i) = self.suggestion_at(dip) {
            if button == MouseButton::Left {
                self.selected = Some(i);
                self.submit_address();
            }
            return;
        }
        if self.in_chrome(dip) {
            let hit = self.layout().hit(dip);
            self.pressed = hit;
            self.chrome_dirty = true;
            match (hit, button) {
                (Hit::Tab(i), MouseButton::Left) => {
                    if i != self.tabs.active_index() {
                        self.tabs.select(i);
                        self.activated();
                    }
                    if self.side_tabs_on() {
                        window.capture_mouse();
                        self.side_drag_press(i, dip);
                    }
                }
                (Hit::Tab(i) | Hit::TabClose(i) | Hit::TabAudio(i), MouseButton::Middle) => {
                    self.pressed = Hit::None;
                    self.close_tab(i, window);
                }
                (Hit::Tab(i) | Hit::TabClose(i) | Hit::TabAudio(i), MouseButton::Right) => {
                    self.pressed = Hit::None;
                    // (the menu opens under the strip at the pointer's x, or at the pointer in
                    // the side panel)
                    let at = if self.side_tabs_on() {
                        cp
                    } else {
                        Point::new(dip.x, 0.0)
                    };
                    self.tab_menu(i, at);
                }
                (Hit::SideEdge, MouseButton::Left) => {
                    window.capture_mouse();
                    self.side_resize_start(dip);
                }
                (Hit::Bookmark(i), _) => {
                    if button != MouseButton::Left {
                        self.pressed = Hit::None;
                    } else {
                        window.capture_mouse();
                    }
                    self.bar_press(i, dip, button, mods);
                }
                (Hit::BookmarksBar, MouseButton::Right) => {
                    self.pressed = Hit::None;
                    self.bookmark_menu(None, Point::new(dip.x, 0.0));
                }
                (Hit::BookmarkOverflow, MouseButton::Left) => {
                    self.pressed = Hit::None;
                    self.bar_overflow();
                }
                (Hit::SiteInfo, MouseButton::Left) => {
                    self.pressed = Hit::None;
                    self.open_site_info();
                }
                (Hit::Shield, MouseButton::Left) => {
                    self.pressed = Hit::None;
                    self.toggle_blocker_panel();
                }
                (Hit::Menu, MouseButton::Left) => {
                    self.pressed = Hit::None;
                    let r = self.layout().menu;
                    let entries = self.app_menu();
                    self.open_menu(entries, Point::new(r.right(), 0.0), Hit::Menu, false);
                }
                (Hit::Address, MouseButton::Left) => {
                    let layout = self.layout();
                    if self.focus != Focus::Address {
                        self.focus_address();
                    } else {
                        let x = dip.x - layout.address_text.x + self.address_scroll;
                        let i = self.fonts.index_at(
                            self.address.text(),
                            x,
                            chrome::address_font_size(),
                        );
                        if self.address.press(i, clicks, mods.shift) {
                            self.address_drag = true;
                            window.capture_mouse();
                        }
                        self.caret_on = true;
                    }
                }
                (Hit::Address, MouseButton::Right) => {
                    self.pressed = Hit::None;
                    self.address_context_menu(dip);
                }
                _ => {}
            }
            if hit != Hit::Address && self.focus == Focus::Address && !matches!(hit, Hit::Tab(_)) {
                self.blur_address();
            }
            return;
        }
        if self.focus == Focus::Address {
            self.blur_address();
        }
        if self.active_crashed() {
            let area = self.page_dips();
            if button == MouseButton::Left
                && SadTab::reload_button(area).contains({
                    let p = self.content_point(dip);
                    Point::new(p.x - self.page_rect().x, p.y)
                })
            {
                self.reload_active();
            }
            return;
        }
        if button == MouseButton::Left && self.in_scrollbar(dip) {
            let Some(id) = self.active_id() else {
                return;
            };
            let view = self.comp.scroll_view(id);
            if let Some(bar) = viewport_scrollbar(&view, self.page_dips()) {
                let y = dip.y - self.chrome_h();
                let grab = if bar.thumb_contains(y) {
                    y - bar.thumb_start
                } else {
                    // A click on the track: jump there with the thumb centered.
                    bar.thumb_len / 2.0
                };
                self.scrollbar_drag = Some(grab);
                self.comp.send(CompMsg::ScrollbarActive(true));
                window.capture_mouse();
                self.drag_scrollbar(dip, grab);
            }
            return;
        }
        window.capture_mouse();
        let p = self.page_point(dip);
        if let Some(id) = self.active_id() {
            self.fullscreen.activated(id);
        }
        self.send_active(TabCommand::Pointer(Pointer::Down(p, button, mods, clicks)));
        if button == MouseButton::Middle {
            // (the page's answer to the press decides whether autoscroll starts)
            self.autoscroll_press(dip);
        }
    }

    fn mouse_up(&mut self, window: &Window, dip: Point, button: MouseButton, mods: Modifiers) {
        window.release_mouse();
        if button == MouseButton::Middle {
            self.autoscroll_released(window);
        }
        if let Some((d, _)) = &mut self.dialog {
            d.mouse_up();
        }
        if std::mem::take(&mut self.swallow_release) {
            return;
        }
        if self.devtools_mouse_up() {
            return;
        }
        if self.bm_drag.is_some() {
            self.pressed = Hit::None;
            self.bookmark_drag_release(window, dip, mods);
            return;
        }
        if self.side_resize_end(dip) || self.side_drag_end(dip) {
            return;
        }
        if self.scrollbar_drag.take().is_some() {
            let over = self.in_scrollbar(dip);
            self.scrollbar_hover = over;
            self.comp.send(CompMsg::ScrollbarActive(over));
            return;
        }
        if self.address_drag {
            self.address_drag = false;
            return;
        }
        let pressed = std::mem::replace(&mut self.pressed, Hit::None);
        if pressed != Hit::None {
            self.chrome_dirty = true;
            let hit = if self.in_chrome(dip) {
                self.layout().hit(dip)
            } else {
                Hit::None
            };
            if hit == pressed && button == MouseButton::Left {
                self.chrome_action(window, hit, mods);
            }
            return;
        }
        if !self.in_chrome(dip) {
            let p = self.page_point(dip);
            self.send_active(TabCommand::Pointer(Pointer::Up(p, button, mods)));
        }
    }

    fn chrome_action(&mut self, window: &Window, hit: Hit, mods: Modifiers) {
        match hit {
            Hit::TabClose(i) => self.close_tab(i, window),
            Hit::TabAudio(i) => self.toggle_tab_mute(i),
            Hit::NewTab => self.new_tab(),
            Hit::SideCollapse => self.toggle_side_collapsed(),
            Hit::Back => self.go_active(-1),
            Hit::Forward => self.go_active(1),
            Hit::Reload => {
                if self.tabs.active().is_some_and(|e| e.tab.state.loading) {
                    self.stop_active();
                } else {
                    self.reload_active();
                }
            }
            Hit::Home => {
                let url = self.home_url();
                if mods.ctrl {
                    self.open_tab(url, Placement::Background);
                } else {
                    self.navigate_active(url);
                }
            }
            Hit::Star => self.bookmark_page(),
            _ => {}
        }
    }

    fn new_tab(&mut self) {
        self.open_tab(Url::parse(START_PAGE).unwrap(), Placement::End);
        self.focus_address();
    }

    fn wheel(
        &mut self,
        dip: Point,
        dx: i32,
        dy: i32,
        mods: Modifiers,
        time: snowlark_paint::Ticks,
    ) {
        let cp = self.content_point(dip);
        if self.popup.is_none() && self.task_manager_wheel(cp, dy) {
            return;
        }
        if let Some((stack, _)) = self.popup.as_mut().filter(|_| !self.dt_popup) {
            if stack.contains(cp) {
                let rows = -(dy as f32 / 120.0 * 3.0).round() as i32;
                if stack.wheel(rows) {
                    self.overlay_dirty = true;
                }
                return;
            }
            self.close_popup();
        }
        if let Some(m) = &mut self.menu {
            if m.contains(cp) {
                let rows = -(dy as f32 / 120.0 * 3.0).round() as i32;
                if m.wheel(rows) {
                    self.overlay_dirty = true;
                }
                return;
            }
            self.close_menu(None);
        }
        if self.devtools_wheel(dip, dy) {
            return;
        }
        if mods.ctrl {
            if dy != 0 {
                let z = self.tabs.active().map_or(1.0, |e| e.zoom);
                self.set_zoom(next_zoom(z, dy.signum()));
            }
            return;
        }
        if self.side_wheel(dip, dy) || self.in_chrome(dip) {
            return;
        }
        let Some(id) = self.active_id() else {
            return;
        };
        let view = self.comp.scroll_view(id);
        let page = view.viewport.height.max(100.0);
        // (Shift turns the wheel sideways.)
        let (dx, dy) = if mods.shift && dx == 0 {
            (-dy, 0)
        } else {
            (dx, dy)
        };
        let delta = Point::new(
            wheel_pixels(dx, self.wheel_lines, view.viewport.width.max(100.0)),
            -wheel_pixels(dy, self.wheel_lines, page),
        );
        let precise = is_precise(dx) || is_precise(dy);
        // A scroll container under the pointer scrolls first.
        let p = self.page_point(dip);
        // The page sees the wheel turn (its listeners can't hold the scroll back: they act as
        // passive ones).
        self.send_active(TabCommand::Pointer(Pointer::Wheel(p, delta, mods)));
        let doc = Point::new(p.x + view.scroll.x, p.y + view.scroll.y);
        if let Some(e) = self.tabs.active() {
            // (a mouse wheel's notches go to the page, which knows where a box that snaps rests:
            // CSS Scroll Snap 1 §6.2; a precise device's scrolls move the box as they come)
            if let Some(s) = scroller_at(&e.tab.state.scrollers, doc, delta) {
                e.tab.send(if precise {
                    TabCommand::ScrollContainer(s.node, delta)
                } else {
                    TabCommand::SnapScroll(Some(s.node), delta)
                });
                return;
            }
            if e.tab.state.snap_viewport && !precise {
                e.tab.send(TabCommand::SnapScroll(None, delta));
                return;
            }
        }
        self.comp.send(CompMsg::ScrollBy {
            tab: id,
            delta,
            smooth: (!precise).then_some(ScrollKind::Wheel),
            input: Some(time),
        });
    }

    fn shortcut(&mut self, window: &Window, key: u16, mods: Modifiers) -> bool {
        let ctrl = mods.ctrl && !mods.alt;
        match key {
            keys::ESCAPE if mods.shift && !mods.ctrl && !mods.alt => {
                self.toggle_task_manager(window)
            }
            keys::ESCAPE if self.tabs.active().is_some_and(|e| e.dt.picking) => self.toggle_pick(),
            keys::F12 => self.toggle_devtools(None),
            keys::F11 if !mods.ctrl && !mods.alt && !mods.shift => {
                self.toggle_browser_fullscreen(window)
            }
            // (Esc ends a page's fullscreen; the page does not get it)
            keys::ESCAPE if self.fullscreen.tab.is_some() => self.exit_tab_fullscreen(window),
            keys::F10 if mods.shift && !mods.ctrl && self.focus == Focus::Page => {
                self.keyboard_context_menu()
            }
            keys::APPS if self.focus == Focus::Page => self.keyboard_context_menu(),
            k if ctrl && !mods.shift && k == b'U' as u16 => self.view_source(),
            k if ctrl && !mods.shift && k == b'F' as u16 => self.open_find(),
            keys::F3 if !mods.alt => self.find_step(!mods.shift),
            k if ctrl && k == b'G' as u16 => self.find_step(!mods.shift),
            k if ctrl && mods.shift && k == b'I' as u16 => self.toggle_devtools(None),
            k if ctrl && mods.shift && k == b'J' as u16 => {
                self.toggle_devtools(Some(crate::devtools::Panel::Console))
            }
            k if ctrl && mods.shift && k == b'C' as u16 => self.toggle_pick(),
            k if ctrl && k == b'B' as u16 && mods.shift => self.toggle_bookmarks_bar(),
            k if ctrl && k == b'D' as u16 && !mods.shift => self.bookmark_page(),
            k if ctrl && !mods.shift && k == b'H' as u16 => self.open_history(),
            k if ctrl && !mods.shift && k == b'J' as u16 => self.open_downloads(),
            k if ctrl && mods.shift && k == b'O' as u16 => self.open_bookmark_manager(),
            keys::DELETE if ctrl && mods.shift => self.open_clear_browsing_data(),
            k if ctrl && k == b'T' as u16 && mods.shift => {
                let last = self.closed.borrow_mut().pop();
                if let Some(url) = last.and_then(|u| Url::parse(&u).ok()) {
                    self.open_tab(url, Placement::End);
                }
            }
            k if ctrl && k == b'T' as u16 => self.new_tab(),
            k if ctrl && !mods.shift && k == b'N' as u16 => self.new_window(window),
            k if ctrl && mods.shift && k == b'N' as u16 => self.new_incognito_window(window),
            k if ctrl && (k == b'W' as u16 || k == keys::F4) => {
                let i = self.tabs.active_index();
                self.close_tab(i, window);
            }
            keys::TAB if ctrl => {
                self.tabs.select_relative(if mods.shift { -1 } else { 1 });
                self.activated();
            }
            keys::PAGE_DOWN if ctrl => {
                self.tabs.select_relative(1);
                self.activated();
            }
            keys::PAGE_UP if ctrl => {
                self.tabs.select_relative(-1);
                self.activated();
            }
            k if ctrl && (b'1' as u16..=b'9' as u16).contains(&k) => {
                self.tabs.select_number((k - b'0' as u16) as u32);
                self.activated();
            }
            keys::F5 | keys::BROWSER_REFRESH => self.reload_active(),
            k if ctrl && k == b'R' as u16 => self.reload_active(),
            keys::LEFT if mods.alt => self.go_active(-1),
            keys::RIGHT if mods.alt => self.go_active(1),
            keys::BROWSER_BACK => self.go_active(-1),
            keys::BROWSER_FORWARD => self.go_active(1),
            keys::HOME if mods.alt => {
                let url = self.home_url();
                self.navigate_active(url);
            }
            keys::BROWSER_HOME => {
                let url = self.home_url();
                self.navigate_active(url);
            }
            keys::F6 => self.focus_address(),
            k if ctrl && k == b'L' as u16 => self.focus_address(),
            k if mods.alt && !mods.ctrl && k == b'D' as u16 => self.focus_address(),
            k if ctrl && (k == keys::OEM_PLUS || k == keys::ADD) => {
                let z = self.tabs.active().map_or(1.0, |e| e.zoom);
                self.set_zoom(next_zoom(z, 1));
            }
            k if ctrl && (k == keys::OEM_MINUS || k == keys::SUBTRACT) => {
                let z = self.tabs.active().map_or(1.0, |e| e.zoom);
                self.set_zoom(next_zoom(z, -1));
            }
            k if ctrl && (k == b'0' as u16 || k == keys::NUMPAD0) => self.set_zoom(1.0),
            _ => return false,
        }
        true
    }

    fn address_key(&mut self, key: u16, mods: Modifiers) -> bool {
        match key {
            keys::ENTER => {
                self.submit_address();
                return true;
            }
            keys::ESCAPE => {
                let url = display_url(&self.active_url());
                if self.address.text() == url && self.suggestions.is_empty() {
                    self.blur_address();
                } else {
                    self.address.set_text(&url);
                    self.address.select_all();
                    self.suggestions.clear();
                    self.selected = None;
                    self.overlay_dirty = true;
                }
            }
            keys::UP | keys::DOWN => {
                if !self.suggestions.is_empty() {
                    let n = self.suggestions.len();
                    self.selected = Some(match (self.selected, key) {
                        (None, keys::DOWN) => 0,
                        (None, _) => n - 1,
                        (Some(i), keys::DOWN) => (i + 1) % n,
                        (Some(i), _) => (i + n - 1) % n,
                    });
                    self.overlay_dirty = true;
                }
            }
            // The caret, selection and editing keys every text field takes (Ctrl+A/C/X/V/Z/Y).
            _ => {
                let clip = self.tab_ctx.clipboard.clone();
                match edit_address_key(&mut self.address, key, mods, clip.as_ref()) {
                    KeyEdit::Ignored => return false,
                    KeyEdit::Changed => self.update_suggestions(),
                    KeyEdit::Moved => {}
                }
            }
        }
        self.caret_on = true;
        self.chrome_dirty = true;
        true
    }

    fn typed(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        let mut buf = [0u8; 4];
        self.address.insert(c.encode_utf8(&mut buf));
        self.update_suggestions();
        let typed = self.address.text().to_owned();
        if let Some(full) = self
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completion(&typed)
        {
            self.address.inline_complete(&full);
        }
        self.caret_on = true;
        self.chrome_dirty = true;
    }
}

impl WindowHandler for Browser {
    fn event(&mut self, window: &Window, event: Event) -> bool {
        let scale = self.scale();
        let dip = |x: i32, y: i32| Point::new(x as f32 / scale, y as f32 / scale);
        let mut used = true;
        // A press, a wheel turn or a key hides the tooltip (it shows again for another element).
        if matches!(
            event,
            Event::MouseDown { .. } | Event::Wheel { .. } | Event::KeyDown { .. } | Event::Focus(_)
        ) {
            self.hide_tooltip(window);
        }
        if matches!(event, Event::MouseLeave) {
            self.title = None;
            self.hide_tooltip(window);
        }
        match event {
            Event::Resized {
                width,
                height,
                maximized,
                minimized,
            } => {
                if !minimized && width > 0 && height > 0 {
                    self.size = (width, height);
                    self.placement_px = Some(window.screen_placement());
                    self.maximized = maximized || window.is_maximized();
                    self.resized();
                }
            }
            Event::DpiChanged(dpi) => {
                self.dpi = scale_override().unwrap_or(dpi);
                self.size = window.client_size();
                self.placement_px = Some(window.screen_placement());
                self.resized();
            }
            Event::Moved { .. } => {
                // The pages' screen coordinates (`screenX`, a mouse event's) follow the window.
                let placement = Some(window.screen_placement());
                if placement != self.placement_px {
                    self.placement_px = placement;
                    self.send_devices();
                }
            }
            Event::MouseMove { x, y, .. } => self.mouse_move(window, dip(x, y)),
            Event::DragEnter { x, y, data, mods } => {
                self.drag_enter(window, dip(x, y), *data, mods)
            }
            Event::DragOver { x, y, mods } => self.drag_over(window, dip(x, y), mods),
            Event::DragLeave => self.drag_leave(window),
            Event::Drop { x, y, mods } => self.drag_drop(window, dip(x, y), mods),
            Event::MouseLeave => {
                if self.hover != Hit::None {
                    self.hover = Hit::None;
                    self.chrome_dirty = true;
                }
                if self.page_hover {
                    self.page_hover = false;
                    self.send_active(TabCommand::Pointer(Pointer::Leave));
                    self.overlay_dirty = true;
                }
                if self.scrollbar_hover && self.scrollbar_drag.is_none() {
                    self.scrollbar_hover = false;
                    self.comp.send(CompMsg::ScrollbarActive(false));
                }
            }
            Event::MouseDown {
                x,
                y,
                button,
                mods,
                clicks,
            } => self.mouse_down(window, dip(x, y), button, mods, clicks),
            Event::MouseUp { x, y, button, mods } => self.mouse_up(window, dip(x, y), button, mods),
            Event::CaptureLost => {
                self.scrollbar_drag = None;
                self.address_drag = false;
            }
            Event::Wheel {
                x,
                y,
                delta_x,
                delta_y,
                mods,
                time,
            } => {
                self.stop_autoscroll(window);
                self.wheel(dip(x, y), delta_x, delta_y, mods, time)
            }
            Event::KeyDown { key, .. } if self.autoscroll.is_some() && key == keys::ESCAPE => {
                self.stop_autoscroll(window);
            }
            Event::KeyDown { key, .. }
                if key == keys::ESCAPE && self.bm_drag.as_ref().is_some_and(|d| d.dragging) =>
            {
                self.cancel_bookmark_drag(window);
            }
            Event::KeyDown { key, .. }
                if self.dock_menu.is_some()
                    && self.dock != Dock::Undocked
                    && key == keys::ESCAPE =>
            {
                self.dock_menu = None;
                self.overlay_dirty = true;
            }
            Event::KeyDown { key, .. } if self.site_info.is_some() && key == keys::ESCAPE => {
                self.close_site_info();
            }
            Event::KeyDown { key, .. } if self.blocker_panel.is_some() && key == keys::ESCAPE => {
                self.close_blocker_panel();
            }
            Event::KeyDown { key, .. } if self.popup.is_some() && !self.dt_popup => {
                let r = self
                    .popup
                    .as_mut()
                    .map_or(crate::menu::MenuResult::None, |(m, _)| m.key(key));
                self.popup_result(window, r);
            }
            Event::KeyDown { key, mods, .. } if self.dialog.is_some() => {
                let clip = self.tab_ctx.clipboard.clone();
                let r = self
                    .dialog
                    .as_mut()
                    .map_or(crate::dialog::DialogResult::None, |(d, _)| {
                        d.key(key, mods, clip.as_ref())
                    });
                self.caret_on = true;
                self.dialog_result(window, r);
                used = !mods.alt;
            }
            Event::Char(_) if self.popup.is_some() && !self.dt_popup => {}
            Event::Char(c) if self.dialog.is_some() => {
                if self.dialog.as_mut().is_some_and(|(d, _)| d.typed(c)) {
                    self.caret_on = true;
                    self.overlay_dirty = true;
                }
            }
            Event::KeyDown { key, mods, .. } if self.menu.is_some() => {
                let r = self
                    .menu
                    .as_mut()
                    .map_or(MenuKey::Ignored, |m| m.key(key, mods.alt));
                match r {
                    MenuKey::Close(c) => self.close_menu(c),
                    MenuKey::Moved => self.overlay_dirty = true,
                    MenuKey::Ignored => used = !mods.alt,
                }
            }
            Event::KeyDown {
                key,
                scan,
                mods,
                repeat,
                text,
            } => {
                used = self.shortcut(window, key, mods)
                    || if self.focus == Focus::Address {
                        self.address_key(key, mods)
                    } else if self.focus == Focus::Devtools && self.dock != Dock::Undocked {
                        self.devtools_key(key, mods)
                    } else if self.focus == Focus::Find && self.find_key(key, mods) {
                        true
                    } else if key == keys::ESCAPE && self.find_box().is_some() {
                        // (Escape closes the find box wherever the keyboard is)
                        self.close_find();
                        true
                    } else {
                        if key == keys::ESCAPE {
                            self.stop_active();
                        } else if let Some(id) = self.active_id() {
                            self.fullscreen.activated(id);
                        }
                        // The page gets the key (it scrolls when nothing in the page takes it);
                        // Alt combinations stay with Windows (the system menu, Alt+F4).
                        self.send_active(TabCommand::Key(KeyInput {
                            kind: KeyKind::Down { repeat },
                            key,
                            scan,
                            text,
                            mods,
                        }));
                        !mods.alt
                    };
            }
            Event::KeyUp { key, scan, mods } => {
                if self.focus == Focus::Page {
                    self.send_active(TabCommand::Key(KeyInput {
                        kind: KeyKind::Up,
                        key,
                        scan,
                        text: None,
                        mods,
                    }));
                }
                used = false
            }
            Event::Ime(e) => {
                if self.focus == Focus::Find {
                    if let window::ImeEvent::Commit(text) = e {
                        for c in text.chars() {
                            self.find_char(c);
                        }
                    }
                } else if self.focus == Focus::Address {
                    // The address bar takes committed text only.
                    if let window::ImeEvent::Commit(text) = e {
                        for c in text.chars() {
                            self.typed(c);
                        }
                    }
                } else if self.menu.is_none() {
                    self.send_active(TabCommand::Ime(e));
                }
            }
            Event::Char(c) if self.menu.is_some() => {
                if self.menu.as_mut().is_some_and(|m| m.typed(c)) {
                    self.overlay_dirty = true;
                }
            }
            Event::Char(c) => {
                if self.focus == Focus::Address {
                    self.typed(c);
                } else if self.focus == Focus::Devtools && self.dock != Dock::Undocked {
                    self.devtools_char(c);
                } else if self.focus == Focus::Find {
                    self.find_char(c);
                } else {
                    self.send_active(TabCommand::Key(KeyInput {
                        kind: KeyKind::Char(c),
                        key: 0,
                        scan: 0,
                        text: Some(c),
                        mods: Modifiers::default(),
                    }));
                }
            }
            Event::CaptionButton { button, action } => match action {
                CaptionAction::Hover => {
                    if self.caption_hover != button {
                        self.caption_hover = button;
                        self.chrome_dirty = true;
                    }
                    if button.is_some() && self.hover != Hit::None {
                        self.hover = Hit::None;
                    }
                }
                CaptionAction::Press => {}
                CaptionAction::Click => match button {
                    Some(CaptionButton::Minimize) => window.minimize(),
                    Some(CaptionButton::Maximize) => window.toggle_maximize(),
                    Some(CaptionButton::Close) => self.request_close(window),
                    None => {}
                },
            },
            Event::Timer(TIMER_CARET) => {
                self.caret_on = !self.caret_on;
                if self.dialog.is_some()
                    || self.focus == Focus::Devtools
                    || self.focus == Focus::Find
                {
                    self.overlay_dirty = true;
                } else {
                    self.chrome_dirty = self.focus == Focus::Address;
                }
            }
            Event::Timer(fullscreen::TIMER_FULLSCREEN) => self.fullscreen_tick(window),
            Event::Timer(TIMER_SPINNER) => {
                self.spinner += 0.35;
                self.chrome_dirty = true;
            }
            Event::Timer(TIMER_TOOLTIP) => {
                window.kill_timer(TIMER_TOOLTIP);
                if let (Some(t), true, None) = (&self.title, self.page_hover, &self.menu) {
                    let at = self.content_point(self.mouse);
                    self.tooltip = Some(Tooltip::new(t, at, self.page_dips(), &self.fonts));
                    self.overlay_dirty = true;
                } else if let (Some(i), false, None) = (
                    Browser::tab_card_target(self.hover),
                    self.page_hover,
                    &self.popup,
                ) {
                    self.show_tab_card(window, i);
                } else if let (Some(t), false, None) = (
                    self.chrome_tooltip(self.hover),
                    self.page_hover,
                    &self.popup,
                ) {
                    // (a chrome element's: under the toolbar, at the top of the page; in the side
                    // panel, under the pointer)
                    let at = if self.mouse.y >= self.chrome_h() {
                        self.content_point(self.mouse)
                    } else {
                        Point::new(self.mouse.x, -16.0)
                    };
                    self.tooltip = Some(Tooltip::new(t, at, self.page_dips(), &self.fonts));
                    self.overlay_dirty = true;
                }
            }
            Event::Timer(TIMER_AUDIO) => self.audio_timer(window),
            Event::Timer(TIMER_CARD) => self.refresh_tab_card(window),
            Event::Timer(TIMER_SAVE) => {
                let _ = self
                    .history
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .save();
            }
            Event::Timer(TIMER_SESSION) => {
                // (the placement first: the session file keeps it too)
                self.save_window(window, false);
                self.save_session();
            }
            Event::Timer(TIMER_HANG) => self.hang_timer(window),
            Event::Timer(TIMER_TASKS) => self.refresh_task_manager(window),
            Event::Timer(TIMER_FOLDER_DROP) => self.bookmark_drag_timer(window),
            Event::Timer(downloads_ui::TIMER_DOWNLOADS) => self.downloads_tick(window),
            Event::Timer(_) => {}
            Event::Wake => {
                if self.incognito_count_changed() {
                    self.chrome_dirty = true;
                }
                // (another regular window may have changed the settings or the bookmarks)
                self.take_shared(window);
                self.drain_tab_events(window);
                if self.current.is_none() {
                    // (chosen in the tools' window's menus)
                    for cmd in std::mem::take(&mut self.deferred) {
                        self.run(window, cmd);
                    }
                    self.open_main_window_tabs();
                }
            }
            Event::ThemeChanged => {
                self.update_theme(window);
                self.wheel_lines = window::wheel_scroll_lines();
            }
            Event::Focus(active) => {
                if active && self.focus == Focus::Devtools && self.dock == Dock::Undocked {
                    // (the undocked panel's keyboard stays in its own window)
                    self.focus = Focus::Page;
                }
                if !active {
                    self.stop_autoscroll(window);
                    self.close_menu(None);
                    self.close_popup();
                    self.close_site_info();
                    self.close_blocker_panel();
                }
                self.window_active = active;
                self.chrome_dirty = true;
            }
            Event::CloseRequested => self.request_close(window),
            Event::EndSession => {
                // (Windows ends the process next: a shutdown is not a crash)
                self.save_window(window, true);
                let _ = self
                    .history
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .save();
                self.end_session();
            }
            Event::Destroyed => {
                self.save_window(window, true);
                let _ = self
                    .history
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .save();
                self.close_session();
                return true;
            }
        }
        if self.focus == Focus::Address
            || self.focus == Focus::Find
            || self.dialog.is_some()
            || (self.focus == Focus::Devtools
                && self.tabs.active().is_some_and(|e| e.dt.focus.is_some()))
        {
            window.set_timer(TIMER_CARET, window::caret_blink_ms());
        } else {
            window.kill_timer(TIMER_CARET);
        }
        self.flush(window);
        used
    }

    fn hit_test(&mut self, _window: &Window, x: i32, y: i32) -> HitArea {
        if self.fullscreen.on() {
            return HitArea::Client;
        }
        let s = self.scale();
        let p = Point::new(x as f32 / s, y as f32 / s);
        match self.layout().hit(p) {
            Hit::Caption => HitArea::Caption,
            Hit::Minimize => HitArea::Minimize,
            Hit::Maximize => HitArea::Maximize,
            Hit::Close => HitArea::Close,
            _ => HitArea::Client,
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .save();
        if let Some(b) = &self.blocker {
            b.stop_updater();
        }
        if self.off_the_record() {
            // (the other Incognito windows show one fewer)
            self.app.incognito_changed();
        }
    }
}

/// The system clipboard, owned by the browser window.
struct WindowClipboard(snowlark_paint::window::WindowHandle);

impl Clipboard for WindowClipboard {
    fn text(&self) -> Option<String> {
        window::clipboard_text(self.0)
    }
    fn set_text(&self, text: &str) {
        window::set_clipboard_text(self.0, text)
    }
    fn html(&self) -> Option<String> {
        window::clipboard_html(self.0)
    }
    fn set_text_and_html(&self, text: &str, html: Option<&str>) {
        window::set_clipboard_text_and_html(self.0, text, html)
    }
}

/// The tabs' event sink: the browser's channel, and a wake of the window's thread to read it.
struct WindowSink {
    tx: snowlark_ipc::Sender<(TabId, TabEvent)>,
    window: snowlark_paint::window::WindowProxy,
}

impl crate::tab::EventSink for WindowSink {
    fn send(&self, id: TabId, event: TabEvent) {
        let _ = self.tx.send((id, event));
        self.window.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pages_place_on_the_screen_follows_the_window_the_chrome_and_the_scale() {
        // A window at (100, 50) of 1300×900 device px at 150%, its client area at (108, 81),
        // 120 device px of chrome above the page: the viewport starts at (72, 134) DIPs.
        let p = screen_placement(Some(((100, 50, 1300, 900), (108, 81))), (0, 120), 1.5, 1.25);
        assert_eq!(p.origin, (72, 134));
        // The page right of a side panel 240 DIPs wide.
        let side = screen_placement(
            Some(((100, 50, 1300, 900), (108, 81))),
            (360, 120),
            1.5,
            1.0,
        );
        assert_eq!(side.origin, (312, 134));
        assert_eq!(p.outer, Some((67, 33, 867, 600)));
        assert_eq!(p.zoom, 1.25);
        // A mouse event at CSS (10, 20) of a page zoomed to 125%: 12.5 and 25 DIPs further.
        assert_eq!(p.to_screen(10.0, 20.0), (85, 159));
        // No window: the viewport is the window, at the origin.
        let none = screen_placement(None, (0, 120), 1.5, 1.0);
        assert_eq!(
            none,
            snowlark_page::ScreenPlacement {
                origin: (0, 0),
                outer: None,
                zoom: 1.0
            }
        );
        assert_eq!(none.to_screen(10.0, 20.0), (10, 20));
    }

    #[test]
    fn zoom_steps() {
        assert_eq!(next_zoom(1.0, 1), 1.1);
        assert_eq!(next_zoom(1.0, -1), 0.9);
        assert_eq!(next_zoom(5.0, 1), 5.0);
        assert_eq!(next_zoom(0.25, -1), 0.25);
        // From an off-step value, the nearest step in that direction.
        assert_eq!(next_zoom(1.05, 1), 1.1);
        assert_eq!(next_zoom(1.05, -1), 1.0);
    }

    #[test]
    fn scale_switch() {
        assert_eq!(parse_scale("1.5"), Some(144));
        assert_eq!(parse_scale(" 1.25 "), Some(120));
        assert_eq!(parse_scale("1"), Some(96));
        assert_eq!(parse_scale("0.1"), None);
        assert_eq!(parse_scale("150%"), None);
    }
}
