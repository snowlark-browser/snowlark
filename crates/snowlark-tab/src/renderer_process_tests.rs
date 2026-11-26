// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tab in a real second process: this test binary, run again with
//! `renderer_process::tests::child_main` as its only test and a role in the environment.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use snowlark_idb::{Change, Durability, IdbService, Key, Msg, Record};
use snowlark_ipc::{connect_to_browser, Bulk, Launch};
use snowlark_net::Url;
use snowlark_page::web_storage::{StorageWrite, WebStorage};
use snowlark_paint::scroll_curve::ScrollKind;
#[cfg_attr(not(windows), allow(unused_imports))]
use snowlark_paint::{Driver, Gpu, Point, Size, VideoId};

use super::*;
use crate::raster_link::Message;
use crate::raster_link::ToRaster;
use crate::services::tests::{local, IdbLoop, WAIT};
use crate::services::RendererPort;
use crate::tab::{IconBitmap, TabState};

const ROLE: &str = "SNOWLARK_RENDERER_TEST_ROLE";
const TAB: TabId = 7;

#[test]
fn child_main() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    if role == "die-early" {
        // (before its hello: the browser's wait for it must end now, not at CONNECT_WAIT)
        std::process::exit(3);
    }
    let port: RendererPort = connect_to_browser(WAIT).unwrap();
    let code = match role.as_str() {
        "tab" => crate::renderer_main::run_renderer(port),
        // (a forgery of the evil-renderer suite, once the test has locked the policy)
        evil if evil.starts_with("evil-") => {
            thread::sleep(Duration::from_millis(500));
            let own = Url::parse("https://www.a.test/page").unwrap();
            let _ = crate::evil::forge(&evil["evil-".len()..], &port, &own, TAB);
            thread::sleep(Duration::from_secs(600));
            0
        }
        forge => {
            // (a `locked-` role gives the test time to lock the policy)
            let forge = match forge.strip_prefix("locked-") {
                Some(rest) => {
                    thread::sleep(Duration::from_millis(500));
                    rest
                }
                None => forge,
            };
            port.send(&forged(&port, forge)).unwrap();
            thread::sleep(Duration::from_secs(600));
            0
        }
    };
    std::process::exit(code);
}

/// What the renderer of [`TAB`] must not send.
fn forged(port: &RendererPort, role: &str) -> Up {
    use crate::services::video;
    match role {
        // (a frame, or the end, of a video the browser never made textures for)
        "forge-video-frame" => Up::Video(video::Up::Frame {
            id: 1,
            generation: 1,
            index: 0,
        }),
        "forge-video-removed" => Up::Video(video::Up::Removed { id: 1 }),
        // (a question sent with nobody to answer)
        "forge-video-pool" => Up::Video(video::Up::Pool {
            id: 1,
            width: 16,
            height: 16,
        }),
        role => Up::Tab(forged_tab(port, role)),
    }
}

fn forged_tab(port: &RendererPort, role: &str) -> TabUp {
    let event = |tab, event: TabEvent| TabUp::Event {
        tab,
        event: services::pack(port, &event).unwrap(),
    };
    let icon = |width, height, pixels, orientation| {
        let icon = IconBitmap {
            width,
            height,
            pixels: vec![0; pixels],
            orientation,
        };
        TabEvent::Favicon(Box::new(icon), vec![])
    };
    let state = |url: &str| {
        TabEvent::State(TabState {
            url: url.into(),
            ..Default::default()
        })
    };
    match role {
        "forge-event-tab" => event(TAB + 1, TabEvent::State(TabState::default())),
        // M5 step 4: the site lock's checks of a tab's events.
        "shown-url" => event(TAB, state("https://b.test/")),
        "own-url" => event(TAB, state("https://www.a.test/x")),
        "open-internal" => event(
            TAB,
            TabEvent::Open(
                Url::parse("snowlark:settings").unwrap(),
                crate::driver::Disposition::NewForegroundTab,
            ),
        ),
        "settings" => event(
            TAB,
            TabEvent::Internal(crate::driver::InternalRequest::Settings("a=b".into())),
        ),
        "forge-crash-tab" => event(TAB + 1, TabEvent::Crashed("no".into())),
        "forge-icon-size" => event(TAB, icon(16, 16, 16 * 15 * 4, 1)),
        "forge-icon-huge" => event(TAB, icon(MAX_ICON + 1, 1, (MAX_ICON as usize + 1) * 4, 1)),
        "forge-icon-orientation" => event(TAB, icon(1, 1, 4, 0)),
        "forge-event-bytes" => TabUp::Event {
            tab: TAB,
            event: Bulk::Inline(vec![250; 9]),
        },
        "forge-scroll-tab" => TabUp::Comp(CompMsg::ScrollTo {
            tab: TAB + 1,
            to: Point::ZERO,
            smooth: false,
            with: None,
        }),
        "forge-scroll-nan" => TabUp::Comp(CompMsg::ScrollBy {
            tab: TAB,
            delta: Point::new(f32::NAN, 0.0),
            smooth: None,
            input: None,
        }),
        "forge-comp-window" => TabUp::Comp(CompMsg::RemoveTab(TAB)),
        "forge-begin-frames-tab" => TabUp::Comp(CompMsg::NeedsBeginFrames {
            tab: TAB + 1,
            needs: true,
        }),
        "forge-raster" => {
            let mut job = crate::raster_thread::tests::job(100, 1, None);
            job.head.base_len = 1000;
            let (head, lists) = ToRaster::Job(Box::new(job)).split();
            TabUp::Raster(RasterUp {
                view: None,
                head,
                bytes: Bulk::Inline(lists),
            })
        }
        // A page's own tab is not a view of its own, nor does it close as one.
        "forge-view-raster" => {
            let job = crate::raster_thread::tests::job(100, 1, None);
            let (head, lists) = ToRaster::Job(Box::new(job)).split();
            TabUp::Raster(RasterUp {
                view: Some(TAB),
                head,
                bytes: Bulk::Inline(lists),
            })
        }
        "forge-close-window" => event(TAB, TabEvent::CloseWindow),
        // (a view tab the browser did not attach is another tab)
        "forge-view-event" => event(TAB + 1, TabEvent::CloseWindow),
        other => panic!("role {other}"),
    }
}

