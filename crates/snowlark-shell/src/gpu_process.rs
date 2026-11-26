// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The GPU process (`docs/process-isolation.md`, M7 stage G1): `snowlark --type=gpu`, and the
//! browser's host for it.
//!
//! The child makes the device, says which it is ([`GpuUp::Ready`]) and serves its pipe: from G3 the
//! windows' compositors and their tabs' raster threads run in it ([`crate::gpu_compositor`]), and
//! from G4 the renderers' video textures are made on its device. It ends with [`EXIT_DEVICE_LOST`] when the device is removed, so a lost
//! device and a crash are one path: the browser's restart.
//!
//! The browser's end ([`GpuHost`]) launches it, notices its end as its pipe breaks (never at a
//! timeout), and launches a new one at once, since the compositor always needs one. Ends the browser
//! did not ask for are counted ([`CrashCount`]): one is forgiven for every [`FORGIVE`] since the last
//! one. The [`CRASH_LIMIT`]th on the graphics card starts the next GPU processes on WARP, with a
//! count of their own (G5 step 4), and the limit there stops the launches ([`GpuStatus::FellBack`];
//! from G3 the browser then composites new windows in its own process, as with
//! `--process-model=single`). `SNOWLARK_GPU_CRASH_LIMIT` sets the limit, for runs that end the GPU
//! process many times on purpose.
//!
//! It is off by default until G5 (restart with the content back): `--gpu-process` or
//! `SNOWLARK_GPU_PROCESS=1` turn it on with `--process-model=tab`. Headless modes never start one.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use snowlark_ipc::{
    BadMessage, BrowserEnd, ChildEnd, Error, Launch, Process, ProcessKind, ProcessPolicy, Received,
    Validate,
};
use snowlark_paint::{Driver, Gpu};
use snowlark_tab::raster_thread::SeamMsg;

use crate::composite::{ScrollView, WindowId};
use crate::tab::{TabCommand, TabId};

/// The GPU process's exit code when its device was removed (a driver reset or update, or the
/// adapter gone): the browser starts another, as after a crash.
pub const EXIT_DEVICE_LOST: i32 = 0x47;
/// Its exit code when it could make no device at all.
pub const EXIT_NO_DEVICE: i32 = 0x48;
/// Crashes not forgiven that stop the launches.
pub const CRASH_LIMIT: u32 = 3;
/// One crash is forgiven for each of these since the last.
pub const FORGIVE: Duration = Duration::from_secs(5 * 60);
/// Set (to 1) for a GPU process that draws with WARP, after the graphics card's crashes reached
/// the limit (G5 step 4).
const WARP_ENV: &str = "SNOWLARK_GPU_WARP";
/// How long the browser waits for a GPU process's hello (at most: a child that dies first ends the
/// wait at once).
const CONNECT_WAIT: Duration = Duration::from_secs(20);
/// How often the GPU process looks whether its device was removed.
const DEVICE_CHECK: Duration = Duration::from_millis(500);
/// What the GPU process may hold in sections (G1 has none; the raster seam's come in G3).
const GPU_QUOTA: u64 = 64 << 20;
/// The GPU process's number among the browser's children (`ProcessPolicy::process`): above the
/// tabs' and the spares' (`snowlark_tab::spare::SPARE_NUMBERS`).
const GPU_PROCESS_NUMBER: u64 = 1 << 41;
/// The longest adapter name a hello may carry.
const MAX_ADAPTER: usize = 256;
/// How long a GPU process asked to quit has before it is ended.
const QUIT_WAIT: Duration = Duration::from_secs(2);
/// `SNOWLARK_TEST_GPU=lose`: the device counts as removed this long after the hello.
const TEST_LOSE_AFTER: Duration = Duration::from_millis(200);
/// How long one of the GPU process's threads may stay in a pass before the browser ends the process
/// (a hang becomes a crash, and the restart follows).
pub const HANG_AFTER: Duration = Duration::from_secs(10);
/// How often the browser reads the GPU process's pass page.
const HANG_CHECK: Duration = Duration::from_secs(1);
/// The most a gap between two reads counts for (the browser suspended or starved).
const HANG_GAP: Duration = Duration::from_secs(2);

/// What the browser tells the GPU process. Only the browser speaks to it (it has no other peer
/// until G4), so window ids and handles come from the browser's word alone.
#[derive(Debug, Serialize, Deserialize)]
pub enum GpuDown {
    /// End: the browser is closing.
    Quit,
    /// The page where the GPU process's threads mark their passes (`snowlark_tab::watchdog::Passes`),
    /// shared by the browser, which watches it. First, once.
    Passes {
        section: snowlark_ipc::RemoteHandle,
    },
    /// Window `window`'s compositor runs here from now on (G3, [`crate::gpu_compositor`]).
    Window {
        window: WindowId,
    },
    /// Window `to`'s compositor presents window `window` (itself, or the undocked developer
    /// tools') into the composition surface `surface`, a handle the browser put in this process.
    Surface {
        to: WindowId,
        window: WindowId,
        surface: snowlark_ipc::RemoteHandle,
    },
    /// A [`CompMsg`] for window `window`'s compositor, encoded (a list may be past a frame).
    Comp {
        window: WindowId,
        msg: snowlark_ipc::Bulk,
    },
    /// A font or image for window `window`'s own lists.
    Resource {
        window: WindowId,
        msg: SeamMsg,
    },
    /// Tab `tab` has a surface in window `window`: what its compositor tells the tab comes up as
    /// [`GpuUp::Tab`].
    Bind {
        window: WindowId,
        tab: TabId,
    },
    /// Tab `tab`'s raster thread, drawing for window `window`'s compositor; its lists name the
    /// videos of tab `videos`'s renderer (the tab's own, or a view's renderer's).
    RasterStart {
        tab: TabId,
        window: WindowId,
        videos: TabId,
    },
    /// A call (G4 step 4): new shared textures on this process's device for `width` × `height`
    /// frames of video `id` of tab `tab`'s renderer (by the renderer's id; made known now if it is
    /// new). Answered with a [`PoolMade`], whose handles the browser takes out of this process.
    VideoPool {
        tab: TabId,
        id: u32,
        width: u32,
        height: u32,
    },
    /// That video's newest frame is in texture `index` of pool `generation`: one it was not given
    /// is refused ([`GpuUp::Refused`] for `tab`).
    VideoFrame {
        tab: TabId,
        id: u32,
        generation: u32,
        index: u32,
    },
    VideoRemoved {
        tab: TabId,
        id: u32,
    },
    /// Tab `tab`'s renderer is gone, and its videos with it.
    VideosGone {
        tab: TabId,
    },
    /// A message of tab `tab`'s raster seam, its bytes passed on from the renderer unread.
    Raster {
        tab: TabId,
        msg: SeamMsg,
    },
    RasterEnd {
        tab: TabId,
    },
    /// This process's end of a link to tab `tab`'s renderer, for the tab's raster seam (G4 step 2):
    /// what comes on it is the tab's, and its job's `Taken` goes back on it. After the tab's
    /// [`GpuDown::RasterStart`]; its handles are in this process's table.
    Link {
        tab: TabId,
        end: snowlark_ipc::LinkEnd,
    },
    /// Window `window` closed.
    CloseWindow {
        window: WindowId,
    },
}

