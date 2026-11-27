// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One media element's playback on Linux, on three threads of its own:
//!
//! - the **decoder** reads the container ([`ffmpeg::Demuxer`] over the fetched resource), decodes
//!   the first video and audio streams, converts pictures to BGRA and sound to 48 kHz stereo
//!   floats, and keeps a few frames and about a second of sound queued ahead;
//! - the **player** keeps the clock (wall time at the playback rate, from a position), shows each
//!   picture in the element's [`VideoSlot`] when the clock reaches it, and reports
//!   [`PlayerEvent`]s and the status;
//! - the **sound** thread writes the queued sound to the desktop's sound server ([`PulseSink`], else
//!   a null sink that keeps time) when the clock is at it, with the element's volume.
//!
//! The clock is the wall clock rather than the sound card's: the sound follows it, dropping what
//! is late and waiting for what is early. At a rate other than 1 the sound is silent.
//!
//! The decoder reads its packets from a [`Packets`] source: the container, or for a Media Source
//! the track buffers ([`super::mse_player`]), which can have nothing yet at the position (the
//! decoder then waits for appends, and the player for data, as it does at a slow network).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use snowlark_audio::realtime::{AudioSink, NullSink, StreamConfig};
use snowlark_paint::video::VideoSlot;
use snowlark_paint::VideoId;

use super::ffmpeg::{
    self, ByteSource, Decoded, Decoder, Demuxer, Frame, Kind, Packet, Resampler, Scaler,
};
use super::mse::MseSource;
use super::mse_player;
use super::pulse::{PulseOutputs, PulseSink};
use crate::audio::{output_muted, Audible, AudioGroup, PlayerAudio};
use crate::resource::Head;
use crate::{ErrorCode, MediaResource, PlayerEvent, PlayerOptions, Ranges, Shared, Source};

/// The sound's rate and channels, whatever the media's.
const RATE: u32 = 48000;
const CHANNELS: u32 = 2;
/// The pictures and seconds of sound the decoder keeps ahead.
const VIDEO_AHEAD: usize = 6;
const AUDIO_AHEAD: f64 = 1.0;
/// Frames per write to the sound server (10 ms).
const PERIOD: usize = 480;

enum Ctl {
    Play,
    Pause,
    Seek(f64),
    Rate(f64),
    Volume(f64, bool),
    Close,
}

/// A media element's player (see the module documentation).
pub struct LinuxPlayer {
    ctl: Sender<Ctl>,
    slot: Option<Arc<VideoSlot>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl LinuxPlayer {
    pub(crate) fn open(options: PlayerOptions, source: Source, shared: Arc<Shared>) -> LinuxPlayer {
        let slot = options.videos.as_ref().map(|v| v.create());
        let (ctl, rx) = mpsc::channel();
        let s = slot.clone();
        let thread = std::thread::Builder::new()
            .name("media player".into())
            .spawn(move || run(options, source, shared, s, rx))
            .ok();
        LinuxPlayer {
            ctl,
            slot,
            thread: Mutex::new(thread),
        }
    }

    pub fn play(&self) {
        let _ = self.ctl.send(Ctl::Play);
    }

    pub fn pause(&self) {
        let _ = self.ctl.send(Ctl::Pause);
    }

    pub fn seek(&self, t: f64) {
        let _ = self.ctl.send(Ctl::Seek(t));
    }

    pub fn set_rate(&self, r: f64) {
        let _ = self.ctl.send(Ctl::Rate(r));
    }

    pub fn set_volume(&self, v: f64, muted: bool) {
        let _ = self.ctl.send(Ctl::Volume(v, muted));
    }

    pub fn video(&self) -> Option<VideoId> {
        self.slot.as_ref().map(|s| s.id())
    }

    pub fn close(&self) {
        let _ = self.ctl.send(Ctl::Close);
        // (the thread is not joined here: closing must not wait on a read blocked on the network;
        // the resource is closed by the caller, which ends such a read)
        drop(self.thread.lock().unwrap().take());
    }
}

/// What a packet source has next.
pub(super) enum Next {
    Packet(Packet),
    /// Nothing at the position yet; more may come ([`Packets::wait`]).
    Pending,
    End,
}

/// Where the decoder's packets come from.
pub(super) trait Packets: Send {
    fn next(&mut self) -> Next;
    /// To the random access point at or before `t` seconds of the media timeline.
    fn seek(&mut self, t: f64);
    /// Waits a short while for more data, after [`Next::Pending`].
    fn wait(&mut self);
}

/// A container's packets.
struct Container {
    demuxer: Demuxer,
    start: f64,
}

impl Packets for Container {
    fn next(&mut self) -> Next {
        match self.demuxer.read() {
            Ok(Some(p)) => Next::Packet(p),
            // (a read error after some packets ends the stream, as a short file would)
            Ok(None) | Err(_) => Next::End,
        }
    }

