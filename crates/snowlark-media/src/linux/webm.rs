// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WebM byte stream format of Media Source Extensions (the MSE Byte Stream Format Registry's
//! "WebM Byte Stream Format"): an initialization segment is the EBML header and the `Segment`'s
//! elements up to and including `Tracks`; a media segment is one `Cluster`, whose `SimpleBlock`s
//! and `BlockGroup`s are the coded frames.
//!
//! A [`WebmReader`] follows one `SourceBuffer`'s bytes element by element, inside a cluster block
//! by block, so that the frames of a cluster appended in parts come out as their blocks arrive. It
//! hands out the same [`Output`]s as the ISO BMFF reader: the tracks of each initialization segment
//! (the first video and the first audio track; others are ignored), then the coded frames in
//! stream order, decode time equal to presentation time.
//!
//! A frame's duration is, in order: the one an Opus packet's table of contents gives (RFC 6716
//! §3.1), the block group's `BlockDuration`, the track's `DefaultDuration`, the distance to the
//! track's next block in the same cluster (the frame is held back until that block or the end of
//! the cluster), and at the end of a cluster the longest duration seen on the track (23 ms for
//! sound, 63 ms for video before any). Laced blocks are not taken, nor tracks numbered above 127.

use crate::fmp4::{Codec, Output, Sample, Track};
use crate::init_segment::{elements, read_id, read_size, uint};

const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const VOID: u32 = 0xEC;
const CRC32: u32 = 0xBF;
const CUES: u32 = 0x1C53_BB6B;
const CHAPTERS: u32 = 0x1043_A770;
const TAGS: u32 = 0x1254_C367;
const ATTACHMENTS: u32 = 0x1941_A469;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;

const TIMECODE_SCALE: u32 = 0x2A_D7B1;

const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const DEFAULT_DURATION: u32 = 0x23_E383;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const CONTENT_ENCODINGS: u32 = 0x6D80;

const TIMECODE: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;
const REFERENCE_BLOCK: u32 = 0xFB;

/// The estimated duration of the last block of a cluster on a track with no duration seen yet, in
/// 100 ns units.
const AUDIO_ESTIMATE: i64 = 230_000;
const VIDEO_ESTIMATE: i64 = 630_000;

/// An empty cluster: ends the initialization segment handed to the demuxer.
const EMPTY_CLUSTER: [u8; 8] = [0x1F, 0x43, 0xB6, 0x75, 0x83, 0xE7, 0x81, 0x00];
/// A `Segment` element's header of unknown size.
const SEGMENT_HEADER: [u8; 12] = [
    0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];
/// The smallest EBML header of a WebM file (`DocType` "webm"), for a stream that had none.
const MINIMAL_EBML: [u8; 12] = [
    0x1A, 0x45, 0xDF, 0xA3, 0x87, 0x42, 0x82, 0x84, b'w', b'e', b'b', b'm',
];

/// One track a block may belong to.
#[derive(Debug)]
struct TrackState {
    number: u32,
    video: bool,
    opus: bool,
    encrypted: bool,
    /// `DefaultDuration`, in 100 ns units.
    default_duration: Option<i64>,
    /// The longest duration of a frame handed out, in 100 ns units.
    longest: Option<i64>,
    /// A frame waiting for its duration (the track's next block in the cluster).
    held: Option<Sample>,
}

impl TrackState {
    /// Hands `s` out, or holds it back when it has no duration yet; first gives the frame held
    /// back its duration, the distance to this one.
    fn add(&mut self, mut s: Sample, duration: Option<i64>, out: &mut Vec<Output>) {
        if let Some(mut h) = self.held.take() {
            h.duration = s.time - h.time;
            self.queue(h, out);
        }
        match duration {
            Some(d) => {
                s.duration = d;
                self.queue(s, out);
            }
            None => self.held = Some(s),
        }
    }

    fn queue(&mut self, s: Sample, out: &mut Vec<Output>) {
        if s.duration > 0 {
            self.longest = Some(self.longest.map_or(s.duration, |l| l.max(s.duration)));
        }
        out.push(Output::Sample(self.number, s));
    }

