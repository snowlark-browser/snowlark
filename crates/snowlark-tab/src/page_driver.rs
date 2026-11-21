// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`PageDriver`]: a [`TabDriver`] over `snowlark_page::Page`, the document with its scripts. The tab's
//! thread runs the page's event loop in slices ([`TabDriver::update`]), forwards pointer and keyboard
//! input to it as DOM events (the page runs the default actions: links, forms, focus, editing), paints
//! its renderer's fragments, and learns from it what the shell shows and does: the title, the icon,
//! the URL, navigations the page started (session history entries), new tabs, script scrolling, the
//! next timer to wake for.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_dom::{local_name, NodeId};
use snowlark_net::Url;
use snowlark_page::input::Modifiers as PageModifiers;
use snowlark_page::keyboard::KeyInput as PageKey;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::input::{keys, Cursor, Modifiers, MouseButton};
use snowlark_paint::{DisplayList, ImageStore, Point, Size, TextEngine};

use crate::devtools::netlog::NetLog;
use crate::devtools::protocol::{DevtoolsCommand, DevtoolsEvent, EditKind, NodeOp, ResultOp};
use crate::driver::*;
use crate::renderer_driver::{base_url, document_title, icon_url, START_HTML, START_PAGE};

/// How long one update may run the page's event loop before the tab thread looks at its commands.
const SLICE: Duration = Duration::from_millis(12);

/// How long one update may search the page for the find box.
const FIND_SLICE: Duration = Duration::from_millis(6);

pub struct PageDriver {
    ctx: DriverContext,
    device: Device,
    page: Page,
    images: Arc<ImageStore>,
    info: DocumentInfo,
    /// The top-level document shown, and its URL as last seen.
    doc: Option<NodeId>,
    doc_url: String,
    /// The shell asked for the navigation in progress (it already has a session entry for it).
    shell_navigation: bool,
    scroll: Point,
    clicks: u32,
    new_tabs: Vec<(Url, Disposition)>,
    committed: Vec<Url>,
    devtools: DevtoolsState,
    /// Navigations the page started to the browser's own addresses (the engine's navigation
    /// filter hands them over): (browsing context, URL).
    intercepted: Rc<RefCell<Vec<(usize, Url)>>>,
    internal_requests: Vec<InternalRequest>,
    context_menu: Option<ContextInfo>,
    /// A `view-source:` address being fetched.
    source: Option<SourceFetch>,
    /// Navigations to the browser's own addresses the page asked for, for the browser to begin.
    begun: Vec<snowlark_page::embedder::BeginNavigation>,
    /// Find in page, while the find box is open (none otherwise: nothing runs for it).
    find: Option<snowlark_page::find::FindSession>,
    /// The counter last handed out ([`TabDriver::take_find_status`]).
    find_sent: Option<FindStatus>,
    /// What the driver keeps of each hosted window's view ([`TabDriver::select_view`]), and the
    /// view selected now with the page's own state parked meanwhile.
    views: std::collections::HashMap<snowlark_page::HostedWindow, ViewDoc>,
    in_view: Option<(snowlark_page::HostedWindow, ViewDoc)>,
}

/// The driver's state of one top-level document shown: the page's own or a hosted window's.
struct ViewDoc {
    device: Device,
    info: DocumentInfo,
    doc: Option<NodeId>,
    doc_url: String,
    scroll: Point,
    clicks: u32,
    committed: Vec<Url>,
    context_menu: Option<ContextInfo>,
    /// The view's find box's session (its document's matches).
    find: Option<snowlark_page::find::FindSession>,
    find_sent: Option<FindStatus>,
}

impl ViewDoc {
    fn new(device: Device) -> ViewDoc {
        ViewDoc {
            device,
            info: DocumentInfo::default(),
            doc: None,
            doc_url: String::new(),
            scroll: Point::ZERO,
            clicks: 0,
            committed: Vec::new(),
            context_menu: None,
            find: None,
            find_sent: None,
        }
    }

    /// Trades places with the driver's own fields.
    fn swap(&mut self, d: &mut PageDriver) {
        std::mem::swap(&mut self.device, &mut d.device);
        std::mem::swap(&mut self.info, &mut d.info);
        std::mem::swap(&mut self.doc, &mut d.doc);
        std::mem::swap(&mut self.doc_url, &mut d.doc_url);
        std::mem::swap(&mut self.scroll, &mut d.scroll);
        std::mem::swap(&mut self.clicks, &mut d.clicks);
        std::mem::swap(&mut self.committed, &mut d.committed);
        std::mem::swap(&mut self.context_menu, &mut d.context_menu);
        std::mem::swap(&mut self.find, &mut d.find);
        std::mem::swap(&mut self.find_sent, &mut d.find_sent);
    }
}

/// The response a `view-source:` address shows, arriving.
struct SourceFetch {
    /// The `view-source:` address.
    url: Url,
    stream: snowlark_net::ResponseStream,
    head: Option<snowlark_net::ResponseHead>,
    body: Vec<u8>,
    cut: bool,
}

/// The schemes of addresses the browser serves itself.
fn is_internal_scheme(scheme: &str) -> bool {
    matches!(scheme, "snowlark" | "view-source")
}

/// What the developer tools follow in the page.
struct DevtoolsState {
    netlog: Arc<NetLog>,
    attached: bool,
    /// The element tree's expanded elements and the DOM version its rows were sent for.
    expanded: HashSet<NodeId>,
    tree_version: Option<u64>,
    tree_sent: Option<Instant>,
    /// The element hovered in the tree, and the one under the pointer in pick mode.
    hovered: Option<NodeId>,
    picking: bool,
    pick_hover: Option<NodeId>,
    /// What highlight was last sent.
    shown: Option<NodeId>,
    events: Vec<DevtoolsEvent>,
}

/// How often at most the element tree is sent again while the DOM changes.
const TREE_INTERVAL: Duration = Duration::from_millis(300);

fn page_mods(m: Modifiers) -> PageModifiers {
    PageModifiers {
        ctrl: m.ctrl,
        shift: m.shift,
        alt: m.alt,
        meta: m.meta,
    }
}

/// The window's clipboard as a page's async Clipboard API sees it.
struct PageClipboard(Arc<dyn Clipboard>);

impl snowlark_page::ClipboardAccess for PageClipboard {
    fn read_text(&self) -> Option<String> {
        self.0.text()
    }
    fn write_text(&self, text: &str) {
        self.0.set_text(text)
    }
}