    fn seek(&mut self, t: f64) {
        let _ = self.demuxer.seek(t + self.start);
    }

    fn wait(&mut self) {}
}

/// A stream the player decodes: its decoder, the index its packets carry and their time base,
/// and (video) its size from the metadata.
pub(super) struct Stream {
    pub decoder: Decoder,
    pub index: usize,
    pub time_base: ffmpeg::Rational,
    pub width: u32,
    pub height: u32,
}

/// The media opened: its packets, streams and duration (seconds; infinite when unknown).
struct Media {
    packets: Box<dyn Packets>,
    video: Option<Stream>,
    audio: Option<Stream>,
    duration: f64,
    /// When the first frame is due on the packets' timeline (the media timeline's zero).
    start: f64,
}

/// The media resource as the demuxer reads it.
struct ResourceBytes(MediaResource);

impl ByteSource for ResourceBytes {
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> Result<usize, String> {
        self.0.read_at(pos, buf)
    }

    fn length(&self) -> Option<u64> {
        self.0.length()
    }
}

/// A picture, due at `t` seconds of the media timeline.
struct Picture {
    t: f64,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Sound from `t` seconds: interleaved stereo at [`RATE`], `used` frames of it played.
struct Sound {
    t: f64,
    samples: Vec<f32>,
    used: usize,
}

impl Sound {
    fn frames(&self) -> usize {
        self.samples.len() / CHANNELS as usize
    }

    fn now(&self) -> f64 {
        self.t + self.used as f64 / RATE as f64
    }

    fn end(&self) -> f64 {
        self.t + self.frames() as f64 / RATE as f64
    }
}

/// What the decoder hands the player and the sound thread.
#[derive(Default)]
struct Queues {
    pictures: VecDeque<Picture>,
    sound: VecDeque<Sound>,
    /// Counts seeks; the decoder works for this one.
    generation: u64,
    /// A seek for the decoder to make (to a time, of a generation).
    seek: Option<(f64, u64)>,
    /// The decoder has reached the seek's time (or the end) for this generation.
    seeked: Option<u64>,
    /// The decoder reached the end of the container (for this generation).
    end: bool,
    /// The latest time queued.
    queued_to: f64,
    failed: Option<String>,
    closed: bool,
}

impl Queues {
    fn sound_ahead(&self) -> f64 {
        match (self.sound.front(), self.sound.back()) {
            (Some(a), Some(b)) => b.end() - a.now(),
            _ => 0.0,
        }
    }
}

type Shared2 = Arc<(Mutex<Queues>, Condvar)>;

/// The media clock: a position that moves at `rate` while running.
#[derive(Clone, Copy)]
struct Clock {
    at: f64,
    since: Instant,
    running: bool,
    rate: f64,
}

impl Clock {
    fn now(&self) -> f64 {
        if self.running {
            self.at + self.since.elapsed().as_secs_f64() * self.rate
        } else {
            self.at
        }
    }

