// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tab in a renderer process of its own (`docs/process-isolation.md`, M4 step 3): the tab's
//! messages on the renderer's pipe, beside the services', and the browser's end of it.
//!
//! The pipe carries, besides the services ([`crate::services`]):
//!
//! - down ([`TabDown`]): the tab's start (its id and device, the first message), its commands, and
//!   [`TabDown::Taken`] each time the browser's raster thread has taken a job;
//! - up ([`TabUp`]): the tab's events, its messages to the compositor and its raster seam (resources
//!   and jobs, [`crate::raster_link`]).
//!
//! A renderer is one tab. What comes up is checked before anything acts on it: an event or a
//! compositor message naming another tab, a compositor message only the window sends, a favicon whose
//! pixels are not its size, and a raster message that fails its `join` each kill the renderer.
//! Display lists, console levels and the rest are checked as they decode (M0's codecs).
//!
//! The browser's end ([`RendererProcess`]) launches the child, serves its services and runs the tab's
//! raster thread, hands events to the window's [`EventSink`] and messages to the compositor, and gives
//! the caller a `Sender<TabCommand>` as a tab thread's would be. When the child ends without the
//! browser closing it (it crashed, was killed or broke a rule), the sink hears
//! [`TabEvent::Crashed`] as soon as its pipe breaks, with what its exit code says ([`CrashKind`]),
//! unless the renderer said it crashed first (a caught panic, whose message is kept). The renderer's
//! end is [`crate::renderer_main::run_renderer`].
//!
//! Hangs (§11, M4 step 5a): the renderer's page thread marks its passes in a section the browser
//! made ([`Liveness::shared`]), so the window's hang check reads a remote tab as a thread's. The
//! user's Kill asks the renderer to terminate the script, and ends the process if that does not end
//! the pass ([`RendererProcess::kill_script`]).
//!
//! Not yet across processes: video (step 4).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use snowlark_ipc::{
    wire, Bulk, Error, Launch, Process, ProcessKind, ProcessPolicy, RemoteHandle, Replier, Section,
};
use snowlark_paint::{Gpu, Point};

use crate::comp::{CompInput, CompMsg};
use crate::driver::Device;
use crate::raster_link::ToRasterHead;
use crate::raster_thread::{RasterEnds, RasterHost, SeamVideos};
use crate::services::{self, BrowserPort, Down, LocalServices, ServiceHost, Up};
use crate::tab::{EventSink, TabCommand, TabEvent, TabId};
use crate::watchdog::{Liveness, LIVENESS_BYTES};

/// The section bytes a renderer may hold at once (its frames, images and fonts on their way up).
pub const RENDERER_QUOTA: u64 = 256 << 20;
/// How long the browser waits for a new renderer to say hello (it stops at once if the child dies).
pub const CONNECT_WAIT: Duration = Duration::from_secs(20);
/// The largest favicon a renderer may send, each way (px).
pub const MAX_ICON: u32 = 4096;
/// The most login forms, and fields in one, a password form report may hold.
pub const MAX_PASSWORD_FORMS: usize = 64;
pub const MAX_PASSWORD_FIELDS: usize = 512;

/// The tab's messages from the browser to its renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum TabDown {
    /// The first message: which tab the renderer is, the viewport it lays out in, and where it
    /// marks its passes for the browser.
    Start {
        tab: TabId,
        device: Device,
        live: LiveShare,
    },
    /// A [`TabCommand`], encoded (a `Navigate` to a `data:` URL may be past a frame).
    Command(Bulk),
    /// The browser's raster thread has taken the job in flight (its lists are decoded): the
    /// tab's, or view tab `view`'s ([`TabCommand::AttachView`]).
    Taken { view: Option<TabId> },
    /// With a GPU process: the renderer's end of a link to it for the raster seam of the tab
    /// (`None`) or of view tab `view`, its handles in the renderer's table. The seam's messages go
    /// there from then on, and their `Taken` comes back on it (`docs/process-isolation.md`, M7 stage
    /// 4, step 2).
    RasterLink {
        view: Option<TabId>,
        end: snowlark_ipc::LinkEnd,
    },
    /// The raster thread of the tab's seam (`None`) or of view tab `view`'s is new: the GPU
    /// process was started again (`docs/process-isolation.md`, M7 stage 5, step 1). The renderer
    /// forgets what it sent, waits for no job in flight, and paints a whole frame again.
    RasterNew { view: Option<TabId> },
    /// The user chose to kill the tab's script: the renderer's IO thread has the page terminate it
    /// (the page thread is busy running it).
    Terminate,
    /// Before [`TabDown::Start`]: the renderer is a spare, which warms up further while it waits
    /// for a tab (a page of its own, made and dropped). One started for a tab is not told.
    WarmUp,
}

