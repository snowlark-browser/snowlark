// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media Source Extensions' media side on Linux: each `SourceBuffer`'s byte stream is parsed into
//! coded frames ([`crate::fmp4::SampleReader`], the ISO BMFF byte stream format), which the coded
//! frame processing algorithm (MSE §3.5.8) places in one track buffer per track by decode time,
//! under the buffer's timestamp offset, append window and mode. The track buffers answer
//! `buffered`, lose frames to `remove()` (§3.5.9, "coded frame removal") and to eviction when they
//! grow past their quota (§3.5.13, "coded frame eviction"), and feed the player ([`super::player`]),
//! which decodes them with the decoders FFmpeg configures from each buffer's initialization
//! segment.
//!
//! The page keeps MSE's state machine (`readyState`, `updating`, the events and exceptions) and
//! calls in here for what touches the media, as on Windows; an append's parsing runs at once, and
//! its `Update`/`UpdateEnd` reports reach the page in a later task through [`MseEvent`]s and a
//! wake.
//!
//! Times are in 100 ns units (`i64`), as [`crate::fmp4::Sample`] has them.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::fmp4::{Codec, Output, SampleReader, Track};
use crate::init_segment::Container;
use crate::mse::{AddError, MseEvent};
use crate::Ranges;

/// 100 ns units per second.
pub(crate) const TICKS: f64 = 10_000_000.0;
/// The bytes of coded frames a buffer keeps before eviction (MSE §3.5.13 leaves it to the user
/// agent): 150 MB with video, 12 MB for sound alone.
const VIDEO_QUOTA: usize = 150 << 20;
const AUDIO_QUOTA: usize = 12 << 20;
/// The most source buffers one MediaSource takes.
const MAX_BUFFERS: usize = 8;

fn ticks(s: f64) -> i64 {
    (s * TICKS).round() as i64
}

fn seconds(t: i64) -> f64 {
    t as f64 / TICKS
}

/// A text track's sample entry (WebVTT, TTML): no track buffer, as no text track is played.
fn is_text(c: &Codec) -> bool {
    matches!(c, Codec::Other(f) if f == b"wvtt" || f == b"stpp")
}

/// The codec of a sample entry (by its original format when protected); none for one the
/// platform has no decoder kind for.
fn sample_entry_codec(c: &Codec) -> Option<crate::caps::Codec> {
    use crate::caps::Codec as C;
    Some(match c {
        Codec::Avc { .. } => C::H264,
        Codec::Aac { .. } => C::Aac,
        Codec::Other(f) => match f {
            b"hvc1" | b"hev1" => C::Hevc,
            b"vp08" => C::Vp8,
            b"vp09" => C::Vp9,
            b"av01" => C::Av1,
            b"mp4a" => C::Aac,
            b".mp3" => C::Mp3,
            b"Opus" => C::Opus,
            b"fLaC" => C::Flac,
            b"alac" => C::Alac,
            b"ac-3" => C::Ac3,
            b"ec-3" => C::Eac3,
            _ => return None,
        },
    })
}

/// One coded frame of a track buffer.
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub pts: i64,
    pub dts: i64,
    pub duration: i64,
    pub key: bool,
    pub data: Arc<[u8]>,
}

impl Frame {
    fn end(&self) -> i64 {
        self.pts + self.duration
    }
}

/// One track's coded frames, in decode order, and the coded frame processing state MSE keeps per
/// track.
#[derive(Debug)]
pub(crate) struct TrackBuffer {
    /// The track's id in the initialization segment.
    pub track: u32,
    pub video: bool,
    pub frames: Vec<Frame>,
    last_decode: Option<i64>,
    last_duration: i64,
    highest_end: Option<i64>,
    need_random_access: bool,
    /// The longest frame seen: two of it is the gap still taken as continuous in `buffered`.
    longest: i64,
    /// Bumped whenever frames go (so a reader knows its place may be gone).
    pub removals: u64,
}

impl TrackBuffer {
    fn new(track: &Track) -> TrackBuffer {
        TrackBuffer {
            track: track.id,
            video: track.video,
            frames: Vec::new(),
            last_decode: None,
            last_duration: 0,
            highest_end: None,
            need_random_access: true,
            longest: 0,
            removals: 0,
        }
    }

