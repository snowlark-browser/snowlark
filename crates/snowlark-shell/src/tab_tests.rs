// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of a tab's thread through the host that drives it.

use std::sync::Arc;
use std::time::Instant;

use snowlark_dom::NodeId;
use snowlark_net::Url;
use snowlark_paint::{Gpu, Point, Size, TextEngine};

use crate::driver::*;
use crate::tab::*;
use crate::tab_host::{Renderer, TabHost};
use std::sync::mpsc::channel;

/// Incognito's partition, with this file's tabs and servers.
#[path = "incognito_tests.rs"]
mod incognito;

/// A tab whose compositor does not exist: its viewport is at the top.
fn spawn(id: TabId, ctx: TabContext, device: Device, url: Url, factory: DriverFactory) -> TabHost {
    TabHost::spawn(id, ctx, Default::default(), device, url, factory)
}

/// A document that loads built-in pages at once, as the start page does.
struct Instant_ {
    info: DocumentInfo,
    text: TextEngine,
    images: Arc<snowlark_paint::ImageStore>,
    /// How long it takes to say how much memory it uses (a busy page).
    memory_ms: u64,
    /// What the next pointer input follows, in this tab.
    link: Option<Url>,
}

impl TabDriver for Instant_ {
    fn memory(&mut self) -> crate::driver::DriverMemory {
        std::thread::sleep(std::time::Duration::from_millis(self.memory_ms));
        crate::driver::DriverMemory {
            js: 1000,
            dom: 24,
            received: 7,
        }
    }
    fn navigate(&mut self, url: Url) {
        self.info.url = Some(url);
        self.info.loading = false;
        self.info.generation += 1;
    }
    fn stop(&mut self) {}
    fn set_device(&mut self, _device: Device) {}
    fn set_scroll(&mut self, _scroll: Point) {}
    fn update(&mut self) -> Update {
        Update::default()
    }
    fn paint(&mut self, _origin: Point) -> Option<snowlark_paint::DisplayList> {
        None
    }
    fn text(&self) -> &TextEngine {
        &self.text
    }
    fn images(&self) -> &snowlark_paint::ImageStore {
        &self.images
    }
    fn image_store(&self) -> Arc<snowlark_paint::ImageStore> {
        self.images.clone()
    }
    fn pointer(&mut self, _input: Pointer) -> PointerResult {
        PointerResult {
            open: self.link.take().map(|u| (u, Disposition::CurrentTab)),
            ..Default::default()
        }
    }
    fn scroll_container(&mut self, _node: NodeId, _delta: Point) -> bool {
        false
    }
    fn info(&self) -> &DocumentInfo {
        &self.info
    }
}

/// How a test driver fails.
#[derive(Clone, Copy, PartialEq)]
enum Fault {
    /// Panics in its update, and again when dropped.
    Panic,
    /// Records a failure the way a V8 callback's guard does, and returns normally.
    Recorded,
}

struct Faulty {
    fault: Fault,
    inner: Instant_,
}

impl Drop for Faulty {
    fn drop(&mut self) {
        if self.fault == Fault::Panic {
            panic!("and its destructor panics too");
        }
    }
}

impl TabDriver for Faulty {
    fn navigate(&mut self, url: Url) {
        self.inner.navigate(url)
    }
    fn stop(&mut self) {}
    fn set_device(&mut self, _device: Device) {}
    fn set_scroll(&mut self, _scroll: Point) {}
    fn update(&mut self) -> Update {
        match self.fault {
            Fault::Panic => panic!("the page failed"),
            Fault::Recorded => snowlark_js::guard::record("a callback failed".into()),
        }
        Update::default()
    }
    fn paint(&mut self, _origin: Point) -> Option<snowlark_paint::DisplayList> {
        None
    }
    fn text(&self) -> &TextEngine {
        &self.inner.text
    }
    fn images(&self) -> &snowlark_paint::ImageStore {
        &self.inner.images
    }
    fn image_store(&self) -> Arc<snowlark_paint::ImageStore> {
        self.inner.images.clone()
    }
    fn pointer(&mut self, _input: Pointer) -> PointerResult {
        PointerResult::default()
    }
    fn scroll_container(&mut self, _node: NodeId, _delta: Point) -> bool {
        false
    }
    fn info(&self) -> &DocumentInfo {
        &self.inner.info
    }
}

fn instant() -> Instant_ {
    Instant_ {
        info: DocumentInfo::default(),
        text: TextEngine::new().unwrap(),
        images: Arc::new(snowlark_paint::ImageStore::new()),
        memory_ms: 0,
        link: None,
    }
}

#[test]
fn a_memory_request_returns_at_once_and_is_answered_later() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, _comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    let device = Device {
        viewport: Size::new(800.0, 600.0),
        scale: 1.0,
        scrollbar: 0.0,
        screen: Default::default(),
    };
    let slow: DriverFactory = Box::new(|_| {
        let mut d = instant();
        d.memory_ms = 400;
        Box::new(d)
    });
    let url = Url::parse("about:blank").unwrap();
    let tab = spawn(1, ctx, device, url, slow);
    // The tab is busy with a slow answer; the window's call to ask returns at once, however
    // many times it asks, and the answer comes later.
    let asked = std::time::Instant::now();
    for _ in 0..3 {
        tab.send(TabCommand::Memory);
    }
    assert!(
        asked.elapsed() < std::time::Duration::from_millis(100),
        "asking for the memory waited for the tab"
    );
    let mut answers = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while answers < 3 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((1, TabEvent::Memory(m))) => {
                assert_eq!((m.js, m.dom, m.images, m.received), (1000, 24, 0, 7));
                // (the bytes received are not memory)
                assert_eq!(m.total(), 1024 + m.tiles);
                answers += 1;
            }
            Ok(_) => {}
            Err(e) => panic!("no answer: {e}"),
        }
    }
    assert!(asked.elapsed() >= std::time::Duration::from_millis(1000));
}

#[test]
fn a_failing_tab_reports_a_crash_and_the_others_go_on() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, _comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    let device = Device {
        viewport: Size::new(800.0, 600.0),
        scale: 1.0,
        scrollbar: 0.0,
        screen: Default::default(),
    };
    let url = Url::parse("about:blank").unwrap();
    let faulty = |fault: Fault| -> DriverFactory {
        Box::new(move |_| {
            Box::new(Faulty {
                fault,
                inner: instant(),
            })
        })
    };
    let healthy: DriverFactory = Box::new(|_| Box::new(instant()));
    let a = spawn(1, ctx.clone(), device, url.clone(), faulty(Fault::Panic));
    let b = spawn(2, ctx.clone(), device, url.clone(), faulty(Fault::Recorded));
    let mut c = spawn(3, ctx, device, url, healthy);
    // (a tab updates and publishes after handling what it is sent)
    for t in [&a, &b, &c] {
        t.send(TabCommand::Wake);
    }
    let mut crashed = std::collections::HashMap::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut c_state = false;
    while crashed.len() < 2 || !c_state {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((id, TabEvent::Crashed(m))) => {
                crashed.insert(id, m);
            }
            Ok((3, TabEvent::State(_))) => c_state = true,
            Ok(_) => {}
            Err(e) => panic!("tabs did not report: {e} (crashed {crashed:?}, state {c_state})"),
        }
    }
    assert_eq!(crashed[&1], "the page failed");
    assert_eq!(crashed[&2], "a callback failed");
    assert!(!crashed.contains_key(&3));
    // (the healthy tab still answers)
    c.navigate(Url::parse("about:home").unwrap());
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok((3, TabEvent::State(s))) if s.url == "about:home" => break,
            Ok(_) => {}
            Err(e) => panic!("the healthy tab stopped: {e}"),
        }
    }
    drop((a, b));
}

#[test]
fn navigating_to_the_page_shown_still_reports_the_load_done() {
    // The window shows a navigation it asks for as loading at once; the tab must answer each one
    // with its state, even when that state is what it last sent (the start page, again).
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, _comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    let device = Device {
        viewport: Size::new(800.0, 600.0),
        scale: 1.0,
        scrollbar: 0.0,
        screen: Default::default(),
    };
    let home = Url::parse("about:home").unwrap();
    let factory: DriverFactory = Box::new(|_| Box::new(instant()));
    let mut tab = spawn(1, ctx, device, home.clone(), factory);
    let next_state = || loop {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok((_, TabEvent::State(s))) => return s,
            Ok(_) => continue,
            Err(e) => panic!("no state from the tab: {e}"),
        }
    };
    // (a tab publishes after handling what it is sent)
    tab.send(TabCommand::Wake);
    let first = next_state();
    assert_eq!((first.url.as_str(), first.loading), ("about:home", false));
    for n in 1..=2 {
        // (what the Home button does)
        tab.navigate(home.clone());
        let s = next_state();
        assert_eq!((s.url.as_str(), s.loading), ("about:home", false));
        assert_eq!(s.navigations, n);
        // (the same page again replaces its history entry)
        assert!(!tab.can_go(-1));
    }
}

#[test]
fn the_session_history_follows_the_page_and_a_restored_tab_loads_where_it_was() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, _comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    let device = Device {
        viewport: Size::new(800.0, 600.0),
        scale: 1.0,
        scrollbar: 0.0,
        screen: Default::default(),
    };
    let home = Url::parse("about:home").unwrap();
    let link = Url::parse("about:link").unwrap();
    let followed = link.clone();
    let factory: DriverFactory = Box::new(move |_| {
        let mut d = instant();
        d.link = Some(followed);
        Box::new(d)
    });
    let mut tab = spawn(1, ctx.clone(), device, home.clone(), factory);
    // (what the window does with a tab's events)
    let hear_until = |tab: &mut TabHost, url: &str| {
        while tab.state.url != url {
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok((_, TabEvent::State(s))) => tab.page_state(s),
                Ok((_, TabEvent::Committed { url, scroll })) => tab.committed(url, scroll),
                Ok((_, TabEvent::DocumentUrl { url, navigation })) => {
                    tab.document_url(url, navigation)
                }
                Ok(_) => {}
                Err(e) => panic!("no {url} from the tab: {e}"),
            }
        }
    };
    tab.send(TabCommand::Wake);
    hear_until(&mut tab, "about:home");
    assert_eq!(tab.history_len(), 1);
    // A link followed in the tab: the renderer loads it and the host gets the entry.
    tab.send(TabCommand::Pointer(Pointer::Down(
        Point::ZERO,
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
        1,
    )));
    hear_until(&mut tab, "about:link");
    assert_eq!(tab.history_len(), 2);
    assert!(tab.can_go(-1) && !tab.can_go(1));
    // Back: the host says what to load.
    tab.go(-1);
    hear_until(&mut tab, "about:home");
    assert!(!tab.can_go(-1) && tab.can_go(1));
    let trail = tab.trail();
    assert_eq!(trail.index, 0);
    // A restored tab shows nothing until it gets its history, then the current entry.
    let mut idle = TabHost::spawn_idle(
        2,
        ctx,
        Default::default(),
        device,
        Renderer::Thread(Box::new(|_| Box::new(instant()))),
    );
    idle.send(TabCommand::Wake);
    assert_eq!(idle.history_len(), 0);
    idle.restore(crate::session::Trail { index: 1, ..trail });
    hear_until(&mut idle, "about:link");
    assert!(idle.can_go(-1) && !idle.can_go(1));
}

#[test]
fn a_crashed_tab_keeps_its_history_on_a_new_renderer() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, _comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    let device = Device {
        viewport: Size::new(800.0, 600.0),
        scale: 1.0,
        scrollbar: 0.0,
        screen: Default::default(),
    };
    let first = Url::parse("about:first").unwrap();
    let second = Url::parse("about:second").unwrap();
    let faulty: DriverFactory = Box::new(|_| {
        Box::new(Faulty {
            fault: Fault::Panic,
            inner: instant(),
        })
    });
    let mut tab = spawn(1, ctx.clone(), device, first, faulty);
    tab.navigate(second);
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok((1, TabEvent::Crashed(_))) => break,
            Ok(_) => {}
            Err(e) => panic!("the tab did not crash: {e}"),
        }
    }
    tab.crashed();
    tab.respawn(
        2,
        ctx,
        device,
        Renderer::Thread(Box::new(|_| Box::new(instant()))),
    );
    assert_eq!(tab.id, 2);
    assert_eq!(tab.history_len(), 2);
    let hear_url = |url: &str| loop {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok((2, TabEvent::State(s))) if s.url == url => return,
            Ok(_) => {}
            Err(e) => panic!("no {url} from the new renderer: {e}"),
        }
    };
    // Reload loads the entry the crash left, and Back the one before it.
    tab.reload();
    hear_url("about:second");
    tab.go(-1);
    hear_url("about:first");
    assert!(!tab.can_go(-1) && tab.can_go(1));
}

