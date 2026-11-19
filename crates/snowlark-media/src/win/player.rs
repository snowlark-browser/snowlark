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

use std::sync::atomic::{AtomicBool, Ordering};
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
    Seek(f64),
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
        let thread = {
            let slot = slot.clone();
            let videos = options.videos.clone();
            let closed = closed.clone();
            std::thread::Builder::new()
                .name("snowlark-media-player".into())
                .spawn(move || {
                    super::ensure_com();
                    run(options, source, shared, rx, slot, videos, closed);
                })
                .ok()
        };
        WinPlayer {
            tx: Mutex::new(tx),
            thread: Mutex::new(thread),
            slot,
            videos: store,
            closed,
        }
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
        self.send(Cmd::Seek(t));
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

fn run(
    options: PlayerOptions,
    source: Source,
    shared: Arc<Shared>,
    rx: Receiver<Cmd>,
    slot: Option<Arc<VideoSlot>>,
    videos: Option<Arc<VideoStore>>,
    closed: Arc<AtomicBool>,
) {
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
    // The copy of frames, and when to copy the current frame again after a seek (a paused video
    // shows the frame at the new position; Media Foundation does not always announce it).
    let mut frame_copy: Option<Arc<FrameCopy>> = None;
    let refresh: Arc<Mutex<Vec<Instant>>> = Arc::default();
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
            let first = Arc::new(AtomicBool::new(true));
            player.VideoFrameAvailable(&TypedEventHandler::new(
                move |p: windows::core::Ref<MediaPlayer>, _| {
                    if let Some(p) = p.as_ref() {
                        copy.copy(p);
                        if first.swap(false, Ordering::AcqRel) {
                            shared.push(PlayerEvent::Frame);
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
            let shared = shared.clone();
            let refresh = refresh.clone();
            session.SeekCompleted(&TypedEventHandler::new(
                move |s: windows::core::Ref<MediaPlaybackSession>, _| {
                    let now = Instant::now();
                    refresh.lock().unwrap().extend([
                        now + Duration::from_millis(60),
                        now + Duration::from_millis(250),
                    ]);
                    if let Some(s) = s.as_ref() {
                        let mut st = shared.status.lock().unwrap();
                        st.position = s.Position().map(seconds).unwrap_or(st.position);
                        st.sampled = Instant::now();
                    }
                    shared.push(PlayerEvent::Seeked);
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
            Some(Cmd::Seek(t)) => {
                if let Some(s) = &session {
                    let _ = s.SetPosition(timespan(t));
                    let mut st = shared.status.lock().unwrap();
                    st.position = t;
                    st.sampled = Instant::now();
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
            let due = {
                let mut r = refresh.lock().unwrap();
                let now = Instant::now();
                let n = r.len();
                r.retain(|t| *t > now);
                r.len() != n
            };
            if due {
                if let Some(c) = &frame_copy {
                    c.copy(&player);
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
