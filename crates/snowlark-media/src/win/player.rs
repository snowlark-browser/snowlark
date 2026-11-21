// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A `MediaPlayer` per media element, driven by a thread of its own: commands go to the thread in
//! order (opening waits there for the resource's first response, never on the page's thread),
//! Media Foundation's events come back on its threads and become [`PlayerEvent`]s, and the
//! thread samples the playback position and buffered ranges while playing.
//!
//! In a browser tab the thread also keeps the player's place in its tab's
//! [`crate::AudioGroup`]: whether it is audible (sampled each turn with the position) and the tab's
//! mute, applied to Media Foundation's own mute without touching the element's.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::DirectX::Direct3D11::IDirect3DSurface;
use windows::Media::Core::MediaSource;
use windows::Media::Playback::{
    MediaPlaybackItem, MediaPlaybackSession, MediaPlaybackState, MediaPlayer, MediaPlayerError,
    MediaPlayerFailedEventArgs,
};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11SurfaceFromDXGISurface;

use snowlark_paint::video::{VideoSlot, VideoStore};
use snowlark_paint::VideoId;

use crate::audio::{output_muted, Audible, PlayerAudio};
use crate::resource::Head;
use crate::{ErrorCode, PlayerEvent, PlayerOptions, Ranges, Shared, Source};

enum Cmd {
    Play,
    Pause,
    /// A seek: the time and the seek's number.
    Seek(f64, u64),
    /// Media Foundation finished a seek, and the one that waited starts: its time.
    SeekNext(f64),
    Rate(f64),
    Volume(f64, bool),
    Close,
}

/// The Windows side of a [`crate::Player`].
pub struct WinPlayer {
    tx: Mutex<Sender<Cmd>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    slot: Option<Arc<VideoSlot>>,
    videos: Option<Arc<VideoStore>>,
    closed: Arc<AtomicBool>,
    /// The number of the newest seek asked for.
    seeks_asked: AtomicU64,
    /// The number of the newest seek reported done.
    seeked: Arc<AtomicU64>,
}

/// How long a finished seek waits for Media Foundation to announce the frame at its target
/// before the player copies the current frame itself (it does not always announce it for a
/// paused video).
const SETTLE: Duration = Duration::from_millis(60);

/// How long after a seek reported done the frame is copied once more (the frame Media Foundation
/// announced, or held at [`SETTLE`], may not have been the one at the target yet).
const RECOPY: Duration = Duration::from_millis(190);

/// How long a seek may run before the player stops waiting for Media Foundation to finish it.
const SEEK_GIVE_UP: Duration = Duration::from_secs(3);

/// The seeks of a player (HTML §4.8.11.9 "seeking"). One runs at a time; one asked for while it
/// runs waits, replaced by any newer one, so the element never shows a frame of a seek it gave
/// up. A seek is done once the frame at its target is copied: until then the element shows the
/// frame it had, and frames Media Foundation decodes on the way (from the key frame before the
/// target, or at a target given up) are not copied.
#[derive(Default)]
struct Seeks {
    /// The seek Media Foundation runs: its number and when it started.
    running: Option<(u64, Instant)>,
    /// The newest seek asked for while one ran: its time and number.
    pending: Option<(f64, u64)>,
    /// A seek Media Foundation finished that waits for the frame at its target: its number and
    /// when it finished.
    settling: Option<(u64, Instant)>,
    /// When to copy the frame once more after the last seek (none once another starts).
    recopy: Option<Instant>,
}

impl Seeks {
    /// Whether a seek runs or waits (a frame now is not one at the target).
    fn busy(&self) -> bool {
        self.running.is_some() || self.pending.is_some()
    }