fn button_index(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

/// The window cursor for a CSS `cursor` value (its last keyword: the fallback of a URL list).
pub fn cursor_for(css: &str) -> Cursor {
    let keyword = css
        .rsplit(',')
        .next()
        .unwrap_or(css)
        .trim()
        .to_ascii_lowercase();
    match keyword.as_str() {
        "pointer" => Cursor::Hand,
        "text" => Cursor::IBeam,
        "vertical-text" => Cursor::VerticalText,
        "wait" => Cursor::Wait,
        "progress" => Cursor::Progress,
        "crosshair" | "cell" => Cursor::Crosshair,
        "move" | "all-scroll" => Cursor::Move,
        "grab" => Cursor::Grab,
        "grabbing" => Cursor::Grabbing,
        "not-allowed" | "no-drop" => Cursor::NotAllowed,
        "ew-resize" | "col-resize" | "e-resize" | "w-resize" => Cursor::ResizeEW,
        "ns-resize" | "row-resize" | "n-resize" | "s-resize" => Cursor::ResizeNS,
        "nwse-resize" | "nw-resize" | "se-resize" => Cursor::ResizeNWSE,
        "nesw-resize" | "ne-resize" | "sw-resize" => Cursor::ResizeNESW,
        "help" => Cursor::Help,
        "none" => Cursor::Hidden,
        "alias" => Cursor::Alias,
        "copy" => Cursor::Copy,
        "context-menu" => Cursor::ContextMenu,
        "zoom-in" => Cursor::ZoomIn,
        "zoom-out" => Cursor::ZoomOut,
        _ => Cursor::Arrow,
    }
}

impl PageDriver {
    pub fn new(ctx: DriverContext) -> PageDriver {
        let device = Device::default();
        let netlog = Arc::new(NetLog::default());
        let console = if std::env::var("SNOWLARK_CONSOLE").is_ok_and(|v| v == "1") {
            snowlark_page::ConsoleOutput::Stderr
        } else {
            snowlark_page::ConsoleOutput::Quiet
        };
        let page = Page::new(PageOptions {
            net: Some(ctx.net.observed(netlog.clone())),
            console,
            viewport: (device.viewport.width as i32, device.viewport.height as i32),
            render: Some(RenderSetup {
                net: None,
                text: ctx.text.clone(),
                device_pixel_ratio: device.scale,
                scrollbar_width: device.scrollbar,
                trace: std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1"),
                dark: false,
            }),
            content_filter: ctx.content_filter.clone(),
            broadcast: Some(ctx.broadcast.clone()),
            web_storage: Some(ctx.web_storage.clone()),
            idb: Some(ctx.idb.clone()),
            ..Default::default()
        });
        // The page's network and timer wake-ups wake the tab's thread too.
        page.engine().waker().set_forward(Some(ctx.wake.clone()));
        // Media elements decode into the window's video store, which the compositor draws.
        page.engine()
            .set_media_config(snowlark_page::media::MediaConfig {
                videos: ctx.videos.clone(),
                // (the tab strip's speaker and the tab's mute)
                audio: ctx.audio.clone(),
                audio_out: ctx.audio_out.clone(),
                cdm: ctx.cdm.clone(),
                ..Default::default()
            });
        // (console messages are kept for the developer tools, whenever they open)
        page.engine().devtools_console(true);
        // M5: the browser begins every top-level navigation and hands the response back to
        // commit; the page fetches none itself.
        page.engine().set_browser_navigation(true);
        // `window.open()`: a new window needs a user gesture (HTML transient activation). One with
        // an opener is hosted by this page and shown by the browser as a tab of its own, a view of
        // the page (`TabCommand::AttachView`); one with no opener (`noopener`, a `_blank` link)
        // opens as an ordinary tab of the browser's, in a renderer of its own.
        page.engine().set_popup_blocker(true);
        page.engine().set_shows_hosted_windows(true);
        // (fullscreen is the window's: the page asks it, and it answers)
        page.engine().set_fullscreen_by_embedder(true);
        // (a canvas's frame shows when its canvas thread has read it: no rendering update waits)
        page.engine().set_canvas_frames_async(true);
        // (the async Clipboard API reaches the window's clipboard)
        page.engine()
            .set_clipboard(Some(Arc::new(PageClipboard(ctx.clipboard.clone()))));
        let intercepted: Rc<RefCell<Vec<(usize, Url)>>> = Rc::default();
        let sink = intercepted.clone();
        page.engine().set_navigation_filter(
            &["snowlark", "view-source"],
            Some(Box::new(move |context, url| {
                if !is_internal_scheme(url.scheme()) {
                    return false;
                }
                sink.borrow_mut().push((context, url.clone()));
                true
            })),
        );
        let doc = page.engine().document();
        PageDriver {
            ctx,
            device,
            page,
            images: Arc::new(ImageStore::new()),
            info: DocumentInfo::default(),
            doc,
            doc_url: String::new(),
            shell_navigation: false,
            scroll: Point::ZERO,
            clicks: 1,
            new_tabs: Vec::new(),
            committed: Vec::new(),
            devtools: DevtoolsState {
                netlog,
                attached: false,
                expanded: HashSet::new(),
                tree_version: None,
                tree_sent: None,
                hovered: None,
                picking: false,
                pick_hover: None,
                shown: None,
                events: Vec::new(),
            },
            intercepted,
            internal_requests: Vec::new(),
            context_menu: None,
            source: None,
            begun: Vec::new(),
            find: None,
            find_sent: None,
            views: std::collections::HashMap::new(),
            in_view: None,
        }
    }

    /// Starts fetching what a `view-source:` address shows.
    fn view_source(&mut self, url: Url) {
        let inner = match self.ctx.internal.request(InternalQuery::ViewSourceTarget {
            url: url.as_str().to_owned(),
        }) {
            InternalAnswer::ViewSourceTarget(t) => t.and_then(|t| Url::parse(&t).ok()),
            _ => None,
        };
        let Some(inner) = inner else {
            self.info.loading = false;
            self.page.show_error_page(&url, unsupported_view_source());
            return;
        };
        let wake = self.ctx.wake.clone();
        let stream = self.ctx.net.fetch_with_notify(
            snowlark_net::Request::navigation(inner),
            Arc::new(move || wake()),
        );
        self.source = Some(SourceFetch {
            url,
            stream,
            head: None,
            body: Vec::new(),
            cut: false,
        });
    }

    /// Takes what arrived of a `view-source:` fetch; shows the source once it is all in.
    fn poll_source(&mut self) {
        let Some(s) = &mut self.source else {
            return;
        };
        let mut done = None;
        while let Some(e) = s.stream.try_next() {
            match e {
                snowlark_net::FetchEvent::Head(h) => s.head = Some(h),
                snowlark_net::FetchEvent::Body(b) => {
                    let room = MAX_SOURCE.saturating_sub(s.body.len());
                    if b.len() > room {
                        s.cut = true;
                    }
                    s.body.extend_from_slice(&b[..b.len().min(room)]);
                }
                snowlark_net::FetchEvent::Done => {
                    done = Some(Ok(()));
                    break;
                }
                snowlark_net::FetchEvent::Error(e) => {
                    done = Some(Err(e));
                    break;
                }
                snowlark_net::FetchEvent::Violation(_) => {}
            }
        }
        let Some(result) = done else {
            return;
        };
        let s = self.source.take().expect("the fetch");
        self.info.loading = false;
        match result {
            Ok(()) => {
                let headers = s.head.map(|h| h.headers).unwrap_or_default();
                let headers = headers
                    .iter()
                    .map(|(n, v)| (n.to_owned(), v.to_owned()))
                    .collect();
                let html = match self.ctx.internal.request(InternalQuery::ViewSource {
                    url: s.url.as_str().to_owned(),
                    headers,
                    body: s.body,
                    cut: s.cut,
                }) {
                    InternalAnswer::ViewSource { html } => html,
                    _ => String::new(),
                };
                self.page.load_html(&html, &s.url);
            }
            Err(e) => {
                let failure = snowlark_page::error_page::Failure::Network(e);
                self.page.show_error_page(&s.url, failure);
            }
        }
    }

    /// Shows a page of the browser's own (`url` is a `snowlark:` address).
    fn show_internal(&mut self, url: &Url) {
        self.info.loading = false;
        // (the settings form's submissions show the settings page itself)
        let (shown, html) = match self.ctx.internal.request(InternalQuery::Page {
            url: url.as_str().to_owned(),
        }) {
            InternalAnswer::Page { shown, html } => {
                (Url::parse(&shown).unwrap_or_else(|_| url.clone()), html)
            }
            _ => (url.clone(), None),
        };
        match html {
            Some(html) => self.page.load_html(&html, &shown),
            None => {
                let failure =
                    snowlark_page::error_page::Failure::Network(snowlark_net::NetError::new(
                        snowlark_net::NetErrorKind::UnsupportedScheme,
                        format!("Snowlark has no page at {url}."),
                    ));
                self.page.show_error_page(url, failure);
            }
        }
    }

    /// Navigations the page started to the browser's own addresses: the settings page's form
    /// applies, links between the browser's pages work, and a web page's are dropped (a web page
    /// may neither open nor drive the browser's pages).
    fn take_intercepted(&mut self) {
        let list = std::mem::take(&mut *self.intercepted.borrow_mut());
        if list.is_empty() {
            return;
        }
        let from = Url::parse(&self.doc_url).ok();
        let from_internal =
            from.as_ref().is_some_and(|u| u.scheme() == "snowlark") && !self.info.error_page;
        for (context, url) in list {
            let top = context == snowlark_page::embedder::TOP_CONTEXT;
            if !top || !from_internal {
                if std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1") {
                    eprintln!("[page] refused a navigation to {url} from {}", self.doc_url);
                }
                continue;
            }
            // (the About page is a settings address too)
            let settings = |u: &Url| {
                matches!(
                    self.ctx.internal.kind(u),
                    InternalKind::Settings | InternalKind::About
                )
            };
            let from_settings = from.as_ref().is_some_and(settings);
            if from_settings && settings(&url) && url.path() == "/apply" {
                self.internal_requests.push(InternalRequest::Settings(
                    url.query().unwrap_or("").to_owned(),
                ));
                continue;
            }
            // (and the history page's, to remove visits)
            let history = |u: &Url| self.ctx.internal.kind(u) == InternalKind::History;
            if from.as_ref().is_some_and(history) && history(&url) && url.path() == "/apply" {
                self.internal_requests.push(InternalRequest::History(
                    url.query().unwrap_or("").to_owned(),
                ));
                continue;
            }
            // (and the bookmark manager's, to change the bookmarks)
            let bookmarks = |u: &Url| self.ctx.internal.kind(u) == InternalKind::Bookmarks;
            if from.as_ref().is_some_and(bookmarks) && bookmarks(&url) && url.path() == "/apply" {
                self.internal_requests.push(InternalRequest::Bookmarks(
                    url.query().unwrap_or("").to_owned(),
                ));
                continue;
            }
            // (and the downloads page's, to act on a download or the list)
            let downloads = |u: &Url| self.ctx.internal.kind(u) == InternalKind::Downloads;
            if from.as_ref().is_some_and(downloads) && downloads(&url) && url.path() == "/apply" {
                self.internal_requests.push(InternalRequest::Downloads(
                    url.query().unwrap_or("").to_owned(),
                ));
                continue;
            }
            // A link between the browser's pages: a navigation of the tab's own, which the
            // browser begins (it holds the session history).
            self.begun.push(snowlark_page::embedder::BeginNavigation {
                seq: 0,
                params: snowlark_net::navigation::NavigationParams::user(url),
            });
        }
    }

    /// The factory the shell uses by default.
    pub fn factory() -> DriverFactory {
        Box::new(|ctx| Box::new(PageDriver::new(ctx)) as Box<dyn TabDriver>)
    }

    pub fn page(&mut self) -> &mut Page {
        &mut self.page
    }

    fn show_html(&mut self, html: &str, url: &Url) {
        self.page.load_html(html, url);
    }

    /// What changed in the shown document: a new one committed, its URL, title, icon, loading.
    fn read_document(&mut self, changed: bool) {
        let engine = self.page.engine().clone();
        let Some(doc) = engine.shown_document() else {
            return;
        };
        // An error page shows the URL that failed.
        let failed = engine.error_page_url(doc);
        self.info.error_page = failed.is_some();
        let url = match failed {
            Some(u) => u.to_string(),
            None => engine.dom().document_url(doc).to_owned(),
        };
        if self.doc != Some(doc) {
            self.doc = Some(doc);
            self.info.generation += 1;
            // (the developer tools are of the page's own document)
            if self.in_view.is_none() {
                self.devtools_new_document();
            }
            self.scroll = Point::ZERO;
            if self.in_view.is_some() || !std::mem::take(&mut self.shell_navigation) {
                if let Ok(u) = Url::parse(&url) {
                    self.committed.push(u);
                }
            }
        }
        if url != self.doc_url {
            self.doc_url = url.clone();
            self.info.url = Url::parse(&url).ok().or(self.info.url.take());
        }
        let complete = match self.in_view {
            Some(_) => engine.document_complete(doc),
            None => self.page.load_complete(),
        };
        self.info.loading = !complete && url != "about:blank";
        if changed || self.info.title.is_empty() {
            let dom = engine.dom();
            self.info.title = document_title(&dom, doc);
            let base = base_url(&dom, doc);
            self.info.icon = icon_url(&dom, doc, &base);
        }
    }

    /// The scrollable size, fixed boxes and scroll containers of the last layout.
    fn read_layout(&mut self) {
        let engine = self.page.engine().clone();
        let viewport = self.device.viewport;
        let look = engine.viewport_scrollbar();
        let bar = look.width;
        let laid = engine.with_renderer(|r, _| {
            let root = r.fragments()?.clone();
            let mut scan = crate::renderer_driver::Scan::default();
            crate::renderer_driver::scan_fragments(&root, &mut scan);
            Some((
                Size::new(
                    // (the scrollport is the viewport without the bar)
                    (root.overflow.x + root.overflow.width).max(viewport.width - bar),
                    r.document_height().max(viewport.height),
                ),
                scan.fixed,
                r.images().clone(),
            ))
        });
        if let Some(Some((content, fixed, images))) = laid {
            self.info.content = content;
            self.info.fixed = fixed;
            self.info.scrollbar = bar;
            self.images = images;
        }
        self.info.color_scheme = look.color_scheme;
        self.info.scrollbar_colors = look.colors;
        self.info.background = engine.canvas_color();
        self.info.page_background = engine.page_background();
        let doc = self.doc;
        let (root_el, body) = {
            let dom = engine.dom();
            let root = doc.and_then(|d| dom.document_element(d));
            let body = root.and_then(|h| {
                dom.children(h)
                    .find(|&c| dom.is_html_element_named(c, &local_name!("body")))
            });
            (root, body)
        };
        // (`overflow: hidden` containers and sub-pixel overflow are not user-scrollable)
        self.info.scrollers = engine
            .user_scroll_containers()
            .into_iter()
            .filter(|(n, rect, _, _)| {
                Some(*n) != root_el
                    && Some(*n) != body
                    && rect.width >= crate::renderer_driver::MIN_SCROLLER
                    && rect.height >= crate::renderer_driver::MIN_SCROLLER
            })
            .chain(engine.frame_scrollers())
            .map(|(node, rect, scroll, max)| Scroller {
                node,
                rect,
                scroll,
                max,
            })
            .collect();
    }

    // ----- the developer tools -----------------------------------------------------------------------

    /// A new document: the tools' view of the old one goes.
    fn devtools_new_document(&mut self) {
        self.page.engine().devtools_clear_forced();
        self.page.engine().devtools_clear_results();
        let dt = &mut self.devtools;
        dt.expanded.clear();
        dt.tree_version = None;
        dt.hovered = None;
        dt.pick_hover = None;
        // (the console starts over with the document, whether the tools show or not)
        dt.events.push(DevtoolsEvent::Navigated);
        if dt.attached {
            dt.events.push(DevtoolsEvent::Details(None));
        }
        if dt.shown.take().is_some() {
            dt.events.push(DevtoolsEvent::Highlight(None));
        }
    }

    /// The document element and body, expanded when the tree first shows.
    fn devtools_default_expanded(&mut self) {
        let engine = self.page.engine().clone();
        let dom = engine.dom();
        let Some(root) = self.doc.and_then(|d| dom.document_element(d)) else {
            return;
        };
        self.devtools.expanded.insert(root);
        if let Some(body) = dom
            .children(root)
            .find(|&c| dom.is_html_element_named(c, &local_name!("body")))
        {
            self.devtools.expanded.insert(body);
        }
    }

    fn send_tree(&mut self) {
        let engine = self.page.engine().clone();
        if self.devtools.expanded.is_empty() {
            self.devtools_default_expanded();
        }
        let rows = engine.devtools_tree(&self.devtools.expanded);
        self.devtools.tree_version = Some(engine.devtools_dom_version());
        self.devtools.tree_sent = Some(Instant::now());
        self.devtools.events.push(DevtoolsEvent::Tree(rows));
    }

    /// Sends the highlight of what is hovered (in pick mode, or in the tree) when it changed, or
    /// again when `force` (the page scrolled or changed).
    fn send_highlight(&mut self, force: bool) {
        let dt = &self.devtools;
        let node = if dt.picking {
            dt.pick_hover
        } else {
            dt.hovered
        };
        if node == dt.shown && !force {
            return;
        }
        if node.is_none() && dt.shown.is_none() {
            return;
        }
        let h = node.and_then(|n| self.page.engine().devtools_highlight(n));
        self.devtools.shown = node;
        self.devtools.events.push(DevtoolsEvent::Highlight(h));
    }

    /// Selects `node` for the tools: its details, and the ancestors to expand to show it.
    fn devtools_pick(&mut self, node: NodeId) {
        let engine = self.page.engine().clone();
        let path = engine.devtools_path(node);
        self.devtools.expanded.extend(path.iter().copied());
        self.devtools.events.push(DevtoolsEvent::Picked(node, path));
        self.devtools.events.push(DevtoolsEvent::Details(
            engine.devtools_details(node).map(|d| (node, d)),
        ));
        self.send_tree();
    }

    /// A row of the element tree's context menu, done to `node` (a node that has gone is left
    /// alone).
    fn devtools_node(&mut self, node: NodeId, op: NodeOp) {
        let engine = self.page.engine().clone();
        if !engine.dom().is_alive(node) {
            return;
        }
        match op {
            NodeOp::Copy(kind) => {
                if let Some(text) = engine.devtools_copy(node, kind) {
                    self.devtools.events.push(DevtoolsEvent::Clipboard(text));
                }
            }
            NodeOp::Delete => {
                let parent = {
                    let dom = engine.dom();
                    dom.parent(node).filter(|&p| dom.is_element(p))
                };
                if self.page.with(|e, cx| e.devtools_delete(cx, node)) {
                    match parent {
                        Some(p) => self.devtools_pick(p),
                        None => self.send_tree(),
                    }
                }
            }
            NodeOp::Duplicate => {
                if let Some(copy) = self.page.with(|e, cx| e.devtools_duplicate(cx, node)) {
                    self.devtools_pick(copy);
                }
            }
            NodeOp::ToggleHide => {
                self.page.with(|e, cx| e.devtools_toggle_hidden(cx, node));
                self.send_tree();
            }
            NodeOp::ScrollIntoView => {
                self.page
                    .with(|e, cx| e.devtools_scroll_into_view(cx, node));
                self.send_highlight(true);
            }
            NodeOp::Focus => self.page.with(|e, cx| e.devtools_focus(cx, node)),
            NodeOp::ExpandRecursively => {
                let all = engine.devtools_expandable_descendants(node);
                self.devtools.expanded.insert(node);
                self.devtools.expanded.extend(all);
                self.send_tree();
            }
            NodeOp::CollapseChildren => {
                for d in engine.devtools_expandable_descendants(node) {
                    self.devtools.expanded.remove(&d);
                }
                self.devtools.expanded.insert(node);
                self.send_tree();
            }
            NodeOp::StoreGlobal => {
                if let Some((name, text)) =
                    self.page.with(|e, cx| e.devtools_store_global(cx, node))
                {
                    self.devtools
                        .events
                        .push(DevtoolsEvent::Stored { name, text });
                }
            }
            NodeOp::Cut => {
                let (html, parent) = {
                    let dom = engine.dom();
                    (
                        engine.devtools_html_text(node),
                        dom.parent(node).filter(|&p| dom.is_element(p)),
                    )
                };
                if let Some(html) = html {
                    if self.page.with(|e, cx| e.devtools_delete(cx, node)) {
                        self.devtools.events.push(DevtoolsEvent::Clipboard(html));
                        match parent {
                            Some(p) => self.devtools_pick(p),
                            None => self.send_tree(),
                        }
                    }
                }
            }
            NodeOp::Paste(markup) => {
                if let Some(first) = self.page.with(|e, cx| e.devtools_paste(cx, node, &markup)) {
                    self.devtools.expanded.insert(node);
                    self.devtools_pick(first);
                }
            }
            NodeOp::ForceState(kind, on) => {
                engine.devtools_force_state(node, kind, on);
                let kinds = engine.devtools_forced_kinds(node);
                self.devtools
                    .events
                    .push(DevtoolsEvent::Forced(node, kinds));
                self.send_highlight(true);
                // (the styles pane follows the new state)
                self.devtools.events.push(DevtoolsEvent::Details(
                    engine.devtools_details(node).map(|d| (node, d)),
                ));
            }
            NodeOp::BeginEdit(kind) => {
                let text = match &kind {
                    EditKind::Html => engine.devtools_html_text(node),
                    EditKind::Text => engine.devtools_text_value(node),
                    EditKind::Attribute(name) => engine.devtools_attribute_text(node, name),
                    EditKind::AddAttribute => Some(String::new()),
                };
                if let Some(text) = text {
                    self.devtools
                        .events
                        .push(DevtoolsEvent::EditText { node, kind, text });
                }
            }
            NodeOp::SetAttributes { replacing, text } => {
                let changed = self
                    .page
                    .with(|e, cx| e.devtools_set_attributes(cx, node, replacing.as_deref(), &text));
                if changed {
                    self.devtools_pick(node);
                }
            }
            NodeOp::SetOuterHtml(markup) => {
                if let Some(first) = self
                    .page
                    .with(|e, cx| e.devtools_set_outer_html(cx, node, &markup))
                {
                    match first {
                        Some(el) => self.devtools_pick(el),
                        None => self.send_tree(),
                    }
                }
            }
            NodeOp::SetText(value)
                if self
                    .page
                    .with(|e, cx| e.devtools_set_text(cx, node, &value)) =>
            {
                self.send_tree();
            }
            NodeOp::SetText(_) => {}
            // (the rest of the menu is shown disabled until it is done)
            _ => {}
        }
    }

    /// What the tools follow after the page ran: console messages, requests, DOM changes.
    fn devtools_update(&mut self, repaint: bool) {
        let engine = self.page.engine().clone();
        let entries = engine.take_console_entries();
        if !entries.is_empty() {
            self.devtools.events.push(DevtoolsEvent::Console(entries));
        }
        if !self.devtools.attached {
            return;
        }
        let net = self.devtools.netlog.take();
        if !net.is_empty() {
            self.devtools.events.push(DevtoolsEvent::Network(net));
        }
        let version = engine.devtools_dom_version();
        let due = self
            .devtools
            .tree_sent
            .is_none_or(|t| t.elapsed() >= TREE_INTERVAL);
        if self.devtools.tree_version != Some(version) && due {
            self.send_tree();
        }
        if repaint && self.devtools.shown.is_some() {
            self.send_highlight(true);
        }
    }

    /// When the tools wait to send the tree again (the DOM changed too soon after the last).
    fn devtools_next(&self) -> Option<Instant> {
        let dt = &self.devtools;
        if !dt.attached || dt.tree_version == Some(self.page.engine().devtools_dom_version()) {
            return None;
        }
        dt.tree_sent.map(|t| t + TREE_INTERVAL)
    }

    fn outcome(&self, o: snowlark_page::input::PointerOutcome) -> PointerResult {
        PointerResult {
            cursor: cursor_for(&o.cursor),
            link: o.link,
            open: None,
            repaint: false,
            title: o.title,
            press: None,
            held: false,
        }
    }
}