/// The tab's [`Liveness`] section, shared with its renderer: the section ([`LIVENESS_BYTES`]) and
/// the browser's clock on it when it was sent.
#[derive(Debug, Serialize, Deserialize)]
pub struct LiveShare {
    pub section: RemoteHandle,
    pub now: u64,
}

/// The tab's messages from its renderer to the browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum TabUp {
    /// A [`TabEvent`] of tab `tab`, encoded (a favicon or a devtools answer may be past a frame).
    Event {
        tab: TabId,
        event: Bulk,
    },
    /// For the compositor: only the scrolls a page asks for of its own tab.
    Comp(CompMsg),
    Raster(RasterUp),
}

/// One message of the raster seam ([`crate::raster_link::ToRaster`]): its head and the bytes beside it.
#[derive(Debug, Serialize, Deserialize)]
pub struct RasterUp {
    /// The seam's: the tab's (`None`), or a view tab's the browser attached
    /// ([`RendererProcess::attach_view`]).
    pub(crate) view: Option<TabId>,
    pub(crate) head: ToRasterHead,
    pub(crate) bytes: Bulk,
}

/// What a renderer process's tab reaches in the browser's window: the device its raster thread draws
/// with, the compositor, and where its events go.
pub struct WindowEnds {
    pub gpu: Arc<Gpu>,
    pub compositor: Sender<CompInput>,
    pub events: Arc<dyn EventSink>,
    /// With a GPU process: its raster threads, which draw the tab's frames in place of the
    /// window's (`gpu` and `compositor` are then unused by its seams).
    pub gpu_raster: Option<Arc<dyn crate::raster_thread::GpuRaster>>,
}

/// The browser's handle to a renderer process running one tab. Dropping it leaves the child to the
/// [`TabCommand::Close`] its commands' last sender sends (as a tab thread is left).
pub struct RendererProcess {
    commands: snowlark_ipc::Sender<TabCommand>,
    process: Arc<Process>,
    audio: Arc<services::audio::AudioHost>,
    net: Arc<services::net::NetHost>,
    port: BrowserPort,
    live: Arc<Liveness>,
    ending: Arc<Ending>,
    /// A kill of the tab's script is under way and the tab has not been seen to get going again.
    killing: Arc<AtomicBool>,
    /// The view tabs the renderer serves ([`RendererProcess::attach_view`]), and what makes their
    /// raster threads.
    views: Views,
    raster_ends: RasterEnds,
    video_ids: SeamVideos,
}

/// A renderer's view tabs.
type Views = Arc<std::sync::Mutex<ViewTable>>;

#[derive(Default)]
struct ViewTable {
    /// Each with its raster thread: the only tabs besides its own the renderer may speak for.
    attached: std::collections::HashMap<TabId, RasterHost>,
    /// Views the browser detached: what the renderer sends for them before it hears of it is
    /// dropped (it broke no rule).
    detached: std::collections::HashSet<TabId>,
    /// The browser closed the renderer's own tab while views were attached
    /// ([`RendererProcess::release`]): the process, which the last view's detach reaps.
    orphan: Option<(Arc<Process>, Duration)>,
}

/// Waits up to `wait` for `process` to end on its own (it does once it has nothing left to
/// serve), then ends it.
fn reap(process: Arc<Process>, wait: Duration) {
    let _ = std::thread::Builder::new()
        .name("snowlark renderer reaper".into())
        .spawn(move || {
            if process.wait(wait).is_none() {
                process.kill();
            }
        });
}

fn views_of(views: &Views) -> std::sync::MutexGuard<'_, ViewTable> {
    views.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a renderer's message for tab `of` is to the browser.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Speaker {
    Own,
    View,
    Detached,
    Other,
}

fn speaker(tab: TabId, views: &Views, of: TabId) -> Speaker {
    if of == tab {
        return Speaker::Own;
    }
    let table = views_of(views);
    if table.attached.contains_key(&of) {
        Speaker::View
    } else if table.detached.contains(&of) {
        Speaker::Detached
    } else {
        Speaker::Other
    }
}