/// What the GPU process tells the browser.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GpuUp {
    /// The device is made: the adapter's name, and whether it is WARP (no graphics card). Once,
    /// first.
    Ready { adapter: String, software: bool },
    /// The frame [`CompMsg::Ack`] `seq` asked for in window `window` is on its way to the screen.
    Presented { window: WindowId, seq: u64 },
    /// Tab `tab`'s viewport scroll in window `window` (`None`: the tab has gone from it).
    Scroll {
        window: WindowId,
        tab: TabId,
        view: Option<ScrollView>,
    },
    /// What window `window`'s compositor tells tab `tab`: only a compositor's commands.
    Tab {
        window: WindowId,
        tab: TabId,
        command: TabCommand,
    },
    /// Tab `tab`'s raster thread has decoded a job's lists.
    Taken { tab: TabId },
    /// A message of tab `tab`'s raster seam failed its checks: its renderer broke a rule.
    Refused { tab: TabId, why: String },
    /// Window `window`'s compositor could not be made, for the reason `why`: the window shows
    /// nothing new until another surface or size is tried.
    NoCompositor { window: WindowId, why: String },
}

/// The longest reason a [`GpuUp::Refused`] or [`GpuUp::NoCompositor`] may carry.
pub(crate) const MAX_WHY: usize = 1024;

/// The answer to [`GpuDown::VideoPool`]: none when the textures could not be made.
#[derive(Debug, Serialize, Deserialize)]
pub struct PoolMade(pub Option<MadePool>);

/// A video's new textures: their generation, and a handle to each in the GPU process's table, which
/// the browser takes out ([`snowlark_ipc::Connection::take_from_child`]).
#[derive(Debug, Serialize, Deserialize)]
pub struct MadePool {
    pub generation: u32,
    pub handles: Vec<snowlark_ipc::RemoteHandle>,
}

impl Validate for PoolMade {
    fn validate(self, _policy: &ProcessPolicy) -> Result<PoolMade, BadMessage> {
        match &self.0 {
            Some(p) if p.handles.len() != snowlark_paint::video::POOL_TEXTURES => {
                Err(BadMessage("a video pool of the wrong size".into()))
            }
            _ => Ok(self),
        }
    }
}

impl Validate for GpuUp {
    fn validate(self, _policy: &ProcessPolicy) -> Result<GpuUp, BadMessage> {
        let finite = |p: &snowlark_paint::Point| p.x.is_finite() && p.y.is_finite();
        let bad = |why: &str| Err(BadMessage(why.into()));
        match &self {
            GpuUp::Ready { adapter, .. } if adapter.len() > MAX_ADAPTER => {
                bad("an adapter name over its cap")
            }
            GpuUp::Refused { why, .. } | GpuUp::NoCompositor { why, .. } if why.len() > MAX_WHY => {
                bad("a reason over its cap")
            }
            GpuUp::Scroll { view: Some(v), .. }
                if ![v.scroll, v.target, v.max].iter().all(finite)
                    || ![
                        v.viewport.width,
                        v.viewport.height,
                        v.content.width,
                        v.content.height,
                        v.bar,
                    ]
                    .iter()
                    .all(|n| n.is_finite()) =>
            {
                bad("a scroll position that is not one")
            }
            // (what a compositor tells a tab, nothing else: `renderer_process`'s own checks of a
            // command apply once it is on the tab's pipe)
            GpuUp::Tab { command, .. } => {
                if snowlark_tab::raster_thread::compositor_command(command) {
                    Ok(self)
                } else {
                    bad("a command no compositor sends")
                }
            }
            _ => Ok(self),
        }
    }
}

/// The GPU process's end of its pipe.
pub type GpuChildPort = ChildEnd<GpuUp, GpuDown>;
/// The browser's end of the GPU process's pipe.
pub type GpuPort = BrowserEnd<GpuDown, GpuUp>;

/// The GPU process: makes the device, says hello, and runs until the browser says to quit or is gone
/// (0), or the device is removed ([`EXIT_DEVICE_LOST`]). `SNOWLARK_TEST_GPU=lose` makes the device
/// count as removed soon after the hello, `crash` aborts after it, and `hang` stays in a pass once
/// the browser watches: tests of the browser's restart and fallback.
pub fn run_child(port: GpuChildPort) -> i32 {
    let test = std::env::var("SNOWLARK_TEST_GPU").unwrap_or_default();
    let made = if std::env::var(WARP_ENV).is_ok_and(|v| v == "1") {
        Gpu::with_driver(Driver::Warp)
    } else {
        Gpu::new()
    };
    let gpu = match made {
        Ok(gpu) => gpu,
        Err(e) => {
            eprintln!("[gpu] no device: {e}");
            return EXIT_NO_DEVICE;
        }
    };
    let ready = GpuUp::Ready {
        adapter: gpu.adapter().chars().take(MAX_ADAPTER / 4).collect(),
        software: gpu.driver() == Driver::Warp,
    };
    if port.send(&ready).is_err() {
        return 0;
    }
    if test == "crash" || (test == "crash-hardware" && gpu.driver() != Driver::Warp) {
        let _ = port.flush(Duration::from_secs(1));
        std::process::abort();
    }
    let lose_at = (test == "lose").then(|| Instant::now() + TEST_LOSE_AFTER);
    let mut child = crate::gpu_compositor::Child::new(gpu.clone(), port.clone());
    // (this thread's passes: each message it handles)
    let mut mark = snowlark_tab::watchdog::PassMark::default();
    loop {
        mark.end();
        let received = port.recv_timeout(DEVICE_CHECK);
        mark.begin();
        match received {
            Ok(Received::Message(GpuDown::Quit)) | Err(Error::Closed) => return 0,
            Ok(Received::Message(GpuDown::Passes { section })) => {
                if let Some(passes) = child.watch(section) {
                    mark = passes.mark();
                    mark.begin();
                    if test == "hang" {
                        loop {
                            std::thread::sleep(Duration::from_secs(60));
                        }
                    }
                }
            }
            Ok(Received::Message(m)) => child.handle(m),
            Ok(Received::Call(m, replier)) => child.call(m, replier),
            Err(Error::Timeout) => {}
            Err(e) => {
                eprintln!("[gpu] pipe: {e}");
                return 0;
            }
        }
        if gpu.is_lost() || lose_at.is_some_and(|t| Instant::now() >= t) {
            eprintln!("[gpu] the device was removed");
            return EXIT_DEVICE_LOST;
        }
    }
}

/// Whether the browser runs a GPU process: with renderer processes (`tab`) and `--gpu-process` or
/// `SNOWLARK_GPU_PROCESS=1`.
pub fn wanted(model: crate::ProcessModel, switch: bool) -> bool {
    model == crate::ProcessModel::Tab
        && (switch || std::env::var("SNOWLARK_GPU_PROCESS").is_ok_and(|v| v == "1"))
}

/// How the browser starts its GPU process: this executable with `--type=gpu` and this process's own
/// switches (`SNOWLARK_*` but the pipe's, `RUST_BACKTRACE`). A crash leaves a minidump in
/// `crash_dumps`.
pub fn gpu_launch(crash_dumps: &std::path::Path) -> Launch {
    // (an executable that cannot be named does not start: that counts as a crash)
    let exe = std::env::current_exe().unwrap_or_default();
    let mut launch = Launch::new(exe).arg("--type=gpu").crash_dumps(crash_dumps);
    for (name, _) in std::env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if (name.starts_with("SNOWLARK_") && name != "SNOWLARK_IPC_PIPE")
            || name == "RUST_BACKTRACE"
        {
            launch = launch.allow_env(name);
        }
    }
    launch
}

/// The GPU process's crashes, with the older ones forgiven.
#[derive(Clone, Debug)]
pub struct CrashCount {
    recent: u32,
    last: Option<Instant>,
    limit: u32,
}