impl PageDriver {
    /// Starts loading `url` (`reload`: the document shown, again). A fetchable URL is the
    /// browser's to begin (the engine hands it over: `Engine::set_browser_navigation`), and
    /// nothing of the tab's state changes until the commit.
    fn load(&mut self, url: Url, reload: bool) {
        if let Some((window, _)) = &self.in_view {
            // A view's window navigates itself (the browser's own pages open in tabs of their own).
            if matches!(
                url.scheme(),
                "http" | "https" | "file" | "data" | "blob" | "about"
            ) {
                self.page
                    .engine()
                    .navigate_hosted_window(*window, url, reload);
            }
            return;
        }
        if matches!(url.scheme(), "http" | "https" | "file" | "data" | "blob") {
            if reload {
                self.page.reload(url);
            } else {
                self.page.navigate(url);
            }
            return;
        }
        self.info.url = Some(url.clone());
        self.info.loading = true;
        self.shell_navigation = true;
        self.source = None;
        match url.scheme() {
            "snowlark" => self.show_internal(&url),
            "view-source" => self.view_source(url),
            "about" => {
                self.info.loading = false;
                if url.as_str() == START_PAGE {
                    // (the browser's own new tab page when it has one, the built-in one else)
                    let html = match self.ctx.internal.request(InternalQuery::Page {
                        url: url.as_str().to_owned(),
                    }) {
                        InternalAnswer::Page { html: Some(h), .. } => Some(h),
                        _ => None,
                    };
                    self.show_html(html.as_deref().unwrap_or(START_HTML), &url);
                } else {
                    self.show_html("", &url);
                }
            }
            "http" | "https" | "file" | "data" if reload => self.page.reload(url),
            "http" | "https" | "file" | "data" => self.page.navigate(url),
            other => {
                self.info.loading = false;
                let failure =
                    snowlark_page::error_page::Failure::Network(snowlark_net::NetError::new(
                        snowlark_net::NetErrorKind::UnsupportedScheme,
                        format!("Snowlark doesn't open {other}: addresses."),
                    ));
                self.page.show_error_page(&url, failure);
            }
        }
    }
}

