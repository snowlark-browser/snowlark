// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's half of a tab. A tab is two ends of a channel: the [`TabHost`] here, on the window's
//! thread, and the renderer ([`crate::tab`]) on a thread of its own, which owns the document. The
//! host keeps what must outlive a renderer and what the browser decides: the session history, which
//! navigation is the latest, and the state the window reads. It never waits for the renderer: it
//! sends commands ([`TabCommand`]) and takes what the renderer's events ([`TabEvent`]) say about the
//! session.
//!
//! A navigation begins here, whoever asked for it (`docs/process-isolation.md`, M5; Chromium's
//! `NavigationRequest`): the host makes the request through the browser's network, follows its
//! redirects there, and when the response's head is in ([`TabHost::navigation_ready`]) decides what
//! becomes of it (`NavigationRequest::OnResponseStarted`): a 204 or 205 and a download commit
//! nothing, the rest is handed to the renderer to commit ([`TabCommand::Commit`]), the body as a
//! stream the renderer adopts from its network. The page asks for its own navigations (a link, a
//! form, `location`: [`TabEvent::BeginNavigation`]) and the host begins them the same way; the
//! session history entry is the commit's. The pages the renderer serves itself (`about:home`,
//! `snowlark:`) are told to load as before ([`TabCommand::Navigate`]).
//!
//! The session history holds an entry per same-document commit too ([`TabEvent::SameDocument`]),
//! so Back and Forward within a document traverse in it ([`TabCommand::GoInDocument`]), and
//! `history.go()` asks the host ([`TabEvent::Traverse`]).
//!
//! A renderer that crashes takes nothing of this with it: the host starts a new one
//! ([`TabHost::respawn`]) and back, forward and the scroll positions go on.
//!
//! A renderer belongs to one site (M5 step 2; Chromium's `SiteInstance`, whose site is set at its
//! first commit, `SiteInstanceImpl::SetSite`): a navigation whose response is from another site
//! (`RenderFrameHostManager::ShouldSwapBrowsingInstancesForNavigation`: the current instance has a
//! site and `SiteInstanceImpl::IsNavigationSameSite` says no) commits in a new renderer
//! ([`TabHost::needs_swap`], [`TabHost::swap_renderer`]), which the window starts as it does after
//! a crash, and the old one closes; the session history stays with the host. A response with no
//! site of its own (an opaque origin: a `data:` document) stays where it is
//! (`RenderFrameHostManager::CanUseSourceSiteInstance`), as does a renderer that has no site yet
//! (it takes the first). The pages the browser serves itself (`about:home`, `snowlark:`) are a site
//! of their own, as Chromium's WebUI pages are (the same function: a WebUI URL is a security swap).
//!
//! The renderer runs on a thread of the browser's process ([`Renderer::Thread`], the default) or in
//! a process of its own (`--process-model=tab`, [`Renderer::Process`]; `docs/process-isolation.md`,
//! M4). The host is the same for both: commands go to a `Sender<TabCommand>` and events come back
//! through the context's sink.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use snowlark_ipc::{channel, Launch, Sender};
use snowlark_js::code_cache::CodeCacheStore;
use snowlark_media::TabMuteControl;
use snowlark_net::navigation::NavigationParams;
use snowlark_net::{FetchEvent, NetClient, ResponseHead, ResponseStream, Url};
use snowlark_page::embedder::{BeginNavigation, SameDocumentChange};
use snowlark_page::{CommitParams, CommitResponse};
use snowlark_paint::Point;
use snowlark_tab::renderer_driver::START_PAGE;
use snowlark_tab::renderer_process::{RendererProcess, Spare, WindowEnds};
use snowlark_tab::services::LocalServices;
use snowlark_tab::spare::SparePool;
use snowlark_tab::tab::{Commit, EventSink};

use crate::composite::{CompInput, SharedScroll};
use crate::driver::{Device, DriverFactory};
use crate::session::{SessionHistory, Trail};
use crate::tab::{TabCommand, TabContext, TabEvent, TabId, TabState};
use crate::watchdog::Liveness;

/// How long a closed tab's renderer process has to end by itself before it is killed.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Where the browser runs its tabs' renderers (`--process-model`). Each tab in a process of its own
/// by default (`docs/process-isolation.md`, M4); `single` keeps every tab on a thread of the
/// browser's process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessModel {
    /// On threads of the browser's process.
    Single,
    /// Each tab in a renderer process of its own.
    #[default]
    Tab,
}

impl ProcessModel {
    /// `single` or `tab`.
    pub fn parse(name: &str) -> Option<ProcessModel> {
        match name {
            "single" => Some(ProcessModel::Single),
            "tab" => Some(ProcessModel::Tab),
            _ => None,
        }
    }

    /// The window's model: the one `--process-model=` names (`flag`), else `SNOWLARK_PROCESS_MODEL`'s
    /// (`env`), else the default. A name that is neither model is the error.
    pub fn choose(flag: Option<&str>, env: Option<&str>) -> Result<ProcessModel, String> {
        match flag.or(env) {
            None => Ok(ProcessModel::default()),
            Some(name) => ProcessModel::parse(name).ok_or_else(|| name.to_owned()),
        }
    }
}

/// What runs a tab's renderer.
pub enum Renderer {
    /// A thread of this process, with the driver the factory makes.
    Thread(DriverFactory),
    /// A process of its own.
    Process(RemoteTab),
}

impl From<DriverFactory> for Renderer {
    fn from(factory: DriverFactory) -> Renderer {
        Renderer::Thread(factory)
    }
}

/// What a renderer process needs beyond the tab's context: how to start it (or the spare it takes),
/// the code cache it is served, and whether the tab starts muted.
pub struct RemoteTab {
    pub launch: Launch,
    pub code_cache: Arc<dyn CodeCacheStore>,
    pub muted: bool,
    /// A warm renderer to give the tab, instead of starting one (`docs/process-isolation.md` §11).
    pub spare: Option<Spare>,
}

/// The spare renderer pool for `model`: with `tab`, one renderer `launch` starts now and keeps
/// ready for the next tab; with `single`, none (nothing is started).
pub fn spare_pool(
    model: ProcessModel,
    launch: impl Fn() -> Launch + Send + Sync + 'static,
) -> Option<SparePool> {
    match model {
        ProcessModel::Single => None,
        ProcessModel::Tab => Some(SparePool::start(launch)),
    }
}

/// The code cache of a browser with no profile folder (or `SNOWLARK_CODE_CACHE=0`): it keeps nothing.
pub struct NoCodeCache;

impl CodeCacheStore for NoCodeCache {
    fn has(&self, _: &str) -> bool {
        false
    }

    fn read_async(
        &self,
        _: &str,
    ) -> std::sync::mpsc::Receiver<Option<snowlark_js::code_cache::Entry>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = tx.send(None);
        rx
    }

    fn store(&self, _: &str, _: snowlark_js::code_cache::CacheKind, _: [u8; 32], _: Vec<u8>) {}

    fn remove(&self, _: &str) {}
}

/// How the browser starts tab `tab`'s renderer process (`None`: a spare's, for a tab not known yet):
/// this executable with `--type=renderer`, and this process's own switches (`SNOWLARK_*`,
/// `RUST_BACKTRACE`: how pages run and what they print). The rest of the environment stays behind
/// (`snowlark_ipc::Launch`'s allow-list). A crash of the renderer leaves a minidump in `crash_dumps`.
pub fn renderer_launch(tab: Option<TabId>, crash_dumps: &std::path::Path) -> Launch {
    // (an executable that cannot be named does not start: the tab shows as crashed)
    let exe = std::env::current_exe().unwrap_or_default();
    let mut launch = Launch::new(exe)
        .arg("--type=renderer")
        .crash_dumps(crash_dumps);
    let crash_tab = std::env::var("SNOWLARK_TEST_CRASH_TAB").ok();
    for (name, _) in std::env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if passes_to_renderer(name, tab, crash_tab.as_deref()) {
            launch = launch.allow_env(name);
        }
    }
    // The renderer sandbox (`snowlark_sandbox`), unless `SNOWLARK_SANDBOX=0`: the renderer's sound
    // plays in the browser, and WebGL's device is opened before the lockdown.
    #[cfg(target_os = "linux")]
    {
        launch = launch.sandbox(std::env::var_os("SNOWLARK_SANDBOX").is_none_or(|v| v != "0"));
    }
    launch
}

/// Whether `SNOWLARK_TEST_CRASH_TAB` names tab `tab`: its renderer must be one started for it, which
/// the crash switches reach, not a spare.
pub fn crash_switch_names(tab: TabId) -> bool {
    std::env::var("SNOWLARK_TEST_CRASH_TAB")
        .is_ok_and(|only| only.trim().parse::<TabId>().is_ok_and(|only| only == tab))
}

/// Whether the variable `name` goes to tab `tab`'s renderer (`None`: a spare's). The testing
/// switches that crash a renderer (`SNOWLARK_TEST_CRASH`, `SNOWLARK_TEST_PANIC`) go only to the tab
/// `SNOWLARK_TEST_CRASH_TAB` names (`crash_tab`), when it names one (never to a spare then): a
/// restarted tab has a new id, so Reload works.
fn passes_to_renderer(name: &str, tab: Option<TabId>, crash_tab: Option<&str>) -> bool {
    // (the pipe's variable is the launch's own)
    if !(name.starts_with("SNOWLARK_") && name != "SNOWLARK_IPC_PIPE") && name != "RUST_BACKTRACE" {
        return false;
    }
    if matches!(name, "SNOWLARK_TEST_CRASH" | "SNOWLARK_TEST_PANIC") {
        if let Some(only) = crash_tab {
            return only
                .trim()
                .parse::<TabId>()
                .is_ok_and(|only| Some(only) == tab);
        }
    }
    true
}