impl Default for CrashCount {
    fn default() -> CrashCount {
        CrashCount::new(CRASH_LIMIT)
    }
}

impl CrashCount {
    /// No crash yet; `limit` of them stop the launches.
    pub fn new(limit: u32) -> CrashCount {
        CrashCount {
            recent: 0,
            last: None,
            limit: limit.max(1),
        }
    }

    /// The limit: [`CRASH_LIMIT`], or `SNOWLARK_GPU_CRASH_LIMIT` (for runs that end the GPU process
    /// many times on purpose, as G5's exit runs do).
    pub fn limit_of_env() -> u32 {
        std::env::var("SNOWLARK_GPU_CRASH_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(CRASH_LIMIT)
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// A crash at `now`: whether it reaches the limit. One earlier crash is forgiven for each
    /// [`FORGIVE`] since the last one.
    pub fn crashed(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            let forgiven = now.saturating_duration_since(last).as_secs() / FORGIVE.as_secs();
            self.recent = self
                .recent
                .saturating_sub(u32::try_from(forgiven).unwrap_or(u32::MAX));
        }
        self.last = Some(now);
        self.recent += 1;
        self.recent >= self.limit
    }

    pub fn recent(&self) -> u32 {
        self.recent
    }
}

/// Where the browser's GPU process is.
#[derive(Clone, Debug, PartialEq)]
pub enum GpuStatus {
    /// Being started (or started again), not ready yet.
    Starting,
    /// Running: its process id, its adapter, whether that is WARP, and which launch it is (from 1).
    Running {
        pid: u32,
        adapter: String,
        software: bool,
        launch: u64,
    },
    /// It crashed [`CRASH_LIMIT`] times: no more are started.
    FellBack,
}

/// The browser's end of its GPU process: it keeps one running (module comment). Dropping it ends
/// the process.
pub struct GpuHost {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

struct Inner {
    launch: Box<dyn Fn() -> Launch + Send + Sync>,
    /// [`HANG_AFTER`] (shorter in tests).
    hang_after: Duration,
    state: Mutex<State>,
    changed: Condvar,
    /// Where what the GPU process says of windows and tabs goes.
    routes: crate::gpu_compositor::Routes,
}

/// The most messages held for a GPU process that is starting.
const MAX_PENDING: usize = 4096;

struct State {
    status: GpuStatus,
    process: Option<Arc<Process>>,
    port: Option<GpuPort>,
    /// What was sent while no GPU process was ready: the next one gets it first, in order.
    pending: Vec<GpuDown>,
    /// A GPU process was ready before: the next gets the windows again from the routes (G5).
    served: bool,
    launches: u64,
    crashes: CrashCount,
    /// The next GPU process draws with WARP: the crashes on the graphics card reached the limit,
    /// or the device made was WARP already (G5 step 4).
    warp: bool,
    quit: bool,
}

impl GpuHost {
    /// Starts the first GPU process from `launch` (each launch calls it again), and keeps one
    /// running from then on.
    pub fn start(launch: impl Fn() -> Launch + Send + Sync + 'static) -> GpuHost {
        GpuHost::start_watching(launch, HANG_AFTER)
    }

    /// [`GpuHost::start`], with a GPU process ended when one of its threads stays in a pass for
    /// `hang_after`.
    fn start_watching(
        launch: impl Fn() -> Launch + Send + Sync + 'static,
        hang_after: Duration,
    ) -> GpuHost {
        GpuHost::start_with(launch, hang_after, CrashCount::limit_of_env())
    }

    /// [`GpuHost::start_watching`], with `limit` crashes on each kind of device before the next.
    fn start_with(
        launch: impl Fn() -> Launch + Send + Sync + 'static,
        hang_after: Duration,
        limit: u32,
    ) -> GpuHost {
        let inner = Arc::new(Inner {
            launch: Box::new(launch),
            hang_after,
            state: Mutex::new(State {
                status: GpuStatus::Starting,
                process: None,
                port: None,
                pending: Vec::new(),
                served: false,
                launches: 0,
                crashes: CrashCount::new(limit),
                warp: false,
                quit: false,
            }),
            changed: Condvar::new(),
            routes: Default::default(),
        });
        let thread = {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("snowlark gpu host".into())
                .spawn(move || supervise(&inner))
                .ok()
        };
        if thread.is_none() {
            eprintln!("[gpu] no thread for the GPU process's host");
            inner.lock().status = GpuStatus::FellBack;
        }
        GpuHost { inner, thread }
    }

    pub fn status(&self) -> GpuStatus {
        self.inner.lock().status.clone()
    }

    /// The windows' way to the GPU process (cloned into each window's compositor).
    pub fn link(&self) -> GpuLink {
        GpuLink(self.inner.clone())
    }

    /// The process running now (starting, or ready).
    pub fn process(&self) -> Option<Arc<Process>> {
        self.inner.lock().process.clone()
    }

    /// Ends the process running now (the task manager's End task). It counts as a crash, and
    /// another is started.
    pub fn kill(&self) {
        if let Some(p) = self.process() {
            p.kill();
        }
    }

