// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Finds the first initialization segment in the bytes appended to a `SourceBuffer` (MSE §5.5.7,
//! "Initialization Segment Received", run when the segment parser loop has parsed a complete
//! initialization segment), and reads from it what the media element needs for its metadata: the
//! duration and the video's natural size.
//!
//! Media Foundation's `MseStreamSource` parses the segments for playback but tells nobody when an
//! initialization segment arrives; its player only reports the media as opened once it has data at
//! the playback position. Chromium reports metadata from the initialization segments alone
//! (`ChunkDemuxer::OnSourceInitDone` once every source buffer has one, then
//! `PipelineImpl::RendererWrapper::Start`: `InitializeDemuxer`, `ReportMetadata`, and only then the
//! renderer's preroll), so a page whose data starts after 0 still gets `loadedmetadata` and can
//! seek. This scanner is that first step.
//!
//! Formats (MSE byte stream formats): ISO BMFF, where the initialization segment is `ftyp` and
//! `moov` (read as Chromium's `MP4StreamParser::ParseMoov`), and WebM, where it is the EBML header
//! and the `Segment` up to and including `Tracks` (Chromium's `WebMStreamParser::ParseInfoAndTracks`).

/// The byte stream format of a `SourceBuffer` (from the type given to `addSourceBuffer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    Mp4,
    WebM,
}

impl Container {
    /// The format of a MIME type such as `video/webm; codecs="vp9"` (`None` for others).
    pub fn of_type(mime: &str) -> Option<Container> {
        let essence = mime
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match essence.as_str() {
            "video/mp4" | "audio/mp4" => Some(Container::Mp4),
            "video/webm" | "audio/webm" => Some(Container::WebM),
            _ => None,
        }
    }
}

/// What an initialization segment says.
#[derive(Clone, Debug, PartialEq)]
pub struct InitSegment {
    /// The duration in seconds, when it gives one.
    pub duration: Option<f64>,
    /// The natural size of its first video track.
    pub video_size: Option<(u32, u32)>,
    /// The number of audio, video and text tracks.
    pub tracks: u32,
}

/// The most bytes kept while looking for the initialization segment (a `moov` or `Tracks` of more
/// than this is not looked for).
const MAX_KEPT: usize = 4 << 20;

/// Reads appended bytes until the first initialization segment is complete.
#[derive(Debug)]
pub struct InitScanner {
    container: Container,
    /// Bytes of the element or box being read (its header, or all of it when it is kept).
    buf: Vec<u8>,
    /// Bytes still to be skipped of an element or box not kept.
    skip: u64,
    /// WebM: inside the `Segment` element.
    in_segment: bool,
    /// WebM: the duration from `Info`, in seconds.
    duration: Option<f64>,
    /// Gave up: the bytes are not a stream of this format, or the segment is too large.
    failed: bool,
}

impl InitScanner {
    pub fn new(container: Container) -> InitScanner {
        InitScanner {
            container,
            buf: Vec::new(),
            skip: 0,
            in_segment: false,
            duration: None,
            failed: false,
        }
    }