/// The window's handle to a tab. Dropping it asks the renderer to stop (without waiting).
pub struct TabHost {
    pub id: TabId,
    /// What the window shows about the tab.
    pub state: TabState,
    tx: Sender<TabCommand>,
    thread: Option<JoinHandle<()>>,
    session: SessionHistory,
    /// Numbers the navigations sent to the renderer ([`TabState::navigations`]).
    navigations: u64,
    /// Counts the documents the tab's top level has had: each commit of a new one, whichever
    /// renderer it is in.
    documents: u64,
    /// The top level's current document came from an HTTP error response (status 400 or more).
    http_error: bool,
    /// The compositor's scroll positions: where the viewport is when a navigation leaves an entry.
    scroll: SharedScroll,
    /// Whether the renderer's thread gets anywhere, and the way to stop its script.
    live: Arc<Liveness>,
    /// The renderer's process, when it has one.
    process: Option<RendererProcess>,
    /// The tab's mute: its players' group, or the handle on its renderer process's.
    audio: Option<Arc<dyn TabMuteControl>>,
    /// The browser's network, which the host fetches navigations through (`None`: a host of a
    /// test with no network, whose renderer loads by itself).
    net: Option<NetClient>,
    /// Where the host's own events go (the window's channel), to hear of a response.
    events: Option<Arc<dyn EventSink>>,
    /// The navigation begun and not decided on yet.
    pending: Option<Navigation>,
    /// A response that is a download, for the window to save.
    download: Option<Download>,
    /// The site the renderer's documents are of, once one with a site committed: what a new
    /// response is compared with (Chromium's `SiteInstanceImpl::GetSiteInfo`; `None`: `HasSite()`
    /// false, the renderer takes the first site that commits).
    site: Option<String>,
    /// A navigation decided on that commits in a new renderer, waiting for the window to start it
    /// ([`TabHost::swap_renderer`]).
    swap: Option<Swap>,
    /// The tab shows a window its opener's page opened ([`TabHost::view`]): the opener's renderer
    /// serves it, and (in a renderer process) speaks for it while this is held.
    view: Option<Option<snowlark_tab::renderer_process::ViewHandle>>,
    /// A view's renderer process: its opener's ([`TabHost::view`]).
    view_process: Option<Arc<snowlark_ipc::Process>>,
    /// The TLS session (the verified certificate chain) of the response the top level's document
    /// was made from, with that response's URL: taken from the navigation's response and kept
    /// here, never asked of the renderer.
    tls: Option<(Url, Arc<snowlark_net::TlsInfo>)>,
}

/// A navigation that commits in a renderer the tab does not have yet: the new renderer's site,
/// and what to send it.
struct Swap {
    site: Option<String>,
    then: SwapThen,
}

enum SwapThen {
    /// A fetched response to commit ([`TabCommand::Commit`]).
    Commit(Box<(Navigation, ResponseHead)>),
    /// A page the renderer serves itself ([`TabCommand::Navigate`]).
    Load {
        url: Url,
        restore: Option<Point>,
        reload: bool,
    },
}

/// A navigation the host began and has not committed: Chromium's `NavigationRequest` between
/// `BeginNavigation` and `OnResponseStarted`.
struct Navigation {
    /// The host's number for it ([`TabState::navigations`]).
    number: u64,
    /// The URL navigated to (a `view-source:` address: the one shown).
    url: Url,
    /// The page's own request ([`BeginNavigation::seq`]).
    begun: Option<u64>,
    restore: Option<Point>,
    reload: bool,
    /// What the session history did at the start, to take back if nothing commits.
    entry: EntryAt,
    stream: ResponseStream,
    violations: Vec<snowlark_net::csp::Violation>,
    started: Instant,
    /// The navigation is to a `view-source:` address (the fetch is of what it names).
    view_source: bool,
}

/// What the session history did when a navigation began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryAt {
    /// Nothing yet: the page's own navigation gets its entry at the commit (Chromium shows the
    /// old URL until a renderer-initiated navigation commits).
    Commit,
    /// The user's navigation pushed its entry (the address bar shows it while it loads).
    Pushed,
    /// The user's navigation is to the current entry (a reload, the same address again).
    Kept,
    /// A traversal moved the index from `from`.
    Moved(usize),
}

/// A navigation's response that is a download (`Content-Disposition: attachment`, or a type the
/// engine does not render): the window saves it where the user says.
pub struct Download {
    pub url: Url,
    pub head: ResponseHead,
    pub stream: ResponseStream,
}

impl TabHost {
    /// Starts a tab whose document `factory` makes, loading `url`.
    pub fn spawn(
        id: TabId,
        context: TabContext,
        scroll: SharedScroll,
        device: Device,
        url: Url,
        renderer: impl Into<Renderer>,
    ) -> TabHost {
        let mut tab = TabHost::start(id, context, scroll, device, renderer.into());
        tab.enter(url);
        tab
    }

    /// Starts a tab that loads nothing until it is told to (a restored tab, first shown later).
    pub fn spawn_idle(
        id: TabId,
        context: TabContext,
        scroll: SharedScroll,
        device: Device,
        renderer: impl Into<Renderer>,
    ) -> TabHost {
        TabHost::start(id, context, scroll, device, renderer.into())
    }

    fn start(
        id: TabId,
        context: TabContext,
        scroll: SharedScroll,
        device: Device,
        renderer: Renderer,
    ) -> TabHost {
        let live = Arc::new(Liveness::default());
        let compositor = context.compositor.clone();
        let events = context.events.clone();
        let net = context.net.clone();
        let mut host = match renderer {
            Renderer::Thread(factory) => {
                let (tx, rx) = channel();
                let _ = compositor.send(CompInput::bind(id, tx.clone()));
                let audio = context.audio.clone().map(|a| a as Arc<dyn TabMuteControl>);
                let thread = crate::tab::spawn_renderer(
                    id,
                    context,
                    tx.clone(),
                    rx,
                    device,
                    factory,
                    live.clone(),
                );
                let mut host = TabHost::new(id, tx, Some(thread), scroll, live);
                host.audio = audio;
                host
            }
            Renderer::Process(remote) => match start_process(id, context, device, remote) {
                Ok(process) => {
                    let tx = process.sender();
                    let _ = compositor.send(CompInput::bind(id, tx.clone()));
                    // (the renderer's passes, in the section it shares with the browser)
                    let live = process.liveness().clone();
                    let mut host = TabHost::new(id, tx, None, scroll, live);
                    host.audio = Some(process.audio().clone() as Arc<dyn TabMuteControl>);
                    host.process = Some(process);
                    host
                }
                Err(e) => {
                    // A renderer that did not start is a tab that crashed: the window shows it
                    // so, and Reload tries again. (Its commands go nowhere.)
                    let (tx, _) = channel();
                    let why = format!("the renderer process did not start: {e}");
                    events.send(id, TabEvent::Crashed(why));
                    TabHost::new(id, tx, None, scroll, live)
                }
            },
        };
        host.net = Some(net);
        host.events = Some(events);
        host
    }

    fn new(
        id: TabId,
        tx: Sender<TabCommand>,
        thread: Option<JoinHandle<()>>,
        scroll: SharedScroll,
        live: Arc<Liveness>,
    ) -> TabHost {
        TabHost {
            id,
            state: TabState::default(),
            tx,
            thread,
            session: SessionHistory::default(),
            navigations: 0,
            documents: 0,
            http_error: false,
            scroll,
            live,
            process: None,
            audio: None,
            net: None,
            events: None,
            pending: None,
            download: None,
            site: None,
            swap: None,
            view: None,
            view_process: None,
            tls: None,
        }
    }

    /// Tab `id`, showing window `window` that `opener`'s page opened (its
    /// [`TabEvent::OpenWindow`]): a view of that page, which the opener's renderer serves. Its
    /// commands go to that renderer as [`TabCommand::View`] (through a thread of its own, so the
    /// window's call returns at once), the compositor sends it its own, and its frames reach the
    /// compositor as the tab's. It has no network of its own: its navigations are the window's,
    /// which the page makes (the window's document stays in its opener's renderer, behind the
    /// page's cross-origin access checks). A view outlives its opener's tab: the renderer goes
    /// on serving it when the opener's tab closes or moves to another renderer (its process lives
    /// while any of its views does, [`RendererProcess::release`]); it closes its views
    /// ([`TabEvent::CloseWindow`]) or reports them crashed with it.
    pub fn view(
        id: TabId,
        opener: &TabHost,
        window: u64,
        compositor: &std::sync::mpsc::Sender<CompInput>,
        scroll: SharedScroll,
        device: Device,
    ) -> TabHost {
        let (tx, rx) = channel::<TabCommand>();
        let to_opener = opener.tx.clone();
        // (no thread: commands go nowhere, and the tab closes with the opener's renderer)
        let _ = std::thread::Builder::new()
            .name(format!("snowlark view {id}"))
            .spawn(move || {
                for command in rx {
                    let command = TabCommand::View {
                        tab: id,
                        command: Box::new(command),
                    };
                    if to_opener.send(command).is_err() {
                        return;
                    }
                }
            });
        let _ = compositor.send(CompInput::bind(id, tx.clone()));
        // (the renderer speaks for the tab from now on: before it hears of it)
        let handle = opener.process.as_ref().map(|p| p.attach_view(id));
        let mut host = TabHost::new(id, tx, None, scroll, opener.live.clone());
        host.audio = opener.audio.clone();
        host.view = Some(handle);
        host.view_process = opener.process.as_ref().map(|p| p.process().clone());
        opener.send(TabCommand::AttachView {
            window,
            tab: id,
            device,
        });
        host
    }

    /// Whether the tab shows a window its opener's page opened ([`TabHost::view`]).
    pub fn is_view(&self) -> bool {
        self.view.is_some()
    }

    /// The tab's mute (`None` for a renderer process that did not start).
    pub fn audio(&self) -> Option<Arc<dyn TabMuteControl>> {
        self.audio.clone()
    }

    /// The processor time the tab's thread has used so far: for a tab on a thread of the browser's
    /// process (`None` for one in a renderer process, or whose thread has ended).
    pub fn thread_cpu(&self) -> Option<std::time::Duration> {
        self.thread.as_ref().and_then(snowlark_ipc::thread_cpu)
    }

