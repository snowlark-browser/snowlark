// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's PlayReady modules and their sessions, for pages that reach them by messages
//! (Encrypted Media Extensions with the module outside the page's process).
//!
//! One thread of the [`Service`] owns every module and session ([`super::playready`]): the calls
//! are queued to it and answered through each module's [`Sink`], as are the sessions' messages and
//! key status changes, which Media Foundation reports on its own threads. Ids are the caller's.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Media::MediaFoundation::{
    IMFContentDecryptionModule, IMFContentDecryptionModuleSession, IMFMediaEngineEx,
    MF_E_UNSUPPORTED_FORMAT, MF_MEDIA_ENGINE_EVENT, MF_MEDIA_ENGINE_EVENT_ENDED,
    MF_MEDIA_ENGINE_EVENT_ERROR, MF_MEDIA_ENGINE_EVENT_FIRSTFRAMEREADY,
    MF_MEDIA_ENGINE_EVENT_PAUSE, MF_MEDIA_ENGINE_EVENT_PLAYING, MF_MEDIA_ENGINE_EVENT_SEEKED,
    MF_MEDIA_ENGINE_EVENT_WAITING, MF_MEDIA_ENGINE_STATISTIC_FRAMES_DROPPED,
    MF_MEDIA_ENGINE_STATISTIC_FRAMES_RENDERED,
};

use super::playready::{self, SessionEvent};
use super::protected_source;

/// What the service reports for one module's caller.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The module of [`Service::create`] `request` was made, or why not.
    Created {
        request: u64,
        result: Result<(), String>,
    },
    /// A session call ended: [`Service::generate_request`]'s with the session's id.
    Done {
        request: u64,
        result: Result<String, String>,
    },
    /// A message for the licence server: its EME type and bytes.
    Message {
        session: u64,
        kind: &'static str,
        data: Vec<u8>,
    },
    /// The session's key statuses: each key id (as the media carries it) and its EME status.
    Statuses {
        session: u64,
        statuses: Vec<([u8; 16], &'static str)>,
    },
    /// The session's expiration time in milliseconds since 1970 (NaN: none).
    Expiration { session: u64, time: f64 },
    /// The session is closed.
    Closed { session: u64 },
}

/// Where one module's events go (from any thread).
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// What a protected player is told to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Play,
    Pause,
    /// Pauses and drops the samples not yet played, before a seek (the samples from the new
    /// position follow the [`Control::Seek`]).
    Flush,
    /// To this time, in seconds.
    Seek(f64),
    Rate(f64),
    Volume(f64),
    Muted(bool),
    /// The track with this id has no more samples.
    End(u32),
    /// Where the video shows, in screen pixels: the protection window takes this place (the
    /// monitor and adapter the output is protected on) and the engine's picture this size.
    Output {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
}

/// What a protected player reports.
#[derive(Debug)]
pub enum PlayerEvent {
    /// It could not be made, or its engine failed: why.
    Error(String),
    /// The engine's picture: a composition surface handle of this process (the player's own
    /// duplicate, the receiver's to close) for DirectComposition to show.
    Surface(std::os::windows::io::OwnedHandle),
    /// The engine's playback changed, and its time then, in seconds.
    Engine { state: EngineState, time: f64 },
    /// While it has a picture, every [`PROGRESS`]: its time and its counts of frames rendered and
    /// dropped (protected frames cannot be read back: these are the measure).
    Progress {
        time: f64,
        rendered: u32,
        dropped: u32,
    },
}

/// What a protected player's engine reports (the media engine events a page's element follows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    /// The first frame is ready.
    FirstFrame,
    Playing,
    /// Playback stopped for want of data.
    Waiting,
    Paused,
    Seeked,
    Ended,
}

impl EngineState {
    fn of(event: u32) -> Option<EngineState> {
        let e = MF_MEDIA_ENGINE_EVENT(event as i32);
        Some(match e {
            MF_MEDIA_ENGINE_EVENT_FIRSTFRAMEREADY => EngineState::FirstFrame,
            MF_MEDIA_ENGINE_EVENT_PLAYING => EngineState::Playing,
            MF_MEDIA_ENGINE_EVENT_WAITING => EngineState::Waiting,
            MF_MEDIA_ENGINE_EVENT_PAUSE => EngineState::Paused,
            MF_MEDIA_ENGINE_EVENT_SEEKED => EngineState::Seeked,
            MF_MEDIA_ENGINE_EVENT_ENDED => EngineState::Ended,
            _ => return None,
        })
    }
}