#[test]
fn a_tab_answers_the_find_box_and_runs_nothing_without_one() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, _comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    let device = Device {
        viewport: Size::new(800.0, 600.0),
        scale: 1.0,
        scrollbar: 0.0,
        screen: Default::default(),
    };
    let url = Url::parse("data:text/html,<body><p>find me</p><p>FIND <i>m</i>e</p>").unwrap();
    let mut tab = spawn(
        1,
        ctx,
        device,
        url,
        crate::page_driver::PageDriver::factory(),
    );
    // (the host's part of the navigation, as the window does it; the page's first state)
    next_of(&rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if !s.loading => Some(()),
        _ => None,
    });
    let find_until = |done: &dyn Fn(&FindStatus) -> bool| -> FindStatus {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok((_, TabEvent::Find(s))) if done(&s) => return s,
                Ok(_) => {}
                Err(e) => panic!("no answer from the find box: {e}"),
            }
        }
    };
    // (no search before the box opens: the load's events carry no counter)
    tab.send(TabCommand::Find(FindCommand::Open {
        previous: "find me".into(),
    }));
    let s = find_until(&|s| s.total == 2 && !s.searching);
    assert_eq!((s.query.as_str(), s.current), ("find me", Some(0)));
    tab.send(TabCommand::Find(FindCommand::Next));
    assert_eq!(find_until(&|s| s.current == Some(1)).total, 2);
    tab.send(TabCommand::Find(FindCommand::Query("nothing".into())));
    let s = find_until(&|s| s.query == "nothing" && !s.searching);
    assert_eq!((s.total, s.current), (0, None));
    // Closed: the tab says nothing more about it.
    tab.send(TabCommand::Find(FindCommand::Close { select: false }));
    tab.send(TabCommand::Find(FindCommand::Next));
    tab.send(TabCommand::Wake);
    let quiet = std::time::Instant::now() + std::time::Duration::from_millis(400);
    while let Ok((_, e)) =
        rx.recv_timeout(quiet.saturating_duration_since(std::time::Instant::now()))
    {
        assert!(
            !matches!(e, TabEvent::Find(_)),
            "a counter after the box closed"
        );
    }
}

/// Set in a renderer process this test binary starts: [`renderer_child`] runs the tab.
const RENDERER_ROLE: &str = "SNOWLARK_SHELL_TEST_RENDERER";

/// The evil-renderer suite ([`an_evil_renderer_is_refused_and_ended`]): the role, the address the
/// renderer asks before it forges (answered once its page has committed), its page and its tab.
const EVIL_ROLE: &str = "SNOWLARK_SHELL_TEST_EVIL";
const EVIL_GO: &str = "SNOWLARK_SHELL_TEST_EVIL_GO";
const EVIL_OWN: &str = "SNOWLARK_SHELL_TEST_EVIL_OWN";
const EVIL_TAB: &str = "SNOWLARK_SHELL_TEST_EVIL_TAB";

/// The renderer process of [`a_tab_in_a_renderer_process_starts_again_in_a_new_one`] (it does
/// nothing in a normal test run). With [`EVIL_ROLE`] it is a compromised renderer: a thread sends
/// the role's forgery as soon as the test says go.
#[test]
fn renderer_child() {
    if std::env::var_os(RENDERER_ROLE).is_none() {
        return;
    }
    let port: snowlark_tab::services::RendererPort =
        snowlark_ipc::connect_to_browser(std::time::Duration::from_secs(20)).unwrap();
    if let Ok(role) = std::env::var(EVIL_ROLE) {
        let port = port.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let go = std::env::var(EVIL_GO).unwrap();
            let own = Url::parse(&std::env::var(EVIL_OWN).unwrap()).unwrap();
            let tab: TabId = std::env::var(EVIL_TAB).unwrap().parse().unwrap();
            let mut s = std::net::TcpStream::connect(&go).unwrap();
            s.write_all(b"GET /go HTTP/1.1\r\nHost: go\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut answer = Vec::new();
            let _ = s.read_to_end(&mut answer);
            let _ = snowlark_tab::evil::forge(&role, &port, &own, tab);
        });
    }
    std::process::exit(snowlark_tab::renderer_main::run_renderer(port));
}

/// M5 step 5, the evil-renderer suite in the browser's own tab host: a renderer process that
/// sends each forgery of [`snowlark_tab::evil::ROLES`] once its page has committed (so the host has
/// locked it to the page's site) is refused and ended, the tab is a crashed one with the reason,
/// and the browser's storage is as it was.
#[test]
fn an_evil_renderer_is_refused_and_ended() {
    use std::io::{BufRead, Write};
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let storage = snowlark_page::web_storage::WebStorageHost::new();
    let ctx = TabContext {
        web_storage: storage.clone(),
        ..ctx
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (go_tx, go_rx) = channel::<std::net::TcpStream>();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            if first.starts_with("GET /go ") {
                let _ = go_tx.send(conn);
            } else {
                let body = "<title>evil</title>";
                let _ = conn.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    for (i, role) in snowlark_tab::evil::ROLES.iter().enumerate() {
        let id = i as TabId + 1;
        let launch = child_launch()
            .env(EVIL_ROLE, role)
            .env(EVIL_GO, addr.as_str())
            .env(EVIL_OWN, url.as_str())
            .env(EVIL_TAB, id.to_string().as_str());
        let mut tab = TabHost::spawn(
            id,
            ctx.clone(),
            Default::default(),
            DEVICE,
            url.clone(),
            remote(launch),
        );
        next_of(&rx, &mut tab, id, |e| match e {
            TabEvent::State(s) if s.title == "evil" => Some(()),
            TabEvent::Crashed(why) => panic!("{role}: crashed before it loaded: {why}"),
            _ => None,
        });
        let mut go = go_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap_or_else(|e| panic!("{role}: the renderer did not ask to go: {e}"));
        let _ = go.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        drop(go);
        if !snowlark_tab::evil::is_ended(role) {
            // (refused: the tab lives on, and the window hears nothing of it)
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match rx.recv_timeout(left) {
                    Ok((of, e)) if of == id => {
                        drive(&mut tab, &e);
                        assert!(
                            !matches!(e, TabEvent::Crashed(_) | TabEvent::Open(..)),
                            "{role}: {e:?}"
                        );
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            continue;
        }
        let why = next_of(&rx, &mut tab, id, |e| match e {
            TabEvent::Crashed(why) => Some(why),
            TabEvent::Open(..) | TabEvent::Internal(_) => panic!("{role}: served"),
            _ => None,
        });
        if snowlark_tab::evil::is_message(role) {
            assert!(why.contains("broke a rule"), "{role}: {why}");
        }
        tab.crashed();
    }
    assert!(storage.origins().is_empty());
}

fn remote_context() -> (
    TabContext,
    snowlark_net::NetThread,
    std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    std::sync::mpsc::Receiver<crate::composite::CompInput>,
) {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    (ctx, net, rx, comp_rx)
}

fn remote(launch: snowlark_ipc::Launch) -> Renderer {
    Renderer::Process(crate::tab_host::RemoteTab {
        launch,
        code_cache: Arc::new(crate::tab_host::NoCodeCache),
        muted: false,
        spare: None,
    })
}

const DEVICE: Device = Device {
    viewport: Size::new(800.0, 600.0),
    scale: 1.0,
    scrollbar: 0.0,
    screen: snowlark_page::ScreenPlacement {
        origin: (0, 0),
        outer: None,
        zoom: 1.0,
    },
};

/// The next event of tab `id` that `pick` takes, within 20 s, doing meanwhile what the window
/// does with the host's own events (a navigation's response: [`drive`]).
fn next_of<T>(
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    tab: &mut TabHost,
    id: TabId,
    mut pick: impl FnMut(TabEvent) -> Option<T>,
) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((of, e)) if of == id => {
                drive(tab, &e);
                if let Some(t) = pick(e) {
                    return t;
                }
            }
            Ok(_) => {}
            Err(e) => panic!("tab {id} did not say: {e}"),
        }
    }
}

/// The next event of tab `id` that `pick` takes, within 20 s (after the page loaded: nothing
/// of the host's own is expected).
fn next_event<T>(
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    id: TabId,
    mut pick: impl FnMut(TabEvent) -> Option<T>,
) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((of, e)) if of == id => {
                if let Some(t) = pick(e) {
                    return t;
                }
            }
            Ok(_) => {}
            Err(e) => panic!("tab {id} did not say: {e}"),
        }
    }
}

/// What the window does with the events a tab's host handles itself (M5: navigation in the
/// browser).
fn drive(tab: &mut TabHost, e: &TabEvent) {
    match e {
        TabEvent::NavigationReady(n) => tab.navigation_ready(*n),
        TabEvent::BeginNavigation(b) => tab.begin_navigation((**b).clone()),
        TabEvent::SameDocument { change, navigation } => {
            tab.same_document(change.clone(), *navigation)
        }
        TabEvent::Traverse(steps) => tab.go(*steps),
        TabEvent::State(s) => tab.page_state(s.clone()),
        TabEvent::Committed { url, scroll } => tab.committed(url.clone(), *scroll),
        TabEvent::DocumentUrl { url, navigation } => tab.document_url(url.clone(), *navigation),
        _ => {}
    }
}

#[test]
fn a_renderer_process_that_does_not_start_is_a_crashed_tab() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let launch =
        snowlark_ipc::Launch::new(r"Z:\no such folder\snowlark.exe").arg("--type=renderer");
    let url = Url::parse("data:text/html,<title>one</title>").unwrap();
    let mut tab = TabHost::spawn(4, ctx, Default::default(), DEVICE, url, remote(launch));
    let why = next_of(&rx, &mut tab, 4, |e| match e {
        TabEvent::Crashed(m) => Some(m),
        _ => None,
    });
    assert!(
        why.starts_with("the renderer process did not start"),
        "{why}"
    );
    assert!(tab.renderer_process().is_none() && tab.audio().is_none());
    // (the compositor is not given it, and what the window sends it goes nowhere)
    assert!(comp_rx.try_recv().is_err());
    tab.reload();
}

#[test]
fn a_tab_in_a_renderer_process_starts_again_in_a_new_one() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let launch = || {
        snowlark_ipc::Launch::new(std::env::current_exe().unwrap())
            .arg("tab_tests::renderer_child")
            .arg("--exact")
            .arg("--nocapture")
            .env(RENDERER_ROLE, "1")
    };
    let url = Url::parse("data:text/html,<title>one</title>one").unwrap();
    let mut tab = TabHost::spawn(
        1,
        ctx.clone(),
        Default::default(),
        DEVICE,
        url,
        remote(launch()),
    );
    let title = |want: &'static str| {
        move |e: TabEvent| match e {
            TabEvent::State(s) if s.title == want => Some(()),
            _ => None,
        }
    };
    next_of(&rx, &mut tab, 1, title("one"));
    let first = tab.renderer_process().unwrap().clone();
    // Killed: the window hears the tab crashed.
    first.kill();
    let why = next_of(&rx, &mut tab, 1, |e| match e {
        TabEvent::Crashed(m) => Some(m),
        _ => None,
    });
    assert!(why.contains("ended"), "{why}");
    // Reload starts a new process, which loads the entry again.
    tab.crashed();
    tab.respawn(2, ctx, DEVICE, remote(launch()));
    tab.reload();
    next_of(&rx, &mut tab, 2, title("one"));
    let second = tab.renderer_process().unwrap().clone();
    assert_ne!(first.pid(), second.pid());
    // Closing the tab ends its process (by itself, before the reaper's wait).
    drop(tab);
    let code = second.wait(std::time::Duration::from_secs(4));
    assert_eq!(code, Some(0), "the renderer did not end at the close");
}

/// A sandboxed renderer (`snowlark_sandbox`: its namespaces, then its system call filter once it
/// has its tab) runs its page: script, layout and text from the font files it mapped before.
#[cfg(target_os = "linux")]
#[test]
fn a_sandboxed_renderer_runs_its_page() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let url = Url::parse(
        "data:text/html,<span id=s>Hello</span><script>document.title = [typeof WebAssembly, \
         document.getElementById('s').getBoundingClientRect().width > 10].join(' ')</script>",
    )
    .unwrap();
    let mut tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch().sandbox(true)),
    );
    let title = next_of(&rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if !s.title.is_empty() => Some(s.title),
        _ => None,
    });
    assert_eq!(title, "object true");
}

/// This test binary as a renderer process ([`renderer_child`]).
fn child_launch() -> snowlark_ipc::Launch {
    snowlark_ipc::Launch::new(std::env::current_exe().unwrap())
        .arg("tab_tests::renderer_child")
        .arg("--exact")
        .arg("--nocapture")
        .env(RENDERER_ROLE, "1")
        .allow_env("SNOWLARK_TAB_STATS")
}

