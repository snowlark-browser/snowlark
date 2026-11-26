// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Animated images (HTML §4.8.4.3: an image "consisting of multiple frames" shows them in sequence):
//! the frame timeline ([`Timing`]), the container details WIC does not report (GIF's loop count,
//! APNG's frames, animated WebP's durations and loop count) and the frame composition the formats
//! define (disposal and blending), on premultiplied BGRA pixels.
//!
//! Durations follow what browsers do, which no format spec says: a frame of 10 ms or less shows for
//! 100 ms ([`frame_delay`]; Chromium and Firefox do this for GIF, APNG and WebP alike, against ads
//! that ask for a 0 ms flicker). For GIF, whose delays are in hundredths of a second, that is "under
//! 20 ms". Loop counts are as Chromium reads them: a GIF without a NETSCAPE2.0 (or ANIMEXTS1.0)
//! application extension plays once, one with a loop count `n` plays `n + 1` times and `0` means
//! forever; APNG's `num_plays` and WebP's loop count are the number of plays, `0` forever.

use std::ops::Range;

/// The duration a frame shows for, from the duration its file gives (ms): 10 ms or less is 100 ms.
pub fn frame_delay(ms: u32) -> u32 {
    if ms <= 10 {
        100
    } else {
        ms
    }
}

/// When an animation's frames show: each frame's duration and how often the sequence plays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timing {
    /// The end of each frame within one play, in ms from the play's start.
    ends: Vec<u64>,
    /// How many times the sequence plays; `None`: forever.
    plays: Option<u32>,
}

/// The frame an animation shows at a time ([`Timing::at`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameAt {
    pub index: usize,
    /// When (ms from the animation's start) the next frame shows; `None` once the animation has
    /// ended (its last frame stays) or for a single frame.
    pub next: Option<f64>,
}

impl Timing {
    /// A timeline of frames with these durations (ms, before [`frame_delay`]), playing `plays`
    /// times (`None`: forever).
    pub fn new(delays: impl IntoIterator<Item = u32>, plays: Option<u32>) -> Timing {
        let mut end = 0u64;
        let ends = delays
            .into_iter()
            .map(|d| {
                end += u64::from(frame_delay(d));
                end
            })
            .collect();
        Timing {
            ends,
            plays: plays.filter(|&p| p > 0),
        }
    }

    pub fn frames(&self) -> usize {
        self.ends.len()
    }

    /// Frame `i`'s duration (ms, after [`frame_delay`]).
    pub fn delay(&self, i: usize) -> u32 {
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        (self.ends[i] - start) as u32
    }

    /// How many times the sequence plays (`None`: forever).
    pub fn plays(&self) -> Option<u32> {
        self.plays
    }

    /// One play's duration (ms).
    pub fn duration(&self) -> u64 {
        self.ends.last().copied().unwrap_or(0)
    }

    /// Keeps the first `n` frames (a file whose later frames would not decode).
    pub fn truncate(&mut self, n: usize) {
        self.ends.truncate(n.max(1));
    }

    /// The frame shown `elapsed` ms after the animation started: the sequence repeats until it has
    /// played [`Timing::plays`] times, then its last frame stays.
    pub fn at(&self, elapsed: f64) -> FrameAt {
        let n = self.ends.len();
        let total = self.duration() as f64;
        if n <= 1 || total <= 0.0 {
            return FrameAt {
                index: 0,
                next: None,
            };
        }
        let elapsed = if elapsed.is_finite() {
            elapsed.max(0.0)
        } else {
            0.0
        };
        if let Some(p) = self.plays {
            if elapsed >= total * f64::from(p) {
                return FrameAt {
                    index: n - 1,
                    next: None,
                };
            }
        }
        let play = (elapsed / total).floor();
        let t = elapsed - play * total;
        let index = self.ends.partition_point(|&e| e as f64 <= t).min(n - 1);
        let last_play = self.plays.is_some_and(|p| play + 1.0 >= f64::from(p));
        let next = if last_play && index == n - 1 {
            None
        } else {
            Some(play * total + self.ends[index] as f64)
        };
        FrameAt { index, next }
    }
}