    /// The end of a cluster: a frame still held back gets the estimated duration.
    fn end_cluster(&mut self, out: &mut Vec<Output>) {
        if let Some(mut h) = self.held.take() {
            h.duration = self.longest.unwrap_or(if self.video {
                VIDEO_ESTIMATE
            } else {
                AUDIO_ESTIMATE
            });
            // (not taken as the longest: an estimate)
            out.push(Output::Sample(self.number, h));
        }
    }
}

/// Where the reader is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    /// Between segments: the EBML header, `Segment`, `Info`, `Tracks` and the elements skipped
    /// between them, or a cluster.
    Headers,
    /// In a cluster: the bytes of it still to come (`None`: of unknown size).
    Cluster(Option<u64>),
}

/// One `SourceBuffer`'s WebM byte stream (see the module).
pub(crate) struct WebmReader {
    /// Bytes of an element this must see whole.
    pending: Vec<u8>,
    /// Bytes still to pass of an element this does not read.
    skip: u64,
    level: Level,
    /// `TimecodeScale` in nanoseconds.
    scale: u64,
    /// The last EBML header and the current segment's `Info`, as received.
    ebml: Vec<u8>,
    info: Option<Vec<u8>>,
    /// The last initialization segment, as the demuxer opens it.
    init: Vec<u8>,
    tracks: Vec<TrackState>,
    /// Tracks of the initialization segment whose blocks are dropped.
    ignored: Vec<u32>,
    /// The cluster's `Timecode`, and the last block's timecode in it.
    cluster_time: Option<u64>,
    last_block: Option<i16>,
    /// Clusters started.
    segments: u64,
    failed: bool,
}

impl WebmReader {
    pub(crate) fn new() -> WebmReader {
        WebmReader {
            pending: Vec::new(),
            skip: 0,
            level: Level::Headers,
            scale: 1_000_000,
            ebml: Vec::new(),
            info: None,
            init: Vec::new(),
            tracks: Vec::new(),
            ignored: Vec::new(),
            cluster_time: None,
            last_block: None,
            segments: 0,
            failed: false,
        }
    }

    /// The last initialization segment, as a WebM file the demuxer opens (empty before one).
    pub(crate) fn init(&self) -> &[u8] {
        &self.init
    }

    /// How many media segments (clusters) it has started.
    pub(crate) fn segments(&self) -> u64 {
        self.segments
    }

    /// Whether the bytes were not a WebM byte stream (MSE's "append error").
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    /// Whether the reader is between segments with nothing partly received.
    pub(crate) fn at_boundary(&self) -> bool {
        self.level == Level::Headers && self.pending.is_empty() && self.skip == 0
    }

    /// Forgets a partly received element and the frames held back of the cluster (the
    /// `SourceBuffer`'s "reset parser state"); the tracks of the last initialization segment stay.
    pub(crate) fn reset(&mut self) {
        self.pending.clear();
        self.skip = 0;
        self.failed = false;
        self.level = Level::Headers;
        self.cluster_time = None;
        self.last_block = None;
        for t in &mut self.tracks {
            t.held = None;
        }
    }

    /// Takes appended bytes; returns the tracks and frames completed by them, in stream order.
    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<Output> {
        let mut out = Vec::new();
        if self.failed {
            return out;
        }
        let mut data = data;
        if self.skip > 0 {
            let n = (self.skip.min(data.len() as u64)) as usize;
            self.skip -= n as u64;
            data = &data[n..];
        }
        self.pending.extend_from_slice(data);
        let mut at = 0;
        loop {
            if let Level::Cluster(Some(0)) = self.level {
                self.end_cluster(&mut out);
            }
            if at == self.pending.len() {
                break;
            }
            let buf = std::mem::take(&mut self.pending);
            // (a cluster's own header is not part of its body)
            let in_cluster = self.level != Level::Headers;
            let step = self.step(&buf[at..], &mut out);
            self.pending = buf;
            match step {
                Step::Used(n) => {
                    if let (true, Level::Cluster(Some(left))) = (in_cluster, &mut self.level) {
                        *left = left.saturating_sub(n as u64);
                    }
                    at += n;
                }
                Step::Skip(n, rest) => {
                    if let (true, Level::Cluster(Some(left))) = (in_cluster, &mut self.level) {
                        *left = left.saturating_sub(n as u64 + rest);
                    }
                    at += n;
                    self.skip = rest;
                }
                Step::Again => {}
                Step::Need => break,
                Step::Fail => {
                    self.failed = true;
                    break;
                }
            }
        }
        self.pending.drain(..at);
        if self.failed {
            self.pending.clear();
        }
        out
    }