/// M4's exit test (`docs/process-isolation.md`): a tab whose renderer process is killed and
/// reloaded 1,000 times, each reload in a spare as with `--process-model=tab`. The browser's
/// handles and private bytes, measured after 100 cycles (warm-up: the network's, the allocator's
/// and the thread pools' own growth) and at the end, must not grow by more than a cycle's worth
/// in 30 (handles) or 4 KiB a cycle (private bytes): a leak of one handle or one renderer's
/// buffers per cycle is past that many times over.
///
/// `.\dev.ps1 test --release -p snowlark-shell --lib tab_tests::a_thousand_crashes_and_reloads
/// '--' --ignored --nocapture`
#[test]
#[ignore = "1,000 renderer processes, a few minutes: run it with --ignored"]
fn a_thousand_crashes_and_reloads_leave_the_browser_as_it_was() {
    const CYCLES: u64 = 1000;
    const WARM_UP: u64 = 100;
    let (ctx, _net, rx, comp_rx) = remote_context();
    let pool = snowlark_tab::spare::SparePool::start(child_launch);
    let remote = |pool: &snowlark_tab::spare::SparePool| {
        let spare = pool.take();
        assert!(spare.is_some(), "no spare");
        Renderer::Process(crate::tab_host::RemoteTab {
            launch: child_launch(),
            code_cache: Arc::new(crate::tab_host::NoCodeCache),
            muted: false,
            spare,
        })
    };
    let title = |e: TabEvent| match e {
        TabEvent::State(s) if s.title == "cycle" => Some(()),
        TabEvent::Crashed(why) => panic!("crashed before it loaded: {why}"),
        _ => None,
    };
    let crashed = |e: TabEvent| match e {
        TabEvent::Crashed(m) => Some(m),
        _ => None,
    };
    // (what the compositor would do with the frames: hand each back once shown)
    let drain = || {
        while let Ok(m) = comp_rx.try_recv() {
            if let crate::composite::CompInput::Frame(_, f) = m {
                let f = *f;
                f.recycler.recycle(f.frame);
            }
        }
    };
    let url = Url::parse(
        "data:text/html,<title>cycle</title><body style=background:%2308c>a page<canvas id=c>\
         </canvas><script>c.getContext('2d').fillRect(0,0,50,50)</script>",
    )
    .unwrap();
    let mut id: TabId = 1;
    let started = Instant::now();
    let mut tab = TabHost::spawn(
        id,
        ctx.clone(),
        Default::default(),
        DEVICE,
        url,
        remote(&pool),
    );
    next_of(&rx, &mut tab, id, title);
    let first = snowlark_ipc::own_usage().unwrap();
    let mut warm = first;
    for cycle in 1..=CYCLES {
        let process = tab.renderer_process().unwrap().clone();
        process.kill();
        let why = next_of(&rx, &mut tab, id, crashed);
        assert!(why.contains("ended"), "{why}");
        drop(process);
        tab.crashed();
        id += 1;
        tab.respawn(id, ctx.clone(), DEVICE, remote(&pool));
        tab.reload();
        next_of(&rx, &mut tab, id, title);
        drain();
        if cycle == WARM_UP {
            warm = snowlark_ipc::own_usage().unwrap();
        }
        if cycle % 100 == 0 {
            let now = snowlark_ipc::own_usage().unwrap();
            eprintln!(
                "cycle {cycle}: {} handles, {:.1} MiB private, {:.1} s",
                now.handles,
                now.private_bytes as f64 / 1048576.0,
                started.elapsed().as_secs_f64()
            );
        }
    }
    drop(tab);
    // (the last renderer's threads in the browser end with its pipe)
    std::thread::sleep(std::time::Duration::from_millis(500));
    drain();
    let end = snowlark_ipc::own_usage().unwrap();
    eprintln!(
        "start {} handles, {:.1} MiB; after {WARM_UP} cycles {} handles, {:.1} MiB; \
         after {CYCLES} {} handles, {:.1} MiB",
        first.handles,
        first.private_bytes as f64 / 1048576.0,
        warm.handles,
        warm.private_bytes as f64 / 1048576.0,
        end.handles,
        end.private_bytes as f64 / 1048576.0
    );
    let cycles = CYCLES - WARM_UP;
    let handles = i64::from(end.handles) - i64::from(warm.handles);
    assert!(
        handles <= (cycles / 30) as i64,
        "{handles} more handles after {cycles} cycles"
    );
    let bytes = end.private_bytes as i64 - warm.private_bytes as i64;
    assert!(
        bytes <= (cycles * 4096) as i64,
        "{bytes} more private bytes after {cycles} cycles"
    );
}

/// New-tab startup (`docs/process-isolation.md` §11, "Spare process"): from `TabHost::spawn` to the
/// page's first frame from the browser's raster thread, for a tab on a thread (`single`), in a
/// renderer process started for it (`tab`) and in a warm spare (`tab` with a spare), interleaved,
/// after one run of each that is not counted.
///
/// `.\dev.ps1 test --release -p snowlark-shell --lib tab_tests::new_tab_startup '--' --ignored
/// --nocapture`
#[test]
#[ignore = "a measurement: run it with --ignored --nocapture"]
fn new_tab_startup_times() {
    const RUNS: usize = 9;
    let (ctx, _net, rx, comp_rx) = remote_context();
    let pool = snowlark_tab::spare::SparePool::start(child_launch);
    let url = Url::parse(
        "data:text/html,<title>new</title><body style=background:%2308c><h1>A new tab</h1>\
         <p>Some text to lay out and draw.</p>",
    )
    .unwrap();
    let mut times: [Vec<f64>; 3] = Default::default();
    let mut id: TabId = 100;
    for run in 0..=RUNS {
        for (mode, spent) in times.iter_mut().enumerate() {
            let renderer = match mode {
                0 => Renderer::Thread(crate::page_driver::PageDriver::factory()),
                1 => remote(child_launch()),
                _ => {
                    // (a spare that has had time to warm up, as between a user's new tabs)
                    pool.ready(std::time::Duration::from_secs(20)).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    Renderer::Process(crate::tab_host::RemoteTab {
                        launch: child_launch(),
                        code_cache: Arc::new(crate::tab_host::NoCodeCache),
                        muted: false,
                        spare: pool.take(),
                    })
                }
            };
            while comp_rx.try_recv().is_ok() {}
            id += 1;
            let t0 = Instant::now();
            let mut tab = TabHost::spawn(
                id,
                ctx.clone(),
                Default::default(),
                DEVICE,
                url.clone(),
                renderer,
            );
            let until = t0 + std::time::Duration::from_secs(20);
            let first = loop {
                // (the host's part of the navigation, as the window does it)
                while let Ok((of, e)) = rx.try_recv() {
                    if of == id {
                        drive(&mut tab, &e);
                    }
                }
                match comp_rx.recv_timeout(std::time::Duration::from_millis(1)) {
                    Ok(crate::composite::CompInput::Frame(of, f)) if of == id => {
                        let at = t0.elapsed();
                        let f = *f;
                        f.recycler.recycle(f.frame);
                        break at;
                    }
                    Ok(_) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() < until => {}
                    Err(e) => panic!("no frame in mode {mode}: {e}"),
                }
            };
            next_of(&rx, &mut tab, id, |e| match e {
                TabEvent::State(s) if s.title == "new" => Some(()),
                _ => None,
            });
            if run > 0 {
                spent.push(first.as_secs_f64() * 1000.0);
            }
            drop(tab);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
    for (name, mut t) in ["single", "tab", "tab with a spare"].into_iter().zip(times) {
        t.sort_by(f64::total_cmp);
        eprintln!(
            "{name}: median {:.1} ms, {:.1} to {:.1} ms ({:?})",
            t[t.len() / 2],
            t[0],
            t[t.len() - 1],
            t.iter()
                .map(|v| (v * 10.0).round() / 10.0)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn a_page_renders_at_the_begin_frames_it_asks_for() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    let device = Device {
        viewport: Size::new(800.0, 600.0),
        scale: 1.0,
        scrollbar: 0.0,
        screen: Default::default(),
    };
    let url = Url::parse(BEGIN_FRAMES_PAGE).unwrap();
    let tab = spawn(
        1,
        ctx,
        device,
        url,
        crate::page_driver::PageDriver::factory(),
    );
    renders_at_begin_frames(tab, &rx, &comp_rx);
}

/// The same in a renderer process: what it asks of the compositor reaches it (a renderer may tell
/// it whether its own tab needs begin frames, as a renderer's frame sink tells viz
/// `SetNeedsBeginFrame` for itself only), and the begin frames reach the page.
#[test]
fn a_page_in_a_renderer_process_renders_at_the_begin_frames_it_asks_for() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(BEGIN_FRAMES_PAGE).unwrap();
    let tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    renders_at_begin_frames(tab, &rx, &comp_rx);
}

/// Five animation frames, each counted in the title; a click starts five more.
const BEGIN_FRAMES_PAGE: &str = "data:text/html,<title>0</title><script>var n = 0; function f() {     document.title = String(++n); if (n % 5) requestAnimationFrame(f); } requestAnimationFrame(f);     onclick = () => requestAnimationFrame(f);</script>";

fn renders_at_begin_frames(
    tab: TabHost,
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    comp_rx: &std::sync::mpsc::Receiver<crate::composite::CompInput>,
) {
    use crate::composite::{CompInput, CompMsg};
    use snowlark_tab::comp::BeginFrameArgs;
    use std::time::Duration;
    let mut tab = tab;
    // (what the compositor sends a tab it binds)
    tab.send(TabCommand::UseBeginFrames);
    // (the host's part of the navigation, as the window does it; the page's first state)
    next_of(rx, &mut tab, 1, |e| match e {
        TabEvent::State(_) => Some(()),
        _ => None,
    });
    let asks = |needs: bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match comp_rx.recv_timeout(left) {
                Ok(CompInput::Msg(CompMsg::NeedsBeginFrames { tab: 1, needs: n }))
                    if n == needs =>
                {
                    return
                }
                Ok(_) => {}
                Err(e) => panic!("the tab never said it needs begin frames: {needs}: {e}"),
            }
        }
    };
    let title = || {
        let mut title = None;
        while let Ok((_, e)) = rx.recv_timeout(Duration::from_millis(100)) {
            if let TabEvent::State(s) = e {
                title = Some(s.title);
            }
        }
        title
    };
    asks(true);
    // No frame comes from a timer.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        title().as_deref().unwrap_or("0"),
        "0",
        "no begin frame, no frame"
    );
    let interval = Duration::from_micros(5556);
    let vsync = || {
        tab.send(TabCommand::BeginFrame(BeginFrameArgs {
            frame_time: snowlark_paint::Ticks::now(),
            interval,
            missed: false,
        }));
        std::thread::sleep(Duration::from_millis(30));
    };
    for _ in 0..5 {
        vsync();
    }
    assert_eq!(title().as_deref(), Some("5"), "a frame per begin frame");
    // One begin frame more with nothing to do, and the page stops asking.
    vsync();
    asks(false);
    tab.send(TabCommand::Pointer(Pointer::Down(
        Point::new(10.0, 10.0),
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
        1,
    )));
    tab.send(TabCommand::Pointer(Pointer::Up(
        Point::new(10.0, 10.0),
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
    )));
    asks(true);
}

/// A dark page as sites make one: the color on the body, the color scheme left light.
const DARK_PAGE: &str = "data:text/html,<title>dark</title><body style='margin:0;background:rgb(5,11,26)'><div style=height:9000px></div>";

/// What the compositor shows where a frame has no tile (a part scrolled into view before
/// its raster) is the page's background, the canvas background's color, not the white canvas
/// surface under it: a dark page scrolled fast showed white bands.
#[test]
fn a_frame_names_the_pages_background_for_what_it_does_not_cover() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(DARK_PAGE).unwrap();
    let tab = spawn(
        1,
        ctx,
        DEVICE,
        url,
        crate::page_driver::PageDriver::factory(),
    );
    frame_names_the_pages_background(tab, &rx, &comp_rx);
}

/// The same in a renderer process.
#[test]
fn a_frame_from_a_renderer_process_names_the_pages_background() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(DARK_PAGE).unwrap();
    let tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    frame_names_the_pages_background(tab, &rx, &comp_rx);
}