/// How a renderer's end came: the browser closed it, it said it crashed, or the browser killed it.
#[derive(Default)]
struct Ending {
    closing: AtomicBool,
    crashed: AtomicBool,
    killed: AtomicBool,
}

/// How long a kill of a renderer's script has to end the pass it is stuck in before the browser
/// ends the process (§11).
pub const KILL_GRACE: Duration = Duration::from_secs(1);

/// What a renderer process's exit code says about how it ended (§11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashKind {
    /// `0xC00000FD`.
    StackOverflow,
    /// `0xC0000005`.
    AccessViolation,
    /// `0xC0000409`: a fast fail (`abort`, a failed security check).
    FastFail,
    /// 101: a Rust panic that reached the top of the process.
    Panic,
    /// Another code.
    Other(u32),
}

impl CrashKind {
    pub fn of(code: u32) -> CrashKind {
        match code {
            0xC000_00FD => CrashKind::StackOverflow,
            0xC000_0005 => CrashKind::AccessViolation,
            0xC000_0409 => CrashKind::FastFail,
            101 => CrashKind::Panic,
            other => CrashKind::Other(other),
        }
    }

    /// The words the crashed tab shows.
    pub fn describe(self) -> String {
        match self {
            CrashKind::StackOverflow => "a stack overflow".into(),
            CrashKind::AccessViolation => "an access violation".into(),
            CrashKind::FastFail => "a fast fail (abort)".into(),
            CrashKind::Panic => "a panic".into(),
            CrashKind::Other(code) => format!("exit code {code:#x}"),
        }
    }
}

/// The message a crashed tab shows for a renderer that ended without the browser closing it, nor
/// saying it crashed: `killed` by the browser, else its exit `code` (`None` if the pipe broke and
/// the process was still there).
pub fn ended_message(killed: bool, code: Option<u32>, end: &Error) -> String {
    if killed {
        return "the renderer process was ended: it did not respond".into();
    }
    match (end, code.map(CrashKind::of)) {
        (Error::BadMessage(why), _) => format!("the renderer broke a rule: {why}"),
        (_, Some(CrashKind::Other(code))) => {
            format!("the renderer process ended (exit code {code:#x})")
        }
        (_, Some(kind)) => format!(
            "the renderer process ended with {} (exit code {:#x})",
            kind.describe(),
            code.unwrap_or_default()
        ),
        (_, None) => format!("the renderer's pipe closed: {end}"),
    }
}

/// The lines a crashed tab's message gets for the minidump the browser wrote of its renderer: the
/// folder, then the file (each a line of its own, as the crashed tab's page cuts long lines).
pub fn crash_dump_lines(dump: &std::path::Path) -> String {
    let folder = dump
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let file = dump
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("\nA crash dump was saved in {folder}\n{file}")
}

/// A renderer process that said hello and has no tab yet: it makes its driver meanwhile
/// (`crate::renderer_main`), so a tab given to it starts warm (§11, "Spare process").
/// Dropping it ends the child (its pipe breaks before it hears of a tab, and its job closes).
pub struct Spare {
    port: BrowserPort,
    process: Arc<Process>,
}

impl Spare {
    /// Launches `launch` (as for [`RendererProcess::start`]), numbered `process` among the
    /// browser's children, and waits for its hello, no longer than [`CONNECT_WAIT`] or the child's
    /// life.
    pub fn launch(launch: Launch, process: u64) -> Result<Spare, Error> {
        Spare::connect(launch.spawn()?, process)
    }

    /// [`Spare::launch`], for a child already started.
    pub fn connect(spawned: snowlark_ipc::Spawned, process: u64) -> Result<Spare, Error> {
        let policy = ProcessPolicy {
            process,
            kind: ProcessKind::Renderer,
            lock: Default::default(),
        };
        let (port, process) = spawned.connect::<Down, Up>(policy, RENDERER_QUOTA, CONNECT_WAIT)?;
        Ok(Spare { port, process })
    }

    /// Tells the renderer it is a spare, which may wait a while for a tab: it warms up further
    /// meanwhile ([`TabDown::WarmUp`]).
    pub fn warm_up(&self) -> Result<(), Error> {
        self.port.send(&Down::Tab(TabDown::WarmUp))
    }

    /// Whether the child still runs.
    pub fn alive(&self) -> bool {
        self.process.wait(Duration::ZERO).is_none()
    }

    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }
}