    /// One element at the start of `b`.
    fn step(&mut self, b: &[u8], out: &mut Vec<Output>) -> Step {
        let Some((id, il)) = read_id(b) else {
            return if b.len() >= 4 { Step::Fail } else { Step::Need };
        };
        let Some((size, sl)) = read_size(&b[il..]) else {
            return if b.len() >= il + 8 {
                Step::Fail
            } else {
                Step::Need
            };
        };
        let header = il + sl;
        match self.level {
            Level::Headers => self.header_element(b, id, size, header, out),
            Level::Cluster(left) => {
                if left.is_none() && ends_unknown_cluster(id) {
                    // (an element of a higher level ends a cluster of unknown size)
                    self.end_cluster(out);
                    return Step::Again;
                }
                let Some(size) = size else {
                    return Step::Fail;
                };
                if left.is_some_and(|l| header as u64 + size > l) {
                    return Step::Fail;
                }
                self.cluster_element(b, id, size, header, out)
            }
        }
    }

    fn header_element(
        &mut self,
        b: &[u8],
        id: u32,
        size: Option<u64>,
        header: usize,
        out: &mut Vec<Output>,
    ) -> Step {
        match id {
            SEGMENT => {
                self.info = None;
                Step::Used(header)
            }
            CLUSTER => {
                if self.init.is_empty() {
                    // (a cluster before any initialization segment)
                    return Step::Fail;
                }
                self.segments += 1;
                self.cluster_time = None;
                self.last_block = None;
                self.level = Level::Cluster(size);
                Step::Used(header)
            }
            EBML_HEADER | INFO | TRACKS => {
                let Some(size) = size else {
                    return Step::Fail;
                };
                let end = header as u64 + size;
                if (b.len() as u64) < end {
                    return Step::Need;
                }
                let element = &b[..end as usize];
                let body = &b[header..end as usize];
                match id {
                    EBML_HEADER => self.ebml = element.to_vec(),
                    INFO => {
                        self.scale = elements(body)
                            .find(|&(i, _)| i == TIMECODE_SCALE)
                            .map(|(_, v)| uint(v))
                            .filter(|&s| s > 0)
                            .unwrap_or(1_000_000);
                        self.info = Some(element.to_vec());
                    }
                    _ => {
                        let Some(info) = self.info.clone() else {
                            // (`Tracks` before the segment's `Info`)
                            return Step::Fail;
                        };
                        let Some(tracks) = self.tracks(body) else {
                            return Step::Fail;
                        };
                        let ebml = if self.ebml.is_empty() {
                            &MINIMAL_EBML[..]
                        } else {
                            &self.ebml
                        };
                        let mut init = ebml.to_vec();
                        init.extend_from_slice(&SEGMENT_HEADER);
                        init.extend_from_slice(&info);
                        init.extend_from_slice(element);
                        init.extend_from_slice(&EMPTY_CLUSTER);
                        self.init = init;
                        out.push(Output::Tracks(tracks));
                    }
                }
                Step::Used(end as usize)
            }
            SEEK_HEAD | VOID | CRC32 | CUES | CHAPTERS | TAGS | ATTACHMENTS => {
                let Some(size) = size else {
                    return Step::Fail;
                };
                skip(b, header, size)
            }
            _ => Step::Fail,
        }
    }

    fn cluster_element(
        &mut self,
        b: &[u8],
        id: u32,
        size: u64,
        header: usize,
        out: &mut Vec<Output>,
    ) -> Step {
        match id {
            TIMECODE | SIMPLE_BLOCK | BLOCK_GROUP => {
                let end = header as u64 + size;
                if (b.len() as u64) < end {
                    return Step::Need;
                }
                let body = &b[header..end as usize];
                let ok = match id {
                    TIMECODE => {
                        let first = self.cluster_time.is_none();
                        self.cluster_time = Some(uint(body));
                        first
                    }
                    SIMPLE_BLOCK => self.block(body, true, None, false, out),
                    _ => self.block_group(body, out),
                };
                if ok {
                    Step::Used(end as usize)
                } else {
                    Step::Fail
                }
            }
            _ => skip(b, header, size),
        }
    }