fn frame_names_the_pages_background(
    tab: TabHost,
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    comp_rx: &std::sync::mpsc::Receiver<crate::composite::CompInput>,
) {
    use crate::composite::CompInput;
    use std::time::Duration;
    let mut tab = tab;
    next_of(rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if s.title == "dark" => Some(()),
        _ => None,
    });
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match comp_rx.recv_timeout(left) {
            Ok(CompInput::Frame(1, f)) => {
                let (shows, background) = (f.content.height == 9000.0, f.background);
                f.recycle();
                if shows {
                    assert_eq!(background, snowlark_paint::Color::rgb(5, 11, 26));
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => panic!("no frame of the page: {e}"),
        }
    }
}

/// A page 9000 px tall that, on a click, puts in other content (8000 px) and scrolls to 5000 in the
/// same task, as a router does for a same-document navigation.
const SCROLLING_PAGE: &str = "data:text/html,<title>tall</title><body style=margin:0><div id=a     style=height:9000px;background:navy></div><script>onclick = () => {     a.style.height = '8000px'; a.style.background = 'green'; scrollTo(0, 5000); };</script>";

/// When a page scrolls its viewport itself, the compositor is told in the frame painted
/// for the new place: the frame brings the scroll, shows the page's content of then and covers the
/// viewport there, and so does every frame after it. (Told before the frame, the compositor
/// showed the old frame at the new place, then the new content painted around the old place: a
/// blank page for a frame or more.) A scroll that needs no new frame comes as a message naming
/// the last frame painted.
#[test]
fn a_pages_scroll_is_told_with_the_frame_painted_for_it() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(SCROLLING_PAGE).unwrap();
    let tab = spawn(
        1,
        ctx,
        DEVICE,
        url,
        crate::page_driver::PageDriver::factory(),
    );
    scroll_is_told_with_its_frame(tab, &rx, &comp_rx, false);
}

/// A place the compositor reports while the page's scroll is on its way to it (the user's wheel
/// moving the frame on screen, painted for the place before the scroll) names the last of the
/// page's scrolls shown, and the tab does not take it: the page stays where it scrolled, and the
/// frames go on covering the viewport there. (Taken, it scrolled the page back for a frame.)
#[test]
fn a_report_from_before_the_pages_scroll_does_not_undo_it() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(SCROLLING_PAGE).unwrap();
    let tab = spawn(
        1,
        ctx,
        DEVICE,
        url,
        crate::page_driver::PageDriver::factory(),
    );
    scroll_is_told_with_its_frame(tab, &rx, &comp_rx, true);
}

/// The same in a renderer process.
#[test]
fn a_report_from_before_the_pages_scroll_in_a_renderer_process_does_not_undo_it() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(SCROLLING_PAGE).unwrap();
    let tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    scroll_is_told_with_its_frame(tab, &rx, &comp_rx, true);
}

/// The same in a renderer process.
#[test]
fn a_pages_scroll_in_a_renderer_process_is_told_with_the_frame_painted_for_it() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(SCROLLING_PAGE).unwrap();
    let tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    scroll_is_told_with_its_frame(tab, &rx, &comp_rx, false);
}

/// `wheel`: once the frame with the page's scroll has come, the compositor reports the viewport
/// at 40, moved there from the frame on screen before it showed that one.
fn scroll_is_told_with_its_frame(
    tab: TabHost,
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    comp_rx: &std::sync::mpsc::Receiver<crate::composite::CompInput>,
    wheel: bool,
) {
    use crate::composite::{uncovered_rows, CompInput, CompMsg};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;
    let mut tab = tab;
    next_of(rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if s.title == "tall" => Some(()),
        _ => None,
    });
    // The page shows, at its top.
    let until = Instant::now() + Duration::from_secs(20);
    let generation = loop {
        let left = until.saturating_duration_since(Instant::now());
        match comp_rx.recv_timeout(left) {
            Ok(CompInput::Frame(1, f)) => {
                let (shows, generation) = (f.content.height == 9000.0, f.generation);
                f.recycle();
                if shows {
                    break generation;
                }
            }
            Ok(_) => {}
            Err(e) => panic!("no frame of the page: {e}"),
        }
    };
    tab.send(TabCommand::Pointer(Pointer::Down(
        Point::new(10.0, 10.0),
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
        1,
    )));
    tab.send(TabCommand::Pointer(Pointer::Up(
        Point::new(10.0, 10.0),
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
    )));
    let to = Point::new(0.0, 5000.0);
    let mut wheel = wheel;
    // Each frame's number, the scroll it brings, its content height and the rows of the viewport
    // at 5000 it leaves blank.
    let mut frames = Vec::new();
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match comp_rx.recv_timeout(Duration::from_millis(300)) {
            Ok(CompInput::Msg(CompMsg::ScrollTo { tab: 1, .. })) => {
                panic!("a scroll that has a frame of its own comes in the frame")
            }
            Ok(CompInput::Frame(1, f)) => {
                let blank = uncovered_rows(&f, to, DEVICE.viewport.height as u32);
                frames.push((f.number, f.scroll, f.content.height, blank));
                if wheel && f.scroll.is_some() {
                    // The frame on screen, which shows none of the page's scrolls, moved before
                    // this one shows.
                    wheel = false;
                    tab.send(TabCommand::Scrolled(
                        Point::new(0.0, 40.0),
                        snowlark_tab::comp::ShownScroll {
                            generation,
                            serial: 0,
                        },
                    ));
                }
                f.recycle();
            }
            Ok(_) => {}
            // (quiet: the scroll's frame has come, and what followed it)
            Err(RecvTimeoutError::Timeout) => {
                if frames.iter().any(|f| f.1.is_some()) {
                    break;
                }
                assert!(
                    Instant::now() < until,
                    "no frame with the scroll: {frames:?}"
                );
            }
            Err(e) => panic!("the compositor's channel closed: {e}"),
        }
    }
    let first = frames.iter().position(|f| f.1.is_some()).unwrap();
    let scroll = frames[first].1.unwrap();
    assert_eq!((scroll.to, scroll.serial), (to, 1), "{frames:?}");
    for &(number, _, content, blank) in &frames[first..] {
        assert_eq!(
            (content, blank),
            (8000.0, 0),
            "frame {number} of {frames:?}"
        );
    }
}

/// A page 1100 px tall (all of it is in every frame's band) that scrolls to 300 on a click and
/// changes nothing else.
const SHORT_SCROLLING_PAGE: &str = "data:text/html,<title>short</title><body style=margin:0><div     style=height:1100px;background:navy></div><script>onclick = () => scrollTo(0, 300);</script>";

/// A scroll of the page's for which it has nothing new to paint (the frame on screen shows that
/// place already) comes as a message that names the last frame painted, so the compositor shows
/// it no earlier than that frame.
#[test]
fn a_pages_scroll_with_nothing_new_to_paint_names_the_last_frame() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(SHORT_SCROLLING_PAGE).unwrap();
    let tab = spawn(
        1,
        ctx,
        DEVICE,
        url,
        crate::page_driver::PageDriver::factory(),
    );
    scroll_names_the_last_frame(tab, &rx, &comp_rx);
}

/// The same in a renderer process.
#[test]
fn a_pages_scroll_in_a_renderer_process_with_nothing_new_to_paint_names_the_last_frame() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let url = Url::parse(SHORT_SCROLLING_PAGE).unwrap();
    let tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    scroll_names_the_last_frame(tab, &rx, &comp_rx);
}

fn scroll_names_the_last_frame(
    tab: TabHost,
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    comp_rx: &std::sync::mpsc::Receiver<crate::composite::CompInput>,
) {
    use crate::composite::{CompInput, CompMsg};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;
    let mut tab = tab;
    next_of(rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if s.title == "short" => Some(()),
        _ => None,
    });
    // The page's frames, until no more come: the last one's number and document.
    let mut last = None;
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match comp_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(CompInput::Frame(1, f)) => {
                if f.content.height == 1100.0 {
                    last = Some((f.number, f.generation));
                }
                f.recycle();
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) if last.is_some() => break,
            Err(RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < until, "no frame of the page")
            }
            Err(e) => panic!("the compositor's channel closed: {e}"),
        }
    }
    let (number, generation) = last.unwrap();
    tab.send(TabCommand::Pointer(Pointer::Down(
        Point::new(10.0, 10.0),
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
        1,
    )));
    tab.send(TabCommand::Pointer(Pointer::Up(
        Point::new(10.0, 10.0),
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
    )));
    let until = Instant::now() + Duration::from_secs(20);
    let with = loop {
        let left = until.saturating_duration_since(Instant::now());
        match comp_rx.recv_timeout(left) {
            Ok(CompInput::Msg(CompMsg::ScrollTo {
                tab: 1,
                to,
                smooth: false,
                with,
            })) => {
                assert_eq!(to, Point::new(0.0, 300.0));
                break with.expect("the page's scroll names a frame");
            }
            Ok(CompInput::Frame(1, _)) => panic!("nothing new to paint, yet a frame"),
            Ok(_) => {}
            Err(e) => panic!("the scroll did not come: {e}"),
        }
    };
    assert_eq!(
        with,
        crate::composite::FrameRef {
            frame: number,
            generation,
            serial: 1
        }
    );
}

/// The console's copy rows reach a page in a renderer process: an evaluation's value comes back
/// with its kind, Copy object and Copy string contents come back as text for the window's
/// clipboard, and Store as global variable defines `temp1` there.
#[test]
fn the_console_copies_results_of_a_page_in_a_renderer_process() {
    use crate::devtools::protocol::{DevtoolsCommand, DevtoolsEvent, ResultOp};
    use snowlark_page::devtools::ValueKind;
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let url = Url::parse("data:text/html,<title>t</title>").unwrap();
    let mut tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    next_of(&rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if s.title == "t" => Some(()),
        _ => None,
    });
    let devtools = |c: DevtoolsCommand| tab.send(TabCommand::Devtools(c));
    let answer = |pick: &mut dyn FnMut(DevtoolsEvent) -> Option<String>| {
        next_event(&rx, 1, |e| match e {
            TabEvent::Devtools(d) => pick(*d),
            _ => None,
        })
    };
    devtools(DevtoolsCommand::Attach(true));
    devtools(DevtoolsCommand::Eval(1, "({a: 'b'})".into()));
    let shown = answer(&mut |e| match e {
        DevtoolsEvent::EvalResult(1, Ok((text, ValueKind::Object))) => Some(text),
        _ => None,
    });
    assert!(shown.contains('a'), "{shown}");
    devtools(DevtoolsCommand::Result(1, ResultOp::Copy));
    let copied = answer(&mut |e| match e {
        DevtoolsEvent::Clipboard(t) => Some(t),
        _ => None,
    });
    assert_eq!(copied, "{\n    \"a\": \"b\"\n}");
    devtools(DevtoolsCommand::Eval(2, "'x y'".into()));
    devtools(DevtoolsCommand::Result(2, ResultOp::Copy));
    let copied = answer(&mut |e| match e {
        DevtoolsEvent::Clipboard(t) => Some(t),
        _ => None,
    });
    assert_eq!(copied, "x y");
    devtools(DevtoolsCommand::Result(1, ResultOp::Store));
    let name = answer(&mut |e| match e {
        DevtoolsEvent::Stored { name, .. } => Some(name),
        _ => None,
    });
    assert_eq!(name, "temp1");
}

/// Reload loads the current session history entry's URL, which `pushState`, `replaceState` and a
/// fragment navigation set (HTML "reload"; Chromium's `NavigationControllerImpl::Reload` loads the
/// current entry, whose URL `RendererDidNavigateToNewEntry`/`...ToExistingEntry` set at each
/// same-document commit): a pushed URL the server never served is the one asked for.
#[test]
fn reload_loads_the_url_a_page_pushed_or_replaced() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let origin = same_document_server();
    let url = Url::parse(&format!("{origin}/")).unwrap();
    let tab = spawn(
        1,
        ctx,
        DEVICE,
        url,
        crate::page_driver::PageDriver::factory(),
    );
    reloads_the_current_url(tab, &rx, &origin);
}

/// The same with the tab in a renderer process.
#[test]
fn reload_in_a_renderer_process_loads_the_url_a_page_pushed_or_replaced() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let origin = same_document_server();
    let url = Url::parse(&format!("{origin}/")).unwrap();
    let tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    reloads_the_current_url(tab, &rx, &origin);
}