    /// The seek that ran is finished: the next one runs (its time is returned, for the player's
    /// thread to start), or the frame at its target is awaited, or (a player without video) it
    /// is done (its number is returned).
    fn finished(&mut self, video: bool) -> Finished {
        let Some((n, _)) = self.running.take() else {
            // (a finish that came after the player gave up on the seek)
            return Finished::Nothing;
        };
        match self.pending.take() {
            Some((t, m)) => {
                self.running = Some((m, Instant::now()));
                Finished::Next(t)
            }
            None if video => {
                self.settling = Some((n, Instant::now()));
                Finished::Settling
            }
            None => Finished::Done(n),
        }
    }
}

enum Finished {
    Nothing,
    Next(f64),
    Settling,
    Done(u64),
}

/// Reports seek `n` done.
fn report_seeked(seeked: &AtomicU64, shared: &Shared, n: u64) {
    seeked.store(n, Ordering::Release);
    shared.push(PlayerEvent::Seeked);
}

fn seconds(t: windows::Foundation::TimeSpan) -> f64 {
    t.Duration as f64 / 1e7
}

fn timespan(s: f64) -> windows::Foundation::TimeSpan {
    windows::Foundation::TimeSpan {
        Duration: (s.max(0.0) * 1e7) as i64,
    }
}

fn duration_of(session: &MediaPlaybackSession) -> f64 {
    match session.NaturalDuration() {
        Ok(d) if d.Duration == i64::MAX => f64::INFINITY,
        Ok(d) if d.Duration > 0 => seconds(d),
        _ => f64::NAN,
    }
}

fn ranges_of(
    v: windows::core::Result<windows_collections::IVectorView<windows::Media::MediaTimeRange>>,
) -> Ranges {
    let mut r = Ranges::new();
    if let Ok(v) = v {
        for range in v {
            r.add(seconds(range.Start), seconds(range.End));
        }
    }
    r
}

/// What the frame copy needs on Media Foundation's threads.
struct FrameCopy {
    slot: Arc<VideoSlot>,
    videos: Arc<VideoStore>,
    copying: AtomicBool,
    /// A frame came while copying.
    again: AtomicBool,
    shared: Arc<Shared>,
}

impl FrameCopy {
    /// Copies the player's current frame. A frame that comes while a copy runs is not dropped:
    /// the copy runs again for it once the first is done (a paused video's seek gives one frame,
    /// and nothing else would show it).
    fn copy(&self, player: &MediaPlayer) -> bool {
        self.again.store(true, Ordering::Release);
        if self.copying.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut ok;
        loop {
            self.again.store(false, Ordering::Release);
            ok = self.copy_once(player);
            self.copying.store(false, Ordering::Release);
            if !self.again.load(Ordering::Acquire) || self.copying.swap(true, Ordering::AcqRel) {
                break;
            }
        }
        ok
    }

    fn copy_once(&self, player: &MediaPlayer) -> bool {
        let r = (|| -> windows::core::Result<bool> {
            let gpu = self.videos.gpu().ok_or(windows::core::Error::empty())?;
            let session = player.PlaybackSession()?;
            let (w, h) = (session.NaturalVideoWidth()?, session.NaturalVideoHeight()?);
            if w == 0 || h == 0 {
                return Ok(false);
            }
            let target = self.slot.begin_frame(gpu, w, h)?;
            // SAFETY: wraps the slot's texture surface (a resource of the shared device) as the WinRT
            // surface CopyFrameToVideoSurface takes; the wrapper holds its own reference.
            let surface: IDirect3DSurface =
                unsafe { CreateDirect3D11SurfaceFromDXGISurface(&target.surface)? }.cast()?;
            {
                // (the copy may use the shared device's immediate context, as Direct2D does)
                let _lock = gpu.lock();
                player.CopyFrameToVideoSurface(&surface)?;
            }
            Ok(self.slot.end_frame(target.index))
        })();
        match r {
            Ok(shown) => {
                if shown {
                    self.videos.frame_ready(self.slot.id());
                    // (the page shows the frame instead of the poster from the first one on)
                    if self.slot.frame_count() == 1 {
                        self.shared.push(PlayerEvent::Frame);
                    }
                }
                true
            }
            Err(e) => {
                if std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1") {
                    eprintln!("[media] frame copy failed: {e}");
                }
                false
            }
        }
    }
}