impl PageDriver {
    /// M5: a navigation the browser began, committed here ([`TabDriver::commit`]).
    fn commit_navigation(&mut self, commit: crate::tab::Commit) {
        let crate::tab::Commit {
            params,
            body,
            view_source,
            ..
        } = commit;
        self.info.loading = true;
        self.shell_navigation = true;
        self.source = None;
        let wake = self.ctx.wake.clone();
        let stream = body.and_then(|id| self.ctx.net.adopt(id, Some(Arc::new(move || wake()))));
        if let Some(url) = view_source {
            self.info.url = Some(url.clone());
            match params.response {
                snowlark_page::CommitResponse::Fetched { head } => {
                    let head = *head;
                    let (sender, pending) = snowlark_net::ResponseStream::remote(|| {});
                    let stream = match stream {
                        Some(s) => s,
                        None => {
                            sender.send(snowlark_net::FetchEvent::Done);
                            pending
                        }
                    };
                    self.source = Some(SourceFetch {
                        url,
                        stream,
                        head: Some(head),
                        body: Vec::new(),
                        cut: false,
                    });
                    self.poll_source();
                }
                snowlark_page::CommitResponse::Failed(e) => {
                    self.info.loading = false;
                    let failure = snowlark_page::error_page::Failure::Network(e);
                    self.page.show_error_page(&url, failure);
                }
                snowlark_page::CommitResponse::Direct => {
                    self.info.loading = false;
                    self.page.show_error_page(&url, unsupported_view_source());
                }
            }
            return;
        }
        self.info.url = Some(match &params.response {
            snowlark_page::CommitResponse::Fetched { head } => {
                head.url().cloned().unwrap_or_else(|| params.url.clone())
            }
            _ => params.url.clone(),
        });
        self.page.commit_navigation(params, stream);
    }
}

/// The error of a `view-source:` address whose target the browser does not show the source of.
fn unsupported_view_source() -> snowlark_page::error_page::Failure {
    snowlark_page::error_page::Failure::Network(snowlark_net::NetError::new(
        snowlark_net::NetErrorKind::UnsupportedScheme,
        "Snowlark shows the source of web pages and files only.".to_owned(),
    ))
}

impl TabDriver for PageDriver {
    fn navigate(&mut self, url: Url) {
        self.load(url, false);
    }

    fn browser_navigates(&self) -> bool {
        true
    }

    fn commit(&mut self, commit: crate::tab::Commit) {
        self.commit_navigation(commit);
    }

    fn go_in_document(&mut self, index: usize) {
        self.page.go_in_document(index);
    }

    fn take_begun_navigations(&mut self) -> Vec<snowlark_page::embedder::BeginNavigation> {
        let mut begun = std::mem::take(&mut self.begun);
        begun.extend(self.page.engine().take_begun_navigations());
        begun
    }

    fn take_same_document_changes(&mut self) -> Vec<snowlark_page::embedder::SameDocumentChange> {
        self.page.engine().take_same_document_changes()
    }

    fn take_traversals(&mut self) -> Vec<isize> {
        self.page.engine().take_traversals()
    }

    fn take_fullscreen_asks(&mut self) -> Vec<bool> {
        self.page.engine().take_fullscreen_asks()
    }

    fn fullscreen(&mut self, on: bool) {
        self.page.engine().fullscreen_viewport_answer(on);
    }

    fn reload(&mut self, url: Url) {
        self.load(url, true);
    }

    fn stop(&mut self) {
        self.source = None;
        self.page.engine().stop_loading_top();
        self.info.loading = false;
    }

    fn take_window_changes(&mut self) -> Vec<snowlark_page::WindowChange> {
        self.page.engine().take_window_changes()
    }

    fn select_view(&mut self, view: Option<snowlark_page::HostedWindow>) -> bool {
        match view {
            Some(w) => {
                if self.in_view.as_ref().is_some_and(|(v, _)| *v == w) {
                    return true;
                }
                self.select_view(None);
                if !self.page.engine().begin_view(w) {
                    return false;
                }
                // (a new view's viewport is its window's until the first `set_device` sets the
                // view's)
                let device = Device {
                    viewport: snowlark_paint::Size::new(0.0, 0.0),
                    ..self.device
                };
                let mut state = self
                    .views
                    .remove(&w)
                    .unwrap_or_else(|| ViewDoc::new(device));
                // (the driver's own fields are the view's now, and `state` the page's)
                state.swap(self);
                self.in_view = Some((w, state));
                true
            }
            None => {
                let Some((w, mut state)) = self.in_view.take() else {
                    return true;
                };
                state.swap(self);
                self.page.engine().end_view();
                if self.page.engine().hosted_window_document(w).is_some() {
                    self.views.insert(w, state);
                }
                true
            }
        }
    }

    fn view_update(&mut self) -> Update {
        let engine = self.page.engine().clone();
        // (the view's find box: a slice of its search, as `update` does the page's)
        let mut next = None;
        if let Some(find) = &mut self.find {
            let work = find.work(&engine, self.page.load_complete(), FIND_SLICE);
            next = if work.busy {
                Some(Instant::now())
            } else {
                work.next
            };
        }
        let repaint = engine.take_needs_paint();
        let doc_changed = engine.shown_document() != self.doc;
        self.read_document(repaint || doc_changed);
        if repaint {
            self.read_layout();
        }
        Update {
            repaint,
            next,
            scroll_to: engine.take_scroll_request(),
        }
    }

    fn close_view(&mut self, view: snowlark_page::HostedWindow) {
        if self.in_view.as_ref().is_some_and(|(v, _)| *v == view) {
            self.select_view(None);
        }
        self.views.remove(&view);
        self.page.engine().close_hosted_window(view);
    }

    fn close_own_page(&mut self) {
        self.select_view(None);
        self.page.with(|e, cx| e.close_own_top_level(cx));
    }

    fn set_device(&mut self, device: Device) {
        if device == self.device {
            return;
        }
        self.device = device;
        let engine = self.page.engine();
        if let Some((w, _)) = &self.in_view {
            // (scale and scroll bar are the page's window's, as the page's own view has them; the
            // view's window sits where it is)
            engine.set_viewport(device.viewport);
            engine.set_hosted_window_screen(*w, device.screen);
            return;
        }
        engine.set_viewport(device.viewport);
        engine.set_device_pixel_ratio(device.scale);
        engine.set_scrollbar_width(device.scrollbar);
        engine.set_screen_placement(device.screen);
    }

    fn use_begin_frames(&mut self) {
        self.page.engine().use_begin_frames();
    }

    fn begin_frame(&mut self, args: snowlark_page::BeginFrame) {
        self.page.engine().begin_frame(args);
    }

    fn needs_begin_frames(&self) -> bool {
        self.page.engine().needs_begin_frames()
    }

    fn set_scroll(&mut self, scroll: Point) {
        self.scroll = scroll;
        self.page.engine().viewport_scrolled(scroll);
        if self.devtools.shown.is_some() {
            self.send_highlight(true);
        }
    }

    fn update(&mut self) -> Update {
        self.page.run_for(SLICE);
        self.take_intercepted();
        self.poll_source();
        let engine = self.page.engine().clone();
        // Find in page: a slice of the search, and the highlights it puts in the renderers (which
        // ask for the repaint below).
        let mut find_next = None;
        if let Some(find) = &mut self.find {
            let work = find.work(&engine, self.page.load_complete(), FIND_SLICE);
            find_next = if work.busy {
                Some(Instant::now())
            } else {
                work.next
            };
        }
        let repaint = engine.take_needs_paint();
        let doc_changed = engine.shown_document() != self.doc;
        self.read_document(repaint || doc_changed);
        if repaint {
            self.read_layout();
        }
        for (url, background) in engine.take_open_requests() {
            self.new_tabs.push((
                url,
                if background {
                    Disposition::NewBackgroundTab
                } else {
                    Disposition::NewForegroundTab
                },
            ));
        }
        self.devtools_update(repaint);
        let mut next = engine.next_wakeup();
        if let Some(at) = self.devtools_next() {
            next = Some(next.map_or(at, |n| n.min(at)));
        }
        if let Some(at) = find_next {
            next = Some(next.map_or(at, |n| n.min(at)));
        }
        if engine.has_ready_task() {
            next = Some(Instant::now());
        }
        Update {
            repaint,
            next,
            scroll_to: engine.take_scroll_request(),
        }
    }

    fn paint(&mut self, origin: Point) -> Option<DisplayList> {
        // (scroll containers' scroll bars are painted with them, by layout)
        self.page.engine().paint(origin, self.device.scale)
    }

    fn text(&self) -> &TextEngine {
        &self.ctx.text
    }

    fn images(&self) -> &ImageStore {
        &self.images
    }
    fn image_store(&self) -> Arc<ImageStore> {
        self.images.clone()
    }

    fn terminator(&self) -> Option<snowlark_page::Terminator> {
        Some(self.page.terminator())
    }

    fn memory(&mut self) -> crate::driver::DriverMemory {
        let m = self.page.memory();
        crate::driver::DriverMemory {
            js: (m.js_heap + m.js_external) as u64,
            dom: m.dom as u64,
            received: self.devtools.netlog.received(),
        }
    }

    fn take_held_pointer(&mut self) -> Option<PointerResult> {
        let o = self.page.take_held_outcome()?;
        Some(self.outcome(o))
    }

    fn drag(&mut self, input: DragInput) -> snowlark_page::dnd::DragOperation {
        use snowlark_page::dnd::DragOperation;
        match input {
            DragInput::Enter { at, data, mods } => {
                self.page.drag_over(Some(&data), at, page_mods(mods))
            }
            DragInput::Over { at, mods } => self.page.drag_over(None, at, page_mods(mods)),
            DragInput::Leave => {
                self.page.drag_leave();
                DragOperation::None
            }
            DragInput::Drop { at, data, mods } => {
                self.page.drop_dragged(Some(&data), at, page_mods(mods))
            }
        }
    }