/// A server whose every page (`/slow` after 1.5 s) titles itself with how many pages the server
/// had served and its own URL's path, query and fragment, and on each click moves to another URL
/// of the same document: `pushState` at `/`, `replaceState` at `/watch`, a fragment navigation at
/// `/replaced`, `pushState` again after that.
fn same_document_server() -> String {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicU32, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let served = Arc::new(AtomicU32::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let served = served.clone();
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&request);
                let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
                let response = if path == "/favicon.ico" {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                } else {
                    if path == "/slow" {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                    }
                    let n = served.fetch_add(1, Ordering::SeqCst) + 1;
                    let page = format!(
                        "<!doctype html><title>?</title><body style='margin:0;height:100vh'><script>\
                         function show() {{ document.title = '{n} ' + location.pathname + location.search + location.hash; }}\
                         show();\
                         onclick = () => {{\
                           if (location.pathname == '/') history.pushState(null, '', '/watch?v=1');\
                           else if (location.pathname == '/watch') history.replaceState(null, '', '/replaced');\
                           else if (location.hash == '') location.hash = 'frag';\
                           else history.pushState(null, '', '/old-page-pushed');\
                           show();\
                         }};</script>"
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                        page.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
    origin
}

fn reloads_the_current_url(
    mut tab: TabHost,
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    origin: &str,
) {
    // (what the window does with a tab's events)
    let hear_until = |tab: &mut TabHost, title: &str| {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while tab.state.title != title {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok((_, e)) => drive(tab, &e),
                Err(e) => panic!("no title {title:?} (it is {:?}): {e}", tab.state.title),
            }
        }
    };
    let click = |tab: &TabHost| {
        for p in [
            Pointer::Down(
                Point::new(10.0, 10.0),
                snowlark_paint::input::MouseButton::Left,
                Default::default(),
                1,
            ),
            Pointer::Up(
                Point::new(10.0, 10.0),
                snowlark_paint::input::MouseButton::Left,
                Default::default(),
            ),
        ] {
            tab.send(TabCommand::Pointer(p));
        }
    };
    let current = |tab: &TabHost| tab.current_url().map(|u| u.as_str().to_owned());
    let at = |path: &str| Some(format!("{origin}{path}"));
    hear_until(&mut tab, "1 /");
    // pushState: the entry takes the new URL, and Reload asks the server for it.
    click(&tab);
    hear_until(&mut tab, "1 /watch?v=1");
    assert_eq!(current(&tab), at("/watch?v=1"));
    tab.reload();
    hear_until(&mut tab, "2 /watch?v=1");
    // replaceState and a fragment navigation, the same.
    click(&tab);
    hear_until(&mut tab, "2 /replaced");
    assert_eq!(current(&tab), at("/replaced"));
    click(&tab);
    hear_until(&mut tab, "2 /replaced#frag");
    assert_eq!(current(&tab), at("/replaced#frag"));
    tab.reload();
    hear_until(&mut tab, "3 /replaced#frag");
    // (M5: the pushState and the fragment navigation are entries of their own)
    assert_eq!(tab.history_len(), 3);
    // A URL the page shown takes while a navigation of the window's loads is that document's: the
    // new entry keeps the URL it is loading (Chromium's pending entry).
    tab.navigate(Url::parse(&format!("{origin}/slow")).unwrap());
    click(&tab);
    hear_until(&mut tab, "3 /old-page-pushed");
    assert_eq!(current(&tab), at("/slow"));
    hear_until(&mut tab, "4 /slow");
    assert_eq!(current(&tab), at("/slow"));
    assert_eq!(tab.history_len(), 4);
}

// ----- M5: navigation in the browser ---------------------------------------------------------------

/// A server answering each path with the HTML given.
fn serve_html(pages: Vec<(&'static str, &'static str)>) -> String {
    serve_html_at(
        "127.0.0.1:0",
        pages.into_iter().map(|(p, b)| (p, b.to_owned())).collect(),
    )
}

/// `serve_html` on `addr` (another loopback address is another site).
fn serve_html_at(addr: &str, pages: Vec<(&'static str, String)>) -> String {
    let answers = pages
        .into_iter()
        .map(|(p, body)| {
            (
                p,
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
            )
        })
        .collect();
    serve_answers_at(addr, answers)
}

/// Serves each path's whole HTTP answer (status line, headers and body), whatever the method.
fn serve_answers_at(addr: &str, pages: Vec<(&'static str, String)>) -> String {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind(addr).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let pages = pages.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let mut body_len = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            body_len = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                // (a request's body is read, so closing the connection does not reset it)
                let mut body = vec![0; body_len];
                let _ = reader.read_exact(&mut body);
                let path = first.split(' ').nth(1).unwrap_or("").to_owned();
                let answer = match pages.iter().find(|(p, _)| *p == path) {
                    Some((_, answer)) => answer.clone(),
                    None => {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into()
                    }
                };
                let _ = conn.write_all(answer.as_bytes());
            });
        }
    });
    base
}

/// A tab whose page navigates through the browser (M5): the user's navigation is fetched by the
/// host and committed by the renderer; a link the page follows begins in the host and gets its
/// entry at the commit; `pushState` is an entry; Back within the document traverses in place
/// (`popstate`), and `history.forward()` from the page asks the host; Back to another document
/// loads it; Reload loads the entry again.
fn navigates_through_the_browser(
    ctx: TabContext,
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    renderer: Renderer,
) {
    use crate::devtools::protocol::{DevtoolsCommand, DevtoolsEvent};
    let base = serve_html(vec![
        (
            "/one",
            "<title>one</title><a id=l href=/two>two</a>",
        ),
        (
            "/two",
            "<title>two</title><script>onpopstate = e => document.title = 'pop' + (e.state ? e.state.n : 'null');</script>",
        ),
    ]);
    let one = Url::parse(&format!("{base}/one")).unwrap();
    let two = Url::parse(&format!("{base}/two")).unwrap();
    let mut tab = TabHost::spawn(1, ctx, Default::default(), DEVICE, one.clone(), renderer);
    let title = |want: &'static str| {
        move |e: TabEvent| match e {
            TabEvent::State(s) if s.title == want => Some(s),
            _ => None,
        }
    };
    next_of(rx, &mut tab, 1, title("one"));
    assert_eq!((tab.history_len(), tab.current_url()), (1, Some(&one)));
    tab.send(TabCommand::Devtools(DevtoolsCommand::Attach(true)));
    let eval = |tab: &mut TabHost, n: u64, js: &str| {
        tab.send(TabCommand::Devtools(DevtoolsCommand::Eval(n, js.into())));
        next_of(rx, tab, 1, move |e| match e {
            TabEvent::Devtools(d) => match *d {
                DevtoolsEvent::EvalResult(m, r) if m == n => Some(r),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|e| panic!("{js}: {e}"));
    };
    // The page's link: the host begins it, the renderer commits it, the entry is the commit's.
    eval(&mut tab, 1, "document.getElementById('l').click()");
    next_of(rx, &mut tab, 1, title("two"));
    assert_eq!((tab.history_len(), tab.current_url()), (2, Some(&two)));
    assert!(tab.can_go(-1) && !tab.can_go(1));
    // pushState: an entry of the same document.
    // (the tab says so before it answers the eval, in the same pass: `eval` drove it)
    eval(&mut tab, 2, "history.pushState({n: 1}, '', '/two/p')");
    assert_eq!(tab.history_len(), 3);
    assert_eq!(
        tab.current_url().map(Url::as_str),
        Some(format!("{base}/two/p").as_str())
    );
    // Back within the document: `popstate` with a null state, no load.
    tab.go(-1);
    next_of(rx, &mut tab, 1, title("popnull"));
    assert_eq!(tab.current_url(), Some(&two));
    // `history.forward()` from the page: the host traverses, in the document again.
    eval(&mut tab, 3, "history.forward()");
    next_of(rx, &mut tab, 1, title("pop1"));
    assert_eq!(tab.trail().index, 2);
    // Back twice: another document loads, and forward is possible.
    tab.go(-2);
    next_of(rx, &mut tab, 1, title("one"));
    assert_eq!((tab.trail().index, tab.current_url()), (0, Some(&one)));
    assert!(tab.can_go(1));
    // Reload: the entry loads again (the state says of which navigation: the fifth after the
    // tab's first load, which is numbered 0: the link, Back, Forward, Back twice, Reload).
    tab.reload();
    assert!(tab.navigating());
    next_of(rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if s.title == "one" && s.navigations == 5 && !s.loading => Some(s),
        _ => None,
    });
    assert_eq!((tab.history_len(), tab.trail().index), (3, 0));
}

#[test]
fn a_page_navigates_through_the_browser() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let (compositor, _comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: net.client(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: snowlark_page::BroadcastHub::default(),
        web_storage: snowlark_page::web_storage::WebStorageHost::new(),
        idb: snowlark_page::IdbBackend::in_memory(),
        caches: snowlark_page::CacheBackend::in_memory(),
        service_workers: None,
        cdm: None,
    };
    navigates_through_the_browser(
        ctx,
        &rx,
        Renderer::Thread(crate::page_driver::PageDriver::factory()),
    );
}

/// The same with the page in a renderer process: the response's body crosses the pipe as a
/// handed-over stream.
#[test]
fn a_page_in_a_renderer_process_navigates_through_the_browser() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    navigates_through_the_browser(ctx, &rx, remote(child_launch()));
}

/// Back within a document, a crash, then Forward: the entry is of the document the crash took, and
/// the new renderer has none, so it loads (a history step is to another document when the
/// renderer has committed nothing). Before, the host traversed "in place" in the empty renderer and
/// the tab stayed blank.
#[test]
fn forward_from_a_crashed_tab_loads_an_entry_of_the_lost_document() {
    use crate::devtools::protocol::{DevtoolsCommand, DevtoolsEvent};
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let base = serve_html(vec![
        ("/one", "<title>one</title>"),
        (
            "/two",
            "<title>two</title><script>onpopstate = () => document.title = 'pop';</script>",
        ),
        ("/two/p", "<title>two-p</title>"),
    ]);
    let one = Url::parse(&format!("{base}/one")).unwrap();
    let two = Url::parse(&format!("{base}/two")).unwrap();
    let pushed = Url::parse(&format!("{base}/two/p")).unwrap();
    let mut tab = TabHost::spawn(
        1,
        ctx.clone(),
        Default::default(),
        DEVICE,
        one,
        remote(child_launch()),
    );
    let title = |want: &'static str| {
        move |e: TabEvent| match e {
            TabEvent::State(s) if s.title == want => Some(s),
            _ => None,
        }
    };
    // (the tab's first document is not traversed in place: a navigation first)
    next_of(&rx, &mut tab, 1, title("one"));
    tab.navigate(two.clone());
    next_of(&rx, &mut tab, 1, title("two"));
    tab.send(TabCommand::Devtools(DevtoolsCommand::Attach(true)));
    tab.send(TabCommand::Devtools(DevtoolsCommand::Eval(
        1,
        "history.pushState(null, '', '/two/p')".into(),
    )));
    next_of(&rx, &mut tab, 1, |e| match e {
        TabEvent::Devtools(d) => matches!(*d, DevtoolsEvent::EvalResult(1, _)).then_some(()),
        _ => None,
    });
    assert_eq!(tab.history_len(), 3);
    tab.go(-1);
    next_of(&rx, &mut tab, 1, title("pop"));
    assert_eq!(tab.current_url(), Some(&two));

    let process = tab.renderer_process().unwrap().clone();
    process.kill();
    next_of(&rx, &mut tab, 1, |e| match e {
        TabEvent::Crashed(m) => Some(m),
        _ => None,
    });
    tab.crashed();
    tab.respawn(2, ctx, DEVICE, remote(child_launch()));
    tab.go(1);
    let s = next_of(&rx, &mut tab, 2, title("two-p"));
    assert_eq!(s.url, pushed.as_str());
    assert_eq!(tab.current_url(), Some(&pushed));
    assert_eq!((tab.history_len(), tab.trail().index), (3, 2));
}