    /// Whether it gave up on these bytes.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// MSE "reset parser state": the bytes not yet parsed are dropped, and the next append starts
    /// at a segment boundary.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.skip = 0;
        self.in_segment = false;
        self.duration = None;
        self.failed = false;
    }

    /// Takes the bytes of an append; returns the initialization segment when it ends in them.
    pub fn push(&mut self, mut data: &[u8]) -> Option<InitSegment> {
        while !self.failed && !data.is_empty() {
            if self.skip > 0 {
                let n = (self.skip.min(data.len() as u64)) as usize;
                self.skip -= n as u64;
                data = &data[n..];
                continue;
            }
            // Read into `buf` up to what the current element needs.
            let need = match self.needed() {
                Need::More(n) => n,
                Need::Failed => {
                    self.failed = true;
                    break;
                }
                Need::Done(r) => return Some(r),
            };
            let take = need.saturating_sub(self.buf.len()).min(data.len());
            if take == 0 {
                // `needed` asks for no more bytes only when it consumed `buf` itself.
                continue;
            }
            if self.buf.len() + take > MAX_KEPT {
                self.failed = true;
                break;
            }
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
        if self.failed {
            return None;
        }
        match self.needed() {
            Need::Done(r) => Some(r),
            _ => None,
        }
    }

    /// Steps over whole elements in `buf`: how many bytes `buf` must hold to go on, or the result.
    fn needed(&mut self) -> Need {
        loop {
            let step = match self.container {
                Container::Mp4 => self.mp4_step(),
                Container::WebM => self.webm_step(),
            };
            match step {
                // Bytes to skip first: no more for `buf` until they are.
                Step::Again if self.skip > 0 => return Need::More(0),
                Step::Again => continue,
                Step::Need(n) => return Need::More(n),
                Step::Failed => return Need::Failed,
                Step::Done(r) => return Need::Done(r),
            }
        }
    }

    fn mp4_step(&mut self) -> Step {
        let b = &self.buf;
        if b.len() < 8 {
            return Step::Need(8);
        }
        let size32 = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
        let kind = [b[4], b[5], b[6], b[7]];
        let (size, header) = match size32 {
            1 => {
                if b.len() < 16 {
                    return Step::Need(16);
                }
                (u64::from_be_bytes(b[8..16].try_into().unwrap()), 16)
            }
            // A box to the end of the file: not in an MSE byte stream before `moov`.
            0 => return Step::Failed,
            s => (s, 8),
        };
        if size < header as u64 {
            return Step::Failed;
        }
        if &kind == b"moov" {
            if size as usize > MAX_KEPT {
                return Step::Failed;
            }
            if b.len() < size as usize {
                return Step::Need(size as usize);
            }
            let moov = &b[header..size as usize];
            return match parse_moov(moov) {
                Some(r) => Step::Done(r),
                None => Step::Failed,
            };
        }
        if &kind == b"moof" || &kind == b"mdat" {
            // A media segment before any initialization segment.
            return Step::Failed;
        }
        // `ftyp`, `free`, `sidx`, ...: skipped.
        self.skip = size - b.len() as u64;
        self.buf.clear();
        Step::Again
    }

    fn webm_step(&mut self) -> Step {
        let b = &self.buf;
        let Some((id, id_len)) = read_id(b) else {
            return if b.len() >= 4 {
                Step::Failed
            } else {
                Step::Need(b.len() + 1)
            };
        };
        let Some((size, size_len)) = read_size(&b[id_len..]) else {
            return if b.len() >= id_len + 8 {
                Step::Failed
            } else {
                Step::Need(b.len() + 1)
            };
        };
        let header = id_len + size_len;
        match (self.in_segment, id) {
            (false, EBML) => {}
            (false, SEGMENT) => {
                self.in_segment = true;
                self.buf.clear();
                return Step::Again;
            }
            (false, _) => return Step::Failed,
            (true, CLUSTER) => return Step::Failed,
            (true, INFO) | (true, TRACKS) => {
                let Some(size) = size else {
                    return Step::Failed;
                };
                let end = header as u64 + size;
                if end as usize > MAX_KEPT {
                    return Step::Failed;
                }
                if b.len() < end as usize {
                    return Step::Need(end as usize);
                }
                let body = &b[header..end as usize];
                if id == INFO {
                    self.duration = webm_duration(body);
                    self.buf.clear();
                    return Step::Again;
                }
                return match webm_tracks(body) {
                    Some((tracks, video_size)) => Step::Done(InitSegment {
                        duration: self.duration,
                        video_size,
                        tracks,
                    }),
                    None => Step::Failed,
                };
            }
            _ => {}
        }
        // The EBML header, `SeekHead`, `Void`, `Tags`, ...: skipped.
        let Some(size) = size else {
            return Step::Failed;
        };
        self.skip = size - (b.len() - header) as u64;
        self.buf.clear();
        Step::Again
    }
}