pub(super) fn device() -> Device {
    Device {
        viewport: Size::new(800.0, 600.0),
        scale: 1.0,
        scrollbar: 0.0,
        screen: Default::default(),
    }
}

/// A device to raster with: the graphics card's, else WARP's; `None` (and a note) without either.
pub(super) fn gpu() -> Option<Arc<Gpu>> {
    for driver in [Driver::Hardware, Driver::Warp] {
        match Gpu::with_driver(driver) {
            Ok(gpu) => return Some(gpu),
            Err(e) => eprintln!("no {driver:?} adapter: {e}"),
        }
    }
    eprintln!("skipped: no adapter to raster with");
    None
}

pub(super) struct Started {
    pub(super) renderer: RendererProcess,
    pub(super) events: Receiver<(TabId, TabEvent)>,
    pub(super) compositor: Receiver<CompInput>,
}

fn start(role: &str, local: &LocalServices, gpu: Arc<Gpu>) -> Started {
    start_tab(TAB, role, &[], local, gpu).unwrap()
}

/// This test binary as a renderer of `role`, with the environment `env` (a crash switch reaches
/// this one renderer only).
pub(super) fn launch(role: &str, env: &[(&str, &str)]) -> Launch {
    let mut launch = Launch::new(std::env::current_exe().unwrap())
        .arg("renderer_process::tests::child_main")
        .arg("--exact")
        .arg("--nocapture")
        .env(ROLE, role);
    // (the diagnostics switches reach it, as the shell's launch passes them on)
    for (name, value) in std::env::vars().filter(|(n, _)| n.starts_with("SNOWLARK_")) {
        launch = launch.env(&name, &value);
    }
    for (name, value) in env {
        launch = launch.env(name, value);
    }
    launch
}

/// Starts tab `tab` in a renderer of `role` with the environment `env`.
pub(super) fn start_tab(
    tab: TabId,
    role: &str,
    env: &[(&str, &str)],
    local: &LocalServices,
    gpu: Arc<Gpu>,
) -> Result<Started, Error> {
    start_tab_with(tab, launch(role, env), local, gpu)
}

/// Starts tab `tab` in a renderer started by `launch`.
pub(super) fn start_tab_with(
    tab: TabId,
    launch: Launch,
    local: &LocalServices,
    gpu: Arc<Gpu>,
) -> Result<Started, Error> {
    let (events, events_rx) = snowlark_ipc::channel();
    let (compositor, compositor_rx) = mpsc::channel();
    let window = WindowEnds {
        gpu,
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
    };
    let renderer = RendererProcess::start(launch, tab, tab, device(), local, window)?;
    Ok(Started {
        renderer,
        events: events_rx,
        compositor: compositor_rx,
    })
}

/// Navigates the tab to `url` as the tab's host does (M5): fetched through the browser's
/// network `net`, the head taken, the rest handed to the renderer to commit.
pub(super) fn navigate(
    renderer: &RendererProcess,
    net: &Arc<dyn snowlark_net::NetBackend>,
    url: Url,
    navigation: u64,
) {
    let client = snowlark_net::NetClient::with_backend(net.clone());
    let mut stream = client.fetch(snowlark_net::Request::navigation(url.clone()));
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
        .then(|| renderer.hand_over(stream));
    renderer
        .sender()
        .send(TabCommand::Commit(Box::new(crate::tab::Commit {
            navigation,
            restore: None,
            params: snowlark_page::CommitParams {
                url,
                begun: None,
                reload: false,
                response,
                violations: Vec::new(),
                started: Some(Instant::now()),
            },
            body,
            view_source: None,
        })))
        .unwrap();
}