/// How often a player reports [`PlayerEvent::Progress`].
pub const PROGRESS: std::time::Duration = std::time::Duration::from_millis(250);

/// Where one player's events go (from the service's thread).
pub type PlayerSink = Arc<dyn Fn(PlayerEvent) + Send + Sync>;

enum Job {
    Create {
        request: u64,
        keys: u64,
        key_system: String,
        origin_id: String,
        store: PathBuf,
        sink: Sink,
    },
    Generate {
        request: u64,
        keys: u64,
        session: u64,
        init_data_type: String,
        init_data: Vec<u8>,
    },
    Update {
        request: u64,
        session: u64,
        response: Vec<u8>,
    },
    Close {
        request: u64,
        session: u64,
    },
    Remove {
        request: u64,
        session: u64,
    },
    Release {
        keys: u64,
    },
    /// A session's key statuses changed (from Media Foundation's thread).
    Changed {
        session: u64,
    },
    Play {
        player: u64,
        keys: u64,
        tracks: Vec<crate::fmp4::Track>,
        sink: PlayerSink,
    },
    Sample {
        player: u64,
        track: u32,
        sample: crate::fmp4::Sample,
    },
    Control {
        player: u64,
        control: Control,
    },
    End {
        player: u64,
    },
    /// The service is dropped.
    Stop,
}

/// See the module.
pub struct Service {
    tx: Mutex<Sender<Job>>,
}

impl Service {
    /// Starts the service's thread; it ends when the service is dropped.
    pub fn start() -> std::io::Result<Service> {
        let (tx, rx) = channel();
        let worker_tx = tx.clone();
        std::thread::Builder::new()
            .name("playready".into())
            .spawn(move || run(rx, worker_tx))?;
        Ok(Service { tx: Mutex::new(tx) })
    }

    fn send(&self, job: Job) {
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(job);
    }

    /// Makes module `keys` of `key_system` for one origin (its id for the module, its store
    /// folder); `Event::Created` answers `request`.
    pub fn create(
        &self,
        request: u64,
        keys: u64,
        key_system: &str,
        origin_id: &str,
        store: PathBuf,
        sink: Sink,
    ) {
        self.send(Job::Create {
            request,
            keys,
            key_system: key_system.to_owned(),
            origin_id: origin_id.to_owned(),
            store,
            sink,
        });
    }

    /// Makes temporary session `session` of module `keys` and has it generate a licence request.
    pub fn generate_request(
        &self,
        request: u64,
        keys: u64,
        session: u64,
        init_data_type: &str,
        init_data: Vec<u8>,
    ) {
        self.send(Job::Generate {
            request,
            keys,
            session,
            init_data_type: init_data_type.to_owned(),
            init_data,
        });
    }

    pub fn update(&self, request: u64, session: u64, response: Vec<u8>) {
        self.send(Job::Update {
            request,
            session,
            response,
        });
    }

    pub fn close(&self, request: u64, session: u64) {
        self.send(Job::Close { request, session });
    }

    pub fn remove(&self, request: u64, session: u64) {
        self.send(Job::Remove { request, session });
    }

    /// Module `keys` and its sessions are no longer used (nor its players).
    pub fn release(&self, keys: u64) {
        self.send(Job::Release { keys });
    }

    /// Makes protected player `player` of module `keys` for these tracks (each one Media
    /// Foundation is given a type for becomes a stream), playing in a media engine whose picture
    /// is a composition surface ([`PlayerEvent::Surface`]). Its events go to `sink`.
    pub fn play(&self, player: u64, keys: u64, tracks: Vec<crate::fmp4::Track>, sink: PlayerSink) {
        self.send(Job::Play {
            player,
            keys,
            tracks,
            sink,
        });
    }

    /// Queues a sample of track `track` (an id of [`Service::play`]'s tracks).
    pub fn sample(&self, player: u64, track: u32, sample: crate::fmp4::Sample) {
        self.send(Job::Sample {
            player,
            track,
            sample,
        });
    }

    pub fn control(&self, player: u64, control: Control) {
        self.send(Job::Control { player, control });
    }

    /// Player `player` ends.
    pub fn stop(&self, player: u64) {
        self.send(Job::End { player });
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // (the sessions' callbacks hold senders too: the thread ends on this, not on the channel)
        self.send(Job::Stop);
    }
}