impl WinPlayer {
    pub(crate) fn open(options: PlayerOptions, source: Source, shared: Arc<Shared>) -> WinPlayer {
        let slot = options.videos.as_ref().map(|v| v.create());
        let store = options.videos.clone();
        let (tx, rx) = channel();
        let closed = Arc::new(AtomicBool::new(false));
        let seeked = Arc::new(AtomicU64::new(0));
        let thread = {
            let slot = slot.clone();
            let videos = options.videos.clone();
            let closed = closed.clone();
            let ends = Ends {
                rx,
                tx: tx.clone(),
                seeked: seeked.clone(),
            };
            std::thread::Builder::new()
                .name("snowlark-media-player".into())
                .spawn(move || {
                    super::ensure_com();
                    run(options, source, shared, ends, slot, videos, closed);
                })
                .ok()
        };
        WinPlayer {
            tx: Mutex::new(tx),
            thread: Mutex::new(thread),
            slot,
            videos: store,
            closed,
            seeks_asked: AtomicU64::new(0),
            seeked,
        }
    }

    /// Drops a `Seeked` that belongs to a seek given up since (the newest seek is not done yet:
    /// it was asked for after the player reported an earlier one), and keeps at most one.
    pub(crate) fn filter_events(&self, events: &mut Vec<PlayerEvent>) {
        let Some(last) = events.iter().rposition(|e| *e == PlayerEvent::Seeked) else {
            return;
        };
        let current =
            self.seeked.load(Ordering::Acquire) >= self.seeks_asked.load(Ordering::Relaxed);
        let mut i = 0;
        events.retain(|e| {
            let keep = *e != PlayerEvent::Seeked || (i == last && current);
            i += 1;
            keep
        });
    }

    fn send(&self, c: Cmd) {
        let _ = self.tx.lock().unwrap().send(c);
    }

    pub fn play(&self) {
        self.send(Cmd::Play);
    }

    pub fn pause(&self) {
        self.send(Cmd::Pause);
    }

    pub fn seek(&self, t: f64) {
        let n = self.seeks_asked.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(Cmd::Seek(t, n));
    }

    pub fn set_rate(&self, r: f64) {
        self.send(Cmd::Rate(r));
    }

    pub fn set_volume(&self, v: f64, muted: bool) {
        self.send(Cmd::Volume(v, muted));
    }

    pub fn video(&self) -> Option<VideoId> {
        self.slot.as_ref().map(|s| s.id())
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.send(Cmd::Close);
        // (not joined: closing Media Foundation's pipeline can take a moment, and the page never
        // waits for it)
        drop(self.thread.lock().unwrap().take());
        if let (Some(v), Some(s)) = (&self.videos, &self.slot) {
            v.remove(s.id());
        }
    }
}

/// Makes the `MediaSource` for a source (waiting for a resource's first response).
fn media_source(source: Source) -> Result<MediaSource, (ErrorCode, String)> {
    match source {
        Source::Resource { resource, hint } => {
            let head = resource.wait_head(Duration::from_secs(60));
            let content_type = match head {
                Head::Ok { content_type, .. } => content_type,
                Head::Failed { message, network } => {
                    return Err((
                        if network {
                            ErrorCode::Network
                        } else {
                            ErrorCode::SrcNotSupported
                        },
                        message,
                    ))
                }
                Head::Pending => {
                    return Err((
                        ErrorCode::Network,
                        "the media resource did not respond".into(),
                    ))
                }
            };
            let mut first = vec![0u8; 512];
            let n = resource.read_at(0, &mut first).unwrap_or(0);
            first.truncate(n);
            let Some(t) = crate::sniff::effective_type(&content_type, hint.as_deref(), &first)
            else {
                return Err((
                    ErrorCode::SrcNotSupported,
                    format!("unsupported media type \"{content_type}\""),
                ));
            };
            if crate::support().can_play_type(&t) == crate::CanPlay::No {
                return Err((
                    ErrorCode::SrcNotSupported,
                    format!("no decoder for \"{t}\""),
                ));
            }
            let stream = super::bytestream::ByteStream::create(resource)
                .map_err(|e| (ErrorCode::SrcNotSupported, e.to_string()))?;
            MediaSource::CreateFromStream(&stream, &HSTRING::from(t))
                .map_err(|e| (ErrorCode::SrcNotSupported, e.to_string()))
        }
        Source::Mse(m) => m
            .media_source()
            .map_err(|e| (ErrorCode::SrcNotSupported, e.to_string())),
    }
}