/// Serves `page` at every path but the favicon's, on a port of its own: the page's origin.
pub(super) fn serve(page: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buffer[..n]),
                }
            }
            let response = if request.starts_with(b"GET / ") {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    origin
}

/// Reads `seeded` from localStorage and the keys of the database `seeded` the browser made, and
/// shows them in the title.
const PAGE: &str = r#"<!doctype html><title>start</title><body style="background:#0a0">a tab in a renderer process<script>
const stored = localStorage.getItem("seeded");
const open = indexedDB.open("seeded");
open.onerror = () => { document.title = stored + "|open failed"; };
open.onsuccess = () => {
  const keys = open.result.transaction("s").objectStore("s").getAllKeys();
  keys.onsuccess = () => { document.title = stored + "|" + keys.result.join(","); };
  keys.onerror = () => { document.title = stored + "|getAllKeys failed"; };
};
</script>"#;

/// Makes the database `seeded` of `key` at version 1 with a store `s` holding the keys `k1` and `k2`.
fn seed_idb(idb: &dyn IdbService, key: &str) {
    let l = IdbLoop::new(idb);
    idb.open(l.id, 1, key, "seeded", Some(1));
    let Some(Msg::Upgrade {
        conn,
        txn,
        mut schema,
        ..
    }) = l.next()
    else {
        panic!("no upgrade");
    };
    let store = schema.take_id();
    let mut changes = vec![
        Change::Version(1),
        Change::CreateStore {
            id: store,
            name: "s".into(),
            key_path: None,
            auto_increment: false,
        },
    ];
    for k in ["k1", "k2"] {
        changes.push(Change::Put {
            store,
            key: Key::string(k),
            record: Record {
                value: vec![0].into(),
                index_keys: vec![],
            },
        });
    }
    for change in changes {
        idb.change(txn, change);
    }
    idb.commit(txn, Durability::Default);
    assert!(matches!(
        l.next(),
        Some(Msg::Committed { result: Ok(()), .. })
    ));
    idb.upgrade_finished(txn, true);
    idb.connection_closed(conn);
}

/// The next event of the tab, within [`WAIT`].
pub(super) fn next_event(events: &Receiver<(TabId, TabEvent)>, until: Instant) -> Option<TabEvent> {
    let (tab, event) = events
        .recv_timeout(until.saturating_duration_since(Instant::now()))
        .ok()?;
    assert_eq!(tab, TAB);
    Some(event)
}

#[test]
fn a_tab_runs_in_a_renderer_process() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    let origin = serve(PAGE);
    let storage = &l.storage;
    let client = storage.connect(Arc::new(|_| {}));
    storage.open(client, &origin);
    storage.write(
        client,
        StorageWrite::Set {
            origin: origin.clone(),
            key: "seeded".into(),
            value: "from-storage".into(),
            url: format!("{origin}/"),
        },
    );
    seed_idb(&l.idb, &origin);

    let tab = start("tab", &l.services, gpu);
    let commands = tab.renderer.sender();
    navigate(
        &tab.renderer,
        &l.services.net,
        Url::parse(&format!("{origin}/")).unwrap(),
        1,
    );
    let until = Instant::now() + WAIT;
    let mut title = String::new();
    while title != "from-storage|k1,k2" {
        match next_event(&tab.events, until) {
            Some(TabEvent::State(state)) => title = state.title,
            Some(TabEvent::Crashed(why)) => panic!("crashed: {why}"),
            Some(_) => {}
            None => panic!("no title from the page; the last was {title:?}"),
        }
    }
    // A frame of the page reaches the compositor from the browser's raster thread.
    let frame = loop {
        match tab
            .compositor
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(CompInput::Frame(id, frame)) => {
                assert_eq!(id, TAB);
                break frame;
            }
            Ok(_) => {}
            Err(e) => panic!("no frame: {e}"),
        }
    };
    assert_eq!(frame.viewport, device().viewport);

    // The browser closes it: it ends, and the window hears of no crash.
    commands.send(TabCommand::Close).unwrap();
    assert_eq!(tab.renderer.process().wait(WAIT), Some(0));
    let quiet = Instant::now() + Duration::from_millis(500);
    while let Some(event) = next_event(&tab.events, quiet) {
        assert!(!matches!(event, TabEvent::Crashed(_)), "{event:?}");
    }
}

/// A GPU process's raster threads as this test has them: what comes by the browser is counted and
/// its jobs answered there; a seam's link is served on a thread of this process, each job on it
/// counted and answered on the link.
struct LinkedGpu {
    gpu: snowlark_ipc::BrowserEnd<(), ()>,
    _gpu: snowlark_ipc::ChildEnd<(), ()>,
    taken: std::sync::Mutex<std::collections::HashMap<TabId, Box<dyn Fn() + Send + Sync>>>,
    by_browser: std::sync::atomic::AtomicUsize,
    on_link: mpsc::Sender<TabId>,
}