enum Need {
    More(usize),
    Failed,
    Done(InitSegment),
}

enum Step {
    Again,
    Need(usize),
    Failed,
    Done(InitSegment),
}

// ISO BMFF (ISO/IEC 14496-12).

/// The boxes in `data` as (type, body).
fn boxes(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        if data.len() < 8 {
            return None;
        }
        let size32 = u32::from_be_bytes(data[0..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = data[4..8].try_into().unwrap();
        let (size, header) = match size32 {
            1 if data.len() >= 16 => (u64::from_be_bytes(data[8..16].try_into().unwrap()), 16),
            1 => return None,
            0 => (data.len() as u64, 8),
            s => (s, 8),
        };
        if size < header as u64 || size > data.len() as u64 {
            return None;
        }
        let body = &data[header..size as usize];
        data = &data[size as usize..];
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

/// `moov` as Chromium's `MP4StreamParser::ParseMoov` reads it: the duration from `mvex`'s
/// `mehd` when it is not 0, else from `mvhd` when not 0 and not all ones (14496-12 8.2.2.3: "If the
/// duration cannot be determined then duration is set to all 1s"); the natural size of the first
/// video track from its sample entry's size and `pasp`, else the track header's size.
fn parse_moov(moov: &[u8]) -> Option<InitSegment> {
    let mvhd = child(moov, b"mvhd")?;
    let version = *mvhd.first()?;
    let (timescale, duration, all_ones) = if version == 1 {
        (be32(mvhd, 20)?, be64(mvhd, 24)?, u64::MAX)
    } else {
        (be32(mvhd, 12)?, be32(mvhd, 16)? as u64, u32::MAX as u64)
    };
    let fragment_duration = child(moov, b"mvex")
        .and_then(|mvex| child(mvex, b"mehd"))
        .and_then(|mehd| {
            if *mehd.first()? == 1 {
                be64(mehd, 4)
            } else {
                be32(mehd, 4).map(u64::from)
            }
        })
        .unwrap_or(0);
    let duration = if timescale == 0 {
        None
    } else if fragment_duration > 0 {
        Some(fragment_duration as f64 / timescale as f64)
    } else if duration > 0 && duration != all_ones {
        Some(duration as f64 / timescale as f64)
    } else {
        None
    };
    let mut tracks = 0;
    let mut video_size = None;
    for (kind, trak) in boxes(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else {
            continue;
        };
        let handler = child(mdia, b"hdlr").and_then(|h| h.get(8..12));
        match handler {
            Some(b"soun") | Some(b"text") | Some(b"subt") | Some(b"sbtl") => tracks += 1,
            Some(b"vide") => {
                tracks += 1;
                if video_size.is_none() {
                    video_size = mp4_video_size(trak, mdia);
                }
            }
            _ => {}
        }
    }
    Some(InitSegment {
        duration,
        video_size,
        tracks,
    })
}

fn mp4_video_size(trak: &[u8], mdia: &[u8]) -> Option<(u32, u32)> {
    let stsd = child(child(child(mdia, b"minf")?, b"stbl")?, b"stsd")?;
    // `stsd`: version and flags, entry count, then the entries (boxes).
    let (_, entry) = boxes(stsd.get(8..)?).next()?;
    // VisualSampleEntry: 6 reserved, data reference index, 16 pre-defined and reserved, width,
    // height, then 50 more bytes before its child boxes.
    let (w, h) = (be16(entry, 24)? as f64, be16(entry, 26)? as f64);
    if w == 0.0 || h == 0.0 {
        return None;
    }
    let pasp = entry
        .get(78..)
        .and_then(|children| child(children, b"pasp"))
        .and_then(|p| Some((be32(p, 0)?, be32(p, 4)?)))
        .filter(|&(hs, vs)| hs > 0 && vs > 0 && hs != vs);
    // `tkhd`: width and height (16.16 fixed point) end it.
    let tkhd = child(trak, b"tkhd")?;
    let at = if *tkhd.first()? == 1 { 88 } else { 76 };
    let display = (be32(tkhd, at)? >> 16, be32(tkhd, at + 4)? >> 16);
    // Chromium's `VideoAspectRatio::GetNaturalSize`.
    let (mut nw, mut nh) = (w, h);
    if let Some((hs, vs)) = pasp {
        let par = hs as f64 / vs as f64;
        if par >= 1.0 {
            nw = w * par;
        } else {
            nh = h / par;
        }
    } else if display.0 > 0 && display.1 > 0 {
        let dar = display.0 as f64 / display.1 as f64;
        if dar >= w / h {
            nw = h * dar;
        } else {
            nh = w / dar;
        }
    }
    Some((nw.round() as u32, nh.round() as u32))
}

// WebM (Matroska EBML).

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const CLUSTER: u32 = 0x1F43_B675;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const TIMECODE_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_TYPE: u32 = 0x83;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const CROP_BOTTOM: u32 = 0x54AA;
const CROP_TOP: u32 = 0x54BB;
const CROP_LEFT: u32 = 0x54CC;
const CROP_RIGHT: u32 = 0x54DD;
const DISPLAY_WIDTH: u32 = 0x54B0;
const DISPLAY_HEIGHT: u32 = 0x54BA;

/// An element ID (its length marker kept, as the IDs are written).
pub(crate) fn read_id(b: &[u8]) -> Option<(u32, usize)> {
    let first = *b.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || b.len() < len {
        return None;
    }
    Some((b[..len].iter().fold(0u32, |v, &x| (v << 8) | x as u32), len))
}

/// An element size; `None` inside for "unknown" (all value bits set).
pub(crate) fn read_size(b: &[u8]) -> Option<(Option<u64>, usize)> {
    let first = *b.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || b.len() < len {
        return None;
    }
    let mask = if len == 8 { 0 } else { 0xFFu8 >> len };
    let mut v = (first & mask) as u64;
    for &x in &b[1..len] {
        v = (v << 8) | x as u64;
    }
    let unknown = v == (1u64 << (7 * len)) - 1;
    Some((if unknown { None } else { Some(v) }, len))
}

/// The child elements of a master element's body, as (id, body).
pub(crate) fn elements(mut data: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    std::iter::from_fn(move || {
        let (id, il) = read_id(data)?;
        let (size, sl) = read_size(&data[il..])?;
        let end = (il + sl).checked_add(size? as usize)?;
        let body = data.get(il + sl..end)?;
        data = &data[end..];
        Some((id, body))
    })
}

pub(crate) fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0, |v, &x| (v << 8) | x as u64)
}

/// `Info`: `Duration` × `TimecodeScale` (nanoseconds, default 1,000,000), as Chromium's
/// `WebMStreamParser::ParseInfoAndTracks` takes it when the duration is above 0.
fn webm_duration(info: &[u8]) -> Option<f64> {
    let mut scale = 1_000_000u64;
    let mut duration = None;
    for (id, body) in elements(info) {
        match id {
            TIMECODE_SCALE => scale = uint(body),
            DURATION => {
                duration = match body.len() {
                    4 => Some(f32::from_be_bytes(body.try_into().ok()?) as f64),
                    8 => Some(f64::from_be_bytes(body.try_into().ok()?)),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    duration
        .filter(|&d| d > 0.0)
        .map(|d| d * scale as f64 / 1e9)
}

/// `Tracks`: the number of audio, video and subtitle tracks, and the first video track's natural
/// size as Chromium's `WebMVideoClient::InitializeConfig` gives it (the display size, else the
/// pixel size less the crops).
fn webm_tracks(tracks: &[u8]) -> Option<(u32, Option<(u32, u32)>)> {
    let mut count = 0;
    let mut video_size = None;
    for (id, entry) in elements(tracks) {
        if id != TRACK_ENTRY {
            continue;
        }
        let kind = elements(entry)
            .find(|&(i, _)| i == TRACK_TYPE)
            .map(|(_, b)| uint(b));
        match kind {
            Some(1) => {
                count += 1;
                if video_size.is_none() {
                    video_size = elements(entry)
                        .find(|&(i, _)| i == VIDEO)
                        .and_then(|(_, v)| webm_video_size(v));
                }
            }
            Some(2) | Some(0x11) => count += 1,
            _ => {}
        }
    }
    Some((count, video_size))
}

fn webm_video_size(video: &[u8]) -> Option<(u32, u32)> {
    let mut v = [0u64; 8];
    for (id, body) in elements(video) {
        let slot = match id {
            PIXEL_WIDTH => 0,
            PIXEL_HEIGHT => 1,
            CROP_LEFT => 2,
            CROP_RIGHT => 3,
            CROP_TOP => 4,
            CROP_BOTTOM => 5,
            DISPLAY_WIDTH => 6,
            DISPLAY_HEIGHT => 7,
            _ => continue,
        };
        v[slot] = uint(body);
    }
    let w = v[0].checked_sub(v[2] + v[3])?;
    let h = v[1].checked_sub(v[4] + v[5])?;
    if w == 0 || h == 0 {
        return None;
    }
    let w = if v[6] > 0 { v[6] } else { w };
    let h = if v[7] > 0 { v[7] } else { h };
    Some((w as u32, h as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn full(version: u8, rest: &[u8]) -> Vec<u8> {
        let mut v = vec![version, 0, 0, 0];
        v.extend_from_slice(rest);
        v
    }

    /// A `moov` with one video track of `w`×`h` (track header `tw`×`th`) and one audio track.
    fn moov(
        timescale: u32,
        duration: u32,
        mehd: Option<u32>,
        w: u16,
        h: u16,
        tw: u16,
        th: u16,
    ) -> Vec<u8> {
        let mut mvhd = vec![0u8; 8];
        mvhd.extend(timescale.to_be_bytes());
        mvhd.extend(duration.to_be_bytes());
        mvhd.extend([0u8; 80]);
        let mut tkhd = vec![0u8; 72];
        tkhd.extend(((tw as u32) << 16).to_be_bytes());
        tkhd.extend(((th as u32) << 16).to_be_bytes());
        let mut entry = vec![0u8; 24];
        entry.extend(w.to_be_bytes());
        entry.extend(h.to_be_bytes());
        entry.extend([0u8; 50]);
        let mut stsd = full(0, &1u32.to_be_bytes());
        stsd.extend(mp4_box(b"av01", &entry));
        let stbl = mp4_box(b"stbl", &mp4_box(b"stsd", &stsd));
        let video = [
            mp4_box(b"tkhd", &full(0, &tkhd)),
            mp4_box(
                b"mdia",
                &[
                    mp4_box(b"hdlr", &full(0, b"\0\0\0\0vide")),
                    mp4_box(b"minf", &stbl),
                ]
                .concat(),
            ),
        ]
        .concat();
        let audio = mp4_box(b"mdia", &mp4_box(b"hdlr", &full(0, b"\0\0\0\0soun")));
        let mut body = mp4_box(b"mvhd", &full(0, &mvhd));
        body.extend(mp4_box(b"trak", &video));
        body.extend(mp4_box(b"trak", &audio));
        if let Some(d) = mehd {
            body.extend(mp4_box(
                b"mvex",
                &mp4_box(b"mehd", &full(0, &d.to_be_bytes())),
            ));
        }
        mp4_box(b"moov", &body)
    }

    #[test]
    fn an_mp4_initialization_segment_split_across_appends() {
        let mut stream = mp4_box(b"ftyp", b"iso6\0\0\0\0iso6");
        stream.extend(moov(1000, 211_141, None, 640, 360, 640, 360));
        stream.extend(mp4_box(b"moof", &[0u8; 20]));
        for split in [1, 5, 9, 20, stream.len() - 30] {
            let mut s = InitScanner::new(Container::Mp4);
            assert_eq!(s.push(&stream[..split]), None, "split at {split}");
            let r = s.push(&stream[split..]).expect("the init segment");
            assert_eq!(r.duration, Some(211.141));
            assert_eq!(r.video_size, Some((640, 360)));
            assert_eq!(r.tracks, 2);
        }
    }

    #[test]
    fn mp4_durations_and_natural_sizes_as_chromium_reads_them() {
        let read = |m: Vec<u8>| InitScanner::new(Container::Mp4).push(&m).unwrap();
        // The fragment duration wins; a movie duration of 0 or all ones is no duration.
        assert_eq!(
            read(moov(90_000, 0, Some(450_000), 2, 2, 2, 2)).duration,
            Some(5.0)
        );
        assert_eq!(read(moov(1000, 0, None, 2, 2, 2, 2)).duration, None);
        assert_eq!(read(moov(1000, u32::MAX, None, 2, 2, 2, 2)).duration, None);
        // The track header's display size gives the aspect: a wider one grows the width.
        assert_eq!(
            read(moov(1, 1, None, 640, 480, 854, 480)).video_size,
            Some((854, 480))
        );
        assert_eq!(
            read(moov(1, 1, None, 640, 480, 640, 960)).video_size,
            Some((640, 960))
        );
    }

    #[test]
    fn a_media_segment_first_is_not_an_initialization_segment() {
        let mut s = InitScanner::new(Container::Mp4);
        assert_eq!(s.push(&mp4_box(b"moof", &[0u8; 20])), None);
        assert!(s.failed());
        s.reset();
        assert!(!s.failed());
        assert!(s.push(&moov(1, 1, None, 4, 4, 4, 4)).is_some());
    }

    fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = id
            .to_be_bytes()
            .into_iter()
            .skip_while(|&b| b == 0)
            .collect();
        // An 8-byte size, as muxers that patch sizes in later write them.
        v.push(0x01);
        v.extend(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn a_webm_initialization_segment_with_an_unknown_size_segment() {
        let video = [
            el(PIXEL_WIDTH, &[0x02, 0x80]),
            el(PIXEL_HEIGHT, &[0x01, 0x68]),
        ]
        .concat();
        let tracks = [
            el(
                TRACK_ENTRY,
                &[el(TRACK_TYPE, &[1]), el(VIDEO, &video)].concat(),
            ),
            el(TRACK_ENTRY, &el(TRACK_TYPE, &[2])),
        ]
        .concat();
        let info = [
            el(TIMECODE_SCALE, &[0x0F, 0x42, 0x40]),
            el(DURATION, &211_141f64.to_be_bytes()),
        ]
        .concat();
        let mut stream = el(EBML, &[0x42, 0x82, 0x84, b'w', b'e', b'b', b'm']);
        // The Segment, of unknown size.
        stream.extend([
            0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        stream.extend(el(0xEC, &[0; 30]));
        stream.extend(el(INFO, &info));
        stream.extend(el(TRACKS, &tracks));
        stream.extend(el(CLUSTER, &[0; 10]));
        for split in [1, 3, 12, 25, stream.len() - 30] {
            let mut s = InitScanner::new(Container::WebM);
            assert_eq!(s.push(&stream[..split]), None, "split at {split}");
            let r = s.push(&stream[split..]).expect("the init segment");
            assert_eq!(r.duration, Some(211.141));
            assert_eq!(r.video_size, Some((640, 360)));
            assert_eq!(r.tracks, 2);
        }
    }

    #[test]
    fn container_of_a_type() {
        assert_eq!(
            Container::of_type("video/mp4; codecs=\"av01.0.05M.08\""),
            Some(Container::Mp4)
        );
        assert_eq!(
            Container::of_type("audio/webm; codecs=\"opus\""),
            Some(Container::WebM)
        );
        assert_eq!(Container::of_type("audio/mpeg"), None);
    }
}