impl RendererProcess {
    /// Launches `launch` (the renderer's executable and arguments, which call
    /// [`crate::renderer_main::run_renderer`]) as tab `tab` with `device`, numbered `process` among
    /// the browser's children, and serves it from `local` and `window`. Waits for the child's hello,
    /// no longer than [`CONNECT_WAIT`] or the child's life.
    pub fn start(
        launch: Launch,
        process: u64,
        tab: TabId,
        device: Device,
        local: &LocalServices,
        window: WindowEnds,
    ) -> Result<RendererProcess, Error> {
        RendererProcess::assign(Spare::launch(launch, process)?, tab, device, local, window)
    }

    /// Gives `spare` tab `tab` with `device`, and serves it from `local` and `window`. A spare that
    /// died since its hello is a renderer that crashed at once (the window hears `Crashed`), so the
    /// caller checks [`Spare::alive`] first.
    pub fn assign(
        spare: Spare,
        tab: TabId,
        device: Device,
        local: &LocalServices,
        window: WindowEnds,
    ) -> Result<RendererProcess, Error> {
        let Spare {
            port,
            process: child,
        } = spare;
        // (it was started in the background, below the window's priority)
        child.set_background(false);
        // A page hosts the windows it opens with an opener, which navigate themselves from the
        // moment script opens them (before the browser hears of them): `SiteLock::hosts_windows`.
        port.policy().lock.host_windows();
        let section = Section::create(LIVENESS_BYTES)?;
        let shared = port.share_section(&section)?;
        let live = Arc::new(Liveness::shared(section));
        let share = LiveShare {
            section: shared,
            now: live.now_ms(),
        };
        port.send(&Down::Tab(TabDown::Start {
            tab,
            device,
            live: share,
        }))?;
        // (with a GPU process its device makes the textures and draws the frames: G4 step 4)
        let host = match &window.gpu_raster {
            Some(gpu) => ServiceHost::with_videos(port.clone(), local, Some(gpu.videos(tab))),
            None => ServiceHost::new(port.clone(), local),
        };
        let audio = host.audio().clone();
        let net = host.net().clone();
        let video_ids = SeamVideos {
            renderer: tab,
            ids: host.video_ids(),
            players: host.player_ids(),
        };
        let views = Views::default();
        let raster_ends = RasterEnds {
            gpu: window.gpu.clone(),
            compositor: window.compositor.clone(),
            gpu_raster: window.gpu_raster.clone(),
        };
        let view_video_ids = video_ids.clone();
        let ending = Arc::new(Ending::default());
        let site_lock = local.site_lock;
        let browser_port = port.clone();
        let (commands, rx) = snowlark_ipc::channel::<TabCommand>();
        {
            let (port, ending) = (port.clone(), ending.clone());
            std::thread::Builder::new()
                .name(format!("snowlark renderer {tab} commands"))
                .spawn(move || {
                    for command in rx {
                        if command == TabCommand::Close {
                            ending.closing.store(true, Ordering::Release);
                        }
                        if send_command(&port, &command).is_err() {
                            return;
                        }
                    }
                })
                .map_err(|e| no_thread(&child, e))?;
        }
        {
            let exited = child.clone();
            let ending = ending.clone();
            let views = views.clone();
            let raster = RasterHost::start(tab, false, &raster_ends, port.clone(), video_ids);
            std::thread::Builder::new()
                .name(format!("snowlark renderer {tab}"))
                .spawn(move || {
                    let end = host.run_with(&mut |message, replier| {
                        let ends = Ends {
                            port: &port,
                            tab,
                            window: &window,
                            raster: &raster,
                            views: &views,
                            ending: &ending,
                            site_lock,
                        };
                        serve_tab(&ends, message, replier)
                    });
                    // (what it left in the browser's services is undone, and its raster threads
                    // end, before the window hears of it)
                    drop(host);
                    drop(raster);
                    let view_tabs: Vec<TabId> =
                        views_of(&views).attached.drain().map(|(t, _)| t).collect();
                    // (a renderer the browser closed ended as asked, unless views it still
                    // served went with it)
                    if (ending.closing.load(Ordering::Acquire) && view_tabs.is_empty())
                        || ending.crashed.load(Ordering::Acquire)
                    {
                        return;
                    }
                    // (the pipe broke as the process ended: its exit code follows at once)
                    let code = match end {
                        Error::BadMessage(_) => None,
                        _ => exited.wait(Duration::from_secs(1)),
                    };
                    let killed = ending.killed.load(Ordering::Acquire);
                    let mut why = ended_message(killed, code, &end);
                    // (the browser wrote it before it ended the process: there by its exit)
                    if let Some(dump) = exited.crash_dump() {
                        why.push_str(&crash_dump_lines(&dump));
                    }
                    // (the views of its page's windows went with it)
                    for view in view_tabs {
                        window.events.send(view, TabEvent::Crashed(why.clone()));
                    }
                    window.events.send(tab, TabEvent::Crashed(why));
                })
                .map_err(|e| no_thread(&child, e))?;
        }
        Ok(RendererProcess {
            commands,
            process: child,
            audio,
            net,
            port: browser_port,
            live,
            ending,
            killing: Arc::default(),
            views,
            raster_ends,
            video_ids: view_video_ids,
        })
    }