/// M5 step 2, a process per site: a link to another site commits in a new renderer, which the
/// window starts when the host asks (`TabHost::needs_swap`, `swap_renderer`); Back to the first
/// site is a third renderer; the session history is the host's throughout, and the old renderer
/// closes.
fn navigates_across_sites(
    ctx: TabContext,
    rx: &std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    mut renderer: impl FnMut() -> Renderer,
) {
    use crate::devtools::protocol::{DevtoolsCommand, DevtoolsEvent};
    let b = serve_html_at(
        "127.0.0.2:0",
        vec![("/two", "<title>two</title>".to_owned())],
    );
    let a = serve_html_at(
        "127.0.0.1:0",
        vec![(
            "/one",
            format!("<title>one</title><a id=l href={b}/two>two</a>"),
        )],
    );
    let one = Url::parse(&format!("{a}/one")).unwrap();
    let two = Url::parse(&format!("{b}/two")).unwrap();
    let mut tab = TabHost::spawn(
        1,
        ctx.clone(),
        Default::default(),
        DEVICE,
        one.clone(),
        renderer(),
    );
    let title = |want: &'static str| {
        move |e: TabEvent| match e {
            TabEvent::State(s) if s.title == want => Some(s),
            _ => None,
        }
    };
    let ready = |e: TabEvent| match e {
        TabEvent::NavigationReady(_) => Some(()),
        _ => None,
    };
    next_of(rx, &mut tab, 1, title("one"));
    tab.send(TabCommand::Devtools(DevtoolsCommand::Attach(true)));
    tab.send(TabCommand::Devtools(DevtoolsCommand::Eval(
        1,
        "document.getElementById('l').click()".into(),
    )));
    next_of(rx, &mut tab, 1, |e| match e {
        TabEvent::Devtools(d) => match *d {
            DevtoolsEvent::EvalResult(1, _) => Some(()),
            _ => None,
        },
        _ => None,
    });
    // The response is from another site: the host waits for a renderer for it (the window
    // starts one), and the first renderer closes.
    let first = tab.renderer_process().cloned();
    next_of(rx, &mut tab, 1, ready);
    assert!(tab.needs_swap());
    tab.swap_renderer(2, ctx.clone(), DEVICE, renderer());
    assert_eq!(tab.id, 2);
    next_of(rx, &mut tab, 2, title("two"));
    assert_eq!((tab.history_len(), tab.current_url()), (2, Some(&two)));
    assert!(tab.can_go(-1) && !tab.can_go(1));
    if let Some(p) = first {
        assert!(
            p.wait(std::time::Duration::from_secs(5)).is_some(),
            "the first renderer did not close"
        );
    }
    // Back: the first site again, in a third renderer, with the history intact.
    tab.go(-1);
    next_of(rx, &mut tab, 2, ready);
    assert!(tab.needs_swap());
    tab.swap_renderer(3, ctx, DEVICE, renderer());
    next_of(rx, &mut tab, 3, title("one"));
    assert_eq!((tab.trail().index, tab.current_url()), (0, Some(&one)));
    assert!(!tab.can_go(-1) && tab.can_go(1));
}

#[test]
fn a_page_navigates_across_sites() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    navigates_across_sites(ctx, &rx, || {
        Renderer::Thread(crate::page_driver::PageDriver::factory())
    });
}

/// The same with each site's page in a renderer process of its own.
#[test]
fn a_page_in_renderer_processes_navigates_across_sites() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    navigates_across_sites(ctx, &rx, || remote(child_launch()));
}

/// The Fullscreen API across the renderer pipe: the page's request reaches the window as
/// `TabEvent::Fullscreen(true)` and goes on only when the window answers; the window's own exit
/// (the user's Esc) ends the page's fullscreen.
#[test]
fn a_page_in_a_renderer_process_asks_the_window_for_fullscreen() {
    use crate::devtools::protocol::{DevtoolsCommand, DevtoolsEvent};
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let url = Url::parse(
        "data:text/html,<title>t</title><div id=v>x</div>\
         <script>onkeydown = () => v.requestFullscreen().catch(() => {})</script>",
    )
    .unwrap();
    let mut tab = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    next_of(&rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if s.title == "t" => Some(()),
        _ => None,
    });
    tab.send(TabCommand::Devtools(DevtoolsCommand::Attach(true)));
    let mut n = 0;
    let mut eval = |tab: &TabHost, js: &str| {
        n += 1;
        tab.send(TabCommand::Devtools(DevtoolsCommand::Eval(n, js.into())));
        next_event(&rx, 1, |e| match e {
            TabEvent::Devtools(d) => match *d {
                DevtoolsEvent::EvalResult(i, Ok((text, _))) if i == n => Some(text),
                _ => None,
            },
            _ => None,
        })
    };
    let fullscreen = "String(document.fullscreenElement === v)";
    // (a key press is the user activation the request needs)
    tab.send(TabCommand::Key(KeyInput {
        kind: KeyKind::Down { repeat: false },
        key: 0x41,
        scan: 0x1E,
        text: Some('a'),
        mods: Default::default(),
    }));
    next_event(&rx, 1, |e| {
        matches!(e, TabEvent::Fullscreen(true)).then_some(())
    });
    assert_eq!(eval(&tab, fullscreen), "false");
    tab.send(TabCommand::Fullscreen(true));
    let until = |eval: &mut dyn FnMut(&TabHost, &str) -> String, want: &str| {
        for _ in 0..100 {
            if eval(&tab, fullscreen) == want {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("never {want}");
    };
    until(&mut eval, "true");
    tab.send(TabCommand::Fullscreen(false));
    until(&mut eval, "false");
}

/// A page that opens window 7 when it loads an address with `opens` in it, and whose window
/// script closes at the first pointer input in it. What the tab's thread asks of it goes in `log`.
struct Windows {
    inner: Instant_,
    open: bool,
    selected: bool,
    view: DocumentInfo,
    changes: Vec<snowlark_page::WindowChange>,
    log: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Windows {
    fn note(&self, what: String) {
        self.log.lock().unwrap().push(what);
    }
}

impl TabDriver for Windows {
    fn navigate(&mut self, url: Url) {
        if self.selected {
            self.view.url = Some(url);
            self.view.generation += 1;
            return;
        }
        if url.as_str().contains("opens") {
            self.open = true;
            self.changes.push(snowlark_page::WindowChange::Opened {
                window: 7,
                popup: true,
                size: (500.0, 600.0),
                position: (None, None),
            });
        }
        self.inner.navigate(url)
    }
    fn take_window_changes(&mut self) -> Vec<snowlark_page::WindowChange> {
        std::mem::take(&mut self.changes)
    }
    fn select_view(&mut self, view: Option<snowlark_page::HostedWindow>) -> bool {
        match view {
            Some(7) if self.open => self.selected = true,
            Some(_) => return false,
            None => self.selected = false,
        }
        true
    }
    fn close_view(&mut self, view: snowlark_page::HostedWindow) {
        self.open = false;
        self.note(format!("close {view}"));
    }
    fn set_shown(&mut self, shown: bool) {
        self.note(format!("shown {shown}"));
    }
    fn stop(&mut self) {}
    fn set_device(&mut self, device: Device) {
        if self.selected {
            self.note(format!("view device {}", device.viewport.width));
        }
    }
    fn set_scroll(&mut self, _scroll: Point) {}
    fn update(&mut self) -> Update {
        Update::default()
    }
    fn paint(&mut self, _origin: Point) -> Option<snowlark_paint::DisplayList> {
        None
    }
    fn text(&self) -> &TextEngine {
        &self.inner.text
    }
    fn images(&self) -> &snowlark_paint::ImageStore {
        &self.inner.images
    }
    fn image_store(&self) -> Arc<snowlark_paint::ImageStore> {
        self.inner.images.clone()
    }
    fn pointer(&mut self, _input: Pointer) -> PointerResult {
        if self.selected {
            // (the window's script closes it)
            self.open = false;
            self.changes.push(snowlark_page::WindowChange::Closed(7));
        }
        PointerResult::default()
    }
    fn scroll_container(&mut self, _node: NodeId, _delta: Point) -> bool {
        false
    }
    fn info(&self) -> &DocumentInfo {
        if self.selected {
            &self.view
        } else {
            &self.inner.info
        }
    }
}

/// The opener: a key press (the user activation a new window needs) opens `POPUP` as a popup
/// window; a message from it is answered through `source`.
const OPENER_PAGE: &str = "<!doctype html><title>opener</title><script>\
    onkeydown = () => { window.open('POPUP', 'x', 'popup,width=500,height=600') };\
    onmessage = e => { document.title = 'opener got ' + e.data; e.source.postMessage('back', '*') };\
    </script>";
/// The popup: it writes to its opener at once, and closes itself once answered.
const POPUP_PAGE: &str = "<!doctype html><title>popup</title><script>\
    onmessage = e => { document.title = 'popup got ' + e.data; setTimeout(() => window.close(), 50) };\
    if (opener) opener.postMessage('hi', '*'); else document.title = 'no opener';\
    </script>";

/// `window.open()` in the window (HTML "the window open steps", with an opener): the window
/// shows in a tab of its own, a view of the opener's page served by the opener's renderer; `opener`
/// reaches the page, `postMessage` goes both ways, and `window.close()` from the window closes
/// the tab.
/// With `cross_site`, the window is of another site (another loopback address): it stays in the
/// opener's renderer, behind the page's cross-origin access checks.
fn a_popup_opens_talks_and_closes(renderer: impl Fn() -> Renderer, cross_site: bool) {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let compositor = ctx.compositor.clone();
    let popup_at = if cross_site {
        format!(
            "{}/popup",
            serve_html_at("127.0.0.2:0", vec![("/popup", POPUP_PAGE.to_owned())])
        )
    } else {
        "/popup".to_owned()
    };
    let origin = serve_html_at(
        "127.0.0.1:0",
        vec![
            ("/", OPENER_PAGE.replace("POPUP", &popup_at)),
            ("/popup", POPUP_PAGE.to_owned()),
        ],
    );
    let url = Url::parse(&format!("{origin}/")).unwrap();
    let mut opener = TabHost::spawn(1, ctx, Default::default(), DEVICE, url, renderer());
    next_of(&rx, &mut opener, 1, |e| match e {
        TabEvent::State(s) if s.title == "opener" && !s.loading => Some(()),
        TabEvent::Crashed(m) => panic!("crashed: {m}"),
        _ => None,
    });
    opener.send(TabCommand::Key(KeyInput {
        kind: KeyKind::Down { repeat: false },
        key: 0x41,
        scan: 0x1E,
        text: Some('a'),
        mods: Default::default(),
    }));
    let mut popup: Option<TabHost> = None;
    let (mut opener_heard, mut popup_heard, mut closed) = (false, false, false);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !(opener_heard && popup_heard && closed) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let (id, e) = match rx.recv_timeout(left) {
            Ok(event) => event,
            Err(_) => panic!(
                "opener heard {opener_heard}, popup heard {popup_heard}, closed {closed}; \
                 popup {:?}",
                popup.as_ref().map(|p| p.state.clone())
            ),
        };
        if let TabEvent::Crashed(m) = &e {
            panic!("tab {id} crashed: {m}");
        }
        match id {
            1 => {
                drive(&mut opener, &e);
                match e {
                    TabEvent::OpenWindow {
                        window,
                        popup: p,
                        size,
                        ..
                    } => {
                        assert!(p, "a popup window was asked for");
                        assert_eq!(size, (500.0, 600.0));
                        assert!(popup.is_none(), "one window");
                        popup = Some(TabHost::view(
                            2,
                            &opener,
                            window,
                            &compositor,
                            Default::default(),
                            DEVICE,
                        ));
                    }
                    TabEvent::Open(url, _) => panic!("opened {url} as a tab with no opener"),
                    TabEvent::State(s) if s.title == "opener got hi" => opener_heard = true,
                    _ => {}
                }
            }
            2 => {
                let p = popup
                    .as_mut()
                    .expect("the view's events come after it was attached");
                drive(p, &e);
                match e {
                    TabEvent::State(s) => {
                        assert_ne!(s.title, "no opener");
                        if s.title == "popup got back" {
                            assert!(s.url.ends_with("/popup"), "{}", s.url);
                            popup_heard = true;
                        }
                    }
                    TabEvent::CloseWindow => {
                        assert!(popup_heard, "closed before the answer");
                        closed = true;
                    }
                    _ => {}
                }
            }
            other => panic!("an event of tab {other}"),
        }
    }
    // The view's frames reached the compositor as its tab's, drawn on a raster seam of its own.
    let popup_frames = comp_rx
        .try_iter()
        .filter(|input| matches!(input, crate::composite::CompInput::Frame(2, _)))
        .count();
    assert!(popup_frames > 0, "the view painted nothing");
    // (the browser closes the tab at its CloseWindow)
    drop(popup);
    // The opener goes on: the window it opened is closed.
    opener.send(TabCommand::Devtools(
        crate::devtools::protocol::DevtoolsCommand::Attach(true),
    ));
    opener.send(TabCommand::Devtools(
        crate::devtools::protocol::DevtoolsCommand::Eval(1, "String(window.open.length)".into()),
    ));
    next_event(&rx, 1, |e| match e {
        TabEvent::Devtools(d) => matches!(
            *d,
            crate::devtools::protocol::DevtoolsEvent::EvalResult(1, Ok(_))
        )
        .then_some(()),
        _ => None,
    });
}

#[test]
fn a_popup_in_the_openers_thread_opens_talks_and_closes() {
    a_popup_opens_talks_and_closes(
        || Renderer::Thread(crate::page_driver::PageDriver::factory()),
        false,
    );
}

#[test]
fn a_popup_in_the_openers_renderer_process_opens_talks_and_closes() {
    a_popup_opens_talks_and_closes(|| remote(child_launch()), false);
}

#[test]
fn a_popup_of_another_site_in_the_openers_renderer_process_opens_talks_and_closes() {
    a_popup_opens_talks_and_closes(|| remote(child_launch()), true);
}

/// The popup of `a_popup_outlives_its_openers_tab`: its title says whether it has an opener; a key
/// press closes it.
const LASTING_POPUP_PAGE: &str = "<!doctype html><title>popup</title><script>\
    setInterval(() => { document.title = 'opener ' + (opener === null ? 'null' : 'there') }, 30);\
    onkeydown = () => window.close();\
    </script><p>needle needle";

/// A popup outlives its opener's tab: the browser closes the opener's tab (or moves it to another
/// renderer), and the page goes on running for the window it opened, whose `opener` is null from
/// then on (its opener browsing context was discarded). The view still takes input, and closes
/// itself; a renderer process ends with the last view it served.
fn a_popup_outlives_its_openers_tab(renderer: Renderer) {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let compositor = ctx.compositor.clone();
    let origin = serve_html_at(
        "127.0.0.1:0",
        vec![
            ("/", OPENER_PAGE.replace("POPUP", "/popup")),
            ("/popup", LASTING_POPUP_PAGE.to_owned()),
        ],
    );
    let url = Url::parse(&format!("{origin}/")).unwrap();
    let mut opener = TabHost::spawn(1, ctx, Default::default(), DEVICE, url, renderer);
    next_of(&rx, &mut opener, 1, |e| match e {
        TabEvent::State(s) if s.title == "opener" && !s.loading => Some(()),
        TabEvent::Crashed(m) => panic!("crashed: {m}"),
        _ => None,
    });
    let key = || {
        TabCommand::Key(KeyInput {
            kind: KeyKind::Down { repeat: false },
            key: 0x41,
            scan: 0x1E,
            text: Some('a'),
            mods: Default::default(),
        })
    };
    opener.send(key());
    let window = next_of(&rx, &mut opener, 1, |e| match e {
        TabEvent::OpenWindow { window, .. } => Some(window),
        TabEvent::Crashed(m) => panic!("crashed: {m}"),
        _ => None,
    });
    let mut popup = TabHost::view(2, &opener, window, &compositor, Default::default(), DEVICE);
    // (until `want`, from the view; the opener's events, while it is there, drive it)
    let mut opener = Some(opener);
    let wait_for = |popup: &mut TabHost, opener: &mut Option<TabHost>, want: &str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok((id, e)) = rx.recv_timeout(left) else {
                panic!("no {want:?}; the popup is {:?}", popup.state);
            };
            match (id, &e) {
                (2, TabEvent::Crashed(m)) => panic!("the popup crashed: {m}"),
                (2, TabEvent::CloseWindow) if want == "closed" => return,
                (2, TabEvent::CloseWindow) => panic!("the popup closed waiting for {want:?}"),
                (2, _) => {
                    drive(popup, &e);
                    if matches!(&e, TabEvent::State(s) if s.title == want) {
                        return;
                    }
                }
                (1, _) => {
                    if let Some(o) = opener.as_mut() {
                        drive(o, &e);
                    }
                }
                (other, _) => panic!("an event of tab {other}"),
            }
        }
    };
    wait_for(&mut popup, &mut opener, "opener there");
    // Each window's find box searches its own document (the popup's matches are not the
    // opener's).
    let find = |q: &str| {
        TabCommand::Find(crate::driver::FindCommand::Open {
            previous: q.to_owned(),
        })
    };
    popup.send(find("needle"));
    if let Some(o) = &opener {
        o.send(find("needle"));
    }
    let (mut in_popup, mut in_opener) = (None, None);
    while in_popup.is_none() || in_opener.is_none() {
        let (id, e) = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("find answered {in_popup:?} {in_opener:?}"));
        match (id, e) {
            (2, TabEvent::Find(s)) if !s.searching => in_popup = Some(s.total),
            (1, TabEvent::Find(s)) if !s.searching => in_opener = Some(s.total),
            (2, e) => drive(&mut popup, &e),
            (1, e) => {
                if let Some(o) = opener.as_mut() {
                    drive(o, &e);
                }
            }
            _ => {}
        }
    }
    assert_eq!((in_popup, in_opener), (Some(2), Some(0)));
    let process = opener.as_ref().and_then(|o| o.renderer_process().cloned());
    // The browser closes the opener's tab: the popup stays, with no opener.
    drop(opener.take());
    wait_for(&mut popup, &mut opener, "opener null");
    // It still takes input (a key press closes it).
    popup.send(key());
    wait_for(&mut popup, &mut opener, "closed");
    drop(popup);
    if let Some(p) = process {
        assert!(
            p.wait(std::time::Duration::from_secs(10)).is_some(),
            "the renderer process ends with its last view"
        );
    }
}