/// How many times a GIF plays, from its NETSCAPE2.0 / ANIMEXTS1.0 application extension: `None`
/// forever (loop count 0), `n + 1` for a loop count `n`, once without the extension. Walks the
/// file's blocks (GIF89a §§17-26) up to the trailer.
pub fn gif_plays(bytes: &[u8]) -> Option<u32> {
    const ONCE: Option<u32> = Some(1);
    if bytes.len() < 13 || !bytes.starts_with(b"GIF") {
        return ONCE;
    }
    let table = |flags: u8| {
        if flags & 0x80 != 0 {
            3usize << ((flags & 7) + 1)
        } else {
            0
        }
    };
    // (the data sub-blocks from `i`: the index after the terminator)
    let skip = |mut i: usize| -> Option<usize> {
        loop {
            let n = *bytes.get(i)? as usize;
            i += 1 + n;
            if n == 0 {
                return Some(i);
            }
        }
    };
    let mut i = 13 + table(bytes[10]);
    loop {
        match bytes.get(i) {
            Some(0x21) => {
                let label = bytes.get(i + 1).copied();
                i += 2;
                if label == Some(0xFF) && bytes.get(i) == Some(&11) {
                    let app = bytes.get(i + 1..i + 12);
                    if app == Some(b"NETSCAPE2.0") || app == Some(b"ANIMEXTS1.0") {
                        let d = i + 12;
                        if bytes.get(d).is_some_and(|&n| n >= 3) && bytes.get(d + 1) == Some(&1) {
                            let lo = *bytes.get(d + 2)? as u32;
                            let hi = *bytes.get(d + 3)? as u32;
                            let loops = lo | hi << 8;
                            return if loops == 0 { None } else { Some(loops + 1) };
                        }
                    }
                }
                let Some(next) = skip(i) else {
                    return ONCE;
                };
                i = next;
            }
            Some(0x2C) => {
                let Some(&flags) = bytes.get(i + 9) else {
                    return ONCE;
                };
                // descriptor, local color table, LZW minimum code size, then the data
                i += 10 + table(flags) + 1;
                let Some(next) = skip(i) else {
                    return ONCE;
                };
                i = next;
            }
            _ => return ONCE,
        }
    }
}

/// An animated WebP's loop, durations and frames (the WebP container, RFC 9649 §2.7: `ANIM` and
/// `ANMF`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebpAnimation {
    /// `None`: forever (loop count 0).
    pub plays: Option<u32>,
    /// Each frame's duration (ms, as the file gives it).
    pub durations: Vec<u32>,
    pub width: u32,
    pub height: u32,
    pub frames: Vec<WebpFrame>,
}

/// One frame of an animated WebP (RFC 9649 §2.7.1.1, `ANMF`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebpFrame {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Alpha-blending (`Over`) or not (`Source`).
    pub blend: Blend,
    /// Disposed to the background (`Background`: transparent, as other engines do, whatever the
    /// `ANIM` background colour) or kept (`None`).
    pub dispose: Dispose,
    /// The frame's image chunks (`ALPH`, then `VP8 ` or `VP8L`), headers and padding included.
    data: Range<usize>,
}

/// The animation of an extended-format WebP with the animation flag; `None` for other files.
pub fn parse_webp_animation(bytes: &[u8]) -> Option<WebpAnimation> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let u24 = |d: &[u8]| u32::from_le_bytes([d[0], d[1], d[2], 0]);
    let mut i = 12;
    let mut animated = false;
    let mut plays = None;
    let (mut width, mut height) = (0, 0);
    let mut durations = Vec::new();
    let mut frames = Vec::new();
    while i + 8 <= bytes.len() {
        let tag = &bytes[i..i + 4];
        let size = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().ok()?) as usize;
        let data = bytes.get(i + 8..(i + 8).checked_add(size)?)?;
        match tag {
            b"VP8X" if data.len() >= 10 => {
                animated = data[0] & 0x02 != 0;
                width = u24(&data[4..7]) + 1;
                height = u24(&data[7..10]) + 1;
            }
            b"ANIM" if data.len() >= 6 => {
                let loops = u16::from_le_bytes([data[4], data[5]]) as u32;
                plays = if loops == 0 { None } else { Some(loops) };
            }
            b"ANMF" if data.len() >= 16 => {
                durations.push(u24(&data[12..15]));
                let flags = data[15];
                frames.push(WebpFrame {
                    x: u24(&data[0..3]) * 2,
                    y: u24(&data[3..6]) * 2,
                    width: u24(&data[6..9]) + 1,
                    height: u24(&data[9..12]) + 1,
                    blend: if flags & 0x02 != 0 {
                        Blend::Source
                    } else {
                        Blend::Over
                    },
                    dispose: if flags & 0x01 != 0 {
                        Dispose::Background
                    } else {
                        Dispose::None
                    },
                    data: i + 8 + 16..i + 8 + size,
                });
            }
            _ => {}
        }
        // (chunks are padded to an even size)
        i += 8 + size + (size & 1);
    }
    (animated && !durations.is_empty()).then_some(WebpAnimation {
        plays,
        durations,
        width,
        height,
        frames,
    })
}