    /// The renderer serves tab `tab` as a view of its page from now on (the browser's next word to
    /// it is [`TabCommand::AttachView`]): the renderer may speak for it, and its frames have a raster
    /// thread here.
    /// Dropping the handle detaches it: the renderer speaks for the tab no more, and its raster
    /// thread ends.
    pub fn attach_view(&self, tab: TabId) -> ViewHandle {
        let raster = RasterHost::start(
            tab,
            true,
            &self.raster_ends,
            self.port.clone(),
            self.video_ids.clone(),
        );
        views_of(&self.views).attached.insert(tab, raster);
        ViewHandle {
            views: self.views.clone(),
            tab,
        }
    }
}

/// A view tab attached to a renderer ([`RendererProcess::attach_view`]); dropping it detaches it.
pub struct ViewHandle {
    views: Views,
    tab: TabId,
}

impl Drop for ViewHandle {
    fn drop(&mut self) {
        let mut table = views_of(&self.views);
        if table.attached.remove(&self.tab).is_some() {
            table.detached.insert(self.tab);
        }
        // (the last view of a renderer whose own tab closed: it ends now)
        if table.attached.is_empty() {
            if let Some((process, wait)) = table.orphan.take() {
                reap(process, wait);
            }
        }
    }
}

impl RendererProcess {
    /// The browser is done with the renderer's own tab (it closed, or moved to another renderer)
    /// and sent it [`TabCommand::Close`]. A renderer with no views ends: it is given `wait` to
    /// end on its own, then ended. One that serves views of its page's windows lives on for them
    /// (the page goes on running, its own top-level document gone), and ends the same way when
    /// the last of them is detached, as a renderer process lives while any of its views does.
    pub fn release(self, wait: Duration) {
        let process = self.process.clone();
        let mut table = views_of(&self.views);
        if table.attached.is_empty() {
            drop(table);
            drop(self);
            reap(process, wait);
        } else {
            table.orphan = Some((process, wait));
        }
    }
}

impl RendererProcess {
    /// The tab's liveness: the renderer's passes, on the browser's clock, and the window's input.
    pub fn liveness(&self) -> &Arc<Liveness> {
        &self.live
    }

    /// The user's Kill (§11): the renderer is asked to terminate the script its page thread runs,
    /// which keeps the page. If the pass it is stuck in has not ended [`KILL_GRACE`] later, or at a
    /// second Kill before the tab got going again, the process is ended, and the tab shows as
    /// crashed ("did not respond"). A hang in native code ends only the second way.
    pub fn kill_script(&self) {
        let pass = self.live.pass();
        if self.killing.swap(true, Ordering::AcqRel) {
            self.end();
            return;
        }
        if pass == 0 {
            // (by the renderer's word it is not in a pass: there is nothing to stop)
            self.killing.store(false, Ordering::Release);
            return;
        }
        let _ = self.port.send(&Down::Tab(TabDown::Terminate));
        let (process, live, killing, ending) = (
            self.process.clone(),
            self.live.clone(),
            self.killing.clone(),
            self.ending.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("snowlark renderer kill".into())
            .spawn(move || {
                // (ends at once if the renderer dies meanwhile)
                if process.wait(KILL_GRACE).is_some() {
                    return;
                }
                if live.pass() == pass {
                    ending.killed.store(true, Ordering::Release);
                    process.kill();
                } else {
                    killing.store(false, Ordering::Release);
                }
            });
        if spawned.is_err() {
            self.end();
        }
    }

    /// Ends the renderer process: the tab shows as crashed, as one the browser ended.
    fn end(&self) {
        self.ending.killed.store(true, Ordering::Release);
        self.process.kill();
    }

    /// Where the tab's commands go (the window's and the compositor's, [`crate::comp::CompMsg::Bind`]).
    pub fn sender(&self) -> snowlark_ipc::Sender<TabCommand> {
        self.commands.clone()
    }

    /// The tab's audio: what the window mutes it through.
    pub fn audio(&self) -> &Arc<services::audio::AudioHost> {
        &self.audio
    }

    /// M5: hands the renderer a navigation response's stream to commit from
    /// ([`services::net::NetHost::hand_over`]): the id [`TabCommand::Commit`] names.
    /// What the browser knows about the renderer: its site lock.
    pub fn policy(&self) -> &ProcessPolicy {
        self.port.policy()
    }

    pub fn hand_over(&self, stream: snowlark_net::ResponseStream) -> u64 {
        self.net.hand_over(stream)
    }

    /// The child process (its id, and a way to kill it).
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }
}