#[test]
fn a_popup_in_the_openers_thread_outlives_its_openers_tab() {
    a_popup_outlives_its_openers_tab(Renderer::Thread(crate::page_driver::PageDriver::factory()));
}

#[test]
fn a_popup_in_the_openers_renderer_process_outlives_its_openers_tab() {
    a_popup_outlives_its_openers_tab(remote(child_launch()));
}

/// A window opened with `noopener` (and a `_blank` link) has no opener: the browser opens it as an
/// ordinary tab (in a renderer of its own), and the page gets `null`.
#[test]
fn a_window_with_no_opener_opens_as_a_tab_of_its_own() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let page = "<!doctype html><title>opener</title><script>\
        onkeydown = () => { document.title = String(window.open('/next', '', 'noopener')) };\
        </script>";
    let origin = serve_html(vec![("/", page), ("/next", "<title>next</title>")]);
    let url = Url::parse(&format!("{origin}/")).unwrap();
    let mut opener = TabHost::spawn(
        1,
        ctx,
        Default::default(),
        DEVICE,
        url,
        remote(child_launch()),
    );
    next_of(&rx, &mut opener, 1, |e| match e {
        TabEvent::State(s) if s.title == "opener" && !s.loading => Some(()),
        _ => None,
    });
    opener.send(TabCommand::Key(KeyInput {
        kind: KeyKind::Down { repeat: false },
        key: 0x41,
        scan: 0x1E,
        text: Some('a'),
        mods: Default::default(),
    }));
    let (mut opened, mut null) = (None, false);
    while opened.is_none() || !null {
        next_of(&rx, &mut opener, 1, |e| {
            match e {
                TabEvent::Open(url, _) => opened = Some(url),
                TabEvent::OpenWindow { .. } => panic!("a window with no opener was hosted"),
                TabEvent::State(s) if s.title == "null" => null = true,
                _ => return None,
            }
            Some(())
        });
    }
    assert!(opened.unwrap().as_str().ends_with("/next"));
}

/// The browser's half: a tab that shows a window its opener's page opened sends its commands
/// through the opener's renderer, is bound to the compositor as itself, keeps a history from the
/// window's documents, and closing it closes the window.
#[test]
fn a_view_tab_is_served_through_its_openers_renderer() {
    let (ctx, _net, rx, comp_rx) = remote_context();
    let compositor = ctx.compositor.clone();
    let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let shared = log.clone();
    let factory: DriverFactory = Box::new(move |_| {
        Box::new(Windows {
            inner: instant(),
            open: false,
            selected: false,
            view: DocumentInfo::default(),
            changes: Vec::new(),
            log: shared,
        })
    });
    let opener = spawn(
        1,
        ctx,
        DEVICE,
        Url::parse("about:blank#opens").unwrap(),
        factory,
    );
    let window = next_event(&rx, 1, |e| match e {
        TabEvent::OpenWindow { window, .. } => Some(window),
        _ => None,
    });
    let mut popup = TabHost::view(2, &opener, window, &compositor, Default::default(), DEVICE);
    assert!(popup.is_view() && !opener.is_view());
    // (the compositor sends the tab's own commands to it: scrolls, begin frames)
    let bound =
        (0..100).find_map(
            |_| match comp_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(crate::composite::CompInput::Handles(
                    crate::composite::CompMsg::Bind { tab: 2, .. },
                    _,
                )) => Some(()),
                _ => None,
            },
        );
    assert!(bound.is_some(), "the view tab was not bound");
    popup.navigate(Url::parse("about:blank#in-the-window").unwrap());
    let state = next_of(&rx, &mut popup, 2, |e| match e {
        TabEvent::State(s) if s.url.ends_with("in-the-window") => Some(s),
        _ => None,
    });
    assert!(!state.loading);
    assert_eq!(
        popup.current_url().map(Url::as_str),
        Some("about:blank#in-the-window")
    );
    // The user closes the view's tab: the window closes.
    drop(popup);
    for _ in 0..100 {
        if log.lock().unwrap().iter().any(|l| l == "close 7") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("the window did not close: {:?}", log.lock().unwrap());
}