    /// Whether the tab's thread (one in the browser's process) has ended.
    #[cfg(test)]
    pub(crate) fn thread_ended(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// The tab's renderer process, when it runs in one (its id, and a way to end it): a view's is
    /// its opener's.
    pub fn renderer_process(&self) -> Option<&Arc<snowlark_ipc::Process>> {
        self.process
            .as_ref()
            .map(|p| p.process())
            .or(self.view_process.as_ref())
    }

    pub fn send(&self, command: TabCommand) {
        if crate::tab::is_input(&command) {
            self.live.input(Instant::now());
        }
        let _ = self.tx.send(command);
    }

    /// The tab's liveness: whether its thread is stuck in a pass, and how to stop its script.
    pub fn liveness(&self) -> &Liveness {
        &self.live
    }

    /// The user's Kill of a stuck tab: stops its script, which keeps the page. A renderer process
    /// whose pass that does not end is ended ([`RendererProcess::kill_script`]).
    pub fn kill_script(&self) {
        match &self.process {
            Some(p) => p.kill_script(),
            None => {
                self.live.kill();
            }
        }
    }

    /// Where the compositor has the viewport (nowhere until a frame of this renderer showed).
    fn scroll(&self) -> Option<Point> {
        self.scroll.lock().unwrap().get(&self.id).map(|v| v.scroll)
    }

    /// The current entry keeps where the viewport is, as it is left.
    fn save_scroll(&mut self) {
        if let Some(p) = self.scroll() {
            self.session.save_scroll(p);
        }
    }

    fn load(&self, url: Url, restore: Option<Point>, reload: bool) {
        self.send(TabCommand::Navigate {
            url,
            restore,
            navigation: self.navigations,
            reload,
        });
    }

    /// The user's navigation to `url`: the entry first (the address bar shows it while it
    /// loads), then the navigation.
    fn enter(&mut self, url: Url) {
        self.save_scroll();
        // HTML §7.4.2.2 "navigate": to the URL of the document shown, the history handling is
        // "replace" (no second entry for the same page).
        let entry = if self.session.current().is_none_or(|e| e.url != url) {
            self.session.push(url.clone());
            EntryAt::Pushed
        } else {
            EntryAt::Kept
        };
        self.begin(NavigationParams::user(url), None, None, entry);
    }

    /// Goes to `url`.
    pub fn navigate(&mut self, url: Url) {
        self.navigations += 1;
        self.enter(url);
    }

    /// Loads the current entry again, keeping the scroll position.
    pub fn reload(&mut self) {
        if let Some(e) = self.session.current() {
            let (url, saved) = (e.url.clone(), e.scroll);
            self.navigations += 1;
            let restore = Some(self.scroll().unwrap_or(saved));
            let mut params = NavigationParams::user(url);
            params.reload = true;
            self.begin(params, None, restore, EntryAt::Kept);
        }
    }

    /// Back (−1) or forward (+1) in the session history. To an entry of the document shown, the
    /// document traverses in place ([`TabCommand::GoInDocument`]); else the entry loads.
    pub fn go(&mut self, delta: isize) {
        if !self.session.can_go(delta) {
            return;
        }
        self.save_scroll();
        let from = self.session.index();
        let from_document = self.session.current().map_or(0, |e| e.document);
        let Some(entry) = self.session.go(delta) else {
            return;
        };
        let (url, scroll, document, item) =
            (entry.url.clone(), entry.scroll, entry.document, entry.item);
        self.navigations += 1;
        if document != 0 && document == from_document && self.pending.is_none() {
            self.state.url = url.to_string();
            self.send(TabCommand::GoInDocument {
                url,
                index: item,
                navigation: self.navigations,
            });
            return;
        }
        self.begin(
            NavigationParams::user(url),
            None,
            Some(scroll),
            EntryAt::Moved(from),
        );
    }

    /// Starts from this history (a session restored after an unexpected exit): its current entry
    /// loads, scrolled to where it was.
    pub fn restore(&mut self, trail: Trail) {
        self.session = SessionHistory::from_trail(trail);
        if let Some(e) = self.session.current() {
            let (url, scroll) = (e.url.clone(), e.scroll);
            self.navigations += 1;
            self.begin(
                NavigationParams::user(url),
                None,
                Some(scroll),
                EntryAt::Kept,
            );
        }
    }

    /// The user's Stop: what is loading stops, a navigation not committed yet included.
    pub fn stop(&mut self) {
        if let Some(nav) = self.pending.take() {
            self.abandon(nav);
        }
        self.send(TabCommand::Stop);
    }

    /// Begins a navigation (Chromium's `NavigationRequest::BeginNavigation`): a fetchable URL is
    /// fetched here, and the response decided on when its head is in
    /// ([`TabHost::navigation_ready`]); the pages the renderer serves itself are told to load.
    fn begin(
        &mut self,
        params: NavigationParams,
        begun: Option<u64>,
        restore: Option<Point>,
        entry: EntryAt,
    ) {
        // (a navigation not decided on yet is replaced: its fetch ends with its stream)
        if let Some(earlier) = self.pending.take() {
            self.take_back(earlier.entry);
        }
        let url = params.url.clone();
        let reload = params.reload;
        let view_source = (url.scheme() == "view-source")
            .then(|| crate::view_source::inner_url(&url))
            .flatten();
        let fetchable = view_source.is_some()
            || matches!(url.scheme(), "http" | "https" | "file" | "data" | "blob");
        let (Some(net), Some(events)) = (&self.net, &self.events) else {
            // (no network of the host's: the renderer loads by itself, as it did before M5)
            if entry == EntryAt::Commit {
                self.enter_now(&url);
            }
            self.new_document(self.navigations);
            self.load(url, restore, reload);
            return;
        };
        let (net, events) = (net.clone(), events.clone());
        if !fetchable {
            if entry == EntryAt::Commit {
                self.enter_now(&url);
            }
            self.new_document(self.navigations);
            // An `about:` document the engine makes (its origin is its initiator's); the rest
            // the renderer serves (the start page, `snowlark:`, an unsupported scheme's error).
            let own = url.scheme() == "about" && url.as_str() != START_PAGE;
            if !own {
                // The browser's own pages are a site of their own: a web renderer never shows
                // one, nor their renderer a web page (Chromium's WebUI swap).
                let site = site_for(&url);
                if self.needs_new_renderer(site.as_deref()) {
                    self.state.url = url.to_string();
                    self.state.loading = true;
                    self.swap = Some(Swap {
                        site,
                        then: SwapThen::Load {
                            url,
                            restore,
                            reload,
                        },
                    });
                    // (the window hears of the swap as it hears of a response)
                    events.send(self.id, TabEvent::NavigationReady(self.navigations));
                    return;
                }
                self.take_site(site);
            }
            if own {
                self.state.url = url.to_string();
                self.state.loading = true;
                self.send(TabCommand::Commit(Box::new(Commit {
                    navigation: self.navigations,
                    restore,
                    params: CommitParams {
                        url,
                        begun,
                        reload,
                        response: CommitResponse::Direct,
                        violations: Vec::new(),
                        started: Some(Instant::now()),
                    },
                    body: None,
                    view_source: None,
                })));
            } else {
                self.load(url, restore, reload);
            }
            return;
        }
        let mut params = params;
        if let Some(inner) = &view_source {
            params.url = inner.clone();
        }
        let request = params.into_request();
        let number = self.navigations;
        let (id, sink) = (self.id, events);
        let notify: Arc<dyn Fn() + Send + Sync> =
            Arc::new(move || sink.send(id, TabEvent::NavigationReady(number)));
        let stream = net.fetch_with_notify(request, notify);
        self.pending = Some(Navigation {
            number,
            url: url.clone(),
            begun,
            restore,
            reload,
            entry,
            stream,
            violations: Vec::new(),
            started: Instant::now(),
            view_source: view_source.is_some(),
        });
        if entry != EntryAt::Commit {
            self.state.url = url.to_string();
            self.state.loading = true;
        }
    }

    /// The page's own navigation's entry, pushed when it commits (or loads) rather than when it
    /// begins.
    fn enter_now(&mut self, url: &Url) {
        self.save_scroll();
        if self.session.current().is_none_or(|e| e.url != *url) {
            self.session.push(url.clone());
        }
    }

    /// Undoes what the session history did when a navigation began that commits nothing.
    fn take_back(&mut self, entry: EntryAt) {
        match entry {
            EntryAt::Commit | EntryAt::Kept => {}
            EntryAt::Pushed => self.session.remove_current(),
            EntryAt::Moved(from) => self.session.set_index(from),
        }
    }

    /// [`TabEvent::NavigationReady`]: the navigation numbered `number` has events: its head, or
    /// its failure. Decides as Chromium's `NavigationRequest::OnResponseStarted` and
    /// `OnRequestFailedInternal` do.
    pub fn navigation_ready(&mut self, number: u64) {
        let Some(nav) = self.pending.as_mut() else {
            // (or a swap the host asked for: the window takes it after this)
            return;
        };
        if nav.number != number {
            return;
        }
        loop {
            match nav.stream.try_next() {
                None => return,
                Some(FetchEvent::Violation(v)) => nav.violations.push(v),
                Some(FetchEvent::Head(head)) => {
                    let nav = self.pending.take().expect("the navigation");
                    self.respond(nav, head);
                    return;
                }
                Some(FetchEvent::Error(e)) => {
                    let nav = self.pending.take().expect("the navigation");
                    self.fail(nav, e);
                    return;
                }
                // (a body before a head: not an event the network sends)
                Some(FetchEvent::Body(_)) | Some(FetchEvent::Done) => return,
            }
        }
    }

    /// The response's head is in. A 204 or 205 and a download commit nothing
    /// (`NavigationRequest::OnResponseStarted`, `response_should_be_rendered_`); the rest the
    /// renderer commits, the body handed over as a stream.
    fn respond(&mut self, nav: Navigation, head: ResponseHead) {
        if head.status == 204 || head.status == 205 {
            self.abandon(nav);
            return;
        }
        let mime = snowlark_net::mime::extract_mime_type(&head.headers)
            .map(|m| m.essence())
            .unwrap_or_default();
        if !nav.view_source && is_download(&head, &mime) {
            let url = head.url().cloned().unwrap_or_else(|| nav.url.clone());
            self.download = Some(Download {
                url,
                head,
                stream: nav.stream,
            });
            self.take_back(nav.entry);
            self.loaded_state();
            return;
        }
        // The renderer is chosen by the response's site, after redirects
        // (`NavigationRequest::OnResponseStarted`, `SelectFrameHostForOnResponseStarted`).
        let site = site_for(head.url().unwrap_or(&nav.url));
        if self.needs_new_renderer(site.as_deref()) {
            self.swap = Some(Swap {
                site,
                then: SwapThen::Commit(Box::new((nav, head))),
            });
            return;
        }
        self.take_site(site);
        self.commit_fetched(nav, head);
    }

    /// Whether a document of `site` needs a renderer other than this one: the renderer has a
    /// site and the document has another (Chromium's
    /// `ShouldSwapBrowsingInstancesForNavigation`, "cross-site navigation" after
    /// `CanUseSourceSiteInstance`: a document with no site of its own stays).
    fn needs_new_renderer(&self, site: Option<&str>) -> bool {
        match (&self.site, site) {
            (Some(have), Some(want)) => have != want,
            _ => false,
        }
    }

    /// The renderer's site is `site`, if it had none (`SiteInstanceImpl::SetSite` at the first
    /// commit of a navigation with a site).
    fn take_site(&mut self, site: Option<String>) {
        if self.site.is_none() {
            self.site = site;
        }
        self.lock_renderer();
    }

    /// Locks the renderer process to the tab's site (`ProcessPolicy::lock`), once there is one:
    /// what the browser's services check the renderer's requests against. A renderer on a thread
    /// has no lock.
    fn lock_renderer(&self) {
        if let (Some(site), Some(process)) = (&self.site, &self.process) {
            if let Err(e) = process.policy().lock.lock_to(site) {
                eprintln!("[site-lock] tab {:?}: {e}", self.id);
            }
        }
    }

    /// Commits a fetched response in the tab's renderer: the entry, then the body handed over.
    fn commit_fetched(&mut self, nav: Navigation, mut head: ResponseHead) {
        self.commit_entry(&nav, head.url());
        self.http_error = head.status >= 400;
        // (the session is the browser's: the renderer gets the head without it)
        self.tls = head
            .connection
            .tls
            .take()
            .map(|t| (head.url().cloned().unwrap_or_else(|| nav.url.clone()), t));
        let body = self.hand_over(nav.stream);
        let view_source = nav.view_source.then(|| nav.url.clone());
        self.send(TabCommand::Commit(Box::new(Commit {
            navigation: nav.number,
            restore: nav.restore,
            params: CommitParams {
                url: nav.url,
                begun: nav.begun,
                reload: nav.reload,
                response: CommitResponse::Fetched {
                    head: Box::new(head),
                },
                violations: nav.violations,
                started: Some(nav.started),
            },
            body,
            view_source,
        })));
    }

    /// The fetch failed before a response: the renderer commits an error page
    /// (`NavigationRequest::OnRequestFailedInternal`, `CommitErrorPage`).
    fn fail(&mut self, nav: Navigation, error: snowlark_net::NetError) {
        self.commit_entry(&nav, None);
        self.tls = None;
        let view_source = nav.view_source.then(|| nav.url.clone());
        self.send(TabCommand::Commit(Box::new(Commit {
            navigation: nav.number,
            restore: nav.restore,
            params: CommitParams {
                url: nav.url,
                begun: nav.begun,
                reload: nav.reload,
                response: CommitResponse::Failed(error),
                violations: nav.violations,
                started: Some(nav.started),
            },
            body: None,
            view_source,
        })));
    }

    /// A navigation that commits nothing: the tab stays as it was.
    fn abandon(&mut self, nav: Navigation) {
        self.take_back(nav.entry);
        self.loaded_state();
    }

    /// The state after a navigation that commits nothing: what the current entry is.
    fn loaded_state(&mut self) {
        self.state.loading = false;
        if let Some(e) = self.session.current() {
            self.state.url = e.url.to_string();
        }
    }

    /// The session history at a commit: the entry the navigation began with takes the response's
    /// URL (redirects); the page's own navigation gets its entry now.
    fn commit_entry(&mut self, nav: &Navigation, final_url: Option<&Url>) {
        // (a `view-source:` entry keeps its address, whatever the source's URL became)
        let url = final_url
            .filter(|_| !nav.view_source)
            .cloned()
            .unwrap_or_else(|| nav.url.clone());
        if nav.entry == EntryAt::Commit {
            self.enter_now(&url);
        } else if self.session.current().is_some_and(|e| e.url != url) {
            self.session.replace_url(url.clone());
        }
        self.new_document(nav.number);
        self.state.url = url.to_string();
        self.state.loading = true;
    }

    /// Hands the rest of a response to the renderer: the id its network adopts it under.
    fn hand_over(&self, stream: ResponseStream) -> Option<u64> {
        match &self.process {
            Some(p) => Some(p.hand_over(stream)),
            None => self.net.as_ref().and_then(|n| n.hand_over(stream)),
        }
    }

    /// The TLS session the shown document's response came over, with the certificate chain the
    /// network verified: while the tab shows a URL of that response's origin (a same-document
    /// navigation keeps it), and never for an error page or a page the renderer made itself.
    pub fn tls(&self) -> Option<&Arc<snowlark_net::TlsInfo>> {
        let (url, tls) = self.tls.as_ref()?;
        if self.state.error_page {
            return None;
        }
        let shown = Url::parse(&self.state.url).ok()?;
        (shown.scheme() == "https" && shown.origin() == url.origin()).then_some(tls)
    }

    /// Whether a navigation the host began is not decided on yet (its response is awaited).
    pub fn navigating(&self) -> bool {
        self.pending.is_some()
    }

    /// A response that was a download, once ([`Download`]).
    pub fn take_download(&mut self) -> Option<Download> {
        self.download.take()
    }

    /// [`TabEvent::BeginNavigation`]: the page asks for a top-level navigation. `javascript:`
    /// never navigates (Chromium's `CanRedirectToURL` and `BeginNavigation` drop it).
    pub fn begin_navigation(&mut self, begin: BeginNavigation) {
        if begin.params.url.scheme() == "javascript" {
            return;
        }
        self.navigations += 1;
        let begun = (begin.seq != 0).then_some(begin.seq);
        self.begin(begin.params, begun, None, EntryAt::Commit);
    }

    /// [`TabEvent::SameDocument`]: the document of entry `navigation` changed its URL in place:
    /// an entry for it (`pushState`, a fragment), or the current one's URL (`replaceState`).
    pub fn same_document(&mut self, change: SameDocumentChange, navigation: u64) {
        let Some(current) = self.session.current() else {
            return;
        };
        if current.document != navigation {
            return;
        }
        let Ok(url) = Url::parse(&change.url) else {
            return;
        };
        if change.push {
            self.save_scroll();
            self.session
                .push_same_document(url.clone(), navigation, change.index);
        } else {
            self.session.replace_url(url.clone());
            self.session.set_document(navigation, change.index);
        }
        if self.pending.is_none() {
            self.state.url = url.to_string();
        }
    }

    /// The renderer crashed: the current entry keeps where the view was, before the compositor
    /// forgets it with the tab's frames.
    pub fn crashed(&mut self) {
        self.save_scroll();
    }

    /// A new renderer in place of one that crashed, numbered `id`. It loads nothing until it is
    /// told to; the session history stays (Reload loads the current entry where it was).
    pub fn respawn(
        &mut self,
        id: TabId,
        context: TabContext,
        device: Device,
        renderer: impl Into<Renderer>,
    ) {
        // (a navigation the crashed renderer was to commit is lost with it)
        self.swap = None;
        self.replace_renderer(id, context, device, renderer.into());
        // The documents went with the renderer: no entry traverses in place in the new one, whose
        // first history step loads its entry, as a reload does (a history step is to another
        // document while the renderer has committed none).
        self.session.forget_documents();
    }

    /// Whether a navigation decided on waits for a new renderer ([`TabHost::swap_renderer`]): its
    /// response is from another site than the renderer's.
    pub fn needs_swap(&self) -> bool {
        self.swap.is_some()
    }

    /// The new renderer for the navigation that waits for one, numbered `id`: the navigation
    /// commits in it (the old renderer closes with the host it had: its session history is this
    /// host's). Chromium's `RenderFrameHostManager::CommitPending` and `UnloadOldFrame`: the old
    /// frame is unloaded, and its process ends when it has no frames left; here the engine fires
    /// no unload events, so the old renderer is closed.
    pub fn swap_renderer(
        &mut self,
        id: TabId,
        context: TabContext,
        device: Device,
        renderer: impl Into<Renderer>,
    ) {
        let Some(swap) = self.swap.take() else {
            return;
        };
        self.replace_renderer(id, context, device, renderer.into());
        self.site = swap.site;
        self.lock_renderer();
        match swap.then {
            SwapThen::Commit(c) => {
                let (nav, head) = *c;
                self.commit_fetched(nav, head)
            }
            SwapThen::Load {
                url,
                restore,
                reload,
            } => self.load(url, restore, reload),
        }
    }

    /// A new renderer for the tab: the host's session history, navigation count and what waits
    /// for a renderer go on; the old renderer closes as the old host drops.
    fn replace_renderer(
        &mut self,
        id: TabId,
        context: TabContext,
        device: Device,
        renderer: Renderer,
    ) {
        let scroll = self.scroll.clone();
        let mut new = TabHost::start(id, context, scroll, device, renderer);
        new.session = std::mem::take(&mut self.session);
        new.navigations = self.navigations;
        new.documents = self.documents;
        new.swap = self.swap.take();
        new.state = TabState {
            url: self.state.url.clone(),
            loading: self.state.loading,
            ..Default::default()
        };
        *self = new;
    }

    pub fn can_go(&self, delta: isize) -> bool {
        self.session.can_go(delta)
    }

    /// A copy of the back/forward history (what the session file keeps).
    pub fn trail(&self) -> Trail {
        self.session.trail()
    }

    /// How many entries the back/forward history has.
    pub fn history_len(&self) -> usize {
        self.session.len()
    }

    /// The site the tab's renderer is locked to (`None`: none yet).
    pub fn site(&self) -> Option<&str> {
        self.site.as_deref()
    }

    /// How many documents the tab's top level has had (each new one counts, in whichever
    /// renderer): what tells a document from the ones before it.
    pub fn documents(&self) -> u64 {
        self.documents
    }

    /// A new document for the current entry, of the navigation numbered `navigation`.
    fn new_document(&mut self, navigation: u64) {
        self.session.set_document(navigation, 0);
        self.documents += 1;
        self.http_error = false;
    }

    /// Whether the top level's current document came from an HTTP error response (400 or more):
    /// a login submitted before it failed, whatever the page shows.
    pub fn http_error(&self) -> bool {
        self.http_error
    }

    /// The current entry's URL.
    pub fn current_url(&self) -> Option<&Url> {
        self.session.current().map(|e| &e.url)
    }

    /// [`TabEvent::Committed`](crate::tab::TabEvent::Committed): a navigation of the page's own.
    pub fn committed(&mut self, url: Url, scroll: Point) {
        // (a navigation to the current entry's URL, like an error page's Reload, replaces it)
        if self.session.current().is_some_and(|e| e.url == url) {
            return;
        }
        self.session.save_scroll(scroll);
        self.session.push(url);
        self.new_document(self.navigations);
    }

    /// [`TabEvent::DocumentUrl`](crate::tab::TabEvent::DocumentUrl): where the current entry's
    /// document is now: where it ended up (redirects), or where `pushState`, `replaceState` or a
    /// fragment navigation took it. The entry takes the URL, so Reload loads the document's
    /// current URL (HTML "reload" and Chromium's `NavigationControllerImpl::Reload` load the
    /// current entry, whose URL a same-document commit set). (The page keeps a document's own
    /// entries; the window keeps one per document.) What a document said that arrived before the
    /// latest navigation is about another entry.
    pub fn document_url(&mut self, url: Url, navigation: u64) {
        if self.view.is_some() && self.session.current().is_none() {
            // (a view's first document is the page's doing: its entry starts the history)
            self.session.push(url);
            self.new_document(self.navigations);
            return;
        }
        if navigation == self.navigations && self.session.current().is_some_and(|e| e.url != url) {
            self.session.replace_url(url);
        }
    }

    /// [`TabEvent::State`](crate::tab::TabEvent::State): the state the window reads, and the
    /// current entry's title (unless the renderer has not heard of the latest navigation yet).
    pub fn page_state(&mut self, state: TabState) {
        if state.navigations == self.navigations
            && !state.title.is_empty()
            && self
                .session
                .current()
                .is_some_and(|e| e.title != state.title)
        {
            self.session.set_title(&state.title);
        }
        self.state = state;
        // (the address bar shows the user's navigation while the host fetches it)
        if let Some(nav) = &self.pending {
            if nav.entry != EntryAt::Commit {
                self.state.url = nav.url.to_string();
                self.state.loading = true;
            }
        }
    }
}

impl Drop for TabHost {
    fn drop(&mut self) {
        let _ = self.tx.send(TabCommand::Close);
        // (detached: the window never waits for a tab, except when profiling, where the page
        // prints its profile as it closes)
        let thread = self.thread.take();
        if snowlark_page::profile::enabled() {
            if let Some(t) = thread {
                let _ = t.join();
            }
        }
        // A renderer process ends at the Close; one that has not within CLOSE_WAIT is killed. One
        // that serves views of its page's windows ends so with the last of them.
        // (When the browser ends, its children's jobs end them.)
        if let Some(renderer) = self.process.take() {
            renderer.release(CLOSE_WAIT);
        }
    }
}

/// The site a document at `url` belongs to, which chooses its renderer (Chromium's
/// `SiteInfo::GetSiteForURLInternal`, `content/browser/site_info.cc`): the schemeful site of the
/// URL's origin (HTML "obtain a site": the scheme and the registrable domain; `file:` URLs are
/// one site, as Chromium's `file://`); the browser's own pages (`about:home`, `snowlark:`) one site
/// of their own; `None` for an opaque origin (a `data:` document, a `blob:` with no creator
/// known), which stays in the renderer that navigates to it (`CanUseSourceSiteInstance`).
pub fn site_for(url: &Url) -> Option<String> {
    if url.as_str() == START_PAGE {
        return Some("snowlark://".into());
    }
    snowlark_net::origin::site_of_url(url)
}

/// Whether a navigation's response is a download rather than a document, as Chromium decides
/// (`download_utils::IsDownload`, `content/browser/loader/download_utils_impl.cc`): a
/// `Content-Disposition: attachment` (`MustDownload`), else a type the engine does not render
/// (`blink::IsSupportedMimeType`) with a successful status (an error's body shows).
pub fn is_download(head: &ResponseHead, mime: &str) -> bool {
    let attachment = head.headers.get("content-disposition").is_some_and(|d| {
        d.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("attachment")
    });
    attachment || (!renders_mime(mime) && head.status / 100 == 2)
}

/// The types a navigation renders, as `blink::IsSupportedMimeType`
/// (`third_party/blink/common/mime_util/mime_util.cc`): images, JavaScript, media, `text/*` but a
/// list of document types that are downloads (calendars, vCards, CSV, RTF…), XML and JSON types.
/// With no type, the engine sniffs the body as HTML does.
pub fn renders_mime(mime: &str) -> bool {
    const UNSUPPORTED_TEXT: &[&str] = &[
        "text/calendar",
        "text/x-calendar",
        "text/x-vcalendar",
        "text/vcalendar",
        "text/vcard",
        "text/x-vcard",
        "text/directory",
        "text/ldif",
        "text/qif",
        "text/x-qif",
        "text/x-csv",
        "text/x-vcf",
        "text/rtf",
        "text/comma-separated-values",
        "text/csv",
        "text/tab-separated-values",
        "text/tsv",
        "text/ofx",
        "text/vnd.sun.j2me.app-descriptor",
        "text/x-ms-iqy",
        "text/x-ms-odc",
        "text/x-ms-rqy",
        "text/x-ms-contact",
    ];
    const SUPPORTED: &[&str] = &[
        "image/svg+xml",
        "application/xml",
        "application/atom+xml",
        "application/rss+xml",
        "application/xhtml+xml",
        "application/json",
        "application/pdf",
        "multipart/x-mixed-replace",
        "application/ecmascript",
        "application/javascript",
        "application/x-ecmascript",
        "application/x-javascript",
    ];
    let m = mime.trim().to_ascii_lowercase();
    if m.is_empty() {
        return true;
    }
    if m.starts_with("text/") {
        return !UNSUPPORTED_TEXT.contains(&m.as_str());
    }
    SUPPORTED.contains(&m.as_str())
        || m.starts_with("image/")
        || m.starts_with("audio/")
        || m.starts_with("video/")
        || (m.starts_with("application/") && m.ends_with("+json"))
}

/// Starts tab `id`'s renderer process, serving it from `context` (the browser's services and the
/// tab's own) and the tab's audio.
fn start_process(
    id: TabId,
    context: TabContext,
    device: Device,
    remote: RemoteTab,
) -> Result<RendererProcess, snowlark_ipc::Error> {
    let events = context.events.clone();
    let local = LocalServices {
        broadcast: context.broadcast.backend().clone(),
        storage: context.web_storage.clone(),
        site_lock: snowlark_tab::services::site_lock::Mode::from_env(),
        code_cache: remote.code_cache,
        clipboard: context.clipboard.clone(),
        internal: context.internal.clone(),
        content_filter: context.content_filter.clone(),
        net: context.net.backend().clone(),
        request_filter: context.net.filter().cloned(),
        idb: context.idb.service().clone(),
        caches: context.caches.service().clone(),
        audible: Arc::new(move |audible| events.send(id, TabEvent::Audible(audible))),
        muted: remote.muted,
        audio_out: context.audio_out.clone(),
        videos: context.videos.clone(),
        cdm: context.cdm.clone(),
        // (a context without the profile's registry: one of the renderer's own, in memory)
        service_workers: match &context.service_workers {
            Some(sw) => sw.host().clone(),
            None => Arc::new(snowlark_sw::LocalServiceWorkers::new(
                Some(context.net.backend().clone()),
                None,
            )),
        },
    };
    let window = WindowEnds {
        gpu: context.gpu,
        compositor: context.compositor,
        gpu_raster: context.gpu_raster,
        events: context.events,
    };
    match remote.spare {
        Some(spare) => RendererProcess::assign(spare, id, device, &local, window),
        // (the tab's id numbers its process among the browser's children: a restarted tab has a
        // new id; spares are numbered above every tab id)
        None => RendererProcess::start(remote.launch, id, id, device, &local, window),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;

    use super::*;
    use crate::composite::ScrollView;

    /// A host with no renderer: the test reads what it sends.
    fn host() -> (TabHost, Receiver<TabCommand>, SharedScroll) {
        let (tx, rx) = channel();
        let scroll = SharedScroll::default();
        (
            TabHost::new(1, tx, None, scroll.clone(), Arc::default()),
            rx,
            scroll,
        )
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn scroll_to(scroll: &SharedScroll, y: f32) {
        let view = ScrollView {
            scroll: Point::new(0.0, y),
            ..Default::default()
        };
        scroll.lock().unwrap().insert(1, view);
    }

    fn navigate(url: &str, restore: Option<Point>, navigation: u64) -> TabCommand {
        TabCommand::Navigate {
            url: u(url),
            restore,
            navigation,
            reload: false,
        }
    }

    fn reload(url: &str, restore: Option<Point>, navigation: u64) -> TabCommand {
        TabCommand::Navigate {
            url: u(url),
            restore,
            navigation,
            reload: true,
        }
    }

    #[test]
    fn navigations_keep_the_history_and_tell_the_renderer_what_to_load() {
        let (mut h, rx, scroll) = host();
        h.navigate(u("https://a.example/"));
        assert_eq!(rx.try_recv(), Ok(navigate("https://a.example/", None, 1)));
        scroll_to(&scroll, 500.0);
        h.navigate(u("https://b.example/"));
        assert_eq!(rx.try_recv(), Ok(navigate("https://b.example/", None, 2)));
        assert!(h.can_go(-1) && !h.can_go(1));
        scroll_to(&scroll, 70.0);
        // Back restores where the entry was left, and remembers where this one was.
        h.go(-1);
        let at = Some(Point::new(0.0, 500.0));
        assert_eq!(rx.try_recv(), Ok(navigate("https://a.example/", at, 3)));
        h.go(1);
        let at = Some(Point::new(0.0, 70.0));
        assert_eq!(rx.try_recv(), Ok(navigate("https://b.example/", at, 4)));
        // Nowhere to go: nothing is sent.
        h.go(1);
        assert!(rx.try_recv().is_err());
        // A reload stays where the view is.
        scroll_to(&scroll, 33.0);
        h.reload();
        let at = Some(Point::new(0.0, 33.0));
        assert_eq!(rx.try_recv(), Ok(reload("https://b.example/", at, 5)));
        // The same page again replaces its entry.
        h.navigate(u("https://b.example/"));
        assert_eq!(rx.try_recv(), Ok(navigate("https://b.example/", None, 6)));
        assert_eq!(h.history_len(), 2);
    }

    #[test]
    fn a_crash_keeps_where_the_view_was() {
        let (mut h, rx, scroll) = host();
        h.navigate(u("https://a.example/"));
        h.navigate(u("https://b.example/"));
        let _ = rx.try_iter().count();
        scroll_to(&scroll, 250.0);
        h.crashed();
        // (the compositor drops the tab's frames, and a new renderer shows nothing yet)
        scroll.lock().unwrap().clear();
        h.reload();
        let at = Some(Point::new(0.0, 250.0));
        assert_eq!(rx.try_recv(), Ok(reload("https://b.example/", at, 3)));
        // Going back before a frame shows does not lose it either.
        h.go(-1);
        assert_eq!(h.trail().entries[1].scroll, Point::new(0.0, 250.0));
    }

    #[test]
    fn a_reload_of_nothing_sends_nothing() {
        let (mut h, rx, _) = host();
        h.reload();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_restored_history_loads_its_current_entry_where_it_was() {
        let (mut h, rx, _) = host();
        let (mut other, _rx, _) = host();
        other.navigate(u("https://a.example/"));
        other.navigate(u("https://b.example/"));
        let mut trail = other.trail();
        trail.entries[1].scroll = Point::new(0.0, 900.0);
        trail.entries[1].title = "B".into();
        h.restore(trail.clone());
        let at = Some(Point::new(0.0, 900.0));
        assert_eq!(rx.try_recv(), Ok(navigate("https://b.example/", at, 1)));
        assert_eq!(h.trail(), trail);
        assert!(h.can_go(-1));
        // (a state of the new navigation)
        let state = TabState {
            navigations: 1,
            ..Default::default()
        };
        h.page_state(state);
        assert_eq!(h.trail(), trail, "no title says nothing about the entry");
        // No history: nothing to load.
        let (mut h, rx, _) = host();
        h.restore(Trail::default());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_pages_own_navigations_become_entries() {
        let (mut h, _rx, _) = host();
        h.navigate(u("https://a.example/"));
        h.committed(u("https://b.example/"), Point::new(0.0, 40.0));
        assert_eq!(h.history_len(), 2);
        // Where the renderer says the entry was left, not where the compositor has moved on to
        // (the new document's frame may be there before the window hears of the commit).
        assert_eq!(h.trail().entries[0].scroll, Point::new(0.0, 40.0));
        // The current entry's URL again: no new entry.
        h.committed(u("https://b.example/"), Point::ZERO);
        assert_eq!(h.history_len(), 2);
        // From the middle of the history the forward entries go.
        h.go(-1);
        h.committed(u("https://c.example/"), Point::ZERO);
        assert_eq!(h.history_len(), 2);
        assert!(!h.can_go(1));
    }

    #[test]
    fn a_redirect_changes_the_current_entry_unless_a_navigation_overtook_it() {
        let (mut h, _rx, _) = host();
        h.navigate(u("https://a.example/"));
        h.document_url(u("https://a.example/landing"), 1);
        assert_eq!(h.trail().entries[0].url.path(), "/landing");
        // The renderer said this before it heard of the navigation to b.
        h.navigate(u("https://b.example/"));
        h.document_url(u("https://a.example/late"), 1);
        assert_eq!(h.trail().entries[1].url.as_str(), "https://b.example/");
    }

    #[test]
    fn titles_go_to_the_entry_the_state_is_about() {
        let (mut h, _rx, _) = host();
        h.navigate(u("https://a.example/"));
        let state = |title: &str, navigations| TabState {
            title: title.into(),
            navigations,
            ..Default::default()
        };
        h.page_state(state("A", 1));
        assert_eq!(h.trail().entries[0].title, "A");
        h.navigate(u("https://b.example/"));
        // The window still reads a late state, but entry b is not titled from it.
        h.page_state(state("A again", 1));
        assert_eq!(h.state.title, "A again");
        assert_eq!(h.trail().entries[1].title, "");
        h.page_state(state("B", 2));
        assert_eq!(h.trail().entries[1].title, "B");
        // A document with no title leaves the entry's.
        h.page_state(state("", 2));
        assert_eq!(h.trail().entries[1].title, "B");
    }

    // ----- M5: navigation in the browser ---------------------------------------------------------

    /// A server answering each path with the raw HTTP response given (`Connection: close`).
    fn serve(responses: Vec<(&'static str, String)>) -> String {
        serve_at("127.0.0.1:0", responses)
    }

    /// `serve` on `addr` (another loopback address is another site).
    fn serve_at(addr: &str, responses: Vec<(&'static str, String)>) -> String {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind(addr).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let responses = responses.clone();
                std::thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
                    let mut first = String::new();
                    let _ = reader.read_line(&mut first);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    let path = first.split(' ').nth(1).unwrap_or("").to_owned();
                    let answer = responses
                        .iter()
                        .find(|(p, _)| *p == path)
                        .map(|(_, r)| r.clone())
                        .unwrap_or_else(|| {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                        });
                    let _ = conn.write_all(answer.as_bytes());
                });
            }
        });
        base
    }

    fn ok_html(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    type Events = Receiver<(TabId, TabEvent)>;

    /// A host with the browser's network and its own events (what the window's channel carries).
    fn host_with_net(net: &snowlark_net::NetThread) -> (TabHost, Receiver<TabCommand>, Events) {
        let (mut h, rx, _) = host();
        let (events, events_rx) = channel();
        h.net = Some(net.client());
        h.events = Some(Arc::new(events));
        (h, rx, events_rx)
    }

    /// Runs the host's part of its pending navigation until it decides (the window does this on
    /// `NavigationReady`).
    fn decide(h: &mut TabHost, events: &Events) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.pending.is_some() {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left) {
                Ok((_, TabEvent::NavigationReady(n))) => h.navigation_ready(n),
                Ok(_) => {}
                Err(e) => panic!("no response for the navigation: {e}"),
            }
        }
    }

    fn committed(rx: &Receiver<TabCommand>) -> Commit {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(TabCommand::Commit(c)) => *c,
            other => panic!("not a commit: {other:?}"),
        }
    }