    fn pointer(&mut self, input: Pointer) -> PointerResult {
        if self.devtools.picking {
            // Pick mode: the page doesn't see the pointer.
            match input {
                Pointer::Move(p) => {
                    self.devtools.pick_hover = self.page.engine().devtools_node_at(p);
                    self.send_highlight(false);
                }
                Pointer::Down(p, MouseButton::Left, _, _) => {
                    self.devtools.picking = false;
                    match self.page.engine().devtools_node_at(p) {
                        Some(n) => self.devtools_pick(n),
                        None => self.devtools.events.push(DevtoolsEvent::PickEnded),
                    }
                    self.devtools.pick_hover = None;
                    self.send_highlight(false);
                }
                Pointer::Leave => {
                    self.devtools.pick_hover = None;
                    self.send_highlight(false);
                }
                _ => {}
            }
            return PointerResult::default();
        }
        match input {
            // (held for the page's next rendering update while it gets begin frames)
            Pointer::Move(p) => match self.page.queue_pointer_move(p, PageModifiers::default()) {
                Some(o) => self.outcome(o),
                None => PointerResult {
                    held: true,
                    ..Default::default()
                },
            },
            Pointer::Down(p, b, mods, clicks) => {
                self.clicks = clicks.max(1);
                let o = self
                    .page
                    .pointer_pressed(p, button_index(b), page_mods(mods), self.clicks);
                let press = PressOutcome {
                    canceled: o.press_canceled,
                    blocked: o.autoscroll_blocked,
                };
                PointerResult {
                    press: Some(press),
                    ..self.outcome(o)
                }
            }
            Pointer::Up(p, b, mods) => {
                let o =
                    self.page
                        .pointer_released(p, button_index(b), page_mods(mods), self.clicks);
                // (a contextmenu event nothing canceled: the window's menu)
                if let Some(el) = self.page.engine().take_context_menu() {
                    self.context_menu = Some(ContextInfo {
                        point: p,
                        target: self.page.engine().context_target(Some(el)),
                        keyboard: false,
                    });
                }
                self.outcome(o)
            }
            Pointer::Wheel(p, delta, mods) => {
                self.page.wheel(p, delta, page_mods(mods));
                PointerResult::default()
            }
            Pointer::Leave => {
                self.page.pointer_left();
                PointerResult::default()
            }
        }
    }

    fn key(&mut self, input: KeyInput) -> KeyResult {
        let mods = input.mods;
        if self.devtools.picking {
            if input.key == keys::ESCAPE && matches!(input.kind, KeyKind::Down { .. }) {
                self.devtools.picking = false;
                self.devtools.pick_hover = None;
                self.devtools.events.push(DevtoolsEvent::PickEnded);
                self.send_highlight(false);
            }
            return KeyResult::default();
        }
        let mut key = PageKey {
            vk: input.key,
            scan: input.scan,
            text: input.text,
            mods: page_mods(mods),
            repeat: false,
            paste: None,
            paste_html: None,
        };
        match input.kind {
            KeyKind::Down { repeat } => {
                key.repeat = repeat;
                let paste_key =
                    (mods.ctrl && input.key == b'V' as u16) || (mods.shift && input.key == 0x2D);
                if paste_key {
                    key.paste = self.ctx.clipboard.text();
                    key.paste_html = self.ctx.clipboard.html();
                }
                let out = self.page.key_down(&key);
                if let Some(text) = out.copy {
                    match out.copy_html {
                        Some(html) => self.ctx.clipboard.set_text_and_html(&text, Some(&html)),
                        None => self.ctx.clipboard.set_text(&text),
                    }
                }
                KeyResult {
                    scroll: if out.unhandled && !mods.ctrl && !mods.alt {
                        key_scroll(input.key, mods, self.device.viewport)
                    } else {
                        None
                    },
                    repaint: false,
                }
            }
            KeyKind::Up => {
                self.page.key_up(&key);
                KeyResult::default()
            }
            KeyKind::Char(c) => {
                // (Enter and Backspace arrive as characters too: their keydown did the work.)
                if c == '\u{8}' || (c == '\r' && input.key == keys::ENTER) {
                    return KeyResult::default();
                }
                self.page.key_char(c, &key);
                KeyResult::default()
            }
        }
    }

    fn viewport(&self) -> Size {
        self.device.viewport
    }

    fn take_new_tabs(&mut self) -> Vec<(Url, Disposition)> {
        std::mem::take(&mut self.new_tabs)
    }

    fn take_committed(&mut self) -> Vec<Url> {
        std::mem::take(&mut self.committed)
    }

    fn scroll_container(&mut self, node: NodeId, delta: Point) -> bool {
        let moved = self.page.engine().scroll_container_by(node, delta);
        if moved {
            if let Some(s) = self.info.scrollers.iter_mut().find(|s| s.node == node) {
                s.scroll = Point::new(
                    (s.scroll.x + delta.x).clamp(0.0, s.max.x),
                    (s.scroll.y + delta.y).clamp(0.0, s.max.y),
                );
            }
        }
        moved
    }

    fn info(&self) -> &DocumentInfo {
        &self.info
    }

    fn find(&mut self, command: FindCommand) {
        let engine = self.page.engine().clone();
        match command {
            FindCommand::Open { previous } => {
                // (the page's selected text, as in Chrome, else what was searched for last)
                let query = engine
                    .find_selected_text()
                    .filter(|t| !t.is_empty())
                    .unwrap_or(previous);
                self.find
                    .get_or_insert_with(snowlark_page::find::FindSession::new)
                    .set_query(&engine, &query);
            }
            FindCommand::Query(query) => {
                if let Some(s) = &mut self.find {
                    s.set_query(&engine, &query);
                }
            }
            FindCommand::Next | FindCommand::Previous => {
                if let Some(s) = &mut self.find {
                    s.step(&engine, command == FindCommand::Next);
                }
            }
            FindCommand::Close { select } => {
                if let Some(mut s) = self.find.take() {
                    s.close(&engine, select);
                }
                self.find_sent = None;
            }
        }
    }

    fn take_find_status(&mut self) -> Option<FindStatus> {
        let status = self.find.as_ref()?.status();
        if self.find_sent.as_ref() == Some(&status) {
            return None;
        }
        self.find_sent = Some(status.clone());
        Some(status)
    }

    fn devtools(&mut self, command: DevtoolsCommand) {
        match command {
            DevtoolsCommand::Attach(on) => {
                self.devtools.attached = on;
                self.devtools.netlog.set_recording(on);
                // (uncaught errors print with their call frames while the tools are open)
                self.page.capture_error_stacks(on);
                if on {
                    self.send_tree();
                } else {
                    self.devtools.hovered = None;
                    self.devtools.picking = false;
                    self.devtools.pick_hover = None;
                    self.send_highlight(false);
                }
            }
            DevtoolsCommand::Eval(id, source) => {
                let r = self.page.with(|e, cx| e.devtools_eval(cx, id, &source));
                // (what the input printed comes before its result)
                let entries = self.page.engine().take_console_entries();
                if !entries.is_empty() {
                    self.devtools.events.push(DevtoolsEvent::Console(entries));
                }
                self.devtools.events.push(DevtoolsEvent::EvalResult(id, r));
            }
            DevtoolsCommand::Result(id, ResultOp::Copy) => {
                if let Some(text) = self.page.with(|e, cx| e.devtools_copy_result(cx, id)) {
                    self.devtools.events.push(DevtoolsEvent::Clipboard(text));
                }
            }
            DevtoolsCommand::Result(id, ResultOp::Store) => {
                if let Some((name, text)) = self.page.with(|e, cx| e.devtools_store_result(cx, id))
                {
                    self.devtools
                        .events
                        .push(DevtoolsEvent::Stored { name, text });
                }
            }
            DevtoolsCommand::ClearResults => self.page.engine().devtools_clear_results(),
            DevtoolsCommand::Tree(expanded) => {
                self.devtools.expanded = expanded.into_iter().collect();
                self.send_tree();
            }
            DevtoolsCommand::Select(node) => {
                let engine = self.page.engine().clone();
                let details = node.and_then(|n| engine.devtools_details(n).map(|d| (n, d)));
                self.devtools.events.push(DevtoolsEvent::Details(details));
                if let Some(n) = node {
                    let kinds = engine.devtools_forced_kinds(n);
                    self.devtools.events.push(DevtoolsEvent::Forced(n, kinds));
                }
            }
            DevtoolsCommand::Hover(node) => {
                self.devtools.hovered = node;
                self.send_highlight(false);
            }
            DevtoolsCommand::Pick(on) => {
                self.devtools.picking = on;
                self.devtools.pick_hover = None;
                self.send_highlight(false);
            }
            DevtoolsCommand::InspectAt(p) => {
                if let Some(n) = self.page.engine().devtools_node_at(p) {
                    self.devtools_pick(n);
                }
            }
            DevtoolsCommand::Node(node, op) => self.devtools_node(node, op),
        }
    }

    fn take_context_menu(&mut self) -> Option<ContextInfo> {
        self.context_menu.take()
    }

    fn context_menu_at(&mut self, at: Option<Point>) {
        let engine = self.page.engine().clone();
        let element = at.and_then(|p| engine.devtools_node_at(p));
        let target = engine.context_target(element);
        self.context_menu = Some(ContextInfo {
            point: at.unwrap_or(Point::new(16.0, 16.0)),
            target,
            keyboard: true,
        });
    }

    fn set_color_scheme(&mut self, dark: bool) {
        self.page.engine().set_color_scheme(dark);
    }

    fn set_shown(&mut self, shown: bool) {
        self.page.engine().set_shown(shown);
    }

    fn take_internal_requests(&mut self) -> Vec<InternalRequest> {
        std::mem::take(&mut self.internal_requests)
    }

    fn clear_site_data(&mut self, site: Option<&str>) {
        self.page.engine().clear_site_storage(site);
    }

    fn take_devtools(&mut self) -> Vec<DevtoolsEvent> {
        std::mem::take(&mut self.devtools.events)
    }

    fn take_select_popup(&mut self) -> Option<snowlark_page::popup::SelectPopup> {
        self.page.engine().take_select_popup()
    }

    fn select_popup_chosen(&mut self, select: NodeId, index: usize) {
        self.page.select_popup_choose(select, index);
    }