/// The tab's thread serves a window the page opened as a view of the page, shown as a tab of its
/// own: the page asks for it, the browser attaches it, the view's commands and events are its tab's,
/// the page is shown while the view is, and the view's tab closes when its window does.
#[test]
fn a_tab_serves_the_windows_its_page_opens_as_views() {
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let shared = log.clone();
    let factory: DriverFactory = Box::new(move |_| {
        Box::new(Windows {
            inner: instant(),
            open: false,
            selected: false,
            view: DocumentInfo::default(),
            changes: Vec::new(),
            log: shared,
        })
    });
    let taken =
        |log: &Arc<std::sync::Mutex<Vec<String>>>| std::mem::take(&mut *log.lock().unwrap());
    let tab = spawn(
        1,
        ctx,
        DEVICE,
        Url::parse("about:blank#opens").unwrap(),
        factory,
    );
    let asked = next_event(&rx, 1, |e| match e {
        TabEvent::OpenWindow {
            window,
            popup,
            size,
            ..
        } => Some((window, popup, size)),
        _ => None,
    });
    assert_eq!(asked, (7, true, (500.0, 600.0)));
    let popup = Device {
        viewport: Size::new(640.0, 480.0),
        ..DEVICE
    };
    tab.send(TabCommand::AttachView {
        window: 7,
        tab: 2,
        device: popup,
    });
    let view = |command: TabCommand| TabCommand::View {
        tab: 2,
        command: Box::new(command),
    };
    // A navigation of the view's tab navigates the window, and its state is the view tab's.
    tab.send(view(TabCommand::Navigate {
        url: Url::parse("about:blank#in-the-window").unwrap(),
        restore: None,
        navigation: 1,
        reload: false,
    }));
    let state = next_event(&rx, 2, |e| match e {
        TabEvent::State(s) => Some(s),
        _ => None,
    });
    assert_eq!(state.url, "about:blank#in-the-window");
    assert_eq!(state.navigations, 1);
    // (a command the page's alone does nothing in a view: the page's own state stays)
    tab.send(view(TabCommand::Stop));
    // The page is shown while the view's tab is, though its own tab is not.
    tab.send(TabCommand::Shown(false));
    tab.send(TabCommand::Memory);
    next_event(&rx, 1, |e| matches!(e, TabEvent::Memory(_)).then_some(()));
    assert_eq!(taken(&log), ["view device 640"]);
    tab.send(view(TabCommand::Shown(false)));
    tab.send(TabCommand::Memory);
    next_event(&rx, 1, |e| matches!(e, TabEvent::Memory(_)).then_some(()));
    assert_eq!(taken(&log), ["shown false"]);
    tab.send(view(TabCommand::Shown(true)));
    // Script in the window closes it: the view's tab hears it.
    tab.send(view(TabCommand::Pointer(Pointer::Move(Point::new(
        5.0, 5.0,
    )))));
    next_event(&rx, 2, |e| matches!(e, TabEvent::CloseWindow).then_some(()));
    assert_eq!(taken(&log), ["shown true", "close 7"]);
    // A view of a window that closed before the browser showed it closes at once.
    tab.send(TabCommand::AttachView {
        window: 7,
        tab: 3,
        device: popup,
    });
    next_event(&rx, 3, |e| matches!(e, TabEvent::CloseWindow).then_some(()));
    // The user closes a view's tab: its window closes (and another view of it with it).
    let open_again = |navigation: u64| {
        tab.send(TabCommand::Navigate {
            url: Url::parse(&format!("about:blank#opens-{navigation}")).unwrap(),
            restore: None,
            navigation,
            reload: false,
        });
        next_event(&rx, 1, |e| {
            matches!(e, TabEvent::OpenWindow { .. }).then_some(())
        });
    };
    open_again(2);
    for id in [4, 5] {
        tab.send(TabCommand::AttachView {
            window: 7,
            tab: id,
            device: popup,
        });
    }
    tab.send(TabCommand::View {
        tab: 4,
        command: Box::new(TabCommand::Close),
    });
    next_event(&rx, 5, |e| matches!(e, TabEvent::CloseWindow).then_some(()));
    assert!(taken(&log).contains(&"close 7".to_owned()));
    // The page's own tab closing leaves its windows' views: the page goes on for them, and the
    // thread ends with the last of them.
    open_again(3);
    tab.send(TabCommand::AttachView {
        window: 7,
        tab: 6,
        device: popup,
    });
    tab.send(TabCommand::Close);
    tab.send(TabCommand::View {
        tab: 6,
        command: Box::new(TabCommand::Shown(false)),
    });
    tab.send(TabCommand::View {
        tab: 6,
        command: Box::new(TabCommand::Shown(true)),
    });
    tab.send(TabCommand::View {
        tab: 6,
        command: Box::new(TabCommand::Pointer(Pointer::Move(Point::new(5.0, 5.0)))),
    });
    next_event(&rx, 6, |e| matches!(e, TabEvent::CloseWindow).then_some(()));
    let log = taken(&log);
    assert!(log.contains(&"shown true".to_owned()), "{log:?}");
    assert!(log.contains(&"close 7".to_owned()), "{log:?}");
    for _ in 0..100 {
        if tab.thread_ended() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("the tab's thread goes on with no view left");
}

// ----- The password manager ------------------------------------------------------------------------

/// Save then fill on a local page: the page reports a login form's submission, the next page's
/// report (no login form) makes it a successful login, which is saved; the login page loaded again
/// reports its form, the browser's choice fills it, and the page's script sees the values only
/// after a click.
// (the store needs its key from the system: Windows' Data Protection API; Linux has none yet)
#[cfg(windows)]
#[test]
fn a_login_saved_after_its_submission_fills_the_form_again() {
    use crate::passwords::*;
    use snowlark_page::passwords::PasswordEvent;
    const LOGIN: &str = "<!doctype html><title>login</title>\
        <style>body{margin:0}button{display:block;width:100px;height:40px}</style>\
        <button id=b type=button onclick=\"u.value='alice';p.value='pw1';f.requestSubmit()\">go</button>\
        <form id=f action=/welcome method=post><input id=u name=user><input id=p name=pass type=password></form>\
        <script>onload=()=>setInterval(()=>{document.title='v:'+u.value+'|'+p.value},20)</script>";
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let origin = serve_html(vec![
        ("/login", LOGIN),
        ("/welcome", "<!doctype html><title>welcome</title>signed in"),
    ]);
    let login = Url::parse(&format!("{origin}/login")).unwrap();
    let mut tab = spawn(
        1,
        ctx,
        DEVICE,
        login.clone(),
        crate::page_driver::PageDriver::factory(),
    );
    // (what the window does with a tab's events, keeping the password reports)
    let mut reports: Vec<PasswordEvent> = Vec::new();
    let hear_until = |tab: &mut TabHost, title: &str, reports: &mut Vec<PasswordEvent>| {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while tab.state.title != title {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok((_, TabEvent::Passwords(p))) => reports.push(p),
                Ok((_, e)) => drive(tab, &e),
                Err(e) => panic!("no title {title:?} (it is {:?}): {e}", tab.state.title),
            }
        }
    };
    let click = |tab: &TabHost, at: Point| {
        tab.send(TabCommand::Pointer(Pointer::Down(
            at,
            snowlark_paint::input::MouseButton::Left,
            Default::default(),
            1,
        )));
        tab.send(TabCommand::Pointer(Pointer::Up(
            at,
            snowlark_paint::input::MouseButton::Left,
            Default::default(),
        )));
    };
    let page_origin_of =
        |tab: &TabHost| page_origin(tab.current_url().unwrap(), tab.site()).unwrap();
    let mut store = PasswordStore::in_memory(Box::new(Dpapi));

    hear_until(&mut tab, "v:|", &mut reports);
    let o = page_origin_of(&tab);
    assert_eq!(o, origin);
    click(&tab, Point::new(10.0, 10.0));
    hear_until(&mut tab, "welcome", &mut reports);
    let submitted = reports
        .iter()
        .find_map(|r| match r {
            PasswordEvent::Submitted(f) => Some(f.clone()),
            _ => None,
        })
        .expect("the submission was reported");
    let pending = pending_login(&o, &submitted).unwrap();
    assert_eq!(
        (pending.username.as_str(), pending.password.as_str()),
        ("alice", "pw1")
    );
    // The welcome page's report: no login form, so the login worked.
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    let after = loop {
        if let Some(PasswordEvent::Rendered(f)) = reports
            .iter()
            .rev()
            .find(|r| matches!(r, PasswordEvent::Rendered(_)))
        {
            if tab.state.title == "welcome" && reports.len() >= 3 {
                break f.clone();
            }
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((_, TabEvent::Passwords(p))) => reports.push(p),
            Ok((_, e)) => drive(&mut tab, &e),
            Err(e) => panic!("no report from the welcome page: {e}"),
        }
    };
    assert!(after.is_empty());
    assert!(!login_failed(&pending, &after));
    assert_eq!(store.offer(&o, "alice", "pw1"), Offer::Save);
    store.save_login(
        &o,
        &pending.username,
        &pending.username_element,
        &pending.password_element,
        &pending.password,
    );

    // The login page again: its report gets the saved login, hidden from script until a click.
    reports.clear();
    tab.navigate(login);
    hear_until(&mut tab, "v:|", &mut reports);
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    let forms = loop {
        if let Some(PasswordEvent::Rendered(f)) = reports.last() {
            break f.clone();
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((_, TabEvent::Passwords(p))) => reports.push(p),
            Ok((_, e)) => drive(&mut tab, &e),
            Err(e) => panic!("no report from the login page: {e}"),
        }
    };
    let (_, fill) = choose_fill(&mut store, &page_origin_of(&tab), &forms).expect("a fill");
    tab.send(TabCommand::FillPassword(fill));
    // (the page's script polls the values for 400 ms: they stay hidden)
    let until = Instant::now() + std::time::Duration::from_millis(400);
    while let Ok((_, e)) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
        drive(&mut tab, &e);
    }
    assert_eq!(tab.state.title, "v:|");
    click(&tab, Point::new(400.0, 300.0));
    hear_until(&mut tab, "v:alice|pw1", &mut reports);
}

/// A tab's events as the window takes them for the password manager (`browser/passwords_ui.rs`):
/// the host's own (navigation) and the page's reports; a login the window would offer to save.
fn window_hears_passwords(
    tab: &mut TabHost,
    logins: &mut crate::passwords::PendingLogins,
    e: TabEvent,
) -> Option<crate::passwords::PendingLogin> {
    match e {
        TabEvent::Passwords(p) => logins.hear(tab, &p),
        e => {
            drive(tab, &e);
            None
        }
    }
}

/// A login shaped like a common one: two forms posting to `login` with the same field names (log
/// in, create account), no `autocomplete`; the submission is answered with a redirect to a page
/// with no password field, which the browser fetches and the renderer commits. The login is
/// judged by that page, and is one to save.
#[test]
fn a_login_posted_and_redirected_is_offered_for_saving() {
    let (origin, saved) = log_in_through_the_window(
        "HTTP/1.1 302 Found\r\nLocation: news\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .into(),
        "news",
    );
    let p = saved.expect("a login to save");
    assert_eq!(p.origin, origin);
    assert_eq!((p.username.as_str(), p.password.as_str()), ("alice", "pw1"));
    assert_eq!(
        (p.username_element.as_str(), p.password_element.as_str()),
        ("acct", "pw")
    );
}

/// The same login answered with its form again (a wrong password): the wait ends, and nothing
/// is offered.
#[test]
fn a_login_whose_form_comes_back_is_not_offered() {
    let (_, saved) = log_in_through_the_window(
        html_answer(
            "<!doctype html><title>bad</title>Bad login.\
            <form action=login method=post><input type=hidden name=goto value=news>\
            <input type=text name=acct> <input type=password name=pw> <input type=submit></form>",
        ),
        "bad",
    );
    assert_eq!(saved, None);
}

/// The same login answered with an HTTP error whose page has no login form: it failed, and
/// nothing is offered.
#[test]
fn a_login_answered_with_an_http_error_is_not_offered() {
    let body = "<!doctype html><title>oops</title>Something went wrong.";
    let (_, saved) = log_in_through_the_window(
        format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        ),
        "oops",
    );
    assert_eq!(saved, None);
}

/// A sign-in by script without a navigation (the page reports that the login form the user typed
/// in left after a request): judged at once, a login to save, and no earlier submission of the
/// tab waits on after it.
#[test]
fn a_sign_in_in_place_is_offered_at_once() {
    use snowlark_page::passwords::{FieldData, FormData, PasswordEvent};
    let origin = serve_answers_at(
        "127.0.0.1:0",
        vec![("/", html_answer("<!doctype html><title>app</title>app"))],
    );
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let mut tab = spawn(
        1,
        ctx,
        DEVICE,
        Url::parse(&format!("{origin}/")).unwrap(),
        crate::page_driver::PageDriver::factory(),
    );
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    while tab.state.title != "app" {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((_, e)) => drive(&mut tab, &e),
            Err(e) => panic!("no page: {e}"),
        }
    }
    let field = |id: u64, kind: &str, name: &str, value: &str| FieldData {
        id,
        kind: kind.into(),
        name: name.into(),
        value: value.into(),
        user_typed: true,
        focusable: true,
        ..Default::default()
    };
    let form = FormData {
        form: 1,
        action: String::new(),
        fields: vec![
            field(2, "text", "user", "alice"),
            field(3, "password", "pw", "pw1"),
        ],
    };
    let mut logins = crate::passwords::PendingLogins::default();
    assert_eq!(
        logins.hear(&tab, &PasswordEvent::Submitted(form.clone())),
        None
    );
    assert!(!logins.is_empty(), "a submission waits for the next page");
    let p = logins
        .hear(&tab, &PasswordEvent::SubmittedInPlace(form))
        .expect("a login to save at once");
    assert_eq!(p.origin, origin);
    assert_eq!((p.username.as_str(), p.password.as_str()), ("alice", "pw1"));
    assert!(logins.is_empty());
}

fn html_answer(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Submits a login form, by a click, on a page shaped like a common login page; the POST gets
/// `answer` and ends at a page titled `title`. The page's origin, and the login the window would
/// offer to save once the submission was judged.
fn log_in_through_the_window(
    answer: String,
    title: &str,
) -> (String, Option<crate::passwords::PendingLogin>) {
    use snowlark_page::passwords::PasswordEvent;
    const LOGIN: &str = "<!doctype html><title>login</title>\
        <style>body{margin:0}#go{position:absolute;left:0;top:0;width:100px;height:40px}</style>\
        <form action=login method=post><input type=hidden name=goto value=news>\
        <input type=text name=acct> <input type=password name=pw> <input id=go type=submit value=login></form>\
        <form action=login method=post><input type=hidden name=creating value=t>\
        <input type=text name=acct> <input type=password name=pw> <input type=submit value='create account'></form>\
        <script>onload=()=>{const f=document.forms[0];f.acct.value='alice';f.pw.value='pw1';\
        document.title='ready'}</script>";
    let origin = serve_answers_at(
        "127.0.0.1:0",
        vec![
            ("/login?goto=news", html_answer(LOGIN)),
            ("/login", answer),
            (
                "/news",
                html_answer("<!doctype html><title>news</title>signed in"),
            ),
        ],
    );
    let (ctx, _net, rx, _comp_rx) = remote_context();
    let login = Url::parse(&format!("{origin}/login?goto=news")).unwrap();
    let mut tab = spawn(
        1,
        ctx,
        DEVICE,
        login,
        crate::page_driver::PageDriver::factory(),
    );
    let mut logins = crate::passwords::PendingLogins::default();
    let mut saved = None;
    let mut submitted = false;
    let mut hear_until = |tab: &mut TabHost, done: &dyn Fn(&TabHost, bool, bool) -> bool| {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !done(tab, submitted, logins.is_empty()) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok((_, e)) => {
                    submitted |= matches!(e, TabEvent::Passwords(PasswordEvent::Submitted(_)));
                    if let Some(p) = window_hears_passwords(tab, &mut logins, e) {
                        saved = Some(p);
                    }
                }
                Err(e) => panic!(
                    "waited in vain (title {:?}, submitted {submitted}, pending {logins:?}): {e}",
                    tab.state.title
                ),
            }
        }
    };
    hear_until(&mut tab, &|t, _, _| t.state.title == "ready");
    tab.send(TabCommand::Pointer(Pointer::Down(
        Point::new(10.0, 10.0),
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
        1,
    )));
    tab.send(TabCommand::Pointer(Pointer::Up(
        Point::new(10.0, 10.0),
        snowlark_paint::input::MouseButton::Left,
        Default::default(),
    )));
    // (judged: the submission was heard, the next page is shown, and nothing waits)
    hear_until(&mut tab, &|t, sub, none_waits| {
        sub && none_waits && t.state.title == title
    });
    (origin, saved)
}