    /// The presentation intervals of the frames, joined across gaps of less than two frames.
    pub fn ranges(&self) -> Vec<(i64, i64)> {
        let mut spans: Vec<(i64, i64)> = self.frames.iter().map(|f| (f.pts, f.end())).collect();
        spans.sort_unstable();
        let slack = self.longest * 2;
        let mut out: Vec<(i64, i64)> = Vec::new();
        for (s, e) in spans {
            match out.last_mut() {
                Some(last) if s <= last.1 + slack => last.1 = last.1.max(e),
                _ => out.push((s, e)),
            }
        }
        out
    }

    fn bytes(&self) -> usize {
        self.frames.iter().map(|f| f.data.len()).sum()
    }

    /// Removes the frames at `indices` (sorted, decode order) and every frame that depends on
    /// them: those after each up to the next random access point.
    fn remove_with_dependents(&mut self, mut doomed: Vec<usize>) {
        if doomed.is_empty() {
            return;
        }
        doomed.sort_unstable();
        doomed.dedup();
        let mut keep = vec![true; self.frames.len()];
        let mut cut = false;
        let mut next = doomed.iter().peekable();
        for (i, f) in self.frames.iter().enumerate() {
            if next.peek() == Some(&&i) {
                next.next();
                keep[i] = false;
                cut = true;
                continue;
            }
            if f.key {
                cut = false;
            }
            if cut && self.video {
                keep[i] = false;
            }
        }
        let mut i = 0;
        self.frames.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
        self.removals += 1;
    }

    /// Inserts `f` at its place in decode order.
    fn insert(&mut self, f: Frame) {
        let at = self.frames.partition_point(|g| g.dts <= f.dts);
        self.frames.insert(at, f);
    }
}

/// One `SourceBuffer`'s media side.
pub(crate) struct Buffer {
    pub id: u32,
    reader: SampleReader,
    /// The first initialization segment's `moov` (FFmpeg configures the decoders from it), and
    /// the tracks it gave.
    pub init: Option<Vec<u8>>,
    pub tracks: Vec<Track>,
    pub track_buffers: Vec<TrackBuffer>,
    timestamp_offset: f64,
    window: (f64, f64),
    sequence: bool,
    group_start: Option<f64>,
    group_end: f64,
}

impl Buffer {
    fn new(id: u32) -> Buffer {
        Buffer {
            id,
            reader: SampleReader::raw(),
            init: None,
            tracks: Vec::new(),
            track_buffers: Vec::new(),
            timestamp_offset: 0.0,
            window: (0.0, f64::INFINITY),
            sequence: false,
            group_start: None,
            group_end: 0.0,
        }
    }

    fn quota(&self) -> usize {
        if self.tracks.iter().any(|t| t.video) {
            VIDEO_QUOTA
        } else {
            AUDIO_QUOTA
        }
    }

    fn bytes(&self) -> usize {
        self.track_buffers.iter().map(|t| t.bytes()).sum()
    }

    /// MSE §3.5.7 "initialization segment received", the media's half: the first segment's tracks
    /// make the track buffers; a later one must have the same kinds of tracks (else a decode error).
    fn init_segment(&mut self, tracks: Vec<Track>) -> Result<(), String> {
        // (§3.5.7 step 3.1-3.2: a codec the user agent does not support is an append error)
        if let Some(t) = tracks
            .iter()
            .find(|t| !is_text(&t.codec) && sample_entry_codec(&t.codec).is_none())
        {
            return Err(format!(
                "track {}: a codec this browser does not take",
                t.id
            ));
        }
        if self.init.is_none() {
            self.track_buffers = tracks
                .iter()
                .filter(|t| !is_text(&t.codec))
                .map(TrackBuffer::new)
                .collect();
            self.init = Some(self.reader.moov().to_vec());
        } else {
            let kinds = |ts: &[Track]| {
                let v = ts.iter().filter(|t| t.video).count();
                (v, ts.len() - v)
            };
            if kinds(&tracks) != kinds(&self.tracks) {
                return Err("an initialization segment with other tracks".into());
            }
            // (the tracks keep their buffers, matched by order of kind)
            for video in [true, false] {
                let ids = tracks.iter().filter(|t| t.video == video).map(|t| t.id);
                for (tb, id) in self
                    .track_buffers
                    .iter_mut()
                    .filter(|b| b.video == video)
                    .zip(ids)
                {
                    tb.track = id;
                }
            }
        }
        self.tracks = tracks;
        for tb in &mut self.track_buffers {
            tb.need_random_access = true;
        }
        Ok(())
    }