/// A thread the browser could not start for a renderer: the child is killed, as it would wait for
/// nothing.
fn no_thread(child: &Process, e: std::io::Error) -> Error {
    eprintln!("[renderer] no thread to serve it: {e}");
    child.kill();
    Error::Refused
}

fn send_command(port: &BrowserPort, command: &TabCommand) -> Result<(), Error> {
    let bytes = wire::to_bytes(command)?;
    let bulk = port.bulk(&bytes)?;
    port.send(&Down::Tab(TabDown::Command(bulk)))
}

/// What serves a renderer's tab messages: its pipe, its tab and its view tabs, where they go.
struct Ends<'a> {
    port: &'a BrowserPort,
    tab: TabId,
    window: &'a WindowEnds,
    raster: &'a RasterHost,
    views: &'a Views,
    ending: &'a Ending,
    site_lock: services::site_lock::Mode,
}

/// Takes one of the tab's messages from the renderer of tab `tab`. One that breaks a rule kills it.
fn serve_tab(ends: &Ends, message: TabUp, replier: Option<Replier>) -> Result<(), Error> {
    let Ends {
        port,
        tab,
        window,
        raster,
        views,
        ending,
        site_lock,
    } = *ends;
    let bad = |why: String| {
        port.bad_message(&why);
        Error::BadMessage(why)
    };
    if replier.is_some() {
        return Err(bad("a tab's message sent as a call".into()));
    }
    // (a view the browser detached meanwhile is one the renderer has not heard of closing yet:
    // what it sent is dropped, not a broken rule)
    match message {
        TabUp::Event { tab: of, event } => {
            let event: TabEvent = services::unpack(port, event)?;
            let who = speaker(tab, views, of);
            if who == Speaker::Detached {
                return Ok(());
            }
            let is_view = who == Speaker::View;
            check_event(tab, of, is_view, &event).map_err(bad)?;
            // (a view's window shows any site's documents: it stays in its opener's renderer,
            // behind the cross-origin access checks of the page)
            if !is_view {
                services::site_lock::check(
                    port,
                    site_lock,
                    services::site_lock::event_violation(port.policy(), &event),
                )?;
            }
            if services::site_lock::refuse(
                port,
                site_lock,
                services::site_lock::event_refused(port.policy(), &event),
            ) {
                return Ok(());
            }
            if let TabEvent::Crashed(_) = event {
                if !is_view {
                    ending.crashed.store(true, Ordering::Release);
                }
            }
            window.events.send(of, event);
        }
        TabUp::Comp(message) => {
            let of = match &message {
                CompMsg::ScrollTo { tab, .. }
                | CompMsg::ScrollBy { tab, .. }
                | CompMsg::NeedsBeginFrames { tab, .. } => *tab,
                _ => tab,
            };
            let as_tab = match speaker(tab, views, of) {
                Speaker::Detached => return Ok(()),
                Speaker::View => of,
                Speaker::Own | Speaker::Other => tab,
            };
            check_comp(as_tab, &message).map_err(bad)?;
            let _ = window.compositor.send(message.into());
        }
        TabUp::Raster(message) => match message.view {
            None => raster.receive(message)?,
            Some(of) => match speaker(tab, views, of) {
                Speaker::View => {
                    if let Some(seam) = views_of(views).attached.get(&of) {
                        seam.receive(message)?;
                    }
                }
                Speaker::Detached => {}
                Speaker::Own | Speaker::Other => {
                    return Err(bad(format!("a raster seam of tab {of}, not a view's")))
                }
            },
        },
    }
    Ok(())
}

