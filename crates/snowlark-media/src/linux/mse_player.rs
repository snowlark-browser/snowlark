// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The player's side of a Media Source Extensions source ([`super::mse`]): the decoders FFmpeg
//! configures from each `SourceBuffer`'s initialization segment, and a packet source that hands
//! the track buffers' coded frames to them in decode order.
//!
//! A track is read from a position: after a seek, from the last random access point at or before
//! the seek's time in the buffered range that holds it; then frame after frame by decode time, so
//! frames appended ahead of the position are read as they come and frames removed behind it don't
//! matter. When a track has nothing buffered at its position the source is **pending** (not at its
//! end): the player waits for the page to append, as MSE §2.4.4 "seeking" and the playback's
//! "waiting" have it. The end comes only once the source has ended (`endOfStream()`) and every
//! track has handed out its last frame.

use std::time::Duration;

use super::ffmpeg::{self, Demuxer, Kind, Memory, Packet};
use super::mse::{MseSource, Ready, State, TrackBuffer, TICKS};
use super::player::{Next, Packets, Stream};

/// The packets' time base: the track buffers' 100 ns units.
const TIME_BASE: ffmpeg::Rational = ffmpeg::Rational {
    num: 1,
    den: 10_000_000,
};
/// How far after a seek's time a buffered range may start and still be the one it plays (a
/// frame or so, as the page's own check allows).
const FUDGE: i64 = 1_000_000;
/// Data that starts this close to zero is the start of the stream: a seek to before it plays
/// from there.
const START_FUDGE: i64 = 10_000_000;

/// Whether the player can open the source: every buffer has had its first initialization segment
/// and the page has run "initialization segment received" for one (it sets the duration).
pub(super) fn ready_to_open(src: &MseSource) -> bool {
    let s = src.state();
    !s.buffers.is_empty() && s.buffers.iter().all(|b| b.init.is_some()) && !s.duration.is_nan()
}

/// The decoders of the first video and the first audio track across the buffers, and the reader
/// of their track buffers.
pub(super) struct Opened {
    pub video: Option<Stream>,
    pub audio: Option<Stream>,
    pub reader: MseReader,
}

/// The stream index the packets of each kind carry.
const VIDEO: usize = 0;
const AUDIO: usize = 1;

pub(super) fn open(ff: &'static ffmpeg::Ffmpeg, src: MseSource) -> Result<Opened, String> {
    let inits: Vec<(u32, Vec<u8>)> = {
        let s = src.state();
        s.buffers
            .iter()
            .filter_map(|b| b.init.clone().map(|m| (b.id, m)))
            .collect()
    };
    let (mut video, mut audio) = (None, None);
    let mut cursors = Vec::new();
    for (id, moov) in inits {
        let demuxer = Demuxer::open(ff, Box::new(Memory(moov.into())))?;
        // (FFmpeg's streams and the track buffers are both in the `moov`'s track order: they are
        // matched by order within each kind)
        let mut nth = [0usize; 2];
        for st in demuxer.streams() {
            let is_video = match st.kind {
                Kind::Video => true,
                Kind::Audio => false,
                Kind::Other => continue,
            };
            let n = nth[is_video as usize];
            nth[is_video as usize] += 1;
            let slot = if is_video { &mut video } else { &mut audio };
            if slot.is_some() {
                continue;
            }
            let index = if is_video { VIDEO } else { AUDIO };
            *slot = Some(Stream {
                decoder: demuxer.decoder(st.index)?,
                index,
                time_base: TIME_BASE,
                width: st.width,
                height: st.height,
            });
            cursors.push(Cursor {
                buffer: id,
                video: is_video,
                nth: n,
                stream: index,
                pos: Pos::Seek(0),
                removals: 0,
                done: false,
            });
        }
    }
    if video.is_none() && audio.is_none() {
        return Err("no audio or video track".into());
    }
    Ok(Opened {
        video,
        audio,
        reader: MseReader {
            ff,
            src,
            cursors,
            version: 0,
        },
    })
}

/// Where a track is read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pos {
    /// A seek to this time, not placed yet (nothing buffered there so far).
    Seek(i64),
    /// From the frame with this decode time (a random access point) on.
    From(i64),
    /// After the frame with this decode time, the last handed out.
    After(i64),
}

/// One track being read.
struct Cursor {
    /// The source buffer, and the track buffer's place among those of its kind there.
    buffer: u32,
    video: bool,
    nth: usize,
    /// The index its packets carry.
    stream: usize,
    pos: Pos,
    /// The track buffer's removal count when the last frame was handed out.
    removals: u64,
    /// Every frame handed out, and the source has ended.
    done: bool,
}