impl WebpAnimation {
    /// Frame `i` as a still WebP file of its own, for a WebP decoder: its `VP8 `/`VP8L` chunk in
    /// the simple format, or, with an `ALPH` chunk, in the extended format with the alpha flag.
    pub fn frame_webp(&self, bytes: &[u8], i: usize) -> Vec<u8> {
        let f = &self.frames[i];
        let chunks = &bytes[f.data.clone()];
        let alpha = chunks.starts_with(b"ALPH");
        let mut body = b"WEBP".to_vec();
        if alpha {
            body.extend_from_slice(b"VP8X");
            body.extend_from_slice(&10u32.to_le_bytes());
            body.extend_from_slice(&[0x10, 0, 0, 0]);
            body.extend_from_slice(&(f.width - 1).to_le_bytes()[..3]);
            body.extend_from_slice(&(f.height - 1).to_le_bytes()[..3]);
        }
        body.extend_from_slice(chunks);
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }
}

/// What happens to a frame's area after it showed (APNG `dispose_op`, GIF disposal method).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispose {
    /// Left as it is.
    None,
    /// Cleared to transparent black.
    Background,
    /// Put back as it was before the frame drew.
    Previous,
}

/// How a frame draws into its area (APNG `blend_op`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    /// Replaces the area, alpha included.
    Source,
    /// Composites over it (source-over).
    Over,
}

/// One APNG frame (PNG Third Edition §11.3.6.2 `fcTL`, §11.3.6.3 `fdAT`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApngFrame {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Duration (ms, as the file gives it).
    pub delay: u32,
    pub dispose: Dispose,
    pub blend: Blend,
    /// The pieces of the file holding the frame's zlib stream (`IDAT` data, or `fdAT` data after
    /// its sequence number).
    data: Vec<Range<usize>>,
}