impl crate::raster_thread::GpuRaster for LinkedGpu {
    fn start(
        &self,
        tab: TabId,
        _videos: TabId,
        taken: Box<dyn Fn() + Send + Sync>,
        _refused: Box<dyn Fn(String) + Send + Sync>,
        _restarted: Box<dyn Fn() + Send + Sync>,
    ) {
        self.taken.lock().unwrap().insert(tab, taken);
    }

    fn videos(&self, _tab: TabId) -> Arc<dyn crate::services::video::VideoBackend> {
        Arc::new(crate::services::video::LocalVideos::new(
            snowlark_paint::video::VideoStore::new(None),
        ))
    }

    fn send(
        &self,
        tab: TabId,
        from: &crate::services::BrowserPort,
        message: crate::raster_thread::SeamMsg,
    ) -> Result<(), Error> {
        let job = message.is_job();
        if job {
            self.by_browser
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let _ = message.open_resource(|b| from.open_bulk(b));
        if job {
            if let Some(taken) = self.taken.lock().unwrap().get(&tab) {
                taken();
            }
        }
        Ok(())
    }

    fn end(&self, tab: TabId) {
        self.taken.lock().unwrap().remove(&tab);
    }

    fn link(
        &self,
        tab: TabId,
        renderer: &crate::services::BrowserPort,
    ) -> Option<snowlark_ipc::LinkEnd> {
        use crate::raster_thread::{SeamDown, SeamMsg, SEAM_BYTES_WAIT, SEAM_RING};
        let [theirs, mine] = renderer.link(&self.gpu, [SEAM_RING, 0]).ok()?;
        let on_link = self.on_link.clone();
        thread::spawn(move || {
            let link = snowlark_ipc::connect_link::<SeamDown, SeamMsg, snowlark_ipc::FromClient>(
                mine, WAIT,
            )
            .unwrap();
            let policy = snowlark_ipc::ProcessPolicy {
                process: 0,
                kind: snowlark_ipc::ProcessKind::Renderer,
                lock: Default::default(),
            };
            while let Ok(snowlark_ipc::Received::Message(m)) = link.recv() {
                let m = link.check(m, &policy).unwrap();
                let job = m.is_job();
                let _ = m.open_resource(|b| link.open_bulk(b, SEAM_BYTES_WAIT));
                if job {
                    let _ = on_link.send(tab);
                    let _ = link.send(&SeamDown::Taken);
                }
            }
        });
        Some(theirs)
    }
}

/// G4 step 2 with a real renderer: given a link for its tab's raster seam, the renderer joins it
/// and sends its frames there, each answered on the link, while the browser's way carries what went
/// before the link was joined at most.
#[test]
fn a_renderers_frames_go_on_its_link_to_the_gpu_process() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    // (a page that paints again and again: the link may be joined after its first frame)
    let origin = serve(
        "<!doctype html><body><script>let n = 0; (function f() { \
         document.body.style.background = n++ % 2 ? 'red' : 'blue'; \
         requestAnimationFrame(f) })()</script>",
    );
    let (pair, child) = snowlark_ipc::loopback::pair::<(), ()>(0).unwrap();
    let (on_link, frames) = mpsc::channel();
    let linked = Arc::new(LinkedGpu {
        gpu: pair,
        _gpu: child,
        taken: Default::default(),
        by_browser: Default::default(),
        on_link,
    });
    let (events, events_rx) = snowlark_ipc::channel();
    let (compositor, _compositor) = mpsc::channel();
    let window = WindowEnds {
        gpu,
        compositor,
        gpu_raster: Some(linked.clone()),
        events: Arc::new(events),
    };
    let renderer =
        RendererProcess::start(launch("tab", &[]), TAB, TAB, device(), &l.services, window)
            .unwrap();
    navigate(
        &renderer,
        &l.services.net,
        Url::parse(&format!("{origin}/")).unwrap(),
        1,
    );
    let until = Instant::now() + WAIT;
    let by_browser = || linked.by_browser.load(std::sync::atomic::Ordering::SeqCst);
    let mut seen = 0;
    let mut before = 0;
    while seen < 4 {
        match frames.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(tab) => {
                assert_eq!(tab, TAB);
                if seen == 0 {
                    before = by_browser();
                }
                seen += 1;
            }
            Err(e) => panic!("{seen} frames on the link: {e}"),
        }
    }
    // (once a frame went on the link, none went by the browser)
    assert_eq!(by_browser(), before, "frames by the browser after the link");
    renderer.sender().send(TabCommand::Close).unwrap();
    assert_eq!(renderer.process().wait(WAIT), Some(0));
    while let Ok((_, event)) = events_rx.try_recv() {
        assert!(!matches!(event, TabEvent::Crashed(_)), "{event:?}");
    }
}