    fn set(&mut self, at: f64, running: bool) {
        self.at = at;
        self.since = Instant::now();
        self.running = running;
    }
}

/// What the sound thread follows.
struct SoundState {
    clock: Clock,
    volume: f64,
    muted: bool,
    generation: u64,
}

fn run(
    options: PlayerOptions,
    source: Source,
    shared: Arc<Shared>,
    slot: Option<Arc<VideoSlot>>,
    rx: Receiver<Ctl>,
) {
    let fail = |code: ErrorCode, message: String| {
        shared.status.lock().unwrap().playing = false;
        shared.push(PlayerEvent::Failed { code, message });
    };
    let Some(ff) = ffmpeg::get() else {
        fail(
            ErrorCode::SrcNotSupported,
            "no FFmpeg on this system".into(),
        );
        return;
    };
    // Commands that came while a Media Source was not ready to open, for the player loop.
    let mut early = Vec::new();
    let (media, resource, mse) = match source {
        Source::Resource { resource, .. } => match open_container(ff, &resource) {
            Ok(m) => (m, Some(resource), None),
            Err((code, e)) => return fail(code, e),
        },
        Source::Mse(src) => {
            // MSE §2.4.? "attaching to a media element": the source opens; the decoders come
            // from the initialization segments, so the player waits for them.
            src.open();
            while !mse_player::ready_to_open(&src) {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(Ctl::Close) | Err(RecvTimeoutError::Disconnected) => {
                        src.close();
                        return;
                    }
                    Ok(c) => early.push(c),
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
            match mse_player::open(ff, src.clone()) {
                Ok(o) => {
                    let d = src.duration();
                    let media = Media {
                        packets: Box::new(o.reader),
                        video: o.video,
                        audio: o.audio,
                        duration: if d.is_nan() { f64::INFINITY } else { d },
                        start: 0.0,
                    };
                    (media, None, Some(src))
                }
                Err(e) => return fail(ErrorCode::SrcNotSupported, e),
            }
        }
    };
    let duration = media.duration;
    let (width, height) = media.video.as_ref().map_or((0, 0), |v| (v.width, v.height));
    let (has_video, has_audio) = (media.video.is_some(), media.audio.is_some());
    {
        let mut st = shared.status.lock().unwrap();
        st.duration = duration;
        st.seekable = if duration.is_finite() {
            Ranges::single(0.0, duration)
        } else {
            Ranges::new()
        };
    }
    shared.push(PlayerEvent::Opened {
        duration,
        width,
        height,
    });

    let queues: Shared2 = Arc::default();
    let decoder = {
        let q = queues.clone();
        std::thread::Builder::new()
            .name("media decoder".into())
            .spawn(move || decode(ff, media, q))
            .ok()
    };
    let sound_state = Arc::new(Mutex::new(SoundState {
        clock: Clock {
            at: 0.0,
            since: Instant::now(),
            running: false,
            rate: options.rate,
        },
        volume: options.volume,
        muted: options.muted,
        generation: 0,
    }));
    let sound = has_audio.then(|| {
        let (q, s, group) = (queues.clone(), sound_state.clone(), options.audio.clone());
        std::thread::Builder::new()
            .name("media sound".into())
            .spawn(move || play_sound(q, s, group))
            .ok()
    });

    play(
        Play {
            shared: &shared,
            slot,
            queues: &queues,
            sound: &sound_state,
            has_video,
            has_audio,
            duration,
            size: (width, height),
            audio: PlayerAudio::new(options.audio.clone()),
            mse: mse.clone(),
        },
        rx,
        early,
    );

    {
        let (m, cv) = &*queues;
        m.lock().unwrap().closed = true;
        cv.notify_all();
    }
    if let Some(r) = resource {
        r.close();
    }
    if let Some(src) = mse {
        src.close();
    }
    if let Some(Some(t)) = sound {
        let _ = t.join();
    }
    if let Some(t) = decoder {
        let _ = t.join();
    }
}

/// Opens a fetched resource's container: its first video and audio streams' decoders.
fn open_container(
    ff: &'static ffmpeg::Ffmpeg,
    resource: &MediaResource,
) -> Result<Media, (ErrorCode, String)> {
    let demuxer = Demuxer::open(ff, Box::new(ResourceBytes(resource.clone()))).map_err(|e| {
        let code = match resource.head() {
            Head::Failed { network: true, .. } => ErrorCode::Network,
            _ => ErrorCode::SrcNotSupported,
        };
        (code, e)
    })?;
    let first = |kind| demuxer.streams().iter().find(|s| s.kind == kind).cloned();
    let (video, audio) = (first(Kind::Video), first(Kind::Audio));
    let stream = |s: &ffmpeg::StreamInfo| {
        demuxer.decoder(s.index).map(|decoder| Stream {
            decoder,
            index: s.index,
            time_base: s.time_base,
            width: s.width,
            height: s.height,
        })
    };
    let streams = (
        video.as_ref().map(stream).transpose(),
        audio.as_ref().map(stream).transpose(),
    );
    let (vs, au) = match streams {
        (Ok(v), Ok(a)) if v.is_some() || a.is_some() => (v, a),
        (Err(e), _) | (_, Err(e)) => return Err((ErrorCode::SrcNotSupported, e)),
        _ => {
            return Err((
                ErrorCode::SrcNotSupported,
                "no audio or video stream".into(),
            ))
        }
    };
    let start = demuxer.start_time();
    let mut duration = demuxer.duration();
    if !duration.is_finite() {
        duration = [&video, &audio]
            .iter()
            .filter_map(|s| s.as_ref().map(|s| s.duration))
            .filter(|d| d.is_finite())
            .fold(f64::NAN, f64::max);
    }
    if !duration.is_finite() {
        duration = f64::INFINITY;
    }
    Ok(Media {
        packets: Box::new(Container { demuxer, start }),
        video: vs,
        audio: au,
        duration,
        start,
    })
}

/// The player thread's part.
struct Play<'a> {
    shared: &'a Arc<Shared>,
    slot: Option<Arc<VideoSlot>>,
    queues: &'a Shared2,
    sound: &'a Arc<Mutex<SoundState>>,
    has_video: bool,
    has_audio: bool,
    duration: f64,
    /// The size of the pictures shown (the metadata's until the first).
    size: (u32, u32),
    audio: PlayerAudio,
    /// A Media Source: its duration is the player's, and it hears of the playback position.
    mse: Option<MseSource>,
}

fn play(mut p: Play, rx: Receiver<Ctl>, mut early: Vec<Ctl>) {
    let mut clock = Clock {
        at: 0.0,
        since: Instant::now(),
        running: false,
        rate: p.sound.lock().unwrap().clock.rate,
    };
    // Whether play() was asked (the clock runs only while it was and data is there).
    let mut wanted = false;
    let mut waiting = false;
    let mut ended = false;
    // A seek (or the opening) whose first picture or sound has not come yet.
    let mut seeking: Option<u64> = Some(0);
    let mut generation = 0u64;
    let mut first_data = false;
    let mut first_shown = false;
    let (mut volume, mut muted) = {
        let s = p.sound.lock().unwrap();
        (s.volume, s.muted)
    };
    loop {
        let wait = if clock.running {
            Duration::from_millis(4)
        } else {
            Duration::from_millis(20)
        };
        let mut commands = std::mem::take(&mut early);
        match rx.recv_timeout(wait) {
            Ok(c) => commands.push(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        commands.extend(rx.try_iter());
        if let Some(src) = &p.mse {
            // (the page sets a Media Source's duration, and the end of stream does)
            let d = src.duration();
            p.duration = if d.is_nan() { f64::INFINITY } else { d };
        }
        for c in commands {
            match c {
                Ctl::Close => return,
                Ctl::Play => {
                    wanted = true;
                    if ended {
                        // (played again after the end: from the start, as HTML's play() seeks)
                        ended = false;
                    }
                }
                Ctl::Pause => {
                    wanted = false;
                    clock.set(clock.now(), false);
                }
                Ctl::Seek(t) => {
                    let t = if p.duration.is_finite() {
                        t.clamp(0.0, p.duration)
                    } else {
                        t.max(0.0)
                    };
                    generation += 1;
                    seeking = Some(generation);
                    ended = false;
                    clock.set(t, false);
                    let (m, cv) = &**p.queues;
                    let mut q = m.lock().unwrap();
                    q.generation = generation;
                    q.seek = Some((t, generation));
                    q.pictures.clear();
                    q.sound.clear();
                    q.end = false;
                    cv.notify_all();
                }
                Ctl::Rate(r) => {
                    clock.set(clock.now(), clock.running);
                    clock.rate = r;
                    p.shared.status.lock().unwrap().rate = r;
                }
                Ctl::Volume(v, m) => {
                    volume = v;
                    muted = m;
                }
            }
        }

        // What the decoder has.
        let (m, _) = &**p.queues;
        let mut q = m.lock().unwrap();
        if let Some(e) = q.failed.take() {
            drop(q);
            p.shared.status.lock().unwrap().playing = false;
            p.shared.push(PlayerEvent::Failed {
                code: ErrorCode::Decode,
                message: e,
            });
            return;
        }
        let mut show: Option<Picture> = None;
        if let Some(g) = seeking {
            if q.seeked == Some(g) && q.generation == g {
                seeking = None;
                show = q.pictures.pop_front();
                if g > 0 {
                    p.shared.push(PlayerEvent::Seeked);
                }
            }
        }
        let has_data = !q.pictures.is_empty() || !q.sound.is_empty();
        if !first_data && (show.is_some() || (!p.has_video && !q.sound.is_empty())) {
            first_data = true;
            if !p.has_video {
                p.shared.push(PlayerEvent::Frame);
            }
        }
        // The clock runs while playing was asked, no seek is pending and data is there.
        let starved = !q.end && !has_data && show.is_none();
        let can_run = wanted && seeking.is_none() && !ended && !starved;
        if can_run && !clock.running {
            clock.set(clock.now(), true);
            waiting = false;
            p.shared.push(PlayerEvent::Playing);
        } else if !can_run && clock.running {
            clock.set(clock.now(), false);
            if wanted && starved && !waiting {
                waiting = true;
                p.shared.push(PlayerEvent::Waiting);
            }
        }
        let now = clock.now();
        while q.pictures.front().is_some_and(|f| f.t <= now) {
            show = q.pictures.pop_front();
        }
        let at_end = q.end && q.pictures.is_empty() && q.sound.is_empty();
        let queued_to = q.queued_to;
        drop(q);
        // (a stream whose pictures change size midway: the element's size follows)
        if let Some(f) = show.as_ref().filter(|f| (f.width, f.height) != p.size) {
            p.size = (f.width, f.height);
            p.shared.push(PlayerEvent::SizeChanged(f.width, f.height));
        }
        let shown_first = show.is_some() && !first_shown;
        first_shown |= show.is_some();
        if let (Some(f), Some(slot)) = (show, &p.slot) {
            slot.present(f.width, f.height, f.pixels);
            if slot.frame_count() == 1 {
                p.shared.push(PlayerEvent::Frame);
            }
        } else if shown_first && p.slot.is_none() {
            p.shared.push(PlayerEvent::Frame);
        }
        if at_end && clock.running && !ended && now >= queued_to - 0.001 {
            ended = true;
            wanted = false;
            let end = if p.duration.is_finite() {
                p.duration
            } else {
                now
            };
            clock.set(end, false);
            p.shared.status.lock().unwrap().playing = false;
            p.shared.push(PlayerEvent::Ended);
        }

        {
            let mut s = p.sound.lock().unwrap();
            s.clock = clock;
            s.volume = volume;
            s.muted = muted;
            s.generation = generation;
        }
        let buffered = match &p.mse {
            Some(src) => {
                // (eviction keeps what is after the position)
                src.set_position(clock.now());
                src.buffered_all()
            }
            None if queued_to > 0.0 => Ranges::single(0.0, queued_to.min(p.duration)),
            None => Ranges::new(),
        };
        {
            let mut st = p.shared.status.lock().unwrap();
            st.position = clock.now();
            st.sampled = Instant::now();
            st.playing = clock.running;
            st.rate = clock.rate;
            st.buffered = buffered;
            if p.mse.is_some() {
                st.duration = p.duration;
                st.seekable = if p.duration.is_finite() {
                    Ranges::single(0.0, p.duration)
                } else {
                    Ranges::new()
                };
            }
        }
        p.audio.update(&Audible {
            playing: clock.running,
            volume,
            muted,
            has_audio: Some(p.has_audio),
        });
    }
}

fn decode(ff: &'static ffmpeg::Ffmpeg, mut s: Media, queues: Shared2) {
    let (m, cv) = &*queues;
    let mut frame = Frame::new(ff);
    let mut scaler = Scaler::new(ff);
    let mut resampler = Resampler::new(ff, RATE, CHANNELS, false);
    let mut generation = 0u64;
    // The seek's time: what comes before it is dropped.
    let mut from = 0.0f64;
    let mut reached = false;
    loop {
        {
            let mut q = m.lock().unwrap();
            loop {
                if q.closed {
                    return;
                }
                if let Some((t, g)) = q.seek.take() {
                    drop(q);
                    s.packets.seek(t);
                    for st in [&mut s.video, &mut s.audio].into_iter().flatten() {
                        st.decoder.flush();
                    }
                    (generation, from, reached) = (g, t, false);
                    q = m.lock().unwrap();
                    q.end = false;
                    q.queued_to = t;
                    continue;
                }
                let full = s.video.is_none() || q.pictures.len() >= VIDEO_AHEAD;
                let full = full && (s.audio.is_none() || q.sound_ahead() >= AUDIO_AHEAD);
                if q.end || full {
                    q = cv.wait_timeout(q, Duration::from_millis(50)).unwrap().0;
                    continue;
                }
                break;
            }
        }
        let pkt = match s.packets.next() {
            Next::Packet(p) => Some(p),
            Next::End => None,
            Next::Pending => {
                s.packets.wait();
                continue;
            }
        };
        let mut out: Vec<Picture> = Vec::new();
        let mut sound: Vec<Sound> = Vec::new();
        let mut take = |dec: &mut Decoder,
                        tb: ffmpeg::Rational,
                        video: bool,
                        out: &mut Vec<Picture>,
                        sound: &mut Vec<Sound>|
         -> Result<(), String> {
            loop {
                match dec.receive(&mut frame)? {
                    Decoded::Frame => {}
                    Decoded::Again | Decoded::End => return Ok(()),
                }
                let pts = frame.pts();
                let t = if pts == ffmpeg::NOPTS {
                    from
                } else {
                    tb.seconds(pts) - s.start
                };
                if video {
                    // (pictures before the seek's time are dropped, but one within a frame of it)
                    if t < from - 0.04 {
                        continue;
                    }
                    if let Some((width, height, pixels)) = scaler.bgra(&frame) {
                        out.push(Picture {
                            t,
                            width,
                            height,
                            pixels,
                        });
                    }
                } else if let Some(mut planes) = resampler.convert(&frame) {
                    let mut samples = planes.pop().unwrap_or_default();
                    let mut t = t;
                    if t < from {
                        let skip = (((from - t) * RATE as f64) as usize * CHANNELS as usize)
                            .min(samples.len());
                        samples.drain(..skip);
                        t = from;
                    }
                    if !samples.is_empty() {
                        sound.push(Sound {
                            t,
                            samples,
                            used: 0,
                        });
                    }
                }
            }
        };
        let result =
            match &pkt {
                Some(pkt) => {
                    let i = pkt.stream();
                    if let Some(v) = s.video.as_mut().filter(|v| v.index == i) {
                        let tb = v.time_base;
                        v.decoder
                            .send(Some(pkt))
                            .and_then(|_| take(&mut v.decoder, tb, true, &mut out, &mut sound))
                    } else if let Some(a) = s.audio.as_mut().filter(|a| a.index == i) {
                        let tb = a.time_base;
                        a.decoder
                            .send(Some(pkt))
                            .and_then(|_| take(&mut a.decoder, tb, false, &mut out, &mut sound))
                    } else {
                        Ok(())
                    }
                }
                None => {
                    let mut r = Ok(());
                    if let Some(v) = s.video.as_mut() {
                        let tb = v.time_base;
                        r =
                            r.and(v.decoder.send(None).and_then(|_| {
                                take(&mut v.decoder, tb, true, &mut out, &mut sound)
                            }));
                    }
                    if let Some(a) = s.audio.as_mut() {
                        let tb = a.time_base;
                        r =
                            r.and(a.decoder.send(None).and_then(|_| {
                                take(&mut a.decoder, tb, false, &mut out, &mut sound)
                            }));
                    }
                    r
                }
            };
        let mut q = m.lock().unwrap();
        if q.generation != generation || q.seek.is_some() {
            continue; // (a seek came meanwhile: what was decoded is of the old position)
        }
        if let Err(e) = result {
            // (a broken packet is skipped, as players do; a decoder that fails at the start fails)
            if !reached {
                q.failed = Some(e);
                cv.notify_all();
                return;
            }
        }
        for p in &out {
            q.queued_to = q.queued_to.max(p.t);
        }
        for snd in &sound {
            q.queued_to = q.queued_to.max(snd.end());
        }
        let got = !out.is_empty() || (s.video.is_none() && !sound.is_empty());
        q.pictures.extend(out);
        q.sound.extend(sound);
        if pkt.is_none() {
            q.end = true;
        }
        if !reached && (got || pkt.is_none()) {
            reached = true;
            q.seeked = Some(generation);
        }
        cv.notify_all();
    }
}

/// Where the sound goes.
enum Out {
    Server(PulseSink),
    Null(NullSink),
}

impl Out {
    fn open() -> Out {
        let config = StreamConfig {
            sample_rate: RATE as f32,
            channels: CHANNELS,
            period_frames: PERIOD,
        };
        if PulseOutputs::new().is_some() {
            let mut s = PulseSink::default();
            if s.start(&config).is_ok() {
                return Out::Server(s);
            }
        }
        let mut n = NullSink::default();
        let _ = n.start(&config);
        Out::Null(n)
    }

    fn latency(&self) -> f64 {
        match self {
            Out::Server(s) => s.latency(),
            Out::Null(_) => 0.0,
        }
    }

    fn write(&mut self, samples: &[f32]) -> bool {
        match self {
            Out::Server(s) => s.write_interleaved(samples).is_ok(),
            Out::Null(n) => {
                if n.wait_period().is_err() {
                    return false;
                }
                let frames = samples.len() / CHANNELS as usize;
                n.write(&vec![vec![0.0; frames]; CHANNELS as usize]).is_ok()
            }
        }
    }

    fn flush(&mut self) {
        if let Out::Server(s) = self {
            s.flush();
        }
    }
}

fn play_sound(queues: Shared2, state: Arc<Mutex<SoundState>>, group: Option<Arc<AudioGroup>>) {
    let mut out = Out::open();
    let mut generation = 0;
    let mut buf = vec![0.0f32; PERIOD * CHANNELS as usize];
    let (m, _) = &*queues;
    loop {
        let (clock, gain, g) = {
            let s = state.lock().unwrap();
            let tab_muted = group.as_ref().is_some_and(|g| g.muted());
            let silent = output_muted(s.muted, tab_muted) || (s.clock.rate - 1.0).abs() > 1e-9;
            (
                s.clock,
                if silent {
                    0.0
                } else {
                    s.volume.clamp(0.0, 1.0) as f32
                },
                s.generation,
            )
        };
        if g != generation {
            generation = g;
            out.flush();
        }
        buf.fill(0.0);
        {
            let mut q = m.lock().unwrap();
            if q.closed {
                return;
            }
            if clock.running {
                // The sound written now leaves the output after the latency.
                let due = clock.now() + out.latency();
                // (late sound is dropped)
                while q.sound.front().is_some_and(|s| s.end() < due - 0.05) {
                    q.sound.pop_front();
                }
                if let Some(s) = q.sound.front_mut() {
                    if s.now() < due - 0.05 {
                        s.used = (((due - s.t) * RATE as f64) as usize).min(s.frames());
                    }
                }
                let mut filled = 0;
                while filled < PERIOD {
                    let Some(s) = q.sound.front_mut() else { break };
                    if s.now() > due + 0.02 + filled as f64 / RATE as f64 {
                        break; // (early: silence until it is due)
                    }
                    let n = (s.frames() - s.used).min(PERIOD - filled);
                    let ch = CHANNELS as usize;
                    for (o, i) in buf[filled * ch..(filled + n) * ch]
                        .iter_mut()
                        .zip(&s.samples[s.used * ch..(s.used + n) * ch])
                    {
                        *o = i * gain;
                    }
                    s.used += n;
                    filled += n;
                    if s.used >= s.frames() {
                        q.sound.pop_front();
                    }
                }
            }
        }
        if !out.write(&buf) {
            // (the server went away: keep time without it)
            out = Out::Null(NullSink::default());
            let _ = match &mut out {
                Out::Null(n) => n.start(&StreamConfig {
                    sample_rate: RATE as f32,
                    channels: CHANNELS,
                    period_frames: PERIOD,
                }),
                Out::Server(_) => Ok(()),
            };
        }
    }
}