/// Whether this Windows has the PlayReady module this offers: Windows 10 2004 (build 19041) or
/// later, the first with `IMFMediaEngineClassFactory4`.
pub fn system_supports() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 32];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: the buffer and its byte length are passed together; the value is a string.
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows::core::w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            windows::core::w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if read.is_err() {
        return false;
    }
    let text = String::from_utf16_lossy(&buf[..(len as usize / 2).min(buf.len())]);
    text.trim_end_matches('\0')
        .parse::<u32>()
        .is_ok_and(|b| b >= 19041)
}

struct Module {
    cdm: IMFContentDecryptionModule,
    sink: Sink,
}

struct Session {
    keys: u64,
    session: IMFContentDecryptionModuleSession,
}

fn error(e: &windows::core::Error) -> String {
    format!("{:#010x}: {}", e.code().0 as u32, e.message())
}

/// The session's id (`GetSessionId`).
fn session_id(s: &IMFContentDecryptionModuleSession) -> Option<String> {
    // SAFETY: the string is a CoTaskMem allocation this function reads and frees.
    unsafe {
        let p: PWSTR = s.GetSessionId().ok()?;
        if p.is_null() {
            return None;
        }
        let id = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        id.filter(|i| !i.is_empty())
    }
}

/// The window a protected media engine checks its output protection against: a window of this
/// thread that is never shown, placed where the video shows so that the protection (and, on a
/// machine with several adapters, the adapter) is the one of that monitor.
struct ProtectionWindow(HWND);

impl ProtectionWindow {
    fn new() -> windows::core::Result<ProtectionWindow> {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;
        unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            // SAFETY: the default handling of a message for a window of ours.
            unsafe { DefWindowProcW(h, m, w, l) }
        }
        let class = windows::core::w!("SnowlarkVideoProtection");
        // SAFETY: a window class of this module and a window of it on this thread; the window is
        // destroyed in Drop on the same thread (the service's), which pumps its messages.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc_),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // (registered once per process: a second registration fails, and the first stands)
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_NOPARENTNOTIFY
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOREDIRECTIONBITMAP,
                class,
                windows::core::w!(""),
                WS_POPUP | WS_DISABLED | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            Ok(ProtectionWindow(hwnd))
        }
    }

    fn place(&self, x: i32, y: i32, width: i32, height: i32) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        // SAFETY: our own live window, moved without activation or showing.
        let _ = unsafe {
            SetWindowPos(
                self.0,
                Some(HWND_BOTTOM),
                x,
                y,
                width.max(1),
                height.max(1),
                SWP_NOACTIVATE,
            )
        };
    }
}

impl Drop for ProtectionWindow {
    fn drop(&mut self) {
        // SAFETY: our own window, destroyed on the thread that made it.
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0) };
    }
}

/// A protected player on the service's thread.
struct Player {
    keys: u64,
    engine: IMFMediaEngineEx,
    feed: protected_source::Feed,
    /// The track id of each stream.
    streams: Vec<u32>,
    sink: PlayerSink,
    events: Receiver<(u32, usize, u32)>,
    /// (dropped after the engine: the engine is shut down first)
    window: ProtectionWindow,
    surface: bool,
    size: (i32, i32),
    progress: std::time::Instant,
}

impl Player {
    fn new(
        cdm: &IMFContentDecryptionModule,
        keys: u64,
        tracks: &[crate::fmp4::Track],
        sink: PlayerSink,
    ) -> windows::core::Result<Player> {
        use protected_source::{StreamSpec, Trust};
        let mut streams = Vec::new();
        let mut specs = Vec::new();
        for t in tracks {
            if let Some(media_type) = protected_source::media_type(t) {
                streams.push(t.id);
                specs.push(StreamSpec {
                    media_type: media_type?,
                    protected: t.encryption.is_some(),
                });
            }
        }
        if specs.is_empty() {
            return Err(windows::core::Error::from(MF_E_UNSUPPORTED_FORMAT));
        }
        let (source, feed) = protected_source::create(specs, Arc::new(Trust::new(cdm.clone())))?;
        let extension = protected_source::extension(source);
        let window = ProtectionWindow::new()?;
        let (tx, events) = channel();
        let engine =
            playready::protected_engine(cdm, tx, Some(&extension), Some(window.0 .0 as isize))?;
        // SAFETY: a plain call on the live engine.
        unsafe { engine.SetSource(&windows::core::BSTR::from(protected_source::URL))? };
        Ok(Player {
            keys,
            engine,
            feed,
            streams,
            sink,
            events,
            window,
            surface: false,
            size: (0, 0),
            progress: std::time::Instant::now(),
        })
    }