/// What a stand-in GPU process's raster thread got by the browser.
#[derive(Debug, PartialEq)]
enum Got {
    Font,
    Image,
    Job,
}

/// A GPU process's raster threads with no link: each message by the browser is told to `got`, and
/// a job is answered at once. It keeps each tab's `restarted`, for the test to start "a new process".
struct RestartingGpu {
    taken: std::sync::Mutex<std::collections::HashMap<TabId, Box<dyn Fn() + Send + Sync>>>,
    restarted: std::sync::Mutex<std::collections::HashMap<TabId, Box<dyn Fn() + Send + Sync>>>,
    got: std::sync::Mutex<mpsc::Sender<Got>>,
    /// The place each frame named for its view, in order.
    restores: std::sync::Mutex<Vec<Option<snowlark_paint::Point>>>,
}

impl crate::raster_thread::GpuRaster for RestartingGpu {
    fn start(
        &self,
        tab: TabId,
        _videos: TabId,
        taken: Box<dyn Fn() + Send + Sync>,
        _refused: Box<dyn Fn(String) + Send + Sync>,
        restarted: Box<dyn Fn() + Send + Sync>,
    ) {
        self.taken.lock().unwrap().insert(tab, taken);
        self.restarted.lock().unwrap().insert(tab, restarted);
    }

    fn videos(&self, _tab: TabId) -> Arc<dyn crate::services::video::VideoBackend> {
        Arc::new(crate::services::video::LocalVideos::new(
            snowlark_paint::video::VideoStore::new(None),
        ))
    }

    fn send(
        &self,
        tab: TabId,
        from: &crate::services::BrowserPort,
        message: crate::raster_thread::SeamMsg,
    ) -> Result<(), Error> {
        if let Some(restore) = message.restore() {
            self.restores.lock().unwrap().push(restore);
        }
        let got = if message.is_job() {
            Got::Job
        } else {
            match message.open_resource(|b| from.open_bulk(b)) {
                Ok(snowlark_paint::Resource::FontAdd { .. }) => Got::Font,
                Ok(_) => Got::Image,
                Err(e) => panic!("a resource that does not open: {e}"),
            }
        };
        let job = got == Got::Job;
        let _ = self.got.lock().unwrap().send(got);
        if job {
            if let Some(taken) = self.taken.lock().unwrap().get(&tab) {
                taken();
            }
        }
        Ok(())
    }

    fn end(&self, tab: TabId) {
        self.taken.lock().unwrap().remove(&tab);
        self.restarted.lock().unwrap().remove(&tab);
    }

    fn link(
        &self,
        _tab: TabId,
        _renderer: &crate::services::BrowserPort,
    ) -> Option<snowlark_ipc::LinkEnd> {
        None
    }
}

/// G5 step 1 with a real renderer: a page at rest whose raster thread is new (the GPU process was
/// started again) sends its fonts again and a whole frame, within a second, though nothing on the
/// page changed. That frame names no place: the compositor made again was given the tab's place
/// by the browser, moved by the wheel turned meanwhile, which the tab's own would undo.
#[test]
fn a_renderer_told_its_raster_thread_is_new_sends_its_fonts_and_a_frame_again() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    let origin = serve("<!doctype html><p style='font: 20px Arial'>Some text");
    let (got_tx, got) = mpsc::channel();
    let stand_in = Arc::new(RestartingGpu {
        taken: Default::default(),
        restarted: Default::default(),
        got: std::sync::Mutex::new(got_tx),
        restores: Default::default(),
    });
    let (events, events_rx) = snowlark_ipc::channel();
    let (compositor, _compositor) = mpsc::channel();
    let window = WindowEnds {
        gpu,
        compositor,
        gpu_raster: Some(stand_in.clone()),
        events: Arc::new(events),
    };
    let renderer =
        RendererProcess::start(launch("tab", &[]), TAB, TAB, device(), &l.services, window)
            .unwrap();
    navigate(
        &renderer,
        &l.services.net,
        Url::parse(&format!("{origin}/")).unwrap(),
        1,
    );
    // The first frame, with its font ahead of it; then the page rests.
    let until = Instant::now() + WAIT;
    let mut fonts = 0;
    loop {
        match got.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Got::Font) => fonts += 1,
            Ok(Got::Job) if fonts > 0 => break,
            Ok(_) => {}
            Err(e) => panic!("no frame with text: {e}"),
        }
    }
    while got.recv_timeout(Duration::from_millis(700)).is_ok() {}
    // A new raster thread: the renderer is told, and everything comes again.
    stand_in.restores.lock().unwrap().clear();
    let restarted = Instant::now();
    (stand_in.restarted.lock().unwrap()[&TAB])();
    let mut again = Vec::new();
    while !again.contains(&Got::Job) {
        match got.recv_timeout(Duration::from_secs(1).saturating_sub(restarted.elapsed())) {
            Ok(g) => again.push(g),
            Err(_) => panic!("not within a second of the restart: {again:?}"),
        }
    }
    assert!(again.contains(&Got::Font), "{again:?}");
    assert_eq!(*stand_in.restores.lock().unwrap(), [None]);
    renderer.sender().send(TabCommand::Close).unwrap();
    assert_eq!(renderer.process().wait(WAIT), Some(0));
    while let Ok((_, event)) = events_rx.try_recv() {
        assert!(!matches!(event, TabEvent::Crashed(_)), "{event:?}");
    }
}