    /// Waits until the status is one `want` accepts, at most `timeout`; returns the status then.
    pub fn wait_until(&self, timeout: Duration, want: impl Fn(&GpuStatus) -> bool) -> GpuStatus {
        let end = Instant::now() + timeout;
        let mut state = self.inner.lock();
        loop {
            let now = Instant::now();
            if want(&state.status) || now >= end {
                return state.status.clone();
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, end - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Drop for GpuHost {
    fn drop(&mut self) {
        let (process, port) = {
            let mut state = self.inner.lock();
            state.quit = true;
            (state.process.take(), state.port.take())
        };
        if let Some(port) = port {
            let _ = port.send(&GpuDown::Quit);
            let _ = port.flush(Duration::from_millis(500));
            port.close();
        }
        if let Some(p) = process {
            if p.wait(QUIT_WAIT).is_none() {
                p.kill();
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A window's way to the GPU process: what it sends goes to the one running now, or to the next
/// once it is ready; what comes up for it reaches it through [`GpuLink::routes`].
#[derive(Clone)]
pub struct GpuLink(Arc<Inner>);

impl GpuLink {
    pub fn status(&self) -> GpuStatus {
        self.0.lock().status.clone()
    }

    /// Which launch is ready now (from 1; 0: none).
    pub fn launch(&self) -> u64 {
        match self.0.lock().status {
            GpuStatus::Running { launch, .. } => launch,
            _ => 0,
        }
    }

    /// Sends `msg` to the GPU process, or holds it for the next while one starts. Past the crash
    /// limit it is dropped.
    pub(crate) fn send(&self, msg: GpuDown) {
        let mut state = self.0.lock();
        match &state.port {
            Some(port) => {
                let _ = port.send(&msg);
            }
            None if state.status != GpuStatus::FellBack && state.pending.len() < MAX_PENDING => {
                state.pending.push(msg)
            }
            None => {}
        }
    }

    /// Sends `msg` to the GPU process of launch `launch` only: dropped when another runs now, or
    /// none (what names that process's state means nothing to the next).
    pub(crate) fn send_to(&self, launch: u64, msg: GpuDown) {
        let state = self.0.lock();
        if let (Some(port), GpuStatus::Running { launch: l, .. }) = (&state.port, &state.status) {
            if *l == launch {
                let _ = port.send(&msg);
            }
        }
    }

    /// The port of the GPU process once one is ready, at most `timeout` from now, with its launch
    /// and whether it draws with WARP. `None` then, or past the crash limit.
    pub(crate) fn ready(&self, timeout: Duration) -> Option<(GpuPort, u64, bool)> {
        self.with_port(timeout, |_| ())?;
        let state = self.0.lock();
        match (&state.port, &state.status) {
            (
                Some(port),
                GpuStatus::Running {
                    launch, software, ..
                },
            ) => Some((port.clone(), *launch, *software)),
            _ => None,
        }
    }

    /// Runs `f` with the port of the GPU process once one is ready, at most `timeout` from now
    /// (`None` then, or past the crash limit). What `f` sends is in order with [`GpuLink::send`].
    pub(crate) fn with_port<R>(
        &self,
        timeout: Duration,
        f: impl FnOnce(&GpuPort) -> R,
    ) -> Option<R> {
        let end = Instant::now() + timeout;
        let mut state = self.0.lock();
        loop {
            if let Some(port) = &state.port {
                return Some(f(port));
            }
            let now = Instant::now();
            if state.quit || state.status == GpuStatus::FellBack || now >= end {
                return None;
            }
            state = self
                .0
                .changed
                .wait_timeout(state, end - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub(crate) fn routes(&self) -> &crate::gpu_compositor::Routes {
        &self.0.routes
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, mut state: MutexGuard<'_, State>, status: GpuStatus) {
        state.status = status;
        drop(state);
        self.changed.notify_all();
    }
}

/// The host's thread: launches a GPU process, serves it until it ends, counts the end and launches
/// the next, until the host is dropped or the crashes reach the limit.
fn supervise(inner: &Inner) {
    loop {
        let mut launch = (inner.launch)();
        if inner.lock().warp {
            launch = launch.env(WARP_ENV, "1");
        }
        let (process, end) = match launch.spawn() {
            Ok(spawned) => {
                let process = spawned.process().clone();
                {
                    let mut state = inner.lock();
                    if state.quit {
                        process.kill();
                        return;
                    }
                    state.process = Some(process.clone());
                    state.launches += 1;
                }
                let policy = ProcessPolicy {
                    process: GPU_PROCESS_NUMBER,
                    kind: ProcessKind::Gpu,
                    lock: Default::default(),
                };
                let end = match spawned.connect::<GpuDown, GpuUp>(policy, GPU_QUOTA, CONNECT_WAIT) {
                    Ok((port, _)) => serve(inner, &port, &process),
                    Err(e) => End::Pipe(e),
                };
                (Some(process), end)
            }
            Err(e) => (None, End::Pipe(e)),
        };
        // (its pipe broke or it broke a rule: it is ending, or is ended now)
        let code = process.as_ref().and_then(|p| {
            p.wait(Duration::from_millis(500)).or_else(|| {
                p.kill();
                p.wait(Duration::from_secs(1))
            })
        });
        let mut state = inner.lock();
        state.process = None;
        state.port = None;
        // (a window waiting for a frame stops waiting)
        inner.routes.lost();
        if state.quit {
            return;
        }
        let why = match &end {
            End::Pipe(e) => ended(code, e),
            End::Hung(why) => why.clone(),
        };
        eprintln!("[gpu] the GPU process ended: {why}");
        if state.crashes.crashed(Instant::now()) {
            let limit = state.crashes.limit();
            // (the graphics card first, then WARP, then the browser's own process: Chromium's
            // order of GPU modes, each with its own count)
            if !state.warp {
                eprintln!("[gpu] {limit} GPU process crashes on the graphics card: the next draws with WARP");
                state.warp = true;
                state.crashes = CrashCount::new(limit);
            } else {
                eprintln!("[gpu] {limit} GPU process crashes: no more are started");
                inner.set(state, GpuStatus::FellBack);
                // (the open windows composite in the browser from now on, as new ones do)
                inner.routes.fall_back();
                return;
            }
        }
        inner.set(state, GpuStatus::Starting);
    }
}

/// Why the browser stopped serving a GPU process.
enum End {
    /// Its pipe ended, or it broke a rule.
    Pipe(Error),
    /// It hung, and was ended.
    Hung(String),
}

/// Serves a connected GPU process until its pipe ends, or it hangs: returns why. It is hung when it
/// sends no hello in [`CONNECT_WAIT`], or one of its threads stays in a pass for the host's
/// `hang_after` (the page it marks them in is shared first: [`GpuDown::Passes`]).
fn serve(inner: &Inner, port: &GpuPort, process: &Process) -> End {
    let passes = snowlark_tab::watchdog::Passes::create()
        .and_then(|p| port.share_section(p.section()).map(|h| (p, h)));
    let passes = match passes {
        Ok((passes, section)) => {
            let _ = port.send(&GpuDown::Passes { section });
            Some(passes)
        }
        Err(e) => {
            eprintln!("[gpu] no pass page for the GPU process (its hangs go unseen): {e}");
            None
        }
    };
    let mut watch = snowlark_tab::watchdog::HangWatch::new(inner.hang_after, HANG_GAP);
    let connected = Instant::now();
    let mut checked = connected;
    let mut ready = false;
    loop {
        let received = port.recv_timeout(HANG_CHECK);
        let now = Instant::now();
        if now.saturating_duration_since(checked) >= HANG_CHECK {
            checked = now;
            let hung = if !ready && now.saturating_duration_since(connected) >= CONNECT_WAIT {
                Some(format!("it hung: no hello in {} s", CONNECT_WAIT.as_secs()))
            } else {
                passes
                    .as_ref()
                    .and_then(|p| watch.check(&p.read(), now))
                    .map(|_| {
                        format!(
                            "it hung: a thread stayed in a pass for {} s",
                            watch.after().as_secs()
                        )
                    })
            };
            if let Some(why) = hung {
                process.kill();
                return End::Hung(why);
            }
        }
        let message = match received {
            Ok(Received::Message(m)) => m,
            Ok(Received::Call(..)) => {
                port.bad_message("a call");
                return End::Pipe(Error::BadMessage("a call".into()));
            }
            Err(Error::Timeout) => continue,
            Err(e) => return End::Pipe(e),
        };
        match port.check(message) {
            Ok(GpuUp::Ready { adapter, software }) if !ready => {
                ready = true;
                let mut state = inner.lock();
                if state.quit {
                    return End::Pipe(Error::Closed);
                }
                // The first gets what windows sent while it started, in order. One started after
                // another ended gets every window and tab again from what the browser keeps of
                // them, which holds all that waited (G5 step 1).
                let pending = std::mem::take(&mut state.pending);
                let restarted = if std::mem::replace(&mut state.served, true) {
                    inner.routes.replay(port)
                } else {
                    for m in pending {
                        let _ = port.send(&m);
                    }
                    Vec::new()
                };
                state.port = Some(port.clone());
                // (a device that is WARP already has no other to fall back to before the browser)
                state.warp |= software;
                let launch = state.launches;
                eprintln!("[gpu] GPU process {} on {adapter}", process.pid());
                inner.set(
                    state,
                    GpuStatus::Running {
                        pid: process.pid(),
                        adapter,
                        software,
                        launch,
                    },
                );
                // (each renderer sends its frame again, to this process now)
                for r in restarted {
                    r();
                }
            }
            Ok(GpuUp::Ready { .. }) => {
                port.bad_message("a second hello");
                return End::Pipe(Error::BadMessage("a second hello".into()));
            }
            Ok(_) if !ready => {
                port.bad_message("a message before its hello");
                return End::Pipe(Error::BadMessage("a message before its hello".into()));
            }
            Ok(up) => inner.routes.deliver(up),
            Err(e) => return End::Pipe(e),
        }
    }
}

/// Why a GPU process ended, from its exit `code` and its pipe's `end`.
fn ended(code: Option<u32>, end: &Error) -> String {
    match (code, end) {
        (_, Error::BadMessage(why)) => format!("it broke a rule: {why}"),
        (Some(c), _) if c == EXIT_DEVICE_LOST as u32 => "its device was removed".into(),
        (Some(c), _) if c == EXIT_NO_DEVICE as u32 => "it could make no device".into(),
        (Some(c), _) => {
            use snowlark_tab::renderer_process::CrashKind;
            match CrashKind::of(c) {
                CrashKind::Other(_) => format!("exit code {c:#x}"),
                kind => format!("{} (exit code {c:#x})", kind.describe()),
            }
        }
        (None, e) => format!("it did not start or its pipe closed: {e}"),
    }
}

// (the GPU process and its windows are Windows': elsewhere the compositor stays in the browser)
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    const ROLE: &str = "SNOWLARK_GPU_TEST_ROLE";
    const WAIT: Duration = Duration::from_secs(20);
    /// Six launches of this test binary, each crashing (three on the graphics card, three on WARP).
    const FALL_BACK_WAIT: Duration = Duration::from_secs(90);

    /// This test binary as a GPU process: run again with this as its only test and a role.
    #[test]
    fn gpu_child() {
        let Ok(role) = std::env::var(ROLE) else {
            return;
        };
        if role == "die-early" {
            // (before its hello: the browser's wait for it must end now, not at CONNECT_WAIT)
            std::process::exit(3);
        }
        let port: GpuChildPort = snowlark_ipc::connect_to_browser(WAIT).unwrap();
        std::process::exit(run_child(port));
    }

    /// A launch of this binary as a GPU process with `role` (and `test`: `SNOWLARK_TEST_GPU`), each
    /// launch counted in `launches`.
    fn launch(
        role: &'static str,
        test: Option<&'static str>,
        launches: Arc<std::sync::atomic::AtomicU32>,
    ) -> impl Fn() -> Launch + Send + Sync + 'static {
        move || {
            launches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut l = Launch::new(std::env::current_exe().unwrap())
                .arg("gpu_process::tests::gpu_child")
                .arg("--exact")
                .arg("--nocapture")
                .env(ROLE, role);
            if let Some(t) = test {
                l = l.env("SNOWLARK_TEST_GPU", t);
            }
            l
        }
    }

    fn running(s: &GpuStatus) -> bool {
        matches!(s, GpuStatus::Running { .. })
    }

    fn launch_number(s: &GpuStatus) -> u64 {
        match s {
            GpuStatus::Running { launch, .. } => *launch,
            _ => 0,
        }
    }

    #[test]
    fn a_killed_gpu_process_is_replaced_within_a_second() {
        let host = GpuHost::start(launch("gpu", None, Default::default()));
        let first = host.wait_until(WAIT, running);
        let GpuStatus::Running { pid, adapter, .. } = &first else {
            panic!("no GPU process: {first:?}");
        };
        assert!(!adapter.is_empty());
        let process = host.process().unwrap();
        assert_eq!(process.pid(), *pid);
        let killed = Instant::now();
        host.kill();
        let second = host.wait_until(WAIT, |s| launch_number(s) == 2);
        let took = killed.elapsed();
        let GpuStatus::Running { pid: pid2, .. } = second else {
            panic!("not started again: {second:?}");
        };
        assert_ne!(pid2, *pid);
        assert!(took < Duration::from_secs(1), "took {took:?}");
        // Dropping the host ends the process, which is asked to quit (exit code 0).
        let process = host.process().unwrap();
        drop(host);
        assert_eq!(process.wait(Duration::from_secs(4)), Some(0));
    }

    #[test]
    fn a_gpu_process_that_dies_before_its_hello_is_noticed_at_once() {
        let launches = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let started = Instant::now();
        let host = GpuHost::start(launch("die-early", None, launches.clone()));
        let status = host.wait_until(FALL_BACK_WAIT, |s| *s == GpuStatus::FellBack);
        assert_eq!(status, GpuStatus::FellBack);
        // Three launches on the graphics card and three on WARP, each noticed as the child ended
        // (CONNECT_WAIT is 20 s).
        assert_eq!(
            launches.load(std::sync::atomic::Ordering::SeqCst),
            2 * CRASH_LIMIT
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_lost_device_ends_the_process_and_three_losses_fall_back() {
        let launches = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let host = GpuHost::start(launch("gpu", Some("lose"), launches.clone()));
        let status = host.wait_until(FALL_BACK_WAIT, |s| *s == GpuStatus::FellBack);
        assert_eq!(status, GpuStatus::FellBack);
        assert_eq!(
            launches.load(std::sync::atomic::Ordering::SeqCst),
            2 * CRASH_LIMIT
        );
        assert!(host.process().is_none());
    }

    #[test]
    fn a_crashing_gpu_process_is_started_again_until_the_limit() {
        let launches = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let host = GpuHost::start(launch("gpu", Some("crash"), launches.clone()));
        assert_eq!(
            host.wait_until(FALL_BACK_WAIT, |s| *s == GpuStatus::FellBack),
            GpuStatus::FellBack
        );
        assert_eq!(
            launches.load(std::sync::atomic::Ordering::SeqCst),
            2 * CRASH_LIMIT
        );
    }

    /// G5 step 4: a GPU process that keeps failing on the graphics card is started on WARP once
    /// its crashes there reach the limit, before the browser composites in its own process.
    #[test]
    fn crashes_on_the_graphics_card_fall_back_to_warp_first() {
        let launches = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let host = GpuHost::start(launch("gpu", Some("crash-hardware"), launches.clone()));
        let status = host.wait_until(WAIT, |s| launch_number(s) > CRASH_LIMIT as u64);
        let GpuStatus::Running {
            software, launch, ..
        } = status
        else {
            panic!("not started on WARP: {status:?}");
        };
        assert!(software);
        assert_eq!(launch, CRASH_LIMIT as u64 + 1);
        std::thread::sleep(Duration::from_millis(500));
        assert!(running(&host.status()), "{:?}", host.status());
    }

    #[test]
    fn a_crash_is_forgiven_for_each_five_minutes_since_the_last() {
        let t = Instant::now();
        let mut c = CrashCount::default();
        assert!(!c.crashed(t));
        assert!(!c.crashed(t + Duration::from_secs(60)));
        // Five minutes after the last: one of the two is forgiven, so this is the second.
        assert!(!c.crashed(t + Duration::from_secs(60) + FORGIVE));
        assert_eq!(c.recent(), 2);
        // Ten minutes later: both forgiven.
        assert!(!c.crashed(t + Duration::from_secs(60) + FORGIVE * 3));
        assert_eq!(c.recent(), 1);
        // Two more at once reach the limit.
        let now = t + FORGIVE * 4;
        assert!(!c.crashed(now));
        assert!(c.crashed(now));
    }

    #[test]
    fn the_hello_is_checked() {
        let policy = ProcessPolicy {
            process: GPU_PROCESS_NUMBER,
            kind: ProcessKind::Gpu,
            lock: Default::default(),
        };
        let ok = GpuUp::Ready {
            adapter: "a".repeat(MAX_ADAPTER),
            software: false,
        };
        assert!(ok.validate(&policy).is_ok());
        let long = GpuUp::Ready {
            adapter: "a".repeat(MAX_ADAPTER + 1),
            software: false,
        };
        assert!(long.validate(&policy).is_err());
    }

    #[test]
    fn only_tab_mode_with_the_switch_runs_one() {
        assert!(!wanted(crate::ProcessModel::Single, true));
        assert!(wanted(crate::ProcessModel::Tab, true));
    }

    #[test]
    fn an_ending_is_described_once() {
        assert_eq!(
            ended(Some(0xffff_ffff), &Error::Closed),
            "exit code 0xffffffff"
        );
        assert_eq!(
            ended(Some(EXIT_DEVICE_LOST as u32), &Error::Closed),
            "its device was removed"
        );
        assert_eq!(
            ended(Some(0xC000_0005), &Error::Closed),
            "an access violation (exit code 0xc0000005)"
        );
    }

    /// G3 step 2: a window's compositor runs in the GPU process. A resize's frame is presented there
    /// and the window's wait for it answered up the pipe; once that process is killed, a wait ends
    /// at once instead of at its timeout (the window's `WM_SIZE` wait).
    #[test]
    fn a_windows_frame_is_presented_in_the_gpu_process_and_waits_end_with_it() {
        use crate::composite::{CompMsg, CompositorThread};
        use snowlark_paint::{Color, DisplayList, ImageStore, TextEngine};
        let host = GpuHost::start(launch("gpu", None, Default::default()));
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let comp = CompositorThread::start(
            Gpu::new().unwrap(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        assert!(comp.surfaces() && comp.gpu_raster().is_some());
        let resize = |width| CompMsg::Resize {
            width,
            height: 150,
            chrome_height: 30,
            dpi_scale: 1.0,
            chrome: Some((DisplayList::new(), Color::WHITE)),
            overlay: None,
        };
        assert!(comp.send_and_wait(resize(200), WAIT));
        assert!(comp.send_and_wait(resize(240), WAIT));
        host.kill();
        let t = Instant::now();
        assert!(!comp.send_and_wait(resize(260), Duration::from_secs(10)));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        drop(comp);
    }

    /// G4 step 2 across processes: a renderer's seam link reaches the GPU process's raster thread
    /// for its tab. A job's `Taken` comes back on the link (the browser hears none), and a frame
    /// that fails its checks there is reported to the browser for that tab.
    #[test]
    fn a_seam_link_to_the_gpu_process_answers_on_itself() {
        use crate::composite::CompositorThread;
        use snowlark_paint::{ImageStore, TextEngine};
        use snowlark_tab::raster_thread::{SeamDown, SeamMsg};
        use snowlark_tab::services::{Down, Up};
        let host = GpuHost::start(launch("gpu", None, Default::default()));
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let comp = CompositorThread::start(
            Gpu::new().unwrap(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        let raster = comp.gpu_raster().unwrap();
        let (taken, taken_rx) = std::sync::mpsc::channel();
        let (refused, refused_rx) = std::sync::mpsc::channel();
        raster.start(
            7,
            7,
            Box::new(move || {
                let _ = taken.send(());
            }),
            Box::new(move |why| {
                let _ = refused.send(why);
            }),
            Box::new(|| {}),
        );
        let (renderer, _renderer) = snowlark_ipc::loopback::pair::<Down, Up>(1 << 20).unwrap();
        let end = raster.link(7, &renderer).unwrap();
        let link =
            snowlark_ipc::connect_link::<SeamMsg, SeamDown, snowlark_ipc::FromHost>(end, WAIT)
                .unwrap();
        SeamMsg::blank_frame(1, 0).send_on(&link, WAIT).unwrap();
        assert!(matches!(
            link.recv_timeout(WAIT),
            Ok(Received::Message(SeamDown::Taken))
        ));
        assert!(taken_rx.recv_timeout(Duration::from_millis(300)).is_err());
        SeamMsg::blank_frame(2, 1).send_on(&link, WAIT).unwrap();
        let why = refused_rx.recv_timeout(WAIT).unwrap();
        assert!(why.contains("lists"), "{why}");
        raster.end(7);
        drop(comp);
    }

    /// G5 step 1 with a real GPU process: one that is killed is replaced, and the new one gets the
    /// window again (its compositor, on a new surface, answers a wait though the window sent nothing
    /// but the wait) and
    /// the tab's raster thread (a frame that fails its checks is refused there for the tab), and
    /// the renderer is told within a second of the kill.
    #[test]
    fn a_gpu_process_started_again_gets_the_windows_and_tabs_again() {
        use crate::composite::{CompMsg, CompositorThread};
        use snowlark_paint::{Color, DisplayList, ImageStore, TextEngine};
        use snowlark_tab::raster_thread::SeamMsg;
        use snowlark_tab::services::{Down, Up};
        let host = GpuHost::start(launch("gpu", None, Default::default()));
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        // (composed by DWM: a surface whose swap chain's process ended then takes no other chain)
        window.show_without_activating(-10_000, -10_000).unwrap();
        let comp = CompositorThread::start(
            Gpu::new().unwrap(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        let raster = comp.gpu_raster().unwrap();
        let (refused, refused_rx) = std::sync::mpsc::channel();
        let (restarted, restarted_rx) = std::sync::mpsc::channel();
        let (refused, restarted) = (Mutex::new(refused), Mutex::new(restarted));
        raster.start(
            7,
            7,
            Box::new(|| {}),
            Box::new(move |why| {
                let _ = refused.lock().unwrap().send(why);
            }),
            Box::new(move || {
                let _ = restarted.lock().unwrap().send(Instant::now());
            }),
        );
        let resize = CompMsg::Resize {
            width: 200,
            height: 150,
            chrome_height: 30,
            dpi_scale: 1.0,
            chrome: Some((DisplayList::new(), Color::WHITE)),
            overlay: None,
        };
        assert!(comp.send_and_wait(resize, WAIT));
        let killed = Instant::now();
        host.kill();
        let told = restarted_rx
            .recv_timeout(WAIT)
            .expect("the renderer is not told");
        let took = told - killed;
        assert!(took < Duration::from_secs(1), "took {took:?}");
        assert_eq!(launch_number(&host.status()), 2);
        assert!(comp.send_and_wait(CompMsg::Dark(true), Duration::from_secs(5)));
        // (the window's compositor was made there: on a surface of its own, G5 step 2)
        let id = crate::composite::WindowId::from(window.handle());
        assert_eq!(host.link().routes().no_compositor(id), None);
        let (renderer, _renderer) = snowlark_ipc::loopback::pair::<Down, Up>(1 << 20).unwrap();
        raster
            .send(7, &renderer, SeamMsg::blank_frame(2, 1))
            .unwrap();
        let why = refused_rx.recv_timeout(WAIT).expect("no raster thread");
        assert!(why.contains("lists"), "{why}");
        raster.end(7);
        drop(comp);
    }

    /// G5: past the crash limit, a window that was open composites in the browser, as a new one
    /// does: its compositor is made again there (it answers a wait), its tab's raster thread with
    /// it (a frame that fails its checks is refused for the tab, a good one is taken), and the
    /// renderer is told its raster thread is new. Here one crash on each kind of device is the
    /// limit: the first GPU process is killed, the next (on WARP) dies before its hello.
    #[test]
    fn past_the_crash_limit_an_open_window_composites_in_the_browser() {
        use crate::composite::{CompMsg, CompositorThread};
        use snowlark_paint::{Color, DisplayList, ImageStore, TextEngine};
        use snowlark_tab::raster_thread::SeamMsg;
        use snowlark_tab::services::{Down, Up};
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (first, later) = (
            launch("gpu", None, count.clone()),
            launch("die-early", None, count.clone()),
        );
        let started = count.clone();
        let host = GpuHost::start_with(
            move || match started.load(std::sync::atomic::Ordering::SeqCst) {
                0 => first(),
                _ => later(),
            },
            HANG_AFTER,
            1,
        );
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        window.show_without_activating(-10_000, -10_000).unwrap();
        let comp = CompositorThread::start(
            Gpu::new().unwrap(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        let raster = comp.gpu_raster().unwrap();
        let (taken, taken_rx) = std::sync::mpsc::channel();
        let (refused, refused_rx) = std::sync::mpsc::channel();
        let (restarted, restarted_rx) = std::sync::mpsc::channel();
        let (taken, refused, restarted) = (
            Mutex::new(taken),
            Mutex::new(refused),
            Mutex::new(restarted),
        );
        raster.start(
            7,
            7,
            Box::new(move || {
                let _ = taken.lock().unwrap().send(());
            }),
            Box::new(move |why| {
                let _ = refused.lock().unwrap().send(why);
            }),
            Box::new(move || {
                let _ = restarted.lock().unwrap().send(());
            }),
        );
        let resize = CompMsg::Resize {
            width: 200,
            height: 150,
            chrome_height: 30,
            dpi_scale: 1.0,
            chrome: Some((DisplayList::new(), Color::WHITE)),
            overlay: None,
        };
        assert!(comp.send_and_wait(resize, WAIT));
        host.kill();
        assert_eq!(
            host.wait_until(WAIT, |s| *s == GpuStatus::FellBack),
            GpuStatus::FellBack
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        restarted_rx
            .recv_timeout(WAIT)
            .expect("the renderer is not told");
        assert!(comp.send_and_wait(CompMsg::Dark(true), Duration::from_secs(5)));
        let (renderer, _renderer) = snowlark_ipc::loopback::pair::<Down, Up>(1 << 20).unwrap();
        assert!(raster
            .send(7, &renderer, SeamMsg::blank_frame(2, 1))
            .is_err());
        let why = refused_rx.recv_timeout(WAIT).expect("no raster thread");
        assert!(why.contains("lists"), "{why}");
        while taken_rx.try_recv().is_ok() {}
        raster
            .send(7, &renderer, SeamMsg::blank_frame(3, 0))
            .unwrap();
        taken_rx.recv_timeout(WAIT).expect("a frame is not taken");
        raster.end(7);
        drop(comp);
    }

    /// G3 step 3: a GPU process one of whose threads stays in a pass is ended, as a crash, and
    /// another started (here after 1 s in the pass, 10 s in the browser).
    #[test]
    fn a_gpu_process_that_stays_in_a_pass_is_ended_and_replaced() {
        let host = GpuHost::start_watching(
            launch("gpu", Some("hang"), Default::default()),
            Duration::from_secs(1),
        );
        assert!(running(&host.wait_until(WAIT, running)));
        let hung = Instant::now();
        let second = host.wait_until(WAIT, |s| launch_number(s) == 2);
        assert_eq!(launch_number(&second), 2, "{second:?}");
        // (1 s in the pass, a read each second, then the launch)
        let took = hung.elapsed();
        assert!(took < Duration::from_secs(6), "took {took:?}");
    }

    /// What waits for work is not in a pass: a GPU process whose dispatcher, window compositor and
    /// tab raster thread sit idle is not taken for hung, however short the limit.
    #[test]
    fn a_gpu_process_waiting_for_work_is_not_taken_for_hung() {
        use crate::composite::{CompMsg, CompositorThread};
        use snowlark_paint::{Color, DisplayList, ImageStore, TextEngine};
        let host = GpuHost::start_watching(
            launch("gpu", None, Default::default()),
            Duration::from_secs(1),
        );
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let comp = CompositorThread::start(
            Gpu::new().unwrap(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        let raster = comp.gpu_raster().unwrap();
        raster.start(7, 7, Box::new(|| {}), Box::new(|_| {}), Box::new(|| {}));
        let resize = |width| CompMsg::Resize {
            width,
            height: 150,
            chrome_height: 30,
            dpi_scale: 1.0,
            chrome: Some((DisplayList::new(), Color::WHITE)),
            overlay: None,
        };
        assert!(comp.send_and_wait(resize(200), WAIT));
        std::thread::sleep(Duration::from_millis(3_500));
        assert_eq!(launch_number(&host.status()), 1, "{:?}", host.status());
        assert!(comp.send_and_wait(resize(240), WAIT));
        raster.end(7);
        drop(comp);
    }

    /// G5 step 4: a GPU process whose device was removed (`SNOWLARK_TEST_GPU=lose`, the `is_lost`
    /// test hook) takes the restart path a kill does: the next one gets the tabs again, and their
    /// renderers are told.
    #[test]
    fn a_removed_device_gives_the_windows_to_the_next_gpu_process() {
        use crate::composite::CompositorThread;
        use snowlark_paint::{ImageStore, TextEngine};
        let host = GpuHost::start(launch("gpu", Some("lose"), Default::default()));
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let comp = CompositorThread::start(
            Gpu::new().unwrap(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        let raster = comp.gpu_raster().unwrap();
        let (restarted, restarted_rx) = std::sync::mpsc::channel();
        let restarted = Mutex::new(restarted);
        raster.start(
            7,
            7,
            Box::new(|| {}),
            Box::new(|_| {}),
            Box::new(move || {
                let _ = restarted.lock().unwrap().send(());
            }),
        );
        restarted_rx
            .recv_timeout(WAIT)
            .expect("the renderer is not told");
        raster.end(7);
        drop(comp);
    }

    /// G5 step 3 with a real GPU process: after it is killed, a renderer's videos ask the new one
    /// for textures (`VideoStore::repool`), and their frames go to it from then on (a frame that
    /// names a pool it did not make is refused there, where one for the dead process is dropped).
    #[test]
    fn a_renderers_videos_get_textures_of_the_new_gpu_process() {
        use crate::composite::CompositorThread;
        use snowlark_paint::video::{PoolHandles, VideoExport, VideoStore};
        use snowlark_paint::{ImageStore, TextEngine, VideoId};
        use snowlark_tab::services::video::VideoBackend;
        struct Renderer(Arc<dyn VideoBackend>);
        impl VideoExport for Renderer {
            fn driver(&self) -> Option<Driver> {
                Some(if self.0.software()? {
                    Driver::Warp
                } else {
                    Driver::Hardware
                })
            }
            fn pool(&self, id: VideoId, width: u32, height: u32) -> Option<PoolHandles> {
                self.0.pool(id.0, width, height)
            }
            fn frame(&self, id: VideoId, generation: u32, index: u32) {
                assert!(self.0.frame(id.0, generation, index));
            }
            fn removed(&self, id: VideoId) {
                self.0.removed(id.0);
            }
        }
        let host = GpuHost::start(launch("gpu", None, Default::default()));
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let comp = CompositorThread::start(
            Gpu::new().unwrap(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        let raster = comp.gpu_raster().unwrap();
        let (refused_tx, refused) = std::sync::mpsc::channel::<String>();
        let refused_tx = Mutex::new(refused_tx);
        let refuse = move |why| {
            let _ = refused_tx.lock().unwrap().send(why);
        };
        raster.start(7, 7, Box::new(|| {}), Box::new(refuse), Box::new(|| {}));
        let videos = raster.videos(7);
        let renderer = VideoStore::exporting(Arc::new(Renderer(videos.clone())));
        let rgpu = renderer.gpu().unwrap().clone();
        let slot = renderer.create();
        slot.upload(&rgpu, 4, 4, &[1; 64]).unwrap();
        renderer.frame_ready(slot.id());
        host.kill();
        assert_eq!(
            launch_number(&host.wait_until(WAIT, |s| launch_number(s) == 2)),
            2
        );
        renderer.repool();
        slot.upload(&rgpu, 4, 4, &[2; 64]).unwrap();
        renderer.frame_ready(slot.id());
        assert!(refused.recv_timeout(Duration::from_millis(500)).is_err());
        // (the new process made generation 1 of its own: 99 is none of its)
        assert!(videos.frame(slot.id().0, 99, 0));
        let why = refused
            .recv_timeout(WAIT)
            .expect("the frame went to no process");
        assert!(why.contains("not given"), "{why}");
        raster.end(7);
        drop(comp);
    }

    /// G5: past the crash limit, a renderer's videos get textures of the window's store in the
    /// browser when they ask again (`VideoStore::repool`, on being told the raster thread is new),
    /// their frames reach that store, and a frame that names a pool it did not make is refused
    /// here. One crash on each kind of device is the limit, as above.
    #[test]
    fn past_the_crash_limit_a_renderers_videos_get_textures_of_the_browser() {
        use crate::composite::{CompInput, CompositorThread};
        use snowlark_paint::video::{PoolHandles, VideoExport, VideoStore};
        use snowlark_paint::{ImageStore, TextEngine, VideoId};
        use snowlark_tab::services::video::VideoBackend;
        struct Renderer(Arc<dyn VideoBackend>);
        impl VideoExport for Renderer {
            fn driver(&self) -> Option<Driver> {
                Some(if self.0.software()? {
                    Driver::Warp
                } else {
                    Driver::Hardware
                })
            }
            fn pool(&self, id: VideoId, width: u32, height: u32) -> Option<PoolHandles> {
                self.0.pool(id.0, width, height)
            }
            fn frame(&self, id: VideoId, generation: u32, index: u32) {
                assert!(self.0.frame(id.0, generation, index));
            }
            fn removed(&self, id: VideoId) {
                self.0.removed(id.0);
            }
        }
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (first, later) = (
            launch("gpu", None, count.clone()),
            launch("die-early", None, count.clone()),
        );
        let started = count.clone();
        let host = GpuHost::start_with(
            move || match started.load(std::sync::atomic::Ordering::SeqCst) {
                0 => first(),
                _ => later(),
            },
            HANG_AFTER,
            1,
        );
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        window.show_without_activating(-10_000, -10_000).unwrap();
        let gpu = Gpu::new().unwrap();
        let comp = CompositorThread::start(
            gpu.clone(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        // (the window's own store, as the browser makes it)
        let store = VideoStore::new(Some(gpu));
        let (shown_tx, shown) = std::sync::mpsc::channel();
        let shown_tx = Mutex::new(shown_tx);
        store.set_notify(Some(Arc::new(move |id| {
            let _ = shown_tx.lock().unwrap().send(id);
        })));
        comp.send(CompInput::videos(store.clone()));
        let raster = comp.gpu_raster().unwrap();
        let (restarted, restarted_rx) = std::sync::mpsc::channel();
        let restarted = Mutex::new(restarted);
        raster.start(
            7,
            7,
            Box::new(|| {}),
            Box::new(|_| {}),
            Box::new(move || {
                let _ = restarted.lock().unwrap().send(());
            }),
        );
        let videos = raster.videos(7);
        let renderer = VideoStore::exporting(Arc::new(Renderer(videos.clone())));
        let rgpu = renderer.gpu().unwrap().clone();
        let slot = renderer.create();
        slot.upload(&rgpu, 4, 4, &[1; 64]).unwrap();
        renderer.frame_ready(slot.id());
        // (made and shown in the GPU process)
        assert!(shown.recv_timeout(Duration::from_millis(500)).is_err());
        host.kill();
        assert_eq!(
            host.wait_until(WAIT, |s| *s == GpuStatus::FellBack),
            GpuStatus::FellBack
        );
        restarted_rx
            .recv_timeout(WAIT)
            .expect("the renderer is not told");
        renderer.repool();
        shown
            .recv_timeout(WAIT)
            .expect("the video's frame does not reach the browser's store");
        slot.upload(&rgpu, 4, 4, &[2; 64]).unwrap();
        renderer.frame_ready(slot.id());
        shown.recv_timeout(WAIT).expect("the next frame does not");
        // (the browser's store made generation 1 of its own: 99 is none of its)
        assert!(!videos.frame(slot.id().0, 99, 0));
        videos.gone();
        raster.end(7);
        drop(comp);
    }

    /// G4 step 4 with a real GPU process: a renderer's video textures are made there and come out
    /// into the browser, a renderer's own device opens them and copies its frames in, the GPU
    /// process takes each frame, and one that names a pool the renderer was not given is refused
    /// for its tab.
    #[test]
    fn a_renderers_video_textures_come_from_the_gpu_process() {
        use crate::composite::CompositorThread;
        use snowlark_paint::video::{PoolHandles, VideoExport, VideoStore};
        use snowlark_paint::{ImageStore, TextEngine, VideoId};
        use snowlark_tab::services::video::VideoBackend;
        /// What a renderer's `RemoteVideo` asks, handed straight to the browser's end.
        struct Renderer(Arc<dyn VideoBackend>);
        impl VideoExport for Renderer {
            fn driver(&self) -> Option<Driver> {
                Some(if self.0.software()? {
                    Driver::Warp
                } else {
                    Driver::Hardware
                })
            }
            fn pool(&self, id: VideoId, width: u32, height: u32) -> Option<PoolHandles> {
                self.0.pool(id.0, width, height)
            }
            fn frame(&self, id: VideoId, generation: u32, index: u32) {
                assert!(self.0.frame(id.0, generation, index));
            }
            fn removed(&self, id: VideoId) {
                self.0.removed(id.0);
            }
        }
        let host = GpuHost::start(launch("gpu", None, Default::default()));
        assert!(running(&host.wait_until(WAIT, running)));
        let window = snowlark_paint::window::HiddenWindow::new().unwrap();
        let comp = CompositorThread::start(
            Gpu::new().unwrap(),
            window.handle(),
            Arc::new(TextEngine::new().unwrap()),
            Arc::new(ImageStore::new()),
            &Default::default(),
            Some(host.link()),
        );
        let raster = comp.gpu_raster().unwrap();
        let (refused_tx, refused) = std::sync::mpsc::channel::<String>();
        let refused_tx = Mutex::new(refused_tx);
        let refuse = move |why| {
            let _ = refused_tx.lock().unwrap().send(why);
        };
        raster.start(7, 7, Box::new(|| {}), Box::new(refuse), Box::new(|| {}));
        let videos = raster.videos(7);
        let renderer = VideoStore::exporting(Arc::new(Renderer(videos.clone())));
        let rgpu = renderer.gpu().unwrap().clone();
        let slot = renderer.create();
        for n in 0..3u8 {
            slot.upload(&rgpu, 4, 4, &[n; 64]).unwrap();
            renderer.frame_ready(slot.id());
        }
        assert!(refused.recv_timeout(Duration::from_millis(500)).is_err());
        // (its only pool is generation 1)
        assert!(videos.frame(slot.id().0, 99, 0));
        let why = refused.recv_timeout(WAIT).unwrap();
        assert!(why.contains("not given"), "{why}");
        raster.end(7);
        drop(comp);
    }
}