    fn ime(&mut self, event: snowlark_paint::input::ImeEvent) {
        use snowlark_paint::input::ImeEvent;
        match event {
            ImeEvent::Start => self.page.composition_start(),
            ImeEvent::Update { text, cursor } => self.page.composition_update(&text, cursor),
            ImeEvent::Commit(text) => self.page.composition_commit(&text),
            ImeEvent::End => self.page.composition_end(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_net::{NetConfig, NetThread};

    fn driver(net: &NetThread) -> PageDriver {
        let mut d = PageDriver::new(DriverContext {
            videos: None,
            net: net.client(),
            text: Arc::new(TextEngine::new().unwrap()),
            wake: Arc::new(|| {}),
            clipboard: Arc::new(MemoryClipboard::default()),
            internal: Arc::new(TestPages),
            content_filter: None,
            audio: None,
            audio_out: None,
            broadcast: snowlark_page::BroadcastHub::default(),
            web_storage: snowlark_page::web_storage::WebStorageHost::new(),
            idb: snowlark_page::IdbBackend::in_memory(),
            cdm: None,
        });
        d.set_device(Device {
            viewport: Size::new(800.0, 600.0),
            scale: 1.0,
            scrollbar: 0.0,
            screen: Default::default(),
        });
        d
    }

    /// The browser's pages as far as the driver cares: a title per kind (the real ones are the
    /// shell's, and tested there).
    struct TestPages;

    impl InternalPages for TestPages {
        fn request(&self, q: InternalQuery) -> InternalAnswer {
            let kind = |url: &str| match Url::parse(url) {
                Ok(u) if u.scheme() == "snowlark" && u.host_str() == Some("credits") => {
                    InternalKind::Credits
                }
                Ok(u) if u.scheme() == "snowlark" && u.host_str() == Some("history") => {
                    InternalKind::History
                }
                Ok(u) if u.scheme() == "snowlark" && u.host_str() == Some("bookmarks") => {
                    InternalKind::Bookmarks
                }
                Ok(u) if u.scheme() == "snowlark" && u.host_str() == Some("downloads") => {
                    InternalKind::Downloads
                }
                Ok(u) if u.scheme() == "snowlark" && u.host_str() == Some("settings") => {
                    if u.path().trim_end_matches('/') == "/help" {
                        InternalKind::About
                    } else {
                        InternalKind::Settings
                    }
                }
                _ => InternalKind::Other,
            };
            match q {
                InternalQuery::Page { url } => {
                    let html = match kind(&url) {
                        // (a form that submits to the apply address, as the real page's does)
                        InternalKind::Settings => Some(
                            "<title>Settings</title><form action='snowlark://settings/apply'>                             <input type=submit name=theme value=light></form>",
                        ),
                        InternalKind::About => Some("<title>About Snowlark</title>"),
                        InternalKind::Credits => Some("<title>Credits</title>"),
                        InternalKind::History => Some("<title>History</title>"),
                        InternalKind::Bookmarks => Some("<title>Bookmarks</title>"),
                        InternalKind::Downloads => Some("<title>Downloads</title>"),
                        InternalKind::Other => None,
                    };
                    InternalAnswer::Page {
                        html: html.map(str::to_owned),
                        shown: url,
                    }
                }
                InternalQuery::Kind { url } => InternalAnswer::Kind(kind(&url)),
                InternalQuery::ViewSourceTarget { url } => InternalAnswer::ViewSourceTarget(
                    url.strip_prefix("view-source:").map(str::to_owned),
                ),
                InternalQuery::ViewSource { .. } => InternalAnswer::ViewSource {
                    html: String::new(),
                },
            }
        }
    }

    fn settle(d: &mut PageDriver, until: impl Fn(&PageDriver) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            d.update();
            browse(d);
            if until(d) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    thread_local! {
        /// How many navigations the test's browser committed for the driver.
        static BROWSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn browsed() -> usize {
        BROWSED.with(|b| b.get())
    }

    /// The browser's part of the driver's navigations (M5): each one it asked for is fetched
    /// through its network, the head taken, and the rest handed over to commit; an `about:` URL
    /// commits directly, and the browser's own pages are told to load.
    fn browse(d: &mut PageDriver) {
        for begin in d.take_begun_navigations() {
            BROWSED.with(|b| b.set(b.get() + 1));
            let url = begin.params.url.clone();
            let navigation = begin.seq;
            let fetchable = matches!(url.scheme(), "http" | "https" | "file" | "data" | "blob");
            if !fetchable {
                if url.scheme() == "about" {
                    d.commit(crate::tab::Commit {
                        navigation,
                        restore: None,
                        params: snowlark_page::CommitParams {
                            url,
                            begun: Some(begin.seq),
                            reload: false,
                            response: snowlark_page::CommitResponse::Direct,
                            violations: Vec::new(),
                            started: None,
                        },
                        body: None,
                        view_source: None,
                    });
                } else {
                    d.navigate(url);
                }
                continue;
            }
            let reload = begin.params.reload;
            let mut stream = d.ctx.net.fetch(begin.params.into_request());
            let response = loop {
                match stream.next_blocking() {
                    Some(snowlark_net::FetchEvent::Head(h)) => {
                        break snowlark_page::CommitResponse::Fetched { head: Box::new(h) }
                    }
                    Some(snowlark_net::FetchEvent::Error(e)) => {
                        break snowlark_page::CommitResponse::Failed(e)
                    }
                    Some(_) => {}
                    None => panic!("the fetch of {url} ended without a response"),
                }
            };
            let body = matches!(response, snowlark_page::CommitResponse::Fetched { .. })
                .then(|| d.ctx.net.hand_over(stream))
                .flatten();
            d.commit(crate::tab::Commit {
                navigation,
                restore: None,
                params: snowlark_page::CommitParams {
                    url,
                    begun: Some(begin.seq),
                    reload,
                    response,
                    violations: Vec::new(),
                    started: Some(Instant::now()),
                },
                body,
                view_source: None,
            });
        }
    }

    fn data_url(html: &str) -> Url {
        Url::parse(&format!("data:text/html,{}", html.replace('#', "%23"))).unwrap()
    }

    #[test]
    fn scripted_document_events_and_navigation() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(data_url(
            "<title>Start</title><body style='margin:0'><a id=l href='data:text/html,<title>Next</title>' \
             style='display:block;height:40px'>go</a><input id=i style='display:block;height:30px'>\
             <script>document.title = 'Scripted';</script>",
        ));
        settle(&mut d, |d| {
            d.info().title == "Scripted" && !d.info().loading
        });
        assert_eq!(d.info().title, "Scripted");
        assert_eq!(d.info().generation, 1);
        assert!(d.paint(Point::ZERO).is_some());
        // Typing into a clicked field.
        let none = Modifiers::default();
        d.pointer(Pointer::Down(
            Point::new(5.0, 55.0),
            MouseButton::Left,
            none,
            1,
        ));
        d.pointer(Pointer::Up(Point::new(5.0, 55.0), MouseButton::Left, none));
        for c in "ok".chars() {
            d.key(KeyInput {
                kind: KeyKind::Down { repeat: false },
                key: c.to_ascii_uppercase() as u16,
                scan: 0,
                text: Some(c),
                mods: none,
            });
            d.key(KeyInput {
                kind: KeyKind::Char(c),
                key: 0,
                scan: 0,
                text: Some(c),
                mods: none,
            });
        }
        assert_eq!(
            d.page()
                .eval("document.getElementById('i').value", "t")
                .unwrap(),
            "ok"
        );
        // A page key that nothing takes scrolls the viewport.
        d.pointer(Pointer::Down(
            Point::new(400.0, 300.0),
            MouseButton::Left,
            none,
            1,
        ));
        d.pointer(Pointer::Up(
            Point::new(400.0, 300.0),
            MouseButton::Left,
            none,
        ));
        let r = d.key(KeyInput {
            kind: KeyKind::Down { repeat: false },
            key: keys::PAGE_DOWN,
            scan: 0x151,
            text: None,
            mods: none,
        });
        assert!(matches!(r.scroll, Some(KeyScroll::By(_))));
        // A link click navigates; the new document is a committed session entry.
        let over = d.pointer(Pointer::Move(Point::new(5.0, 10.0)));
        assert_eq!(over.cursor, Cursor::Hand);
        d.pointer(Pointer::Down(
            Point::new(5.0, 10.0),
            MouseButton::Left,
            none,
            1,
        ));
        d.pointer(Pointer::Up(Point::new(5.0, 10.0), MouseButton::Left, none));
        settle(&mut d, |d| d.info().title == "Next");
        assert_eq!(d.info().title, "Next");
        assert_eq!(d.info().generation, 2);
        // (the link's navigation began in the browser, as the first load did: M5)
        assert_eq!(browsed(), 2);
        assert!(d.take_committed().is_empty());
    }

    /// The steps of a report on reddit, in the calls the window makes: hover opens a box over the
    /// whole viewport clipped to a strip (a tooltip's), a press on a field in a shadow tree with the
    /// pointer moving a little before the release, then typing.
    #[test]
    fn hover_then_a_press_reaches_a_field_in_a_shadow_tree() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(data_url(
            "<title>Start</title><style>body { margin: 0 }              #b { display: block; width: 100px; height: 40px }              #area { display: none; position: fixed; inset: 0; z-index: 5;              clip-path: polygon(0px 48px, 100px 48px, 120px 56px, 0px 56px) }</style>             <button id=b>Sign Up</button><div id=host style='margin-top:60px'></div>             <div id=area></div><script>             host.attachShadow({ mode: 'open' }).innerHTML = `<input id=f              style='display:block;width:300px;height:30px;margin:0;padding:0;border:0'>`;             b.addEventListener('mouseenter', () => { area.style.display = 'block' });             document.title = 'Ready';</script>",
        ));
        settle(&mut d, |d| d.info().title == "Ready" && !d.info().loading);
        assert_eq!(d.info().title, "Ready");
        let none = Modifiers::default();
        d.pointer(Pointer::Move(Point::new(50.0, 20.0)));
        d.update();
        let field = Point::new(50.0, 115.0);
        let over = d.pointer(Pointer::Move(field));
        assert_eq!(over.cursor, Cursor::IBeam);
        d.pointer(Pointer::Down(field, MouseButton::Left, none, 1));
        d.pointer(Pointer::Move(Point::new(52.0, 116.0)));
        d.pointer(Pointer::Move(Point::new(53.0, 20.0)));
        d.pointer(Pointer::Up(field, MouseButton::Left, none));
        for c in "ok".chars() {
            d.key(KeyInput {
                kind: KeyKind::Down { repeat: false },
                key: c.to_ascii_uppercase() as u16,
                scan: 0,
                text: Some(c),
                mods: none,
            });
            d.key(KeyInput {
                kind: KeyKind::Char(c),
                key: 0,
                scan: 0,
                text: Some(c),
                mods: none,
            });
        }
        assert_eq!(
            d.page()
                .eval("host.shadowRoot.getElementById('f').value", "t")
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn developer_tools_see_the_page() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(data_url(
            "<title>T</title><body style='margin:0'><h1 id=h style='margin:0;height:50px'>Hi</h1>\
             <p class=x>para</p><script>console.log('hello', 1); console.warn('w');\
             setTimeout(() => { missing(); }, 0);</script>",
        ));
        settle(&mut d, |d| !d.info().loading && d.info().title == "T");
        // Console messages come whether or not the tools are open.
        let mut console = Vec::new();
        let start = Instant::now();
        while console.len() < 3 && start.elapsed() < Duration::from_secs(10) {
            d.update();
            for e in d.take_devtools() {
                if let DevtoolsEvent::Console(c) = e {
                    console.extend(c);
                }
            }
        }
        let texts: Vec<(&str, &str)> = console.iter().map(|e| (e.level, e.text.as_str())).collect();
        assert_eq!(texts[0], ("log", "hello 1"));
        assert_eq!(texts[1], ("warn", "w"));
        assert_eq!(texts[2].0, "error");
        assert!(texts[2].1.contains("missing is not defined"), "{texts:?}");
        assert!(console[2].source.is_some());
        // Attached: the tree comes; evaluation answers in the page's realm.
        d.devtools(DevtoolsCommand::Attach(true));
        let events = d.take_devtools();
        let rows = events
            .iter()
            .find_map(|e| match e {
                DevtoolsEvent::Tree(r) => Some(r.clone()),
                _ => None,
            })
            .expect("the element tree");
        assert!(rows.iter().any(|r| matches!(&r.kind,
            snowlark_page::devtools::DomRowKind::Open { tag, .. } if tag == "h1")));
        d.devtools(DevtoolsCommand::Eval(7, "document.title + 1".into()));
        assert!(d.take_devtools().contains(&DevtoolsEvent::EvalResult(
            7,
            Ok(("T1".into(), snowlark_page::devtools::ValueKind::String))
        )));
        // The console's menu on results: Copy object (Chrome's `toStringForClipboard`), Copy
        // string contents, an element's markup, Store as global variable; none after a clear.
        let copied = |d: &mut PageDriver, id: u64, src: &str| {
            d.devtools(DevtoolsCommand::Eval(id, src.into()));
            d.take_devtools();
            d.devtools(DevtoolsCommand::Result(id, ResultOp::Copy));
            d.take_devtools().into_iter().find_map(|e| match e {
                DevtoolsEvent::Clipboard(t) => Some(t),
                _ => None,
            })
        };
        assert_eq!(
            copied(&mut d, 8, "({a: [1, 'x']})").as_deref(),
            Some("{\n    \"a\": [\n        1,\n        \"x\"\n    ]\n}")
        );
        assert_eq!(copied(&mut d, 9, "'q\\n'").as_deref(), Some("q\n"));
        assert_eq!(
            copied(&mut d, 10, "document.querySelector('h1')").as_deref(),
            Some("<h1 id=\"h\" style=\"margin:0;height:50px\">Hi</h1>")
        );
        // (a cycle: JSON.stringify throws, so String(value))
        assert_eq!(
            copied(&mut d, 11, "var o = {}; o.o = o; o").as_deref(),
            Some("[object Object]")
        );
        assert_eq!(copied(&mut d, 12, "5"), None);
        d.devtools(DevtoolsCommand::Result(8, ResultOp::Store));
        let stored = d.take_devtools();
        assert!(
            stored.iter().any(|e| matches!(e,
                DevtoolsEvent::Stored { name, .. } if name == "temp1")),
            "{stored:?}"
        );
        d.devtools(DevtoolsCommand::Eval(13, "temp1.a[1]".into()));
        assert!(d.take_devtools().contains(&DevtoolsEvent::EvalResult(
            13,
            Ok(("x".into(), snowlark_page::devtools::ValueKind::String))
        )));
        d.devtools(DevtoolsCommand::ClearResults);
        d.devtools(DevtoolsCommand::Result(9, ResultOp::Copy));
        assert!(!d
            .take_devtools()
            .iter()
            .any(|e| matches!(e, DevtoolsEvent::Clipboard(_))));
        // Pick mode: a move highlights the element under the pointer, a press picks it, and the
        // page sees neither.
        d.devtools(DevtoolsCommand::Pick(true));
        d.take_devtools();
        d.pointer(Pointer::Move(Point::new(10.0, 10.0)));
        let ev = d.take_devtools();
        let h = ev
            .iter()
            .find_map(|e| match e {
                DevtoolsEvent::Highlight(h) => h.clone(),
                _ => None,
            })
            .expect("a highlight");
        assert!(h.label.starts_with("h1#h"), "{}", h.label);
        assert_eq!(h.border.height, 50.0);
        let none = Modifiers::default();
        d.pointer(Pointer::Down(
            Point::new(10.0, 75.0),
            MouseButton::Left,
            none,
            1,
        ));
        let ev = d.take_devtools();
        assert!(
            ev.iter().any(|e| matches!(e, DevtoolsEvent::Picked(..))),
            "{ev:?}"
        );
        assert!(ev.iter().any(|e| matches!(e,
            DevtoolsEvent::Details(Some((_, det))) if det.title == "p.x")));
    }

    #[test]
    fn clearing_site_data_empties_its_session_storage() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        let url = Url::parse("https://www.example.com/").unwrap();
        d.page().load_html("<title>S</title>", &url);
        let set = "sessionStorage.setItem('s', '1'); sessionStorage.length";
        assert_eq!(d.page().eval(set, "t").unwrap(), "1");
        // Another site's data stays.
        d.clear_site_data(Some("other.org"));
        assert_eq!(d.page().eval("sessionStorage.length", "t").unwrap(), "1");
        // A host of the site clears the site.
        d.clear_site_data(Some("shop.example.com"));
        assert_eq!(d.page().eval("sessionStorage.length", "t").unwrap(), "0");
        d.page().eval(set, "t").unwrap();
        d.clear_site_data(None);
        assert_eq!(d.page().eval("sessionStorage.length", "t").unwrap(), "0");
    }

    #[test]
    fn only_the_settings_page_applies_settings() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        // A web page's navigations to the browser's addresses go nowhere.
        d.navigate(data_url(
            "<title>Web</title><script>location = 'snowlark://settings/apply?theme=dark';</script>",
        ));
        settle(&mut d, |d| d.info().title == "Web" && !d.info().loading);
        for _ in 0..20 {
            d.update();
        }
        assert!(d.take_internal_requests().is_empty());
        assert_eq!(d.info().title, "Web");
        // The settings page's own form applies.
        d.navigate(Url::parse("snowlark://settings/").unwrap());
        settle(&mut d, |d| d.info().title == "Settings");
        assert_eq!(d.info().title, "Settings");
        d.page()
            .eval("location = 'snowlark://settings/apply?theme=dark'", "t")
            .unwrap();
        settle(&mut d, |d| !d.internal_requests.is_empty());
        assert_eq!(
            d.take_internal_requests(),
            [InternalRequest::Settings("theme=dark".into())]
        );
        // A control of its form submits it.
        d.page()
            .eval("document.querySelector('input[value=light]').click()", "t")
            .unwrap();
        settle(&mut d, |d| !d.internal_requests.is_empty());
        let r = d.take_internal_requests();
        assert!(
            matches!(&r[..], [InternalRequest::Settings(q)] if q.contains("theme=light")),
            "{r:?}"
        );
        // The settings page can't drive the history page's form, nor the history page the
        // settings': each applies only its own.
        d.page()
            .eval("location = 'snowlark://history/apply?r=1-2+x'", "t")
            .unwrap();
        for _ in 0..20 {
            d.update();
        }
        assert!(d.take_internal_requests().is_empty());
        // (a link between the browser's pages, which the browser would begin)
        assert_eq!(d.take_begun_navigations().len(), 1);
        d.navigate(Url::parse("snowlark://history/").unwrap());
        settle(&mut d, |d| d.info().title == "History");
        d.page()
            .eval("location = 'snowlark://settings/apply?theme=dark'", "t")
            .unwrap();
        for _ in 0..20 {
            d.update();
        }
        assert!(d.take_internal_requests().is_empty());
        assert_eq!(d.take_begun_navigations().len(), 1);
        d.page()
            .eval("location = 'snowlark://history/apply?r=1-2+x'", "t")
            .unwrap();
        settle(&mut d, |d| !d.internal_requests.is_empty());
        assert_eq!(
            d.take_internal_requests(),
            [InternalRequest::History("r=1-2+x".into())]
        );
        // The bookmark manager's changes come only from the bookmark manager.
        d.page()
            .eval("location = 'snowlark://bookmarks/apply?op=undo'", "t")
            .unwrap();
        for _ in 0..20 {
            d.update();
        }
        assert!(d.take_internal_requests().is_empty());
        assert_eq!(d.take_begun_navigations().len(), 1);
        d.navigate(Url::parse("snowlark://bookmarks/").unwrap());
        settle(&mut d, |d| d.info().title == "Bookmarks");
        d.page()
            .eval("location = 'snowlark://history/apply?r=1-2+x'", "t")
            .unwrap();
        for _ in 0..20 {
            d.update();
        }
        assert!(d.take_internal_requests().is_empty());
        assert_eq!(d.take_begun_navigations().len(), 1);
        d.page()
            .eval("location = 'snowlark://bookmarks/apply?op=undo'", "t")
            .unwrap();
        settle(&mut d, |d| !d.internal_requests.is_empty());
        assert_eq!(
            d.take_internal_requests(),
            [InternalRequest::Bookmarks("op=undo".into())]
        );
        // The downloads page's actions come only from the downloads page.
        d.page()
            .eval("location = 'snowlark://downloads/apply?op=show&id=1'", "t")
            .unwrap();
        for _ in 0..20 {
            d.update();
        }
        assert!(d.take_internal_requests().is_empty());
        assert_eq!(d.take_begun_navigations().len(), 1);
        d.navigate(Url::parse("snowlark://downloads/").unwrap());
        settle(&mut d, |d| d.info().title == "Downloads");
        d.page()
            .eval("location = 'snowlark://downloads/apply?op=show&id=1'", "t")
            .unwrap();
        settle(&mut d, |d| !d.internal_requests.is_empty());
        assert_eq!(
            d.take_internal_requests(),
            [InternalRequest::Downloads("op=show&id=1".into())]
        );
        // The About page is a page of its own, not the settings page.
        d.navigate(Url::parse("snowlark://settings/help").unwrap());
        settle(&mut d, |d| d.info().title == "About Snowlark");
        assert_eq!(d.info().title, "About Snowlark");
        // The Credits page too, reached from a link of the About page.
        d.navigate(Url::parse("snowlark://credits").unwrap());
        settle(&mut d, |d| d.info().title == "Credits");
        assert_eq!(d.info().title, "Credits");
        // An address of no page shows an error page.
        d.navigate(Url::parse("snowlark://nothing/").unwrap());
        settle(&mut d, |d| d.info().error_page);
        assert!(d.info().error_page);
    }

    #[test]
    fn right_clicks_ask_for_the_context_menu_unless_canceled() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(data_url(
            "<title>C</title><body style='margin:0'><a href='https://a.example/x' \
             style='display:block;height:40px'>link</a><div id=n style='height:40px'>no</div>\
             <img src='https://a.example/i.png' style='display:block;width:40px;height:40px'>\
             <script>n.addEventListener('contextmenu', e => e.preventDefault());</script>",
        ));
        settle(&mut d, |d| d.info().title == "C" && !d.info().loading);
        let none = Modifiers::default();
        let right = |d: &mut PageDriver, y: f32| {
            let p = Point::new(10.0, y);
            d.pointer(Pointer::Down(p, MouseButton::Right, none, 1));
            d.pointer(Pointer::Up(p, MouseButton::Right, none));
            d.take_context_menu()
        };
        let link = right(&mut d, 20.0).expect("a menu over the link");
        assert_eq!(
            link.target.link.as_ref().map(|u| u.as_str()),
            Some("https://a.example/x")
        );
        assert!(link.target.image.is_none());
        // The page canceled it here.
        assert!(right(&mut d, 60.0).is_none());
        let img = right(&mut d, 100.0).expect("a menu over the image");
        assert_eq!(
            img.target.image.as_ref().map(|u| u.as_str()),
            Some("https://a.example/i.png")
        );
        // The keyboard's menu needs no event.
        d.context_menu_at(None);
        assert!(d.take_context_menu().is_some_and(|c| c.keyboard));
    }

    #[test]
    fn cursors() {
        assert_eq!(cursor_for("pointer"), Cursor::Hand);
        assert_eq!(cursor_for("url(x.cur), text"), Cursor::IBeam);
        assert_eq!(cursor_for("default"), Cursor::Arrow);
        assert_eq!(cursor_for("none"), Cursor::Hidden);
        assert_eq!(cursor_for("vertical-text"), Cursor::VerticalText);
        assert_eq!(cursor_for("alias"), Cursor::Alias);
        assert_eq!(cursor_for("copy"), Cursor::Copy);
        assert_eq!(cursor_for("context-menu"), Cursor::ContextMenu);
        assert_eq!(cursor_for("zoom-in"), Cursor::ZoomIn);
        assert_eq!(cursor_for("zoom-out"), Cursor::ZoomOut);
        assert_eq!(cursor_for("grab"), Cursor::Grab);
        assert_eq!(cursor_for("grabbing"), Cursor::Grabbing);
    }

    /// The window's resizes reach the page as a viewport in CSS px and a device pixel ratio, each on
    /// its own: after any number of them, `innerWidth` is the CSS px width, `devicePixelRatio` the
    /// scale, `vw` follows the width and `rem` the root element's font size (a page at 150% whose
    /// `rem` text grew to 1.6 times after a resize: bug 21).
    #[test]
    fn resizes_keep_viewport_and_scale_apart() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.set_device(Device {
            viewport: Size::new(800.0, 600.0),
            scale: 1.5,
            scrollbar: 0.0,
            screen: Default::default(),
        });
        d.navigate(data_url(
            "<style>html{font-size:10px} #vw{font-size:2vw} \
             #rem{font-size:1.6rem;max-width:90vw} #rem i{display:block;width:2.4rem;height:1px}\
             </style><p id=vw>a</p><div id=rem>b<i></i></div>\
             <script>document.title='ready'</script>",
        ));
        settle(&mut d, |d| d.info().title == "ready" && !d.info().loading);
        // (2vw within the 1/60 px of stylo's app units)
        let probe = "(f => [innerWidth, devicePixelRatio, \
                      Math.abs(parseFloat(f('#vw')) - innerWidth / 50) < 0.02, \
                      f('#rem'), document.querySelector('#rem i').getBoundingClientRect().width]\
                      .join())(s => getComputedStyle(document.querySelector(s)).fontSize)";
        for (i, w) in [803.0f32, 796.0, 1000.0, 1001.0, 640.0, 800.0]
            .into_iter()
            .enumerate()
        {
            d.set_device(Device {
                viewport: Size::new(w, 600.0),
                scale: 1.5,
                scrollbar: 0.0,
                screen: Default::default(),
            });
            for _ in 0..4 {
                d.update();
            }
            let r = d.page().eval(probe, "t").unwrap();
            assert_eq!(r, format!("{w},1.5,true,16px,24"), "resize {i}");
        }
    }