#[test]
fn a_renderer_killed_mid_page_is_a_crashed_tab_at_once() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    let origin = serve("<!doctype html><title>t</title><script>setInterval(() => document.title = Date.now(), 1)</script>");
    let tab = start("tab", &l.services, gpu);
    navigate(
        &tab.renderer,
        &l.services.net,
        Url::parse(&format!("{origin}/")).unwrap(),
        1,
    );
    let until = Instant::now() + WAIT;
    // (the page is running: its title changes)
    let mut titles = 0;
    while titles < 3 {
        match next_event(&tab.events, until) {
            Some(TabEvent::State(s)) if s.title.parse::<f64>().is_ok() => titles += 1,
            Some(TabEvent::Crashed(why)) => panic!("crashed: {why}"),
            Some(_) => {}
            None => panic!("the page did not run"),
        }
    }
    let killed = Instant::now();
    tab.renderer.process().kill();
    loop {
        match next_event(&tab.events, until) {
            Some(TabEvent::Crashed(why)) => {
                eprintln!("crashed after {:?}: {why}", killed.elapsed());
                break;
            }
            Some(_) => {}
            None => panic!("no crash heard"),
        }
    }
    assert!(
        killed.elapsed() < Duration::from_millis(500),
        "the crash took {:?}",
        killed.elapsed()
    );
}

/// M5 step 4: a renderer locked to a site that shows a document of another site, opens the
/// browser's own pages or changes its settings dies; its own documents, and the browser's own
/// renderer's settings, reach the window.
#[test]
fn a_renderer_locked_to_a_site_speaks_for_its_site_only() {
    let Some(gpu) = gpu() else { return };
    for role in ["locked-shown-url", "locked-settings"] {
        let mut l = local();
        l.services.site_lock = crate::services::site_lock::Mode::Enforce;
        let tab = start(role, &l.services, gpu.clone());
        tab.renderer
            .policy()
            .lock
            .lock_to("https://a.test")
            .unwrap();
        assert_eq!(tab.renderer.process().wait(WAIT), Some(9), "{role}");
        match next_event(&tab.events, Instant::now() + WAIT) {
            Some(TabEvent::Crashed(why)) => assert!(why.contains("broke a rule"), "{role}: {why}"),
            other => panic!("{role}: {other:?}"),
        }
    }
    // Opening the browser's page is dropped, not fatal: an honest renderer passes a web page's
    // `window.open` of it on.
    {
        let mut l = local();
        l.services.site_lock = crate::services::site_lock::Mode::Enforce;
        let tab = start("locked-open-internal", &l.services, gpu.clone());
        tab.renderer
            .policy()
            .lock
            .lock_to("https://a.test")
            .unwrap();
        assert_eq!(tab.renderer.process().wait(Duration::from_secs(2)), None);
        let heard = next_event(&tab.events, Instant::now());
        assert!(heard.is_none(), "{heard:?}");
    }
    for (role, site) in [
        ("locked-own-url", "https://a.test"),
        ("locked-settings", "snowlark://"),
    ] {
        let mut l = local();
        l.services.site_lock = crate::services::site_lock::Mode::Enforce;
        let tab = start(role, &l.services, gpu.clone());
        tab.renderer.policy().lock.lock_to(site).unwrap();
        match next_event(&tab.events, Instant::now() + WAIT) {
            Some(TabEvent::State(_)) | Some(TabEvent::Internal(_)) => {}
            other => panic!("{role}: {other:?}"),
        }
        assert_eq!(
            tab.renderer.process().wait(Duration::from_secs(1)),
            None,
            "{role}"
        );
        assert_eq!(tab.renderer.policy().lock.noted(), 0, "{role}");
    }
}