    fn control(&mut self, control: Control) -> windows::core::Result<()> {
        // SAFETY: plain calls on the live engine with live data.
        unsafe {
            match control {
                Control::Play => self.engine.Play(),
                Control::Pause => self.engine.Pause(),
                Control::Flush => {
                    let paused = self.engine.Pause();
                    self.feed.flush();
                    paused
                }
                Control::Seek(t) => self.engine.SetCurrentTime(t),
                Control::Rate(r) => self.engine.SetPlaybackRate(r),
                Control::Volume(v) => self.engine.SetVolume(v),
                Control::Muted(m) => self.engine.SetMuted(m),
                Control::End(track) => {
                    if let Some(i) = self.streams.iter().position(|&s| s == track) {
                        self.feed.end(i);
                    }
                    Ok(())
                }
                Control::Output {
                    x,
                    y,
                    width,
                    height,
                } => {
                    self.window.place(x, y, width, height);
                    let size = (width.max(1), height.max(1));
                    if size != self.size {
                        let rect = RECT {
                            left: 0,
                            top: 0,
                            right: size.0,
                            bottom: size.1,
                        };
                        self.engine.UpdateVideoStream(None, Some(&rect), None)?;
                        self.size = size;
                    }
                    Ok(())
                }
            }
        }
    }

    /// Reports what the engine did since the last call.
    fn poll(&mut self) {
        // SAFETY: plain calls on the live engine.
        unsafe {
            while let Ok((event, _, _)) = self.events.try_recv() {
                let time = self.engine.GetCurrentTime();
                if let Some(state) = EngineState::of(event) {
                    (self.sink)(PlayerEvent::Engine { state, time });
                }
                if event == MF_MEDIA_ENGINE_EVENT_ERROR.0 as u32 {
                    let why = self
                        .engine
                        .GetError()
                        .map(|e| {
                            format!(
                                "media engine error {} ({:#010x})",
                                e.GetErrorCode(),
                                e.GetExtendedErrorCode()
                                    .err()
                                    .map_or(0, |x| x.code().0 as u32)
                            )
                        })
                        .unwrap_or_else(|e| error(&e));
                    (self.sink)(PlayerEvent::Error(why));
                }
            }
            if !self.surface {
                if let Ok(handle) = self.engine.GetVideoSwapchainHandle() {
                    if !handle.is_invalid() && !handle.0.is_null() {
                        self.surface = true;
                        match duplicate(handle) {
                            Ok(h) => (self.sink)(PlayerEvent::Surface(h)),
                            Err(e) => (self.sink)(PlayerEvent::Error(error(&e))),
                        }
                    }
                }
            }
            if self.surface && self.progress.elapsed() >= PROGRESS {
                self.progress = std::time::Instant::now();
                let stat = |id| {
                    self.engine
                        .GetStatistics(id)
                        .map(|v| v.Anonymous.Anonymous.Anonymous.ulVal)
                        .unwrap_or(0)
                };
                (self.sink)(PlayerEvent::Progress {
                    time: self.engine.GetCurrentTime(),
                    rendered: stat(MF_MEDIA_ENGINE_STATISTIC_FRAMES_RENDERED),
                    dropped: stat(MF_MEDIA_ENGINE_STATISTIC_FRAMES_DROPPED),
                });
            }
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: a plain call on the live engine; it releases the source and its streams.
        let _ = unsafe { self.engine.Shutdown() };
    }
}

/// This process's own handle to what `handle` names.
fn duplicate(handle: HANDLE) -> windows::core::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut out = HANDLE::default();
    // SAFETY: duplicating a live handle within this process into an out parameter, which then
    // becomes an OwnedHandle (closed with it).
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut out,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )?;
        Ok(std::os::windows::io::OwnedHandle::from_raw_handle(out.0))
    }
}

/// Handles the messages of this thread's windows (the players' protection windows).
fn pump() {
    use windows::Win32::UI::WindowsAndMessaging::*;
    let mut msg = MSG::default();
    // SAFETY: the standard message loop of this thread, with a live MSG.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = DispatchMessageW(&msg);
        }
    }
}