/// The player thread's ends of its channel, and where it reports the seek done last.
struct Ends {
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    seeked: Arc<AtomicU64>,
}

fn run(
    options: PlayerOptions,
    source: Source,
    shared: Arc<Shared>,
    ends: Ends,
    slot: Option<Arc<VideoSlot>>,
    videos: Option<Arc<VideoStore>>,
    closed: Arc<AtomicBool>,
) {
    let Ends { rx, tx, seeked } = ends;
    let trace = std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1");
    let player = match MediaPlayer::new() {
        Ok(p) => p,
        Err(e) => {
            shared.push(PlayerEvent::Failed {
                code: ErrorCode::Decode,
                message: format!("Media Foundation: {e}"),
            });
            return;
        }
    };
    // The copy of frames; the seeks; and when to copy the frame once more after a seek that
    // ended without an announced frame.
    let mut frame_copy: Option<Arc<FrameCopy>> = None;
    let seeks: Arc<Mutex<Seeks>> = Arc::default();
    // The element's volume and mute, whether it plays and has sound; and the output's mute (the
    // element's or the tab's).
    let mut audio = PlayerAudio::new(options.audio.clone());
    let mut sound = Audible::new(options.volume, options.muted);
    let mut out_muted = output_muted(options.muted, audio.tab_muted());
    let setup = (|| -> windows::core::Result<()> {
        player.SetAutoPlay(false)?;
        player.SetIsVideoFrameServerEnabled(true)?;
        player.SetVolume(options.volume.clamp(0.0, 1.0))?;
        player.SetIsMuted(out_muted)?;
        let session = player.PlaybackSession()?;
        // Opened: the metadata.
        {
            let shared = shared.clone();
            player.MediaOpened(&TypedEventHandler::new(
                move |p: windows::core::Ref<MediaPlayer>, _| {
                    if let Some(p) = p.as_ref() {
                        let s = p.PlaybackSession()?;
                        let duration = duration_of(&s);
                        {
                            let mut st = shared.status.lock().unwrap();
                            st.duration = duration;
                            st.seekable = if duration.is_finite() {
                                Ranges::single(0.0, duration)
                            } else {
                                ranges_of(s.GetSeekableRanges())
                            };
                        }
                        shared.push(PlayerEvent::Opened {
                            duration,
                            width: s.NaturalVideoWidth().unwrap_or(0),
                            height: s.NaturalVideoHeight().unwrap_or(0),
                        });
                    }
                    Ok(())
                },
            ))?;
        }
        {
            let shared = shared.clone();
            player.MediaEnded(&TypedEventHandler::new(move |_, _| {
                shared.status.lock().unwrap().playing = false;
                shared.push(PlayerEvent::Ended);
                Ok(())
            }))?;
        }
        {
            let shared = shared.clone();
            player.MediaFailed(&TypedEventHandler::new(
                move |_, a: windows::core::Ref<MediaPlayerFailedEventArgs>| {
                    let (code, message) = match a.as_ref() {
                        Some(a) => {
                            let code = match a.Error().unwrap_or(MediaPlayerError::Unknown) {
                                MediaPlayerError::Aborted => ErrorCode::Aborted,
                                MediaPlayerError::NetworkError => ErrorCode::Network,
                                MediaPlayerError::SourceNotSupported => ErrorCode::SrcNotSupported,
                                _ => ErrorCode::Decode,
                            };
                            let hr = a.ExtendedErrorCode().map(|h| h.0).unwrap_or(0);
                            (
                                code,
                                format!("{} (0x{hr:08X})", a.ErrorMessage().unwrap_or_default()),
                            )
                        }
                        None => (ErrorCode::Decode, String::new()),
                    };
                    shared.status.lock().unwrap().playing = false;
                    shared.push(PlayerEvent::Failed { code, message });
                    Ok(())
                },
            ))?;
        }
        if let (Some(slot), Some(videos)) = (&slot, &videos) {
            let copy = Arc::new(FrameCopy {
                slot: slot.clone(),
                videos: videos.clone(),
                copying: AtomicBool::new(false),
                again: AtomicBool::new(false),
                shared: shared.clone(),
            });
            frame_copy = Some(copy.clone());
            let shared = shared.clone();
            let seeks = seeks.clone();
            let seeked = seeked.clone();
            let first = Arc::new(AtomicBool::new(true));
            player.VideoFrameAvailable(&TypedEventHandler::new(
                move |p: windows::core::Ref<MediaPlayer>, _| {
                    if let Some(p) = p.as_ref() {
                        // (held over the copy: a seek that starts meanwhile waits for it, so no
                        // frame on a seek's way is copied)
                        let mut s = seeks.lock().unwrap();
                        if s.busy() {
                            return Ok(());
                        }
                        copy.copy(p);
                        let done = s.settling.take();
                        if done.is_some() {
                            s.recopy = Some(Instant::now() + RECOPY);
                        }
                        drop(s);
                        if first.swap(false, Ordering::AcqRel) {
                            shared.push(PlayerEvent::Frame);
                        }
                        if let Some((n, at)) = done {
                            if trace {
                                eprintln!(
                                    "[media] seek {n} done with its frame after {:.1} ms",
                                    at.elapsed().as_secs_f64() * 1e3
                                );
                            }
                            report_seeked(&seeked, &shared, n);
                        }
                    }
                    Ok(())
                },
            ))?;
        }
        {
            let shared = shared.clone();
            session.PlaybackStateChanged(&TypedEventHandler::new(
                move |s: windows::core::Ref<MediaPlaybackSession>, _| {
                    if let Some(s) = s.as_ref() {
                        let state = s.PlaybackState()?;
                        let position = s.Position().map(seconds).unwrap_or(0.0);
                        if std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1") {
                            let buffering = s.BufferingProgress().unwrap_or(-1.0);
                            eprintln!(
                                "[media] player state {state:?} at {position:.3} (buffering progress {buffering:.2})"
                            );
                        }
                        {
                            let mut st = shared.status.lock().unwrap();
                            st.playing = state == MediaPlaybackState::Playing;
                            st.position = position;
                            st.sampled = Instant::now();
                        }
                        if state == MediaPlaybackState::Playing {
                            shared.push(PlayerEvent::Playing);
                        } else if state == MediaPlaybackState::Buffering {
                            shared.push(PlayerEvent::Waiting);
                        }
                    }
                    Ok(())
                },
            ))?;
        }
        {
            // (the player's thread starts the next seek; the frame that follows ends this one)
            let tx = Mutex::new(tx);
            let shared = shared.clone();
            let seeks = seeks.clone();
            let seeked = seeked.clone();
            let video = slot.is_some();
            session.SeekCompleted(&TypedEventHandler::new(
                move |s: windows::core::Ref<MediaPlaybackSession>, _| {
                    let finished = seeks.lock().unwrap().finished(video);
                    if let Finished::Next(t) = finished {
                        let _ = tx.lock().unwrap().send(Cmd::SeekNext(t));
                        return Ok(());
                    }
                    if let Some(s) = s.as_ref() {
                        let mut st = shared.status.lock().unwrap();
                        st.position = s.Position().map(seconds).unwrap_or(st.position);
                        st.sampled = Instant::now();
                    }
                    if let Finished::Done(n) = finished {
                        report_seeked(&seeked, &shared, n);
                    }
                    Ok(())
                },
            ))?;
        }
        {
            let shared = shared.clone();
            session.NaturalDurationChanged(&TypedEventHandler::new(
                move |s: windows::core::Ref<MediaPlaybackSession>, _| {
                    if let Some(s) = s.as_ref() {
                        let d = duration_of(s);
                        shared.status.lock().unwrap().duration = d;
                        shared.push(PlayerEvent::DurationChanged(d));
                    }
                    Ok(())
                },
            ))?;
        }
        {
            let shared = shared.clone();
            session.NaturalVideoSizeChanged(&TypedEventHandler::new(
                move |s: windows::core::Ref<MediaPlaybackSession>, _| {
                    if let Some(s) = s.as_ref() {
                        shared.push(PlayerEvent::SizeChanged(
                            s.NaturalVideoWidth().unwrap_or(0),
                            s.NaturalVideoHeight().unwrap_or(0),
                        ));
                    }
                    Ok(())
                },
            ))?;
        }
        Ok(())
    })();
    if let Err(e) = setup {
        shared.push(PlayerEvent::Failed {
            code: ErrorCode::Decode,
            message: format!("Media Foundation: {e}"),
        });
        return;
    }
    if options.rate != 1.0 {
        if let Ok(s) = player.PlaybackSession() {
            let _ = s.SetPlaybackRate(options.rate);
        }
    }
    // Commands that came while opening are kept for after it. In a browser tab the source plays
    // as a playback item, whose audio tracks say whether the media has sound at all.
    let mut item: Option<MediaPlaybackItem> = None;
    match media_source(source) {
        Ok(ms) => {
            let set = match audio
                .grouped()
                .then(|| MediaPlaybackItem::Create(&ms).ok())
                .flatten()
            {
                Some(i) => {
                    let r = player.SetSource(&i);
                    item = Some(i);
                    r
                }
                None => player.SetSource(&ms),
            };
            if let Err(e) = set {
                shared.push(PlayerEvent::Failed {
                    code: ErrorCode::SrcNotSupported,
                    message: e.to_string(),
                });
            }
        }
        Err((code, message)) => {
            shared.push(PlayerEvent::Failed { code, message });
        }
    }
    let mut last_ranges = Instant::now() - Duration::from_secs(1);
    let mut last_tracks = last_ranges;
    loop {
        let cmd = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if closed.load(Ordering::Acquire) && !matches!(cmd, Some(Cmd::Close)) {
            break;
        }
        let session = player.PlaybackSession().ok();
        match cmd {
            Some(Cmd::Close) => break,
            Some(Cmd::Play) => {
                let _ = player.Play();
            }
            Some(Cmd::Pause) => {
                let _ = player.Pause();
                if let Some(s) = &session {
                    let mut st = shared.status.lock().unwrap();
                    st.playing = false;
                    st.position = s.Position().map(seconds).unwrap_or(st.position);
                    st.sampled = Instant::now();
                }
            }
            Some(Cmd::Seek(t, n)) => {
                let start = {
                    let mut s = seeks.lock().unwrap();
                    s.settling = None;
                    s.recopy = None;
                    if s.running.is_some() {
                        s.pending = Some((t, n));
                        false
                    } else {
                        s.running = Some((n, Instant::now()));
                        true
                    }
                };
                if let Some(s) = &session {
                    if start {
                        let _ = s.SetPosition(timespan(t));
                    }
                    let mut st = shared.status.lock().unwrap();
                    st.position = t;
                    st.sampled = Instant::now();
                }
            }
            Some(Cmd::SeekNext(t)) => {
                if let Some(s) = &session {
                    let _ = s.SetPosition(timespan(t));
                }
            }
            Some(Cmd::Rate(r)) => {
                if let Some(s) = &session {
                    let _ = s.SetPlaybackRate(r);
                }
                shared.status.lock().unwrap().rate = r;
            }
            Some(Cmd::Volume(v, m)) => {
                sound.volume = v;
                sound.muted = m;
                out_muted = output_muted(m, audio.tab_muted());
                let _ = player.SetVolume(v.clamp(0.0, 1.0));
                let _ = player.SetIsMuted(out_muted);
            }
            None => {}
        }
        // A seek Media Foundation never said it finished is taken as finished.
        let given_up = {
            let mut s = seeks.lock().unwrap();
            let late = s
                .running
                .is_some_and(|(_, at)| at.elapsed() >= SEEK_GIVE_UP);
            late.then(|| s.finished(slot.is_some()))
        };
        if let Some(finished) = given_up {
            if trace {
                eprintln!("[media] a seek did not finish in {SEEK_GIVE_UP:?}");
            }
            match finished {
                Finished::Next(t) => {
                    if let Some(s) = &session {
                        let _ = s.SetPosition(timespan(t));
                    }
                }
                Finished::Done(n) => report_seeked(&seeked, &shared, n),
                Finished::Nothing | Finished::Settling => {}
            }
        }
        // Samples.
        if let Some(s) = &session {
            let playing = s.PlaybackState().ok() == Some(MediaPlaybackState::Playing);
            if audio.grouped() {
                // The tab's mute, and whether the player is audible (its tracks are read when it
                // starts playing and then now and then: MSE adds them as segments come).
                let muted = output_muted(sound.muted, audio.tab_muted());
                if muted != out_muted {
                    out_muted = muted;
                    let _ = player.SetIsMuted(muted);
                }
                sound.playing = playing;
                if playing
                    && (sound.has_audio.is_none()
                        || last_tracks.elapsed() >= Duration::from_millis(500))
                {
                    last_tracks = Instant::now();
                    if let Some(i) = &item {
                        sound.has_audio =
                            i.AudioTracks().and_then(|t| t.Size()).ok().map(|n| n > 0);
                    }
                }
                audio.update(&sound);
            }
            if playing {
                let mut st = shared.status.lock().unwrap();
                st.position = s.Position().map(seconds).unwrap_or(st.position);
                st.sampled = Instant::now();
                st.playing = true;
            }
            if last_ranges.elapsed() >= Duration::from_millis(250) {
                last_ranges = Instant::now();
                let buffered = ranges_of(s.GetBufferedRanges());
                let seekable = ranges_of(s.GetSeekableRanges());
                let mut st = shared.status.lock().unwrap();
                st.buffered = buffered;
                if !seekable.is_empty() {
                    st.seekable = seekable;
                }
            }
        }
        if let (Some(slot), Some(videos)) = (&slot, &videos) {
            // A finished seek whose frame was not announced in time: the current frame is copied
            // (it is the one at the target once the seek finished), and once more a little
            // later.
            if let Some(c) = &frame_copy {
                let mut s = seeks.lock().unwrap();
                let late = s.settling.filter(|(_, at)| at.elapsed() >= SETTLE);
                if let Some((n, _)) = late {
                    s.settling = None;
                    c.copy(&player);
                    s.recopy = Some(Instant::now() + RECOPY);
                    drop(s);
                    if trace {
                        eprintln!("[media] seek {n} done without an announced frame");
                    }
                    report_seeked(&seeked, &shared, n);
                } else if s.recopy.is_some_and(|t| t <= Instant::now()) {
                    s.recopy = None;
                    if !s.busy() && s.settling.is_none() {
                        c.copy(&player);
                    }
                }
            }
            if slot.promote_stale() {
                videos.frame_ready(slot.id());
                if slot.frame_count() == 1 {
                    shared.push(PlayerEvent::Frame);
                }
            }
        }
    }
    drop(audio);
    let _ = player.Pause();
    let _ = player.Close();
    if let (Some(slot), Some(videos)) = (&slot, &videos) {
        videos.remove(slot.id());
    }
}