    /// MSE §3.5.8 "coded frame processing" for one frame of track `track`.
    fn coded_frame(&mut self, track: u32, sample: crate::fmp4::Sample) {
        let data: Arc<[u8]> = sample.data.into();
        let (pts_in, dts_in) = (sample.time, sample.decode);
        let duration = sample.duration.max(0);
        loop {
            // 1.4-1.5: sequence mode and a group start timestamp.
            if self.sequence {
                if let Some(start) = self.group_start.take() {
                    self.timestamp_offset = start - seconds(pts_in);
                    self.group_end = start;
                    for tb in &mut self.track_buffers {
                        tb.need_random_access = true;
                    }
                }
            }
            // 1.6: the timestamp offset.
            let offset = ticks(self.timestamp_offset);
            let pts = pts_in + offset;
            let dts = dts_in + offset;
            let Some(tb) = self.track_buffers.iter_mut().find(|t| t.track == track) else {
                return;
            };
            // 1.7-1.8: a discontinuity.
            if let Some(last) = tb.last_decode {
                if dts < last || dts - last > 2 * tb.last_duration {
                    if self.sequence {
                        self.group_start = Some(self.group_end);
                    } else {
                        self.group_end = seconds(pts);
                    }
                    for tb in &mut self.track_buffers {
                        tb.last_decode = None;
                        tb.last_duration = 0;
                        tb.highest_end = None;
                        tb.need_random_access = true;
                    }
                    // (the frame again, from the top: in sequence mode with the new group start)
                    continue;
                }
            }
            let end = pts + duration;
            // 1.9-1.10: the append window.
            let (ws, we) = (ticks(self.window.0), self.window.1);
            if pts < ws || (we.is_finite() && end > ticks(we)) {
                tb.need_random_access = true;
                return;
            }
            // 1.11-1.12: a random access point first.
            if tb.need_random_access {
                if !sample.key {
                    return;
                }
                tb.need_random_access = false;
            }
            // 1.13-1.14: frames this one overlaps go, with what depends on them.
            let doomed: Vec<usize> = match tb.highest_end {
                None => tb
                    .frames
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.pts >= pts && f.pts < end)
                    .map(|(i, _)| i)
                    .collect(),
                Some(h) if h <= pts => tb
                    .frames
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.pts >= h && f.pts < end)
                    .map(|(i, _)| i)
                    .collect(),
                Some(_) => Vec::new(),
            };
            // (a frame whose interval holds this one's start is cut off too: §3.5.8 1.13's
            // overlap of a frame already buffered)
            let mut doomed = doomed;
            if tb.last_decode.is_none() {
                if let Some(i) = tb
                    .frames
                    .iter()
                    .position(|f| f.pts < pts && f.end() > pts && tb.video)
                {
                    doomed.push(i);
                }
            }
            tb.remove_with_dependents(doomed);
            // 1.15-1.16: in.
            tb.longest = tb.longest.max(duration);
            tb.insert(Frame {
                pts,
                dts,
                duration,
                key: sample.key,
                data,
            });
            tb.last_decode = Some(dts);
            tb.last_duration = duration;
            tb.highest_end = Some(tb.highest_end.map_or(end, |h| h.max(end)));
            self.group_end = self.group_end.max(seconds(end));
            return;
        }
    }

    /// MSE §3.5.9 "coded frame removal" of `[start, end)` seconds.
    fn remove(&mut self, start: f64, end: f64) {
        let (start, end) = (
            ticks(start),
            if end.is_finite() {
                ticks(end)
            } else {
                i64::MAX
            },
        );
        for tb in &mut self.track_buffers {
            // (to the next random access point at or after the end)
            let remove_end = tb
                .frames
                .iter()
                .filter(|f| f.key && f.pts >= end)
                .map(|f| f.pts)
                .min()
                .unwrap_or(i64::MAX);
            let remove_end = if end == i64::MAX {
                i64::MAX
            } else {
                remove_end
            };
            let doomed: Vec<usize> = tb
                .frames
                .iter()
                .enumerate()
                .filter(|(_, f)| f.pts >= start && f.pts < remove_end)
                .map(|(i, _)| i)
                .collect();
            if !doomed.is_empty() {
                tb.remove_with_dependents(doomed);
                tb.need_random_access = true;
            }
        }
    }

    /// MSE §3.5.13 "coded frame eviction" before an append of `incoming` bytes: frames before the
    /// playback position `now` go, a GOP at a time from the start, until it fits; false when it
    /// still does not.
    fn evict(&mut self, incoming: usize, now: f64) -> bool {
        let quota = self.quota();
        if self.bytes() + incoming <= quota {
            return true;
        }
        let now = ticks(now);
        let mut excess = self.bytes() + incoming - quota;
        for tb in &mut self.track_buffers {
            while excess > 0 {
                // (the first GOP, if it ends before the position)
                let next_key = tb.frames.iter().skip(1).position(|f| f.key).map(|i| i + 1);
                let Some(cut) = next_key else { break };
                if tb.frames[..cut].iter().any(|f| f.end() > now) {
                    break;
                }
                let freed: usize = tb.frames.drain(..cut).map(|f| f.data.len()).sum();
                excess = excess.saturating_sub(freed);
                tb.removals += 1;
            }
        }
        excess == 0
    }

    /// The buffer's `buffered` (MSE §3.1): the intersection of its tracks' ranges, the last end
    /// of each taken to the highest when the source has ended.
    fn buffered(&self, ended: bool) -> Ranges {
        let per: Vec<Vec<(i64, i64)>> = self.track_buffers.iter().map(|t| t.ranges()).collect();
        if per.is_empty() || per.iter().any(|r| r.is_empty()) {
            return Ranges::new();
        }
        let highest = per
            .iter()
            .filter_map(|r| r.last().map(|l| l.1))
            .max()
            .unwrap_or(0);
        let mut out: Option<Ranges> = None;
        for mut r in per {
            if ended {
                if let Some(last) = r.last_mut() {
                    last.1 = highest;
                }
            }
            let r = Ranges::from_pairs(r.into_iter().map(|(s, e)| (seconds(s), seconds(e))));
            out = Some(match out {
                None => r,
                Some(o) => o.intersect(&r),
            });
        }
        out.unwrap_or_default()
    }
}