/// How long the service's thread waits for a job while players play (their events, surface and
/// progress are looked at in between).
const TICK: std::time::Duration = std::time::Duration::from_millis(15);

fn run(rx: Receiver<Job>, tx: Sender<Job>) {
    super::ensure_com();
    let mut modules: HashMap<u64, Module> = HashMap::new();
    let mut sessions: HashMap<u64, Session> = HashMap::new();
    let mut players: HashMap<u64, Player> = HashMap::new();
    let tx = Arc::new(Mutex::new(tx));
    loop {
        let job = if players.is_empty() {
            match rx.recv() {
                Ok(job) => job,
                Err(_) => break,
            }
        } else {
            pump();
            for p in players.values_mut() {
                p.poll();
            }
            match rx.recv_timeout(TICK) {
                Ok(job) => job,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        };
        match job {
            Job::Play {
                player,
                keys,
                tracks,
                sink,
            } => {
                let Some(m) = modules.get(&keys) else {
                    sink(PlayerEvent::Error("no such module".into()));
                    continue;
                };
                match Player::new(&m.cdm, keys, &tracks, sink.clone()) {
                    Ok(p) => {
                        players.insert(player, p);
                    }
                    Err(e) => sink(PlayerEvent::Error(error(&e))),
                }
            }
            Job::Sample {
                player,
                track,
                sample,
            } => {
                if let Some(p) = players.get(&player) {
                    if let Some(i) = p.streams.iter().position(|&s| s == track) {
                        p.feed.push(i, sample);
                    }
                }
            }
            Job::Control { player, control } => {
                if let Some(p) = players.get_mut(&player) {
                    if let Err(e) = p.control(control) {
                        (p.sink)(PlayerEvent::Error(error(&e)));
                    }
                }
            }
            Job::End { player } => {
                players.remove(&player);
            }
            Job::Create {
                request,
                keys,
                key_system,
                origin_id,
                store,
                sink,
            } => {
                let hardware = key_system == playready::RECOMMENDATION_3000;
                let result = playready::create(&key_system, hardware, &origin_id, &store);
                match result {
                    Ok(cdm) => {
                        sink(Event::Created {
                            request,
                            result: Ok(()),
                        });
                        modules.insert(keys, Module { cdm, sink });
                    }
                    Err(e) => sink(Event::Created {
                        request,
                        result: Err(error(&e)),
                    }),
                }
            }
            Job::Generate {
                request,
                keys,
                session,
                init_data_type,
                init_data,
            } => {
                let Some(m) = modules.get(&keys) else {
                    continue;
                };
                let sink = m.sink.clone();
                let notify = {
                    let sink = sink.clone();
                    let tx = tx.clone();
                    Box::new(move |e| match e {
                        SessionEvent::Message(kind, data) => sink(Event::Message {
                            session,
                            kind,
                            data,
                        }),
                        SessionEvent::KeysChanged => {
                            let _ = tx
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .send(Job::Changed { session });
                        }
                    })
                };
                let made = playready::session_with(&m.cdm, notify).and_then(|s| {
                    // SAFETY: a plain call with live data.
                    unsafe { s.GenerateRequest(&HSTRING::from(init_data_type), &init_data) }
                        .map(|()| s)
                });
                let result = match made {
                    Ok(s) => {
                        let id = session_id(&s).unwrap_or_default();
                        sessions.insert(session, Session { keys, session: s });
                        Ok(id)
                    }
                    Err(e) => Err(error(&e)),
                };
                sink(Event::Done { request, result });
            }
            Job::Update {
                request,
                session,
                response,
            } => {
                let Some(s) = sessions.get(&session) else {
                    continue;
                };
                // SAFETY: a plain call with live data.
                let result = unsafe { s.session.Update(&response) }
                    .map(|()| String::new())
                    .map_err(|e| error(&e));
                if let Some(m) = modules.get(&s.keys) {
                    (m.sink)(Event::Done { request, result });
                }
            }
            Job::Close { request, session } => {
                let Some(s) = sessions.remove(&session) else {
                    continue;
                };
                // SAFETY: a plain call on the live session.
                let result = unsafe { s.session.Close() }
                    .map(|()| String::new())
                    .map_err(|e| error(&e));
                if let Some(m) = modules.get(&s.keys) {
                    (m.sink)(Event::Closed { session });
                    (m.sink)(Event::Done { request, result });
                }
            }
            Job::Remove { request, session } => {
                let Some(s) = sessions.get(&session) else {
                    continue;
                };
                // SAFETY: a plain call on the live session.
                let result = unsafe { s.session.Remove() }
                    .map(|()| String::new())
                    .map_err(|e| error(&e));
                if let Some(m) = modules.get(&s.keys) {
                    (m.sink)(Event::Done { request, result });
                }
            }
            Job::Release { keys } => {
                players.retain(|_, p| p.keys != keys);
                sessions.retain(|_, s| {
                    if s.keys != keys {
                        return true;
                    }
                    // SAFETY: a plain call on the live session.
                    let _ = unsafe { s.session.Close() };
                    false
                });
                modules.remove(&keys);
            }
            Job::Stop => break,
            Job::Changed { session } => {
                let Some(s) = sessions.get(&session) else {
                    continue;
                };
                let Some(m) = modules.get(&s.keys) else {
                    continue;
                };
                if let Ok(statuses) = playready::key_statuses(&s.session) {
                    (m.sink)(Event::Statuses { session, statuses });
                }
                // SAFETY: a plain call on the live session.
                if let Ok(time) = unsafe { s.session.GetExpiration() } {
                    (m.sink)(Event::Expiration { session, time });
                }
            }
        }
    }
    players.clear();
    for (_, s) in sessions {
        // SAFETY: a plain call on the live session.
        let _ = unsafe { s.session.Close() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Microsoft's encrypted test file (`SNOWLARK_PLAYREADY_MP4`: a local copy of the file
    /// `playready`'s tests name) licensed and played through the service as a page's messages
    /// drive it: the module and session by the caller's ids, the licence from Microsoft's test
    /// server, then a player fed the tracks and samples a `SourceBuffer`'s reader gives. Its
    /// picture is shown in a window off every monitor that never takes the foreground; protected
    /// frames cannot be read back, so the engine's own counts are the check (network and hardware
    /// DRM; run by hand).
    #[test]
    #[ignore]
    fn protected_playback_through_the_service() {
        use crate::fmp4::{Output, SampleReader};
        use playready::tests::{challenge_of, curl, pssh_of_test_file, TEST_SERVER};
        use windows::Win32::Graphics::DirectComposition::*;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let Some(path) = std::env::var_os("SNOWLARK_PLAYREADY_MP4") else {
            eprintln!("SNOWLARK_PLAYREADY_MP4 is not set: skipped");
            return;
        };
        let service = Service::start().unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |e| {
            let _ = tx.lock().unwrap().send(e);
        });
        let next = || {
            rx.recv_timeout(Duration::from_secs(30))
                .expect("a module event")
        };
        let store = std::env::temp_dir().join("snowlark-playready-test");
        service.create(
            1,
            10,
            playready::RECOMMENDATION_3000,
            "test-origin",
            store,
            sink,
        );
        let created = Event::Created {
            request: 1,
            result: Ok(()),
        };
        assert_eq!(next(), created);
        service.generate_request(2, 10, 20, "cenc", pssh_of_test_file());
        let (mut message, mut done) = (None, false);
        while message.is_none() || !done {
            match next() {
                Event::Message { data, .. } => message = Some(data),
                Event::Done { request: 2, result } => done = result.is_ok(),
                e => eprintln!("{e:?}"),
            }
        }
        let dir = std::env::temp_dir().join("snowlark-playready-service-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("challenge.xml"), challenge_of(&message.unwrap())).unwrap();
        let body = format!("@{}", dir.join("challenge.xml").display());
        let licence = curl(&[
            "-X",
            "POST",
            "-H",
            "Content-Type: text/xml; charset=utf-8",
            "-H",
            "SOAPAction: \"http://schemas.microsoft.com/DRM/2007/03/protocols/AcquireLicense\"",
            "--data-binary",
            &body,
            TEST_SERVER,
        ]);
        service.update(3, 20, licence);
        loop {
            if let Event::Statuses { statuses, .. } = next() {
                if statuses.iter().any(|s| s.1 == "usable") {
                    break;
                }
            }
        }
        let bytes = std::fs::read(path).unwrap();
        let mut reader = SampleReader::new();
        let mut out = Vec::new();
        for piece in bytes.chunks(1 << 20) {
            out.extend(reader.push(piece));
        }
        let tracks = reader.tracks().to_vec();
        let (ptx, prx) = mpsc::channel();
        let ptx = Mutex::new(ptx);
        service.play(
            30,
            10,
            tracks.clone(),
            Arc::new(move |e| {
                let _ = ptx.lock().unwrap().send(e);
            }),
        );
        let samples: Vec<(u32, crate::fmp4::Sample)> = out
            .into_iter()
            .filter_map(|o| match o {
                Output::Sample(id, sample) => Some((id, sample)),
                _ => None,
            })
            .collect();
        let feed = |from: i64| {
            // (each track from its last key frame at or before `from`, as a page refeeds)
            for t in &tracks {
                let mine: Vec<&crate::fmp4::Sample> = samples
                    .iter()
                    .filter(|(id, _)| *id == t.id)
                    .map(|(_, s)| s)
                    .collect();
                let start = mine
                    .iter()
                    .rposition(|s| s.key && s.time <= from)
                    .unwrap_or(0);
                for s in &mine[start..] {
                    service.sample(30, t.id, (*s).clone());
                }
                service.control(30, Control::End(t.id));
            }
        };
        feed(0);
        let (w, h) = (960, 540);
        service.control(30, Control::Muted(true));
        service.control(
            30,
            Control::Output {
                x: -4000,
                y: -4000,
                width: w,
                height: h,
            },
        );
        service.control(30, Control::Play);
        // SAFETY: test code: a plain window, DirectComposition calls and a message loop with
        // live handles.
        unsafe {
            unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
                // SAFETY: the default handling of a message for our own window.
                unsafe { DefWindowProcW(h, m, w, l) }
            }
            let class = windows::core::w!("SnowlarkPlayReadyServiceTest");
            let instance = GetModuleHandleW(None).unwrap();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc_),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
                class,
                windows::core::w!("playready"),
                WS_POPUP,
                -4000,
                -4000,
                w,
                h,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(None).unwrap();
            let target = dcomp.CreateTargetForHwnd(hwnd, true).unwrap();
            let visual = dcomp.CreateVisual().unwrap();
            target.SetRoot(&visual).unwrap();
            let start = Instant::now();
            let mut surface_at = None;
            let mut last = (0.0, 0, 0);
            // A seek back after 5 s: the player is flushed, sent to 1 s, and handed the samples
            // again from the key frame before it.
            let (mut sought, mut seeked, mut after) = (false, false, (0.0, 0));
            while start.elapsed() < Duration::from_secs(14) {
                if !sought && start.elapsed() > Duration::from_secs(5) {
                    sought = true;
                    after = (last.0, last.1);
                    service.control(30, Control::Flush);
                    service.control(30, Control::Seek(1.0));
                    feed(10_000_000);
                    service.control(30, Control::Play);
                }
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = DispatchMessageW(&msg);
                }
                while let Ok(e) = prx.try_recv() {
                    match e {
                        PlayerEvent::Surface(handle) => {
                            use std::os::windows::io::AsRawHandle;
                            surface_at = Some(start.elapsed());
                            let surface = dcomp
                                .CreateSurfaceFromHandle(HANDLE(handle.as_raw_handle()))
                                .unwrap();
                            visual.SetContent(&surface).unwrap();
                            dcomp.Commit().unwrap();
                        }
                        PlayerEvent::Progress {
                            time,
                            rendered,
                            dropped,
                        } => last = (time, rendered, dropped),
                        PlayerEvent::Error(why) => panic!("{why}"),
                        PlayerEvent::Engine {
                            state: EngineState::Seeked,
                            time,
                        } => {
                            eprintln!("seeked at {time:.2}");
                            seeked = sought;
                        }
                        PlayerEvent::Engine { .. } => {}
                    }
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            service.stop(30);
            service.release(10);
            let _ = DestroyWindow(hwnd);
            eprintln!(
                "surface at {surface_at:?}; time {:.2} frames rendered {} dropped {}; before the \
                 seek {:.2} and {} frames",
                last.0, last.1, last.2, after.0, after.1
            );
            assert!(surface_at.is_some(), "no composition surface");
            assert!(last.1 > 100, "{} frames", last.1);
            assert!(seeked, "no seeked event");
            // (playing on from 1 s after the seek, not from where it was)
            assert!(
                after.0 > 3.5 && last.0 < after.0 + 7.0,
                "{after:?} {last:?}"
            );
            assert!(
                last.1 > after.1 + 60,
                "{} frames after the seek",
                last.1 - after.1
            );
        }
    }
}