/// An animated PNG's frames, which WIC does not decode (it shows the default image only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Apng {
    pub width: u32,
    pub height: u32,
    /// `None`: forever (`num_plays` 0).
    pub plays: Option<u32>,
    pub frames: Vec<ApngFrame>,
    /// The default image (`IDAT`) is the animation's first frame (an `fcTL` came before it);
    /// otherwise it is shown only where the animation is not (canvas `drawImage`).
    pub default_is_first: bool,
    ihdr: [u8; 13],
    /// The chunks between `IHDR` and the image data that every frame decodes with (`PLTE`,
    /// `tRNS`, `gAMA`, `iCCP`...).
    shared: Vec<Range<usize>>,
}

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// The APNG animation of a PNG file: `None` for a PNG without a valid `acTL` before its image data
/// (a still image) and for animations the spec calls erroneous (a frame outside the canvas, a first
/// frame that is the default image but does not cover the canvas, no frame data).
pub fn parse_apng(bytes: &[u8]) -> Option<Apng> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let mut i = 8;
    let mut ihdr: Option<[u8; 13]> = None;
    let mut actl: Option<(u32, u32)> = None;
    let mut shared = Vec::new();
    let mut frames: Vec<ApngFrame> = Vec::new();
    let mut idat_seen = false;
    let mut default_is_first = false;
    let be = |d: &[u8], at: usize| u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]);
    while i + 12 <= bytes.len() {
        let len = be(bytes, i) as usize;
        let tag: [u8; 4] = bytes[i + 4..i + 8].try_into().ok()?;
        let start = i + 8;
        let end = start.checked_add(len)?;
        if end + 4 > bytes.len() {
            break;
        }
        let data = &bytes[start..end];
        match &tag {
            b"IHDR" if data.len() == 13 => ihdr = Some(data.try_into().ok()?),
            b"acTL" if data.len() == 8 && !idat_seen => actl = Some((be(data, 0), be(data, 4))),
            b"fcTL" if data.len() == 26 => {
                let (w, h) = ihdr.map(|h| (be(&h, 0), be(&h, 4)))?;
                let frame = ApngFrame {
                    width: be(data, 4),
                    height: be(data, 8),
                    x: be(data, 12),
                    y: be(data, 16),
                    delay: {
                        let num = u16::from_be_bytes([data[20], data[21]]) as u64;
                        let den = match u16::from_be_bytes([data[22], data[23]]) {
                            0 => 100,
                            d => d as u64,
                        };
                        (num * 1000 / den).min(u32::MAX as u64) as u32
                    },
                    dispose: match data[24] {
                        1 => Dispose::Background,
                        2 => Dispose::Previous,
                        _ => Dispose::None,
                    },
                    blend: if data[25] == 1 {
                        Blend::Over
                    } else {
                        Blend::Source
                    },
                    data: Vec::new(),
                };
                let fits = frame.width > 0
                    && frame.height > 0
                    && frame.x.checked_add(frame.width).is_some_and(|r| r <= w)
                    && frame.y.checked_add(frame.height).is_some_and(|b| b <= h);
                if !fits {
                    return None;
                }
                if !idat_seen && frames.is_empty() {
                    // The default image is the first frame: it must cover the canvas.
                    if (frame.x, frame.y, frame.width, frame.height) != (0, 0, w, h) {
                        return None;
                    }
                    default_is_first = true;
                }
                frames.push(frame);
            }
            b"IDAT" => {
                idat_seen = true;
                if default_is_first && frames.len() == 1 {
                    frames[0].data.push(start..end);
                }
            }
            // (the default image's frame takes the IDAT data only)
            b"fdAT" if len >= 4 && idat_seen && !(default_is_first && frames.len() == 1) => {
                if let Some(f) = frames.last_mut() {
                    f.data.push(start + 4..end);
                }
            }
            b"IEND" => break,
            _ => {
                if ihdr.is_some() && !idat_seen && !matches!(&tag, b"acTL" | b"fcTL" | b"IHDR") {
                    shared.push(i..end + 4);
                }
            }
        }
        i = end + 4;
    }
    let ihdr = ihdr?;
    let (_, plays) = actl?;
    // (a frame without data ends the animation there)
    if let Some(n) = frames.iter().position(|f| f.data.is_empty()) {
        frames.truncate(n);
    }
    if frames.is_empty() {
        return None;
    }
    // "If the first frame's dispose_op is APNG_DISPOSE_OP_PREVIOUS it should be treated as
    // APNG_DISPOSE_OP_BACKGROUND."
    if frames[0].dispose == Dispose::Previous {
        frames[0].dispose = Dispose::Background;
    }
    Some(Apng {
        width: be(&ihdr, 0),
        height: be(&ihdr, 4),
        plays: if plays == 0 { None } else { Some(plays) },
        frames,
        default_is_first,
        ihdr,
        shared,
    })
}

impl Apng {
    /// Frame `i` as a PNG file of its own (its size in `IHDR`, the shared chunks, its data as one
    /// `IDAT`), for a PNG decoder.
    pub fn frame_png(&self, bytes: &[u8], i: usize) -> Vec<u8> {
        let f = &self.frames[i];
        let size: usize = f.data.iter().map(|r| r.len()).sum();
        let mut out =
            Vec::with_capacity(64 + size + self.shared.iter().map(|r| r.len()).sum::<usize>());
        out.extend_from_slice(PNG_SIGNATURE);
        let mut ihdr = self.ihdr;
        ihdr[0..4].copy_from_slice(&f.width.to_be_bytes());
        ihdr[4..8].copy_from_slice(&f.height.to_be_bytes());
        chunk(&mut out, b"IHDR", &[&ihdr]);
        for r in &self.shared {
            out.extend_from_slice(&bytes[r.clone()]);
        }
        let pieces: Vec<&[u8]> = f.data.iter().map(|r| &bytes[r.clone()]).collect();
        chunk(&mut out, b"IDAT", &pieces);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    /// Each frame's duration (ms, as the file gives it).
    pub fn delays(&self) -> impl Iterator<Item = u32> + '_ {
        self.frames.iter().map(|f| f.delay)
    }
}