/// M5 step 5, the evil-renderer suite: every forgery of [`crate::evil::ROLES`] from a renderer
/// locked to a site ends it, the window hears why and nothing else of it, and the browser's
/// stores are as they were.
#[test]
fn an_evil_renderer_is_refused_and_ended() {
    let Some(gpu) = gpu() else { return };
    for role in crate::evil::ROLES {
        let mut l = local();
        l.services.site_lock = crate::services::site_lock::Mode::Enforce;
        let tab = start(&format!("evil-{role}"), &l.services, gpu.clone());
        tab.renderer
            .policy()
            .lock
            .lock_to("https://a.test")
            .unwrap();
        if !crate::evil::is_ended(role) {
            // (refused: the renderer lives, and the window hears nothing of it)
            assert_eq!(
                tab.renderer.process().wait(Duration::from_secs(2)),
                None,
                "{role}"
            );
            let heard = next_event(&tab.events, Instant::now());
            assert!(heard.is_none(), "{role}: {heard:?}");
            continue;
        }
        assert_eq!(tab.renderer.process().wait(WAIT), Some(9), "{role}");
        match next_event(&tab.events, Instant::now() + WAIT) {
            Some(TabEvent::Crashed(why)) => {
                if crate::evil::is_message(role) {
                    assert!(why.contains("broke a rule"), "{role}: {why}");
                }
            }
            other => panic!("{role}: {other:?}"),
        }
        assert!(l.storage.origins().is_empty(), "{role}");
    }
}

#[test]
fn a_renderer_that_breaks_a_tab_rule_dies() {
    let Some(gpu) = gpu() else { return };
    let mut survived = Vec::new();
    for role in [
        "forge-event-tab",
        "forge-crash-tab",
        "forge-icon-size",
        "forge-icon-huge",
        "forge-icon-orientation",
        "forge-event-bytes",
        "forge-scroll-tab",
        "forge-scroll-nan",
        "forge-comp-window",
        "forge-begin-frames-tab",
        "forge-raster",
        "forge-view-raster",
        "forge-close-window",
        "forge-view-event",
        "forge-video-frame",
        "forge-video-removed",
        "forge-video-pool",
    ] {
        let mut l = local();
        l.services.videos = Some(snowlark_paint::video::VideoStore::new(Some(gpu.clone())));
        let tab = start(role, &l.services, gpu.clone());
        if tab.renderer.process().wait(Duration::from_secs(5)) != Some(9) {
            tab.renderer.process().kill();
            survived.push(role);
            continue;
        }
        // The window hears that it died for it, and nothing it sent.
        match next_event(&tab.events, Instant::now() + WAIT) {
            Some(TabEvent::Crashed(why)) => assert!(why.contains("broke a rule"), "{role}: {why}"),
            other => panic!("{role}: {other:?}"),
        }
        assert!(
            !matches!(tab.compositor.try_recv(), Ok(CompInput::Msg(_))),
            "{role} reached the compositor"
        );
    }
    assert!(survived.is_empty(), "lived: {survived:?}");
}

#[test]
fn the_checks_take_what_a_tab_sends() {
    let icon = TabEvent::Favicon(
        Box::new(IconBitmap {
            width: 2,
            height: 3,
            pixels: vec![0; 24],
            orientation: 6,
        }),
        vec![1, 2],
    );
    assert_eq!(check_event(TAB, TAB, false, &icon), Ok(()));
    assert_eq!(check_event(TAB, TAB, false, &TabEvent::Blocked), Ok(()));
    for message in [
        CompMsg::ScrollTo {
            tab: TAB,
            to: Point::new(0.0, 10.0),
            smooth: true,
            with: None,
        },
        CompMsg::ScrollBy {
            tab: TAB,
            delta: Point::new(0.0, -10.0),
            smooth: Some(ScrollKind::Wheel),
            input: None,
        },
        CompMsg::NeedsBeginFrames {
            tab: TAB,
            needs: true,
        },
    ] {
        assert_eq!(check_comp(TAB, &message), Ok(()));
    }
    // (another tab's begin frames are not its to ask for)
    assert!(check_comp(
        TAB,
        &CompMsg::NeedsBeginFrames {
            tab: TAB + 1,
            needs: true,
        }
    )
    .is_err());
}

#[test]
fn a_renderer_opens_windows_of_a_size_and_closes_none_of_its_own_tab() {
    let open = |size: (f32, f32)| TabEvent::OpenWindow {
        window: 2,
        popup: true,
        size,
        position: (None, None),
    };
    assert_eq!(check_event(TAB, TAB, false, &open((500.0, 600.0))), Ok(()));
    for size in [
        (f32::NAN, 600.0),
        (500.0, f32::INFINITY),
        (-1.0, 600.0),
        (500.0, 1.0e9),
    ] {
        assert!(
            check_event(TAB, TAB, false, &open(size)).is_err(),
            "{size:?}"
        );
    }
    // (a page's own tab is the user's to close, and to move: a tab is not a popup window)
    assert!(check_event(TAB, TAB, false, &TabEvent::CloseWindow).is_err());
    let rect = TabEvent::WindowRect(100, 100, 500, 600);
    assert!(check_event(TAB, TAB, false, &rect).is_err());
    // A view tab the browser attached speaks for its document, and closes; it opens nothing
    // and navigates nothing of the page's. A tab not attached is another tab.
    let view = TAB + 1;
    assert_eq!(check_event(TAB, view, true, &TabEvent::CloseWindow), Ok(()));
    assert_eq!(check_event(TAB, view, true, &rect), Ok(()));
    assert_eq!(
        check_event(TAB, view, true, &TabEvent::State(Default::default())),
        Ok(())
    );
    assert!(check_event(TAB, view, true, &open((500.0, 600.0))).is_err());
    assert!(check_event(TAB, view, true, &TabEvent::Traverse(-1)).is_err());
    assert!(check_event(TAB, view, false, &TabEvent::CloseWindow).is_err());
    assert!(check_event(TAB, view, false, &TabEvent::Blocked).is_err());
}