/// Whether tab `tab`'s renderer may send `event` as tab `of`'s: its own, or (`view`) a view tab
/// the browser attached to it ([`RendererProcess::attach_view`]).
pub(crate) fn check_event(
    tab: TabId,
    of: TabId,
    view: bool,
    event: &TabEvent,
) -> Result<(), String> {
    if of != tab && !view {
        return Err(format!("an event of tab {of} from tab {tab}'s renderer"));
    }
    if let TabEvent::NavigationReady(_) = event {
        return Err("the browser's NavigationReady from a renderer".into());
    }
    if let TabEvent::CloseWindow | TabEvent::WindowRect(..) = event {
        // (only a view's window closes or moves so: a page does not close or move its own tab)
        if !view {
            return Err("a window's event of the renderer's own tab".into());
        }
    }
    if view {
        // (what a page's tab alone asks for: windows, navigations, the browser's own pages)
        if let TabEvent::OpenWindow { .. }
        | TabEvent::Internal(_)
        | TabEvent::BeginNavigation(_)
        | TabEvent::Traverse(_)
        | TabEvent::Fullscreen(_)
        | TabEvent::Passwords(_)
        | TabEvent::Devtools(_) = event
        {
            return Err(format!("a page's own event from view tab {of}"));
        }
    }
    if let TabEvent::OpenWindow { size, .. } = event {
        let side = |v: f32| v.is_finite() && (0.0..=crate::tab::MAX_WINDOW_SIDE).contains(&v);
        if !side(size.0) || !side(size.1) {
            return Err(format!("a window of {size:?}"));
        }
    }
    if let TabEvent::Passwords(p) = event {
        use snowlark_page::passwords::PasswordEvent;
        let forms = match p {
            PasswordEvent::Submitted(f) | PasswordEvent::SubmittedInPlace(f) => {
                std::slice::from_ref(f)
            }
            PasswordEvent::Rendered(v) | PasswordEvent::Added(v) => v.as_slice(),
        };
        if forms.len() > MAX_PASSWORD_FORMS
            || forms.iter().any(|f| f.fields.len() > MAX_PASSWORD_FIELDS)
        {
            return Err(format!("a password form report of {} forms", forms.len()));
        }
    }
    if let TabEvent::Favicon(icon, _) = event {
        let size = u64::from(icon.width) * u64::from(icon.height) * 4;
        if icon.width == 0
            || icon.height == 0
            || icon.width > MAX_ICON
            || icon.height > MAX_ICON
            || icon.pixels.len() as u64 != size
            || !(1..=8).contains(&icon.orientation)
        {
            return Err(format!(
                "a favicon of {}x{} with {} bytes, orientation {}",
                icon.width,
                icon.height,
                icon.pixels.len(),
                icon.orientation
            ));
        }
    }
    Ok(())
}

/// Whether tab `tab`'s renderer may send `message` to the compositor: a scroll of its own tab to a
/// point that is one, or whether its own tab needs begin frames (a renderer's frame sink sets
/// `SetNeedsBeginFrame` for itself only: `CompositorFrameSinkSupport`, viz).
pub(crate) fn check_comp(tab: TabId, message: &CompMsg) -> Result<(), String> {
    let finite = |p: &Point| p.x.is_finite() && p.y.is_finite();
    let (of, point) = match message {
        CompMsg::ScrollTo { tab, to, .. } => (*tab, to),
        CompMsg::ScrollBy { tab, delta, .. } => (*tab, delta),
        CompMsg::NeedsBeginFrames { tab: of, .. } => {
            if *of != tab {
                return Err(format!(
                    "begin frames for tab {of} from tab {tab}'s renderer"
                ));
            }
            return Ok(());
        }
        other => {
            let name = format!("{other:?}");
            let name = name.split([' ', '(']).next().unwrap_or_default().to_owned();
            return Err(format!("the window's {name} from a renderer"));
        }
    };
    if of != tab {
        return Err(format!("a scroll of tab {of} from tab {tab}'s renderer"));
    }
    if !finite(point) {
        return Err(format!("a scroll to {point:?}"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "renderer_process_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "renderer_lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[path = "spare_tests.rs"]
mod spare_tests;