    /// Runs the driver until `done` holds of the counter, collecting the scrolls it asked for.
    fn find_until(
        d: &mut PageDriver,
        done: impl Fn(&FindStatus) -> bool,
    ) -> (FindStatus, Option<Point>) {
        let mut last = FindStatus::default();
        let mut scroll = None;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            let u = d.update();
            browse(d);
            scroll = u.scroll_to.or(scroll);
            if let Some(s) = d.take_find_status() {
                last = s;
            }
            if done(&last) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        (last, scroll)
    }

    fn highlights(d: &mut PageDriver) -> (usize, usize) {
        let list = d.paint(Point::ZERO).unwrap();
        let count = |c| {
            list.items
                .iter()
                .filter(
                    |i| matches!(i, snowlark_paint::Item::FillRect { color, .. } if *color == c),
                )
                .count()
        };
        (
            count(snowlark_layout::find::MATCH_COLOR),
            count(snowlark_layout::find::ACTIVE_COLOR),
        )
    }

    #[test]
    fn find_in_page_counts_highlights_scrolls_and_clears() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(data_url(
            "<title>Find</title><body style='margin:0'><p>top Needle</p>\
             <div style='height:3000px'></div><p>deep <b>need</b>le</p>\
             <script>document.title = 'ready'</script>",
        ));
        settle(&mut d, |d| d.info().title == "ready" && !d.info().loading);
        // Nothing runs for the find box until it opens.
        assert!(d.take_find_status().is_none());
        d.find(FindCommand::Open {
            previous: "needle".into(),
        });
        let (st, scroll) = find_until(&mut d, |s| s.total == 2 && !s.searching);
        assert_eq!(
            (st.query.as_str(), st.total, st.current),
            ("needle", 2, Some(0))
        );
        // (the first match is in view: no scroll)
        assert_eq!(scroll, None);
        assert_eq!(highlights(&mut d), (0, 1));
        // Next: the one 3000px down, across the <b>; the viewport is to scroll there.
        d.find(FindCommand::Next);
        let (st, scroll) = find_until(&mut d, |s| s.current == Some(1));
        assert_eq!(st.current, Some(1));
        assert!(scroll.is_some_and(|p| p.y > 2000.0), "{scroll:?}");
        // Wraps round, in both directions.
        d.find(FindCommand::Next);
        assert_eq!(
            find_until(&mut d, |s| s.current == Some(0)).0.current,
            Some(0)
        );
        d.find(FindCommand::Previous);
        assert_eq!(
            find_until(&mut d, |s| s.current == Some(1)).0.current,
            Some(1)
        );
        // Zero matches.
        d.find(FindCommand::Query("zebra".into()));
        let (st, _) = find_until(&mut d, |s| s.query == "zebra" && !s.searching);
        assert_eq!((st.total, st.current), (0, None));
        assert_eq!(highlights(&mut d), (0, 0));
        // The search leaves the page alone: nothing selected, no scroll of its own.
        assert_eq!(d.page().eval("getSelection().toString()", "t").unwrap(), "");
        // Closing selects the active match and clears the highlights; nothing runs after.
        d.find(FindCommand::Query("deep need".into()));
        find_until(&mut d, |s| s.query == "deep need" && s.total == 1);
        d.find(FindCommand::Close { select: true });
        assert_eq!(highlights(&mut d), (0, 0));
        assert_eq!(
            d.page().eval("getSelection().toString()", "t").unwrap(),
            "deep need"
        );
        assert!(d.take_find_status().is_none());
        d.find(FindCommand::Next);
        d.update();
        assert!(d.take_find_status().is_none());
    }

    #[test]
    fn find_in_page_opens_with_the_selected_text_and_follows_navigation() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(data_url(
            "<body><p id=p>alpha beta alpha</p>\
             <script>document.title = 'one'; const r = document.createRange();\
             r.setStart(document.getElementById('p').firstChild, 6); r.setEnd(document.getElementById('p').firstChild, 10);\
             getSelection().addRange(r);</script>",
        ));
        settle(&mut d, |d| d.info().title == "one" && !d.info().loading);
        d.find(FindCommand::Open {
            previous: "alpha".into(),
        });
        // (the page's selection wins over the last search)
        let (st, _) = find_until(&mut d, |s| s.total == 1 && !s.searching);
        assert_eq!(st.query, "beta");
        d.find(FindCommand::Query("alpha".into()));
        assert_eq!(
            find_until(&mut d, |s| s.query == "alpha" && s.total == 2)
                .0
                .total,
            2
        );
        // A new document: the search is run again on it when it has loaded.
        d.navigate(data_url(
            "<body><p>alpha</p><p>alpha</p><p>alpha</p><script>document.title = 'two'</script>",
        ));
        let (st, _) = find_until(&mut d, |s| {
            s.query == "alpha" && s.total == 3 && !s.searching
        });
        assert_eq!(st.total, 3);
        assert_eq!(highlights(&mut d).0 + highlights(&mut d).1, 3);
    }

    /// The element with this id, in the page the driver shows.
    fn element(d: &PageDriver, id: &str) -> NodeId {
        let engine = d.page.engine();
        let dom = engine.dom();
        let doc = engine.document().unwrap();
        dom.descendants(doc)
            .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some(id))
            .unwrap()
    }

    #[test]
    fn the_element_trees_menu_acts_on_the_page_through_the_tab() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let mut d = driver(&net);
        d.navigate(data_url(
            "<!doctype html><body><div id=box><p id=a>one</p><p id=b>two</p></div><div id=into></div>",
        ));
        settle(&mut d, |d| {
            d.info().generation >= 1 && !d.info().loading && d.page.engine().document().is_some()
        });
        d.devtools(DevtoolsCommand::Attach(true));
        d.take_devtools();
        // Cut: the markup goes to the clipboard and the element goes from the page.
        let b = element(&d, "b");
        d.devtools(DevtoolsCommand::Node(b, NodeOp::Cut));
        let events = d.take_devtools();
        assert!(
            events.contains(&DevtoolsEvent::Clipboard("<p id=\"b\">two</p>".into())),
            "{events:?}"
        );
        assert_eq!(
            d.page()
                .eval("document.getElementById('box').innerHTML", "t")
                .unwrap(),
            "<p id=\"a\">one</p>"
        );
        // Paste it into another element: it is picked, and its parent opens.
        let into = element(&d, "into");
        d.devtools(DevtoolsCommand::Node(
            into,
            NodeOp::Paste("<p id=\"b\">two</p>".into()),
        ));
        let events = d.take_devtools();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, DevtoolsEvent::Picked(..))),
            "{events:?}"
        );
        assert_eq!(
            d.page()
                .eval("document.getElementById('into').innerHTML", "t")
                .unwrap(),
            "<p id=\"b\">two</p>"
        );
        // Force state: reported back with the node, and the page's selectors see it.
        let a = element(&d, "a");
        d.devtools(DevtoolsCommand::Node(a, NodeOp::ForceState(1, true)));
        let events = d.take_devtools();
        assert!(
            events.contains(&DevtoolsEvent::Forced(a, 0b10)),
            "{events:?}"
        );
        assert_eq!(
            d.page()
                .eval("document.getElementById('a').matches(':hover')", "t")
                .unwrap(),
            "true"
        );
        d.devtools(DevtoolsCommand::Node(a, NodeOp::ForceState(1, false)));
        assert!(d.take_devtools().contains(&DevtoolsEvent::Forced(a, 0)));
        // Editing: the editor gets the markup, and the replacement comes back through the DOM.
        d.devtools(DevtoolsCommand::Node(a, NodeOp::BeginEdit(EditKind::Html)));
        let events = d.take_devtools();
        assert!(events.contains(&DevtoolsEvent::EditText {
            node: a,
            kind: EditKind::Html,
            text: "<p id=\"a\">one</p>".into()
        }));
        d.devtools(DevtoolsCommand::Node(
            a,
            NodeOp::SetOuterHtml("<h1 id=\"a\">big</h1>".into()),
        ));
        assert_eq!(
            d.page()
                .eval("document.getElementById('a').tagName", "t")
                .unwrap(),
            "H1"
        );
    }
}