/// Where the media source is (MSE §2.4 `readyState`, as the media side sees it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ready {
    Closed,
    Open,
    Ended,
}

pub(crate) struct State {
    pub ready: Ready,
    pub duration: f64,
    pub buffers: Vec<Buffer>,
    next: u32,
    /// The playback position the player last reported (eviction keeps what is after it).
    pub position: f64,
    /// Bumped by every change the player may wait for.
    pub version: u64,
}

pub(crate) struct Inner {
    state: Mutex<State>,
    /// Signalled with each bump of [`State::version`].
    pub changed: Condvar,
    events: Mutex<Vec<MseEvent>>,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// One `MediaSource` object's media side. Cheap to clone.
#[derive(Clone)]
pub struct MseSource {
    pub(crate) inner: Arc<Inner>,
}

/// Why a source could not be made (never, on Linux; the type matches Windows').
#[derive(Debug)]
pub struct Unsupported;

impl MseSource {
    pub fn new() -> Result<MseSource, Unsupported> {
        Ok(MseSource {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    ready: Ready::Closed,
                    duration: f64::NAN,
                    buffers: Vec::new(),
                    next: 1,
                    position: 0.0,
                    version: 0,
                }),
                changed: Condvar::new(),
                events: Mutex::new(Vec::new()),
                wake: Mutex::new(None),
            }),
        })
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap()
    }

    /// Waits at most `timeout` for a change after `version`; the version now.
    pub(crate) fn wait_change(&self, version: u64, timeout: Duration) -> u64 {
        let s = self.state();
        let (s, _) = self
            .inner
            .changed
            .wait_timeout_while(s, timeout, |s| s.version == version)
            .unwrap();
        s.version
    }

    fn changed(&self, s: &mut State) {
        s.version += 1;
        self.inner.changed.notify_all();
    }

    fn push(&self, e: MseEvent) {
        self.inner.events.lock().unwrap().push(e);
        let w = self.inner.wake.lock().unwrap().clone();
        if let Some(w) = w {
            w();
        }
    }

    /// The player attached the source: it opens (MSE §2.4.? "attaching to a media element").
    pub(crate) fn open(&self) {
        let mut s = self.state();
        if s.ready == Ready::Closed {
            s.ready = Ready::Open;
            self.changed(&mut s);
            drop(s);
            self.push(MseEvent::Opened);
        }
    }

    /// The player let go of the source.
    pub(crate) fn close(&self) {
        let mut s = self.state();
        if s.ready != Ready::Closed {
            s.ready = Ready::Closed;
            self.changed(&mut s);
            drop(s);
            self.push(MseEvent::Closed);
        }
    }

    /// The playback position, for eviction.
    pub(crate) fn set_position(&self, t: f64) {
        self.state().position = t;
    }

    /// Who to wake when events wait.
    pub fn set_wake(&self, wake: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.inner.wake.lock().unwrap() = wake;
    }

    pub fn take_events(&self) -> Vec<MseEvent> {
        std::mem::take(&mut *self.inner.events.lock().unwrap())
    }

    /// "closed", "open" or "ended", as the media side sees it.
    pub fn ready_state(&self) -> &'static str {
        match self.state().ready {
            Ready::Closed => "closed",
            Ready::Open => "open",
            Ready::Ended => "ended",
        }
    }

    /// MSE §3.5.1 `addSourceBuffer` (after the page's checks): a new buffer's id.
    pub fn add_source_buffer(&self, type_: &str) -> Result<u32, AddError> {
        if Container::of_type(type_) != Some(Container::Mp4) {
            return Err(AddError::NotSupported);
        }
        let mut s = self.state();
        if s.ready != Ready::Open {
            return Err(AddError::InvalidState);
        }
        if s.buffers.len() >= MAX_BUFFERS {
            return Err(AddError::QuotaExceeded);
        }
        let id = s.next;
        s.next += 1;
        s.buffers.push(Buffer::new(id));
        self.changed(&mut s);
        Ok(id)
    }

    pub fn remove_source_buffer(&self, id: u32) {
        let mut s = self.state();
        s.buffers.retain(|b| b.id != id);
        self.changed(&mut s);
    }

    /// MSE §3.5.? the "segment parser loop" over `bytes`: the coded frames are in the track
    /// buffers when this returns; `Update` and `UpdateEnd` (or `Error` and `UpdateEnd`) follow as
    /// events. `Err` ("QuotaExceededError") when eviction cannot make room: nothing is appended.
    pub fn append(&self, id: u32, bytes: &[u8]) -> Result<(), String> {
        let mut s = self.state();
        // (MSE §3.5.? "prepare append": an append after the end of stream opens the source again)
        if s.ready == Ready::Ended {
            s.ready = Ready::Open;
        }
        let position = s.position;
        let Some(b) = s.buffers.iter_mut().find(|b| b.id == id) else {
            return Err("no such source buffer".into());
        };
        if !b.evict(bytes.len(), position) {
            return Err("QuotaExceededError".into());
        }
        let mut failed = None;
        let segments = b.reader.segments();
        for out in b.reader.push(bytes) {
            match out {
                Output::Tracks(tracks) => {
                    if let Err(e) = b.init_segment(tracks) {
                        failed = Some(e);
                        break;
                    }
                }
                Output::Sample(track, sample) => {
                    if b.init.is_none() {
                        // (a media segment before any initialization segment)
                        failed = Some("a media segment before the initialization segment".into());
                        break;
                    }
                    b.coded_frame(track, sample);
                }
            }
        }
        if b.init.is_none() && b.reader.segments() > segments {
            failed = Some("a media segment before the initialization segment".into());
        }
        if failed.is_none() && b.reader.failed() {
            // (§3.5.? the segment parser loop: bytes that are not a box of the byte stream)
            failed = Some("not an ISO BMFF byte stream".into());
        }
        self.changed(&mut s);
        drop(s);
        if failed.is_some() {
            self.push(MseEvent::Error(id));
        } else {
            self.push(MseEvent::Update(id));
        }
        self.push(MseEvent::UpdateEnd(id));
        Ok(())
    }

    /// Starts removing `[start, end)` (end infinite: to the end); `Update` and `UpdateEnd` follow.
    pub fn remove(&self, id: u32, start: f64, end: f64) -> Result<(), String> {
        let mut s = self.state();
        let Some(b) = s.buffers.iter_mut().find(|b| b.id == id) else {
            return Err("no such source buffer".into());
        };
        b.remove(start, end);
        self.changed(&mut s);
        drop(s);
        self.push(MseEvent::Update(id));
        self.push(MseEvent::UpdateEnd(id));
        Ok(())
    }

    /// MSE §3.5.? `abort()`'s "reset parser state": a partly appended segment is dropped.
    pub fn abort(&self, id: u32) {
        let mut s = self.state();
        if let Some(b) = s.buffers.iter_mut().find(|b| b.id == id) {
            b.reader.reset();
            if b.sequence {
                b.group_start = Some(b.group_end);
            }
            for tb in &mut b.track_buffers {
                tb.last_decode = None;
                tb.last_duration = 0;
                tb.highest_end = None;
                tb.need_random_access = true;
            }
        }
    }

    /// Appends finish before this returns: never updating between calls.
    pub fn updating(&self, _id: u32) -> bool {
        false
    }

    pub fn buffered(&self, id: u32) -> Ranges {
        let s = self.state();
        let ended = s.ready == Ready::Ended;
        s.buffers
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.buffered(ended))
            .unwrap_or_default()
    }

    /// The media element's buffered ranges: the intersection of every buffer's.
    pub(crate) fn buffered_all(&self) -> Ranges {
        let s = self.state();
        let ended = s.ready == Ready::Ended;
        let mut out: Option<Ranges> = None;
        for b in &s.buffers {
            let r = b.buffered(ended);
            out = Some(match out {
                None => r,
                Some(o) => o.intersect(&r),
            });
        }
        out.unwrap_or_default()
    }

    pub fn set_timestamp_offset(&self, id: u32, t: f64) {
        let mut s = self.state();
        if let Some(b) = s.buffers.iter_mut().find(|b| b.id == id) {
            b.timestamp_offset = t;
            if b.sequence {
                b.group_start = Some(t);
            }
        }
    }

    pub fn set_append_window(&self, id: u32, start: f64, end: f64) {
        let mut s = self.state();
        if let Some(b) = s.buffers.iter_mut().find(|b| b.id == id) {
            b.window = (start, end);
        }
    }

    pub fn set_sequence_mode(&self, id: u32, sequence: bool) {
        let mut s = self.state();
        if let Some(b) = s.buffers.iter_mut().find(|b| b.id == id) {
            if sequence && !b.sequence {
                b.group_start = Some(b.group_end);
            }
            b.sequence = sequence;
        }
    }

    /// MSE's "end of stream algorithm" on the media side (`None`: success).
    pub fn end_of_stream(&self, error: Option<&str>) {
        let mut s = self.state();
        s.ready = Ready::Ended;
        if error.is_none() {
            // (the duration becomes the highest end of what is buffered)
            let highest = s
                .buffers
                .iter()
                .map(|b| b.buffered(true))
                .filter_map(|r| r.end(r.len().wrapping_sub(1)))
                .fold(f64::NAN, f64::max);
            if highest.is_finite() {
                s.duration = highest;
            }
        }
        self.changed(&mut s);
        drop(s);
        self.push(MseEvent::Ended);
    }

    /// The duration the media side has (NaN: none).
    pub fn duration(&self) -> f64 {
        self.state().duration
    }

    /// Sets the duration (infinite or NaN: unknown).
    pub fn set_duration(&self, d: f64) {
        let mut s = self.state();
        s.duration = d;
        self.changed(&mut s);
    }
}

#[cfg(test)]
#[path = "mse_tests.rs"]
mod tests;