    /// A `BlockGroup`: its one `Block`, with the group's `BlockDuration`; a key frame when it
    /// references no other block.
    fn block_group(&mut self, body: &[u8], out: &mut Vec<Output>) -> bool {
        let mut block = None;
        let mut duration = None;
        let mut referenced = false;
        for (id, v) in elements(body) {
            match id {
                BLOCK if block.is_some() => return false,
                BLOCK => block = Some(v),
                BLOCK_DURATION => duration = Some(uint(v)),
                REFERENCE_BLOCK => referenced = true,
                _ => {}
            }
        }
        let Some(block) = block else {
            return false;
        };
        self.block(block, false, duration, referenced, out)
    }

    /// One `SimpleBlock` or `Block`.
    fn block(
        &mut self,
        b: &[u8],
        simple: bool,
        block_duration: Option<u64>,
        referenced: bool,
        out: &mut Vec<Output>,
    ) -> bool {
        if b.len() < 4 || b[0] & 0x80 == 0 {
            // (a track number above 127)
            return false;
        }
        let number = (b[0] & 0x7F) as u32;
        let timecode = i16::from_be_bytes([b[1], b[2]]);
        let flags = b[3];
        if (flags >> 1) & 3 != 0 {
            // (laced)
            return false;
        }
        let key = if simple {
            flags & 0x80 != 0
        } else {
            !referenced
        };
        let Some(cluster) = self.cluster_time else {
            return false;
        };
        if self.last_block.is_some_and(|l| timecode < l) {
            return false;
        }
        let Some(ti) = self.tracks.iter().position(|t| t.number == number) else {
            return self.ignored.contains(&number);
        };
        self.last_block = Some(timecode);
        let Some(ns) = (cluster as i128 + timecode as i128).checked_mul(self.scale as i128) else {
            return false;
        };
        let time = (ns / 100) as i64;
        let scale = self.scale;
        let t = &mut self.tracks[ti];
        let mut data = &b[4..];
        if t.encrypted {
            // (WebM encryption: a signal byte, its low bit set when the frame is encrypted)
            match data.first() {
                Some(signal) if signal & 1 == 0 => data = &data[1..],
                Some(_) => {}
                None => return false,
            }
        }
        let duration = if t.opus && !t.encrypted {
            opus_duration(data)
        } else {
            None
        }
        .or(block_duration.map(|d| (d as i128 * scale as i128 / 100) as i64))
        .or(t.default_duration);
        let sample = Sample {
            data: data.to_vec(),
            time,
            duration: 0,
            decode: time,
            key,
            encryption: None,
        };
        t.add(sample, duration, out);
        true
    }

    fn end_cluster(&mut self, out: &mut Vec<Output>) {
        for t in &mut self.tracks {
            t.end_cluster(out);
        }
        self.level = Level::Headers;
        self.cluster_time = None;
        self.last_block = None;
    }