    #[test]
    fn a_navigation_is_fetched_here_and_handed_to_the_renderer_to_commit() {
        let base = serve(vec![
            ("/page", ok_html("hello")),
            (
                "/r",
                "HTTP/1.1 302 Found\r\nLocation: /page\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .into(),
            ),
        ]);
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        let page = u(&format!("{base}/page"));
        h.navigate(page.clone());
        // The user's navigation: its entry, and the address, at once; nothing to the renderer yet.
        assert_eq!(h.history_len(), 1);
        assert_eq!(
            (h.state.url.as_str(), h.state.loading),
            (page.as_str(), true)
        );
        assert!(rx.try_recv().is_err());
        decide(&mut h, &events);
        let c = committed(&rx);
        assert_eq!((c.navigation, c.restore, c.params.begun), (1, None, None));
        assert_eq!(c.params.url, page);
        let CommitResponse::Fetched { head } = &c.params.response else {
            panic!("{:?}", c.params.response);
        };
        assert_eq!(head.status, 200);
        assert!(c.params.started.is_some() && c.view_source.is_none());
        // The rest of the body is handed over: the renderer's network adopts it.
        let mut body = net
            .client()
            .adopt(c.body.unwrap(), None)
            .expect("handed over");
        let mut bytes = Vec::new();
        while let Some(e) = body.next_blocking() {
            if let FetchEvent::Body(b) = e {
                bytes.extend_from_slice(&b);
            }
        }
        assert_eq!(bytes, b"hello");
        assert_eq!(h.trail().entries[0].document, 1);
        // Reload: the same entry, fetched again (the HTTP cache validates), committed as a reload.
        h.reload();
        decide(&mut h, &events);
        let c = committed(&rx);
        assert!(c.params.reload && c.navigation == 2);
        assert_eq!(h.history_len(), 1);
        // A redirect: the entry and the address take the response's URL.
        h.navigate(u(&format!("{base}/r")));
        decide(&mut h, &events);
        let c = committed(&rx);
        let CommitResponse::Fetched { head } = &c.params.response else {
            panic!()
        };
        assert_eq!(head.url(), Some(&page));
        assert_eq!(h.current_url(), Some(&page));
        assert_eq!(h.state.url, page.as_str());
        assert_eq!(h.history_len(), 2);
        // Navigations the renderer serves itself: the start page is the browser's own site, so
        // from a web renderer it waits for one of its own (step 2; told to load as before in
        // a renderer with no site), and `about:blank` commits with nothing fetched.
        h.navigate(u("about:home"));
        assert!(h.needs_swap() && rx.try_recv().is_err());
        h.swap = None;
        h.site = None;
        h.navigate(u("about:home"));
        assert_eq!(rx.try_recv(), Ok(navigate("about:home", None, 5)));
        h.navigate(u("about:blank"));
        let c = committed(&rx);
        assert!(matches!(c.params.response, CommitResponse::Direct));
        assert_eq!((c.navigation, c.params.url.as_str()), (6, "about:blank"));
    }