/// Appends a PNG chunk of `tag` with the concatenation of `pieces` as its data.
fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], pieces: &[&[u8]]) {
    let len: usize = pieces.iter().map(|p| p.len()).sum();
    out.extend_from_slice(&(len as u32).to_be_bytes());
    let mut crc = crc32fast::Hasher::new();
    crc.update(tag);
    out.extend_from_slice(tag);
    for p in pieces {
        crc.update(p);
        out.extend_from_slice(p);
    }
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

/// A region of a canvas, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// An animation's canvas (premultiplied BGRA) that frames draw into, in order, with their disposal.
#[derive(Clone, Debug)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// The disposal the last frame asked for, done before the next draws.
    pending: Option<(Dispose, Region, Vec<u8>)>,
}

impl Canvas {
    /// A transparent canvas.
    pub fn new(width: u32, height: u32) -> Canvas {
        Canvas {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
            pending: None,
        }
    }

    /// Back to transparent, as before the first frame.
    pub fn reset(&mut self) {
        self.pixels.fill(0);
        self.pending = None;
    }

    /// The region cut to the canvas.
    fn clip(&self, r: Region) -> Region {
        let x = r.x.min(self.width);
        let y = r.y.min(self.height);
        Region {
            x,
            y,
            width: r.width.min(self.width - x),
            height: r.height.min(self.height - y),
        }
    }