    /// `Tracks`: the first video and the first audio track (each other track's blocks are
    /// dropped); none when a track has no number, or the codec of a video or audio track is not
    /// one WebM carries.
    fn tracks(&mut self, body: &[u8]) -> Option<Vec<Track>> {
        let mut tracks = Vec::new();
        let mut states = Vec::new();
        let mut ignored = Vec::new();
        for (id, entry) in elements(body) {
            if id != TRACK_ENTRY {
                continue;
            }
            let field = |want: u32| elements(entry).find(|&(i, _)| i == want).map(|(_, v)| v);
            let number = field(TRACK_NUMBER).map(uint).filter(|&n| n > 0)? as u32;
            let video = match field(TRACK_TYPE).map(uint) {
                Some(1) => true,
                Some(2) => false,
                _ => {
                    ignored.push(number);
                    continue;
                }
            };
            let codec_id = field(CODEC_ID).unwrap_or(&[]);
            let fourcc: [u8; 4] = match (video, codec_id) {
                (true, b"V_VP8") => *b"vp08",
                (true, b"V_VP9") => *b"vp09",
                (true, b"V_AV1") => *b"av01",
                (false, b"A_OPUS") => *b"Opus",
                (false, b"A_VORBIS") => *b"vorb",
                // (a codec WebM does not carry: the buffer refuses the segment)
                _ => [0; 4],
            };
            if tracks.iter().any(|t: &Track| t.video == video) {
                ignored.push(number);
                continue;
            }
            let (mut width, mut height, mut channels, mut sample_rate) = (0, 0, 1, 8000);
            if let Some(v) = field(VIDEO) {
                for (i, x) in elements(v) {
                    match i {
                        PIXEL_WIDTH => width = uint(x) as u16,
                        PIXEL_HEIGHT => height = uint(x) as u16,
                        _ => {}
                    }
                }
            }
            if let Some(a) = field(AUDIO) {
                for (i, x) in elements(a) {
                    match i {
                        CHANNELS => channels = uint(x) as u16,
                        SAMPLING_FREQUENCY => {
                            sample_rate = float(x).map_or(sample_rate, |f| f as u32)
                        }
                        _ => {}
                    }
                }
            }
            tracks.push(Track {
                id: number,
                video,
                timescale: 0,
                codec: Codec::Other(fourcc),
                width,
                height,
                channels,
                sample_rate,
                start: 0,
                encryption: None,
            });
            states.push(TrackState {
                number,
                video,
                opus: fourcc == *b"Opus",
                encrypted: field(CONTENT_ENCODINGS).is_some(),
                default_duration: field(DEFAULT_DURATION)
                    .map(uint)
                    .filter(|&d| d > 0)
                    .map(|d| (d / 100) as i64),
                longest: None,
                held: None,
            });
        }
        self.tracks = states;
        self.ignored = ignored;
        Some(tracks)
    }
}

enum Step {
    /// The element (or its header) was taken: this many bytes.
    Used(usize),
    /// This many bytes taken, and this many more of the element to pass.
    Skip(usize, u64),
    /// Nothing taken, the level changed: again.
    Again,
    Need,
    Fail,
}

/// Passes an element whose body is not read.
fn skip(b: &[u8], header: usize, size: u64) -> Step {
    let end = header as u64 + size;
    if (b.len() as u64) >= end {
        Step::Used(end as usize)
    } else {
        Step::Skip(b.len(), end - b.len() as u64)
    }
}

/// Whether an element of this id, met in a cluster of unknown size, is not one of its children
/// (Matroska's level 0 and 1 elements): the cluster has ended.
fn ends_unknown_cluster(id: u32) -> bool {
    matches!(
        id,
        CLUSTER
            | CUES
            | TAGS
            | SEEK_HEAD
            | CHAPTERS
            | ATTACHMENTS
            | INFO
            | TRACKS
            | EBML_HEADER
            | SEGMENT
    )
}

fn float(b: &[u8]) -> Option<f64> {
    match b.len() {
        4 => Some(f32::from_be_bytes(b.try_into().ok()?) as f64),
        8 => Some(f64::from_be_bytes(b.try_into().ok()?)),
        _ => None,
    }
}

/// The duration of an Opus packet from its table of contents (RFC 6716 §3.1), in 100 ns units.
fn opus_duration(p: &[u8]) -> Option<i64> {
    // Frame durations of the 32 configurations, in 100 ns units: SILK (0-11), hybrid (12-15),
    // CELT (16-31).
    const FRAME: [i64; 32] = [
        100_000, 200_000, 400_000, 600_000, 100_000, 200_000, 400_000, 600_000, 100_000, 200_000,
        400_000, 600_000, 100_000, 200_000, 100_000, 200_000, 25_000, 50_000, 100_000, 200_000,
        25_000, 50_000, 100_000, 200_000, 25_000, 50_000, 100_000, 200_000, 25_000, 50_000,
        100_000, 200_000,
    ];
    let toc = *p.first()?;
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => match p.get(1)? & 0x3F {
            0 => return None,
            n => n as i64,
        },
    };
    Some(FRAME[(toc >> 3) as usize] * frames)
}

#[cfg(test)]
#[path = "webm_tests.rs"]
pub(crate) mod tests;