/// The track buffers as [`Packets`].
pub(super) struct MseReader {
    ff: &'static ffmpeg::Ffmpeg,
    src: MseSource,
    cursors: Vec<Cursor>,
    /// The source's version when `next` last found nothing.
    version: u64,
}

fn track<'a>(s: &'a State, c: &Cursor) -> Option<&'a TrackBuffer> {
    s.buffers
        .iter()
        .find(|b| b.id == c.buffer)?
        .track_buffers
        .iter()
        .filter(|t| t.video == c.video)
        .nth(c.nth)
}

/// The decode time of the random access point a seek to `t` starts from: the last at or before
/// `t` in the buffered range that holds `t` (or starts just after it); none when nothing is
/// buffered there.
fn seek_point(tb: &TrackBuffer, t: i64) -> Option<i64> {
    let ranges = tb.ranges();
    let &(start, _) = match ranges.first() {
        // (a stream whose data starts a little after zero, played from its start)
        Some(first) if t <= first.0 && first.0 < START_FUDGE => first,
        _ => ranges.iter().find(|&&(s, e)| s - FUDGE <= t && t < e)?,
    };
    let anchor = t.max(start);
    tb.frames
        .iter()
        .filter(|f| f.key && f.pts >= start && f.pts <= anchor)
        .max_by_key(|f| f.pts)
        .map(|f| f.dts)
}

/// The index of the cursor's next frame in `tb` (placing a pending seek); none when it has none
/// buffered yet.
fn next_frame(tb: &TrackBuffer, c: &mut Cursor) -> Option<usize> {
    let frames = &tb.frames;
    loop {
        match c.pos {
            Pos::Seek(t) => c.pos = Pos::From(seek_point(tb, t)?),
            Pos::From(d) => {
                let i = frames.partition_point(|f| f.dts < d);
                return (i < frames.len()).then_some(i);
            }
            Pos::After(d) => {
                let i = frames.partition_point(|f| f.dts <= d);
                let kept = i > 0 && frames[i - 1].dts == d;
                if c.removals != tb.removals && !kept {
                    // (the frame handed out last went, with what depended on it: on from the next
                    // random access point)
                    let k = frames[i..].iter().position(|f| f.key)? + i;
                    c.pos = Pos::From(frames[k].dts);
                    return Some(k);
                }
                return (i < frames.len()).then_some(i);
            }
        }
    }
}

impl Packets for MseReader {
    fn next(&mut self) -> Next {
        let s = self.src.state();
        self.version = s.version;
        if s.ready == Ready::Closed {
            return Next::End;
        }
        let ended = s.ready == Ready::Ended;
        // The earliest next frame by decode time across the tracks, as a demuxer interleaves
        // them; nothing while any track still waits for data.
        let mut best: Option<(usize, usize, i64)> = None;
        let mut pending = false;
        for (ci, c) in self.cursors.iter_mut().enumerate() {
            if c.done {
                continue;
            }
            let Some(tb) = track(&s, c) else {
                c.done = true; // (its buffer was removed)
                continue;
            };
            match next_frame(tb, c) {
                Some(i) => {
                    let dts = tb.frames[i].dts;
                    if best.is_none_or(|b| dts < b.2) {
                        best = Some((ci, i, dts));
                    }
                }
                None if ended => c.done = true,
                None => pending = true,
            }
        }
        if pending {
            return Next::Pending;
        }
        let Some((ci, i, _)) = best else {
            return Next::End;
        };
        let c = &mut self.cursors[ci];
        let Some(tb) = track(&s, c) else {
            return Next::Pending;
        };
        let f = &tb.frames[i];
        c.pos = Pos::After(f.dts);
        c.removals = tb.removals;
        match Packet::from_data(self.ff, &f.data, c.stream, f.pts, f.dts, f.key) {
            Some(p) => Next::Packet(p),
            None => Next::End,
        }
    }

    fn seek(&mut self, t: f64) {
        let t = (t.max(0.0) * TICKS).round() as i64;
        for c in &mut self.cursors {
            c.pos = Pos::Seek(t);
            c.done = false;
        }
    }

    fn wait(&mut self) {
        self.version = self
            .src
            .wait_change(self.version, Duration::from_millis(50));
    }
}

#[cfg(test)]
#[path = "mse_player_tests.rs"]
mod tests;