/// Serves `page` at `/` and the media fixture `pattern.mp4` (320 × 180, 4 s, a colour a second) at
/// `/v.mp4`, on a port of its own.
#[cfg_attr(not(windows), allow(dead_code))]
fn serve_video(page: &'static str) -> String {
    let clip = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../snowlark-media/tests/fixtures/pattern.mp4"),
    )
    .expect("the media fixture");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let clip = clip.clone();
            thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                    }
                }
                let (kind, body): (&str, &[u8]) = if request.starts_with(b"GET / ") {
                    ("text/html", page.as_bytes())
                } else if request.starts_with(b"GET /v.mp4 ") {
                    ("video/mp4", &clip)
                } else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            });
        }
    });
    origin
}

/// A `<video>` that plays the fixture in a loop. (Neither class nor id: the harness's content filter
/// hides every element that has one.)
#[cfg_attr(not(windows), allow(dead_code))]
const VIDEO_PAGE: &str = r#"<!doctype html><title>start</title><body style="margin:0">
<video src="/v.mp4" muted autoplay loop style="width:320px;height:180px"></video>"#;

/// M4 step 4: a video playing in a renderer process draws in the browser. Its frames reach the
/// window's store through the textures the browser made, the compositor takes them, and the tab's
/// frames name the video by the window's id.
#[test]
// (video frames need a GPU device: Windows only so far)
#[cfg(windows)]
fn a_video_in_a_renderer_draws_in_the_browser() {
    let Some(gpu) = gpu() else { return };
    let mut l = local();
    let store = snowlark_paint::video::VideoStore::new(Some(gpu.clone()));
    // (another tab's video: the window's ids and the renderer's own differ)
    let other = store.create();
    let (frames, frames_rx) = mpsc::channel();
    store.set_notify(Some(Arc::new(move |id| {
        let _ = frames.send(id);
    })));
    l.services.videos = Some(store.clone());
    let origin = serve_video(VIDEO_PAGE);
    let tab = start("tab", &l.services, gpu.clone());
    navigate(
        &tab.renderer,
        &l.services.net,
        Url::parse(&format!("{origin}/")).unwrap(),
        1,
    );
    let until = Instant::now() + WAIT;
    // The compositor's part: each frame the renderer names becomes the one drawn.
    let mut drawn: Option<(VideoId, snowlark_paint::Frame)> = None;
    let mut quads: Vec<VideoId> = Vec::new();
    let mut shown = 0;
    while Instant::now() < until && (shown < 5 || drawn.is_none() || quads.is_empty()) {
        if let Ok(id) = frames_rx.recv_timeout(Duration::from_millis(20)) {
            store.latch(id);
            if let Some((bitmap, w, h)) = store.front_of(id) {
                shown += 1;
                let frame = snowlark_paint::video::read_bitmap(&gpu, &bitmap, w, h).unwrap();
                drawn = Some((id, frame));
            }
        }
        while let Ok(input) = tab.compositor.try_recv() {
            if let CompInput::Frame(_, frame) = input {
                quads.extend(frame.videos.iter().filter_map(|q| match q.picture {
                    crate::video::Picture::Frames(v) => Some(v),
                    crate::video::Picture::Protected(_) => None,
                }));
            }
        }
        while let Some(event) = next_event(&tab.events, Instant::now()) {
            if let TabEvent::Crashed(why) = event {
                panic!("crashed: {why}");
            }
        }
    }
    let (id, frame) = drawn.expect("no frame of the video reached the browser");
    assert!(shown >= 5, "{shown} frames shown");
    assert_eq!((frame.width, frame.height), (320, 180));
    let p = frame.pixel(160, 90);
    assert!(
        p.r as u32 + p.g as u32 + p.b as u32 > 100,
        "a black frame: {p:?}"
    );
    assert_ne!(id, other.id());
    assert!(
        !quads.is_empty() && quads.iter().all(|q| *q == id),
        "the tab's frames name {quads:?}, not the window's {id:?}"
    );
    // Closed, the renderer's videos leave the window's store.
    tab.renderer.sender().send(TabCommand::Close).unwrap();
    assert_eq!(tab.renderer.process().wait(WAIT), Some(0));
    let gone = Instant::now() + WAIT;
    while store.front_of(id).is_some() && Instant::now() < gone {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        store.front_of(id).is_none(),
        "the video outlived its renderer"
    );
}