    fn rows(&self, r: Region) -> impl Iterator<Item = Range<usize>> + '_ {
        let w = self.width as usize;
        (r.y..r.y + r.height).map(move |y| {
            let a = (y as usize * w + r.x as usize) * 4;
            a..a + r.width as usize * 4
        })
    }

    /// Draws a frame: `piece` (premultiplied BGRA, `piece_width` wide) at `at`, after the previous
    /// frame's disposal; this frame's disposal happens before the next one draws.
    pub fn draw(
        &mut self,
        piece: &[u8],
        piece_width: u32,
        at: Region,
        blend: Blend,
        dispose: Dispose,
    ) {
        if let Some((d, r, saved)) = self.pending.take() {
            match d {
                Dispose::None => {}
                Dispose::Background => {
                    for row in self.rows(r).collect::<Vec<_>>() {
                        self.pixels[row].fill(0);
                    }
                }
                Dispose::Previous => {
                    let mut s = 0;
                    for row in self.rows(r).collect::<Vec<_>>() {
                        let n = row.len();
                        self.pixels[row].copy_from_slice(&saved[s..s + n]);
                        s += n;
                    }
                }
            }
        }
        let r = self.clip(at);
        let saved = if dispose == Dispose::Previous {
            let mut v = Vec::with_capacity(r.width as usize * r.height as usize * 4);
            for row in self.rows(r) {
                v.extend_from_slice(&self.pixels[row]);
            }
            v
        } else {
            Vec::new()
        };
        let pw = piece_width as usize;
        for (dy, row) in self.rows(r).collect::<Vec<_>>().into_iter().enumerate() {
            let s = dy * pw * 4;
            let Some(src) = piece.get(s..s + row.len()) else {
                break;
            };
            let dst = &mut self.pixels[row];
            match blend {
                Blend::Source => dst.copy_from_slice(src),
                Blend::Over => {
                    for (d, s) in dst
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(src.as_chunks::<4>().0)
                    {
                        let a = s[3] as u32;
                        if a == 255 {
                            *d = *s;
                        } else if a != 0 {
                            for k in 0..4 {
                                d[k] = (s[k] as u32 + (d[k] as u32 * (255 - a) + 127) / 255)
                                    .min(255) as u8;
                            }
                        }
                    }
                }
            }
        }
        self.pending = Some((dispose, r, saved));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_delays_show_for_100_ms() {
        assert_eq!(frame_delay(0), 100);
        assert_eq!(frame_delay(10), 100);
        assert_eq!(frame_delay(11), 11);
        assert_eq!(frame_delay(20), 20);
        let t = Timing::new([0, 10, 20, 50], None);
        assert_eq!(
            (0..4).map(|i| t.delay(i)).collect::<Vec<_>>(),
            [100, 100, 20, 50]
        );
        assert_eq!(t.duration(), 270);
    }

    #[test]
    fn frames_follow_their_delays_and_loop_forever() {
        let t = Timing::new([100, 200, 300], None);
        let at = |ms: f64| t.at(ms);
        assert_eq!(
            at(0.0),
            FrameAt {
                index: 0,
                next: Some(100.0)
            }
        );
        assert_eq!(at(99.9).index, 0);
        assert_eq!(
            at(100.0),
            FrameAt {
                index: 1,
                next: Some(300.0)
            }
        );
        assert_eq!(at(299.0).index, 1);
        assert_eq!(
            at(300.0),
            FrameAt {
                index: 2,
                next: Some(600.0)
            }
        );
        // the second play
        assert_eq!(
            at(600.0),
            FrameAt {
                index: 0,
                next: Some(700.0)
            }
        );
        assert_eq!(
            at(6_000_450.0),
            FrameAt {
                index: 2,
                next: Some(6_000_600.0)
            }
        );
        // (before the start, and nonsense, is the first frame)
        assert_eq!(at(-5.0).index, 0);
        assert_eq!(at(f64::NAN).index, 0);
    }

    #[test]
    fn loop_count_ends_on_the_last_frame() {
        let t = Timing::new([100, 100], Some(2));
        assert_eq!(
            t.at(150.0),
            FrameAt {
                index: 1,
                next: Some(200.0)
            }
        );
        assert_eq!(
            t.at(250.0),
            FrameAt {
                index: 0,
                next: Some(300.0)
            }
        );
        // the last frame of the last play: nothing follows
        assert_eq!(
            t.at(350.0),
            FrameAt {
                index: 1,
                next: None
            }
        );
        assert_eq!(
            t.at(400.0),
            FrameAt {
                index: 1,
                next: None
            }
        );
        assert_eq!(
            t.at(1e9),
            FrameAt {
                index: 1,
                next: None
            }
        );
        let once = Timing::new([100, 100, 100], Some(1));
        assert_eq!(once.at(50.0).next, Some(100.0));
        assert_eq!(
            once.at(250.0),
            FrameAt {
                index: 2,
                next: None
            }
        );
        // (zero plays is forever)
        assert_eq!(Timing::new([100, 100], Some(0)).plays(), None);
    }

    #[test]
    fn a_single_frame_never_changes() {
        let t = Timing::new([0], None);
        assert_eq!(
            t.at(1000.0),
            FrameAt {
                index: 0,
                next: None
            }
        );
    }

    fn gif(app: Option<&[u8]>) -> Vec<u8> {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
        // a comment extension first
        g.extend_from_slice(&[0x21, 0xFE, 2, b'h', b'i', 0]);
        if let Some(a) = app {
            g.extend_from_slice(&[0x21, 0xFF, 11]);
            g.extend_from_slice(a);
        }
        g.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x4C, 0x01, 0]);
        g.push(0x3B);
        g
    }

    #[test]
    fn gif_loop_count() {
        assert_eq!(gif_plays(&gif(None)), Some(1));
        assert_eq!(
            gif_plays(&gif(Some(b"NETSCAPE2.0\x03\x01\x00\x00\x00"))),
            None
        );
        assert_eq!(
            gif_plays(&gif(Some(b"NETSCAPE2.0\x03\x01\x02\x00\x00"))),
            Some(3)
        );
        assert_eq!(
            gif_plays(&gif(Some(b"ANIMEXTS1.0\x03\x01\x00\x01\x00"))),
            Some(257)
        );
        // another application's extension
        assert_eq!(gif_plays(&gif(Some(b"XMP DataXMP\x01\x01\x00"))), Some(1));
        assert_eq!(gif_plays(b"GIF89a"), Some(1));
        let mut cut = gif(Some(b"NETSCAPE2.0\x03\x01\x05\x00\x00"));
        cut.truncate(43);
        assert_eq!(gif_plays(&cut), Some(6));
    }

    fn webp_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(tag);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
    }

    #[test]
    fn webp_animation_chunks() {
        let mut body = b"WEBP".to_vec();
        webp_chunk(&mut body, b"VP8X", &[0x02, 0, 0, 0, 9, 0, 0, 9, 0, 0]);
        webp_chunk(&mut body, b"ANIM", &[0, 0, 0, 0, 3, 0]);
        // frame 1: 10 x 10 at (0, 0), blended, kept; frame 2: 4 x 6 at (2, 4), not blended,
        // disposed
        for (d, at, flags) in [
            (70u32, [0u8, 0, 0, 0, 0, 0, 9, 0, 0, 9, 0, 0], 0u8),
            (0, [1, 0, 0, 2, 0, 0, 3, 0, 0, 5, 0, 0], 3),
        ] {
            let mut f = at.to_vec();
            f.extend_from_slice(&d.to_le_bytes()[..3]);
            f.push(flags);
            f.extend_from_slice(b"VP8L\x01\x00\x00\x00\x2f\x00");
            webp_chunk(&mut body, b"ANMF", &f);
        }
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);
        let a = parse_webp_animation(&file).unwrap();
        assert_eq!((a.plays, &a.durations[..]), (Some(3), &[70, 0][..]));
        assert_eq!((a.width, a.height), (10, 10));
        let f = &a.frames;
        assert_eq!((f[0].x, f[0].y, f[0].width, f[0].height), (0, 0, 10, 10));
        assert_eq!((f[0].blend, f[0].dispose), (Blend::Over, Dispose::None));
        assert_eq!((f[1].x, f[1].y, f[1].width, f[1].height), (2, 4, 4, 6));
        assert_eq!(
            (f[1].blend, f[1].dispose),
            (Blend::Source, Dispose::Background)
        );
        // a frame alone: the simple format around its chunk (padded to an even size)
        let still = a.frame_webp(&file, 1);
        assert_eq!(&still[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(still[4..8].try_into().unwrap()) as usize,
            still.len() - 8
        );
        assert_eq!(&still[8..], b"WEBPVP8L\x01\x00\x00\x00\x2f\x00");
        assert_eq!(parse_webp_animation(b"RIFF\0\0\0\0WEBPVP8 "), None);
    }

    fn png_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
        chunk(out, tag, &[data]);
    }

    fn fctl(seq: u32, w: u32, h: u32, x: u32, y: u32, delay: (u16, u16), ops: (u8, u8)) -> Vec<u8> {
        let mut d = Vec::new();
        for v in [seq, w, h, x, y] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        d.extend_from_slice(&delay.0.to_be_bytes());
        d.extend_from_slice(&delay.1.to_be_bytes());
        d.extend_from_slice(&[ops.0, ops.1]);
        d
    }

    fn apng(default_first: bool) -> Vec<u8> {
        let mut p = PNG_SIGNATURE.to_vec();
        png_chunk(&mut p, b"IHDR", &[0, 0, 0, 4, 0, 0, 0, 2, 8, 6, 0, 0, 0]);
        png_chunk(&mut p, b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]);
        png_chunk(&mut p, b"gAMA", &[0, 0, 0xB1, 0x8F]);
        if default_first {
            png_chunk(&mut p, b"fcTL", &fctl(0, 4, 2, 0, 0, (1, 10), (2, 0)));
        }
        png_chunk(&mut p, b"IDAT", b"zlib-a");
        png_chunk(&mut p, b"IDAT", b"zlib-b");
        if !default_first {
            png_chunk(&mut p, b"fcTL", &fctl(0, 4, 2, 0, 0, (5, 0), (0, 0)));
            png_chunk(&mut p, b"fdAT", b"\0\0\0\x01frame0");
        }
        png_chunk(&mut p, b"fcTL", &fctl(2, 2, 1, 1, 1, (0, 0), (1, 1)));
        png_chunk(&mut p, b"fdAT", b"\0\0\0\x03part1");
        png_chunk(&mut p, b"fdAT", b"\0\0\0\x04part2");
        png_chunk(&mut p, b"IEND", &[]);
        p
    }

    #[test]
    fn apng_frames() {
        let bytes = apng(true);
        let a = parse_apng(&bytes).unwrap();
        assert_eq!((a.width, a.height, a.plays), (4, 2, None));
        assert!(a.default_is_first);
        assert_eq!(a.frames.len(), 2);
        assert_eq!(a.delays().collect::<Vec<_>>(), [100, 0]);
        // (a first frame disposing to the previous canvas disposes to the background)
        assert_eq!(a.frames[0].dispose, Dispose::Background);
        assert_eq!(a.frames[1].dispose, Dispose::Background);
        assert_eq!(a.frames[1].blend, Blend::Over);
        assert_eq!((a.frames[1].x, a.frames[1].y, a.frames[1].width), (1, 1, 2));
        let f0 = a.frame_png(&bytes, 0);
        assert!(f0.windows(12).any(|w| w == b"zlib-azlib-b"));
        assert!(f0.windows(4).any(|w| w == b"gAMA"));
        let f1 = a.frame_png(&bytes, 1);
        assert!(f1.windows(10).any(|w| w == b"part1part2"));
        // the IHDR has the frame's size
        assert_eq!(&f1[16..24], &[0, 0, 0, 2, 0, 0, 0, 1]);
        assert!(!f1.windows(4).any(|w| w == b"acTL" || w == b"fcTL"));

        let bytes = apng(false);
        let a = parse_apng(&bytes).unwrap();
        assert!(!a.default_is_first);
        assert_eq!(a.frames.len(), 2);
        // (a 0 denominator is hundredths)
        assert_eq!(a.frames[0].delay, 50);
        assert!(a.frame_png(&bytes, 0).windows(6).any(|w| w == b"frame0"));
    }

    #[test]
    fn apng_errors_are_stills() {
        // a plain PNG
        let mut p = PNG_SIGNATURE.to_vec();
        png_chunk(&mut p, b"IHDR", &[0, 0, 0, 4, 0, 0, 0, 2, 8, 6, 0, 0, 0]);
        png_chunk(&mut p, b"IDAT", b"x");
        png_chunk(&mut p, b"IEND", &[]);
        assert_eq!(parse_apng(&p), None);
        // a frame outside the canvas
        let mut bad = apng(true);
        let at = bad.windows(4).rposition(|w| w == b"fcTL").unwrap();
        bad[at + 4 + 12..at + 4 + 16].copy_from_slice(&3u32.to_be_bytes());
        assert_eq!(parse_apng(&bad), None);
    }

    fn px(c: [u8; 4]) -> Vec<u8> {
        c.to_vec()
    }

    #[test]
    fn canvas_blends_and_disposes() {
        let mut c = Canvas::new(2, 1);
        let red = [0, 0, 255, 255];
        let half_blue = [128, 0, 0, 128];
        let full = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };
        let right = Region {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        };
        c.draw(&[red, red].concat(), 2, full, Blend::Source, Dispose::None);
        assert_eq!(c.pixels, [red, red].concat());
        // source-over, premultiplied
        c.draw(&px(half_blue), 1, right, Blend::Over, Dispose::Previous);
        assert_eq!(&c.pixels[4..], &[128, 0, 127, 255]);
        // "previous": the right pixel is red again before the next frame draws
        c.draw(
            &[0, 0, 0, 0],
            1,
            Region { x: 0, ..right },
            Blend::Over,
            Dispose::Background,
        );
        assert_eq!(c.pixels, [red, red].concat());
        // "background": the left pixel is cleared; a source blend writes transparency
        c.draw(&[0, 0, 0, 0], 1, right, Blend::Source, Dispose::None);
        assert_eq!(c.pixels, [[0, 0, 0, 0], [0, 0, 0, 0]].concat());
        c.reset();
        assert_eq!(c.pixels, vec![0; 8]);
    }
}