    /// A reload while the body of the page's own load is still in the network (handed over, not
    /// read yet): the reload's fetch of the same URL must not wait for it.
    #[test]
    fn a_reload_does_not_wait_for_the_body_of_the_load_before_it() {
        let base = serve(vec![("/page", ok_html("hello"))]);
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        let page = u(&format!("{base}/page"));
        h.navigate(page.clone());
        decide(&mut h, &events);
        let first = committed(&rx);
        // (the renderer has not adopted the body yet)
        h.reload();
        decide(&mut h, &events);
        let c = committed(&rx);
        assert!(c.params.reload && c.navigation == 2);
        // Both bodies read in the end.
        for id in [first.body.unwrap(), c.body.unwrap()] {
            let mut body = net.client().adopt(id, None).expect("handed over");
            let mut bytes = Vec::new();
            while let Some(e) = body.next_blocking() {
                if let FetchEvent::Body(b) = e {
                    bytes.extend_from_slice(&b);
                }
            }
            assert_eq!(bytes, b"hello");
        }
    }

    #[test]
    fn a_204_and_a_download_commit_nothing() {
        let base = serve(vec![
            ("/page", ok_html("hello")),
            (
                "/204",
                "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
            ),
            (
                "/file",
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Disposition: attachment; filename=\"a.html\"\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".into(),
            ),
            (
                "/bin",
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".into(),
            ),
        ]);
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        let page = u(&format!("{base}/page"));
        h.navigate(page.clone());
        decide(&mut h, &events);
        committed(&rx);
        // A 204 (`NavigationRequest::OnResponseStarted`: not rendered): the tab stays where it
        // was, its pushed entry gone, the address back.
        h.navigate(u(&format!("{base}/204")));
        assert_eq!(h.history_len(), 2);
        decide(&mut h, &events);
        assert!(rx.try_recv().is_err());
        assert_eq!(h.history_len(), 1);
        assert_eq!(
            (h.state.url.as_str(), h.state.loading),
            (page.as_str(), false)
        );
        assert!(h.take_download().is_none());
        // A download (`Content-Disposition: attachment`): the window saves it; nothing commits.
        for path in ["/file", "/bin"] {
            h.navigate(u(&format!("{base}{path}")));
            decide(&mut h, &events);
            assert!(rx.try_recv().is_err(), "{path}");
            let d = h.take_download().expect(path);
            assert_eq!(d.url, u(&format!("{base}{path}")));
            assert_eq!(d.head.status, 200);
            assert_eq!(h.history_len(), 1);
            assert_eq!(h.state.url, page.as_str());
        }
        // Back and forward to a 204: the index stays.
        h.navigate(u(&format!("{base}/page?2")));
        decide(&mut h, &events);
        committed(&rx);
        assert_eq!(h.history_len(), 2);
        h.go(-1);
        assert!(h.pending.is_some());
        // (the entry behind is a 204 now: the server's answer is by path, so change the entry)
        h.session.set_index(0);
        h.session.replace_url(u(&format!("{base}/204")));
        h.session.set_index(1);
        h.pending = None;
        h.go(-1);
        decide(&mut h, &events);
        assert_eq!(h.trail().index, 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_download_decision_follows_chromium() {
        let head = |status: u16, headers: &[(&str, &str)]| {
            let mut h = ResponseHead::new(status, "");
            for (n, v) in headers {
                h.headers.append(n, v);
            }
            h
        };
        assert!(!is_download(&head(200, &[]), "text/html"));
        assert!(!is_download(&head(200, &[]), ""));
        assert!(!is_download(&head(200, &[]), "image/png"));
        assert!(!is_download(&head(200, &[]), "application/xhtml+xml"));
        assert!(!is_download(&head(200, &[]), "application/ld+json"));
        assert!(is_download(&head(200, &[]), "application/octet-stream"));
        assert!(is_download(&head(200, &[]), "application/zip"));
        assert!(is_download(&head(200, &[]), "text/csv"));
        // (an error's body shows, whatever its type)
        assert!(!is_download(&head(404, &[]), "application/octet-stream"));
        assert!(is_download(
            &head(404, &[("content-disposition", "attachment")]),
            "text/html"
        ));
        assert!(is_download(
            &head(200, &[("content-disposition", "Attachment; filename=x")]),
            "text/html"
        ));
        assert!(!is_download(
            &head(200, &[("content-disposition", "inline; filename=x")]),
            "text/html"
        ));
    }

    /// A local https server for `a.test` (and `b.test`) with a certificate a test CA issued: the
    /// chain it presents (leaf, CA), the network's trust in that CA, and its port. It answers every
    /// connection with a small page.
    fn tls_server() -> (Vec<Vec<u8>>, snowlark_net::TlsTrust, u16) {
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        use std::io::{Read, Write};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "Snowlark Test CA");
        let ca = ca.self_signed(&ca_key).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut leaf = CertificateParams::new(vec!["a.test".into(), "b.test".into()]).unwrap();
        leaf.distinguished_name.push(DnType::CommonName, "a.test");
        leaf.distinguished_name
            .push(DnType::OrganizationName, "Snowlark Tests");
        let leaf = leaf.signed_by(&key, &ca, &ca_key).unwrap();
        let chain = vec![leaf.der().clone(), ca.der().clone()];
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    chain.clone(),
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
                )
                .unwrap(),
        );
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for tcp in listener.incoming().flatten() {
                let server = server.clone();
                std::thread::spawn(move || {
                    let conn = rustls::ServerConnection::new(server).unwrap();
                    let mut s = rustls::StreamOwned::new(conn, tcp);
                    let mut got = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got.extend_from_slice(&buf[..n]),
                        }
                    }
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
                    );
                    s.conn.send_close_notify();
                    let _ = s.flush();
                });
            }
        });
        (
            chain.iter().map(|c| c.as_ref().to_vec()).collect(),
            snowlark_net::TlsTrust::Custom(Arc::new(client)),
            port,
        )
    }

    /// The network's verified chain reaches the browser half with the navigation's response: the
    /// host keeps it for the document it commits (for the site information panel) while the tab
    /// shows that origin, and the renderer's copy of the head has none.
    #[test]
    fn the_certificate_chain_stays_with_the_browser() {
        let (chain, trust, port) = tls_server();
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig {
            tls: trust,
            host_rules: snowlark_net::HostRule::parse_list("MAP *.test 127.0.0.1"),
            ..snowlark_net::NetConfig::default()
        })
        .unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        assert!(h.tls().is_none());
        let url = u(&format!("https://a.test:{port}/page"));
        h.navigate(url.clone());
        decide(&mut h, &events);
        let c = committed(&rx);
        let CommitResponse::Fetched { head } = &c.params.response else {
            panic!("not fetched: {:?}", c.params.response);
        };
        assert!(head.connection.tls.is_none(), "the renderer's head");
        let tls = h.tls().expect("the document's TLS session").clone();
        assert_eq!(tls.certificates, chain);
        assert_eq!(tls.verifier, snowlark_net::CertVerifier::Custom);
        // (what the viewer reads of it)
        let leaf = crate::cert_viewer::parse(&tls.certificates[0]).unwrap();
        assert_eq!(leaf.subject.common_name.as_deref(), Some("a.test"));
        assert_eq!(leaf.subject.organization.as_deref(), Some("Snowlark Tests"));
        assert_eq!(leaf.issuer.common_name.as_deref(), Some("Snowlark Test CA"));
        assert_eq!(
            crate::cert_viewer::issuer_of(&tls.certificates[0]).as_deref(),
            Some("Snowlark Test CA")
        );
        let mut chains = crate::cert_viewer::Chains::default();
        let page = chains.add("a.test", tls.clone());
        let (id, ..) = crate::cert_viewer::view_of(&u(&page));
        for tab in [
            crate::cert_viewer::Tab::General,
            crate::cert_viewer::Tab::Details,
        ] {
            let html = crate::cert_viewer::certificate_html(
                chains.get(id.unwrap()),
                id.unwrap(),
                tab,
                0,
                false,
                &|_| 0,
            );
            assert!(html.contains("Certificate Viewer: a.test"), "{html}");
            assert!(html.contains("Snowlark Test CA"), "{html}");
        }
        // a same-document navigation keeps it; another origin, or an error page, has none
        h.state.url = format!("https://a.test:{port}/other#x");
        assert!(h.tls().is_some());
        h.state.url = format!("https://b.test:{port}/");
        assert!(h.tls().is_none());
        h.state.url = url.to_string();
        h.state.error_page = true;
        assert!(h.tls().is_none());
        h.state.error_page = false;
        // a failed navigation leaves none
        h.navigate(u("https://a.test:1/"));
        decide(&mut h, &events);
        assert!(matches!(
            committed(&rx).params.response,
            CommitResponse::Failed(_)
        ));
        h.state.url = url.to_string();
        assert!(h.tls().is_none());
    }

    /// The TLS session never crosses a process: a head through the wire format has none.
    #[test]
    fn a_head_on_the_wire_has_no_tls_session() {
        let mut head = ResponseHead::new(200, "OK");
        head.connection.tls = Some(Arc::new(snowlark_net::TlsInfo {
            version: "TLSv1_3".into(),
            cipher_suite: "TLS13_AES_128_GCM_SHA256".into(),
            key_exchange: Some("X25519".into()),
            certificates: vec![vec![1, 2, 3]],
            verifier: snowlark_net::CertVerifier::System,
        }));
        let bytes = postcard::to_allocvec(&head).unwrap();
        let back: ResponseHead = postcard::from_bytes(&bytes).unwrap();
        assert!(back.connection.tls.is_none());
    }

    #[test]
    fn a_failed_fetch_commits_an_error_page() {
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        let url = u("http://127.0.0.1:1/nothing");
        h.navigate(url.clone());
        decide(&mut h, &events);
        let c = committed(&rx);
        assert!(matches!(c.params.response, CommitResponse::Failed(_)));
        assert_eq!((c.params.url, c.body), (url.clone(), None));
        // (the entry stays: Reload tries again)
        assert_eq!(h.current_url(), Some(&url));
        h.reload();
        decide(&mut h, &events);
        let c = committed(&rx);
        assert!(c.params.reload && matches!(c.params.response, CommitResponse::Failed(_)));
        assert_eq!(h.history_len(), 1);
    }

    #[test]
    fn the_pages_own_navigation_gets_its_entry_at_the_commit() {
        let base = serve(vec![("/page", ok_html("hello")), ("/two", ok_html("two"))]);
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        let page = u(&format!("{base}/page"));
        h.navigate(page.clone());
        decide(&mut h, &events);
        committed(&rx);
        // The page's link: no entry, and the address stays, until the response commits
        // (Chromium shows a renderer-initiated navigation's URL only at its commit).
        let two = u(&format!("{base}/two"));
        let mut params = NavigationParams::user(two.clone());
        params.initiator = Some(snowlark_net::navigation::NavInitiator::from_client(
            &snowlark_net::Client::for_url(&page),
        ));
        params.user_activation = false;
        h.begin_navigation(BeginNavigation { seq: 3, params });
        assert_eq!(h.history_len(), 1);
        assert_eq!(h.state.url, page.as_str());
        decide(&mut h, &events);
        let c = committed(&rx);
        assert_eq!((c.navigation, c.params.begun), (2, Some(3)));
        assert_eq!(h.history_len(), 2);
        assert_eq!(h.current_url(), Some(&two));
        assert_eq!(h.state.url, two.as_str());
        // `javascript:` never navigates.
        h.begin_navigation(BeginNavigation {
            seq: 4,
            params: NavigationParams::user(u("javascript:alert(1)")),
        });
        assert!(h.pending.is_none() && rx.try_recv().is_err());
        // A navigation that overtakes one not decided on yet: only the later one commits.
        h.navigate(page.clone());
        h.navigate(two.clone());
        assert_eq!(h.history_len(), 3);
        decide(&mut h, &events);
        let c = committed(&rx);
        assert_eq!((c.navigation, c.params.url), (4, two.clone()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn same_document_entries_traverse_in_the_document_and_the_rest_load() {
        let base = serve(vec![("/page", ok_html("hello")), ("/two", ok_html("two"))]);
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        let page = u(&format!("{base}/page"));
        h.navigate(page.clone());
        decide(&mut h, &events);
        committed(&rx);
        let change = |path: &str, push: bool, index: usize| SameDocumentChange {
            url: format!("{base}{path}"),
            push,
            index,
        };
        // pushState, replaceState, a fragment: entries of document 1.
        h.same_document(change("/p1", true, 1), 1);
        h.same_document(change("/p2", false, 1), 1);
        h.same_document(change("/p2#x", true, 2), 1);
        assert_eq!(h.history_len(), 3);
        assert_eq!(h.state.url, format!("{base}/p2#x"));
        let items: Vec<(u64, usize)> = h
            .trail()
            .entries
            .iter()
            .map(|e| (e.document, e.item))
            .collect();
        assert_eq!(items, [(1, 0), (1, 1), (1, 2)]);
        // (a change the renderer says about an earlier document is not taken)
        h.same_document(change("/late", true, 9), 0);
        assert_eq!(h.history_len(), 3);
        // Back within the document: the renderer traverses in place, nothing is fetched.
        h.go(-1);
        assert_eq!(
            rx.try_recv(),
            Ok(TabCommand::GoInDocument {
                url: u(&format!("{base}/p2")),
                index: 1,
                navigation: 2
            })
        );
        assert!(h.pending.is_none());
        h.go(-1);
        assert_eq!(
            rx.try_recv(),
            Ok(TabCommand::GoInDocument {
                url: page.clone(),
                index: 0,
                navigation: 3
            })
        );
        // `history.go(2)` from the page: the browser traverses.
        h.go(2);
        assert!(matches!(
            rx.try_recv(),
            Ok(TabCommand::GoInDocument { index: 2, .. })
        ));
        // A new document, then Back: the entry of another document loads, scrolled back.
        h.navigate(u(&format!("{base}/two")));
        decide(&mut h, &events);
        let c = committed(&rx);
        assert_eq!(c.navigation, 5);
        assert_eq!(h.trail().entries[3].document, 5);
        h.go(-1);
        assert!(h.pending.is_some());
        decide(&mut h, &events);
        let c = committed(&rx);
        assert_eq!(c.params.url, u(&format!("{base}/p2#x")));
        assert_eq!(c.restore, Some(Point::ZERO));
        assert_eq!(h.trail().index, 2);
        // (it is a new document now: a traversal to its old same-document entries loads them)
        assert_eq!(h.trail().entries[2].document, 6);
        h.go(-1);
        assert!(h.pending.is_some());
        h.stop();
        assert!(h.pending.is_none());
        assert_eq!(h.trail().index, 2);
        assert_eq!(rx.try_recv(), Ok(TabCommand::Stop));
    }

    #[test]
    fn view_source_fetches_what_it_names() {
        let base = serve(vec![("/page", ok_html("<b>hello</b>"))]);
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        let shown = u(&format!("view-source:{base}/page"));
        h.navigate(shown.clone());
        assert_eq!(h.state.url, shown.as_str());
        decide(&mut h, &events);
        let c = committed(&rx);
        assert_eq!(c.view_source, Some(shown.clone()));
        assert_eq!(c.params.url, shown);
        let CommitResponse::Fetched { head } = &c.params.response else {
            panic!()
        };
        assert_eq!(head.url(), Some(&u(&format!("{base}/page"))));
        assert!(c.body.is_some());
        assert_eq!(h.current_url(), Some(&shown));
        // One naming something the browser does not show the source of: the renderer's error.
        h.navigate(u("view-source:snowlark://settings/"));
        assert_eq!(
            rx.try_recv(),
            Ok(navigate("view-source:snowlark://settings/", None, 2))
        );
    }

    /// M5 step 2: a response from another site than the renderer's commits in a new renderer,
    /// which the window starts ([`TabHost::needs_swap`]); a same-site one, a document with no
    /// site of its own and an `about:` document stay; the browser's own pages are a site of
    /// their own.
    #[test]
    fn a_response_from_another_site_waits_for_a_new_renderer() {
        let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
        let (mut h, rx, events) = host_with_net(&net);
        let a = serve(vec![("/one", ok_html("one")), ("/two", ok_html("two"))]);
        let b = serve_at("127.0.0.2:0", vec![("/three", ok_html("three"))]);
        // A renderer with no site takes the first.
        h.navigate(u(&format!("{a}/one")));
        decide(&mut h, &events);
        assert_eq!(committed(&rx).params.url.as_str(), format!("{a}/one"));
        assert_eq!(h.site.as_deref(), Some("http://127.0.0.1"));
        // Same site, a `data:` document, `about:blank`: the same renderer.
        for url in [
            format!("{a}/two"),
            "data:text/html,x".into(),
            "about:blank".into(),
        ] {
            h.navigate(u(&url));
            decide(&mut h, &events);
            assert_eq!(committed(&rx).params.url.as_str(), url, "{url}");
            assert!(!h.needs_swap(), "{url}");
        }
        assert_eq!(h.site.as_deref(), Some("http://127.0.0.1"));
        // Another site: nothing is sent to this renderer; the entry and the address are the
        // navigation's, for the new renderer to commit.
        h.navigate(u(&format!("{b}/three")));
        decide(&mut h, &events);
        assert!(h.needs_swap());
        assert!(rx.try_recv().is_err());
        assert_eq!(
            (h.history_len(), h.state.url.as_str(), h.state.loading),
            (5, format!("{b}/three").as_str(), true)
        );
        // The browser's own page from a web renderer: a swap too, which the host tells the
        // window of as it tells of a response.
        h.swap = None;
        h.navigate(u("snowlark://settings"));
        assert!(h.needs_swap() && rx.try_recv().is_err());
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(1)),
            Ok((1, TabEvent::NavigationReady(_)))
        ));
        // Its renderer never shows a web page.
        h.swap = None;
        h.site = Some("snowlark://".into());
        h.navigate(u(&format!("{a}/one")));
        decide(&mut h, &events);
        assert!(h.needs_swap() && rx.try_recv().is_err());
    }

    #[test]
    fn the_site_of_a_url_follows_chromium() {
        let site = |s: &str| site_for(&u(s));
        assert_eq!(
            site("https://www.example.co.uk/a").as_deref(),
            Some("https://example.co.uk")
        );
        assert_eq!(
            site("http://www.example.co.uk/a").as_deref(),
            Some("http://example.co.uk")
        );
        assert_eq!(
            site("http://127.0.0.2:80/a").as_deref(),
            Some("http://127.0.0.2")
        );
        assert_eq!(site("file:///C:/a.html"), site("file:///D:/b.html"));
        assert_eq!(site("snowlark://settings"), site(START_PAGE));
        assert_eq!(site("data:text/html,x"), None);
    }

    #[test]
    fn the_crash_switches_reach_only_the_tab_named() {
        for name in ["SNOWLARK_TEST_CRASH", "SNOWLARK_TEST_PANIC"] {
            assert!(passes_to_renderer(name, Some(3), None));
            assert!(passes_to_renderer(name, Some(3), Some("3")));
            assert!(!passes_to_renderer(name, Some(4), Some("3")));
            assert!(!passes_to_renderer(name, Some(4), Some("x")));
            // A spare's: everywhere when no tab is named, else nowhere.
            assert!(passes_to_renderer(name, None, None));
            assert!(!passes_to_renderer(name, None, Some("3")));
        }
        assert!(passes_to_renderer("SNOWLARK_PROFILE", Some(4), Some("3")));
        assert!(passes_to_renderer("SNOWLARK_PROFILE", None, Some("3")));
        assert!(passes_to_renderer("RUST_BACKTRACE", Some(4), None));
        assert!(!passes_to_renderer("SNOWLARK_IPC_PIPE", Some(4), None));
        assert!(!passes_to_renderer("PATH", Some(4), None));
    }

    #[test]
    fn the_window_runs_each_tab_in_a_renderer_process_by_default() {
        assert_eq!(ProcessModel::choose(None, None), Ok(ProcessModel::Tab));
        // `single` stays available, by the flag or the environment; the flag wins.
        assert_eq!(
            ProcessModel::choose(None, Some("single")),
            Ok(ProcessModel::Single)
        );
        assert_eq!(
            ProcessModel::choose(Some("single"), None),
            Ok(ProcessModel::Single)
        );
        assert_eq!(
            ProcessModel::choose(Some("single"), Some("tab")),
            Ok(ProcessModel::Single)
        );
        assert_eq!(
            ProcessModel::choose(Some("tab"), Some("single")),
            Ok(ProcessModel::Tab)
        );
        assert_eq!(
            ProcessModel::choose(Some("site"), None),
            Err("site".to_owned())
        );
    }

    #[test]
    fn only_the_tab_model_starts_a_spare() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let launches = Arc::new(AtomicUsize::new(0));
        let counted = |launches: &Arc<AtomicUsize>| {
            let launches = launches.clone();
            move || {
                launches.fetch_add(1, Ordering::SeqCst);
                // (nothing to start: the pool's tries fail at once)
                Launch::new(r"Z:\no such folder\snowlark.exe")
            }
        };
        let none = spare_pool(ProcessModel::Single, counted(&launches));
        assert!(none.is_none());
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            launches.load(Ordering::SeqCst),
            0,
            "single mode started a renderer"
        );
        let pool = spare_pool(ProcessModel::Tab, counted(&launches)).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while launches.load(Ordering::SeqCst) == 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            launches.load(Ordering::SeqCst) >= 1,
            "tab mode started no spare"
        );
        // (a renderer that cannot start is not handed out: tabs start their own)
        assert!(pool.ready(Duration::from_secs(10)).is_none());
        assert!(pool.take().is_none());
    }
}
