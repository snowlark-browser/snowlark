// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The coded samples of a fragmented MP4 byte stream (ISO/IEC 14496-12 movie fragments, as Media
//! Source Extensions' "ISO BMFF Byte Stream Format" appends them), each with its timing and, for a
//! protected track, how it is encrypted (ISO/IEC 23001-7 sample auxiliary information), for a
//! decoder that is handed samples rather than a file: one that decrypts them itself.
//!
//! A [`SampleReader`] follows one `SourceBuffer`'s bytes box by box, as [`crate::cenc::Decryptor`]
//! does and with its parser: an initialization segment's `moov` gives the [`Track`]s (codec
//! configuration, timescale, protection), each media segment (`moof` and its `mdat`) the
//! [`Sample`]s. H.264 samples (ISO/IEC 14496-15) are rewritten from length-prefixed NAL units to
//! Annex B start codes, with the sequence and picture parameter sets of the `avcC` before each
//! key frame, the subsample map following every byte that moves; AAC samples (ISO/IEC 14496-3) are
//! raw access units, the `AudioSpecificConfig` in the track.

use std::collections::HashMap;

use crate::cenc::{self, be16, be32, be64, child, children, header, KeyId, Scheme, Trex};

/// How one sample is encrypted (ISO/IEC 23001-7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleEncryption {
    /// `cenc` (AES-CTR) when false, `cbcs` (AES-CBC with a pattern) when true.
    pub cbc: bool,
    /// The pattern's encrypted and clear blocks (`cbcs`), zero for none.
    pub crypt_skip: (u32, u32),
    /// The key id as the media carries it (big-endian).
    pub key_id: KeyId,
    /// The initialization vector: 8 or 16 bytes.
    pub iv: Vec<u8>,
    /// Clear and encrypted byte counts, in order; empty when the whole sample is encrypted.
    pub subsamples: Vec<(u32, u32)>,
}

/// One coded sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub data: Vec<u8>,
    /// Presentation time and duration in 100 ns units.
    pub time: i64,
    pub duration: i64,
    pub key: bool,
    pub encryption: Option<SampleEncryption>,
}

/// A track's codec and its configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    /// H.264 (`avc1`, `avc3`): the `avcC`'s sequence and picture parameter sets as Annex B, and
    /// the size of the length prefixes of the samples' NAL units.
    Avc {
        parameter_sets: Vec<u8>,
        length_size: u8,
    },
    /// AAC (`mp4a` with an MPEG-4 audio object type): the `AudioSpecificConfig`.
    Aac { config: Vec<u8> },
    /// Any other sample entry, by its (original) format.
    Other([u8; 4]),
}

/// A protected track's scheme and default key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackEncryption {
    pub scheme: Scheme,
    pub key_id: KeyId,
}

/// One track of an initialization segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: u32,
    pub video: bool,
    pub timescale: u32,
    pub codec: Codec,
    /// Video: the coded size of the sample entry.
    pub width: u16,
    pub height: u16,
    /// Audio: channels and sample rate of the sample entry.
    pub channels: u16,
    pub sample_rate: u32,
    /// The media time the presentation starts at (the first edit of `elst`), in the timescale.
    pub start: i64,
    pub encryption: Option<TrackEncryption>,
}

/// What [`SampleReader::push`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// An initialization segment's tracks.
    Tracks(Vec<Track>),
    /// A sample of the track with this id.
    Sample(u32, Sample),
}

/// One `SourceBuffer`'s byte stream (see the module).
#[derive(Default)]
pub struct SampleReader {
    tracks: Vec<Track>,
    protection: HashMap<u32, cenc::Track>,
    defaults: HashMap<u32, Trex>,
    /// The decode time after the last sample of each track, for fragments without `tfdt`.
    next_decode: HashMap<u32, u64>,
    /// Bytes of a box this must see whole.
    pending: Vec<u8>,
    /// Bytes still to pass of a box this does not read.
    skip: u64,
}

const SAMPLE_IS_NON_SYNC: u32 = 0x1_0000;

impl SampleReader {
    pub fn new() -> SampleReader {
        SampleReader::default()
    }

    /// The tracks of the last initialization segment.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Forgets a partly received box (the `SourceBuffer`'s `abort()` and "reset parser state");
    /// the tracks of the last initialization segment stay.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.skip = 0;
    }

    /// Takes appended bytes; returns the tracks and samples completed by them, in stream order.
    pub fn push(&mut self, data: &[u8]) -> Vec<Output> {
        let mut out = Vec::new();
        let mut data = data;
        if self.skip > 0 {
            let n = self.skip.min(data.len() as u64) as usize;
            data = &data[n..];
            self.skip -= n as u64;
        }
        self.pending.extend_from_slice(data);
        let buf = std::mem::take(&mut self.pending);
        let mut p = 0usize;
        while p < buf.len() {
            if self.skip > 0 {
                let n = self.skip.min((buf.len() - p) as u64) as usize;
                p += n;
                self.skip -= n as u64;
                continue;
            }
            let Some((size, typ, h)) = header(&buf[p..]) else {
                break;
            };
            let avail = (buf.len() - p) as u64;
            if size == 0 || size < h as u64 {
                // (a box to the end of the stream, or not a box: nothing more is read)
                p = buf.len();
                self.skip = u64::MAX;
                break;
            }
            match &typ {
                b"moov" => {
                    if avail < size {
                        break;
                    }
                    let e = p + size as usize;
                    self.read_moov(&buf[p..e]);
                    out.push(Output::Tracks(self.tracks.clone()));
                    p = e;
                }
                b"moof" => {
                    if avail < size {
                        break;
                    }
                    let m = p + size as usize;
                    let Some((msize, mtyp, _)) = header(&buf[m..]) else {
                        break;
                    };
                    if &mtyp != b"mdat" || msize == 0 {
                        // (no media data after it: no samples)
                        p = m;
                        continue;
                    }
                    if ((buf.len() - m) as u64) < msize {
                        break;
                    }
                    let e = m + msize as usize;
                    self.read_segment(&buf[p..e], size as usize, &mut out);
                    p = e;
                }
                _ => {
                    let n = size.min(avail);
                    p += n as usize;
                    self.skip = size - n;
                }
            }
        }
        let mut buf = buf;
        buf.drain(..p);
        self.pending = buf;
        out
    }

    fn read_moov(&mut self, b: &[u8]) {
        self.tracks.clear();
        self.protection.clear();
        self.defaults.clear();
        self.next_decode.clear();
        let Some((_, _, h)) = header(b) else {
            return;
        };
        for (s, e, typ, ch) in children(b, h, b.len()) {
            match &typ {
                b"mvex" => self.defaults = cenc::read_mvex(b, s + ch, e),
                b"trak" => {
                    if let Some(t) = self.read_trak(b, s + ch, e) {
                        self.tracks.push(t);
                    }
                }
                _ => {}
            }
        }
    }

    fn read_trak(&mut self, b: &[u8], start: usize, end: usize) -> Option<Track> {
        let (ks, _, kh) = child(b, start, end, b"tkhd")?;
        let id = be32(b, ks + kh + if *b.get(ks + kh)? == 1 { 20 } else { 12 })?;
        let (ms, me, mh) = child(b, start, end, b"mdia")?;
        let (ds, _, dh) = child(b, ms + mh, me, b"mdhd")?;
        let timescale = be32(b, ds + dh + if *b.get(ds + dh)? == 1 { 20 } else { 12 })?;
        let (hs, _, hh) = child(b, ms + mh, me, b"hdlr")?;
        let handler = b.get(hs + hh + 8..hs + hh + 12)?;
        let video = match handler {
            b"vide" => true,
            b"soun" => false,
            _ => return None,
        };
        let mut at = (ms + mh, me);
        for typ in [b"minf", b"stbl", b"stsd"] {
            let (s, e, h) = child(b, at.0, at.1, typ)?;
            at = (s + h, e);
        }
        // (stsd: version and flags, entry_count, then the sample entries: the first is read)
        let &(es, ee, etyp, eh) = children(b, at.0 + 8, at.1).first()?;
        let mut format = etyp;
        let mut encryption = None;
        if matches!(&etyp, b"encv" | b"enca") {
            let (_, _, original, protection) = cenc::read_sinf(b, es, ee, eh, video)?;
            format = original;
            encryption = Some(TrackEncryption {
                scheme: protection.scheme,
                key_id: protection.kid,
            });
            self.protection.insert(id, protection);
        }
        let body = es + eh;
        let mut track = Track {
            id,
            video,
            timescale,
            codec: Codec::Other(format),
            width: 0,
            height: 0,
            channels: 0,
            sample_rate: 0,
            start: edit_start(b, start, end).unwrap_or(0),
            encryption,
        };
        // The boxes after the sample entry's own fields (VisualSampleEntry 78 bytes,
        // AudioSampleEntry 28, more for QuickTime sound descriptions version 1 and 2).
        let fields = if video {
            track.width = be16(b, body + 24)?;
            track.height = be16(b, body + 26)?;
            78
        } else {
            track.channels = be16(b, body + 16)?;
            track.sample_rate = be32(b, body + 24)? >> 16;
            match be16(b, body + 8) {
                Some(1) => 44,
                Some(2) => 64,
                _ => 28,
            }
        };
        match &format {
            b"avc1" | b"avc3" => {
                let (cs, ce, ch) = child(b, body + fields, ee, b"avcC")?;
                if let Some(codec) = read_avcc(&b[cs + ch..ce]) {
                    track.codec = codec;
                }
            }
            b"mp4a" => {
                let (cs, ce, ch) = child(b, body + fields, ee, b"esds")?;
                if let Some(config) = read_esds(&b[cs + ch..ce]) {
                    track.codec = Codec::Aac { config };
                }
            }
            _ => {}
        }
        Some(track)
    }

    fn read_segment(&mut self, b: &[u8], moof_len: usize, out: &mut Vec<Output>) {
        let mut prev_end: Option<usize> = None;
        for (ts, te, typ, th) in children(b, 8, moof_len) {
            if &typ != b"traf" {
                continue;
            }
            let lookup = |id| (self.protection.get(&id), self.defaults.get(&id).copied());
            let Some(traf) = cenc::read_traf(b, ts + th, te, prev_end, &lookup) else {
                continue;
            };
            prev_end = traf.data_end;
            let id = traf.id;
            let Some(track) = self.tracks.iter().find(|t| t.id == id) else {
                continue;
            };
            let protection = traf
                .track
                .as_ref()
                .filter(|t| t.protected && matches!(t.scheme, Scheme::Cenc | Scheme::Cbcs));
            let mut decode = traf
                .decode_time
                .unwrap_or_else(|| self.next_decode.get(&id).copied().unwrap_or(0));
            let to_100ns =
                |v: i64| (v as i128 * 10_000_000 / track.timescale.max(1) as i128) as i64;
            for s in &traf.samples {
                let time = decode as i64 + s.composition_offset as i64 - track.start;
                decode += s.duration as u64;
                let Some(data) = b.get(s.offset..s.offset + s.size) else {
                    continue;
                };
                let depends_on_others = (s.flags >> 24) & 3 == 1;
                let mut key =
                    s.flags & SAMPLE_IS_NON_SYNC == 0 && !(track.video && depends_on_others);
                let mut encryption = protection.map(|p| {
                    let iv_len = if p.iv_size > 0 {
                        p.iv_size as usize
                    } else {
                        p.constant_iv.as_ref().map_or(16, |c| c.len().min(16))
                    };
                    SampleEncryption {
                        cbc: p.scheme == Scheme::Cbcs,
                        crypt_skip: (p.crypt as u32, p.skip as u32),
                        key_id: p.kid,
                        iv: s.iv[..iv_len].to_vec(),
                        subsamples: s
                            .subsamples
                            .iter()
                            .map(|&(c, e)| (c as u32, e as u32))
                            .collect(),
                    }
                });
                let data = match &track.codec {
                    Codec::Avc {
                        parameter_sets,
                        length_size,
                    } => {
                        // The NAL units say whether this is a random access point (an IDR
                        // picture), whatever the container's flags say.
                        if let Some(idr) = avc_idr(data, *length_size as usize) {
                            key = idr;
                        }
                        avc_sample(
                            data,
                            *length_size as usize,
                            if key { parameter_sets } else { &[] },
                            encryption.as_mut(),
                        )
                    }
                    _ => data.to_vec(),
                };
                out.push(Output::Sample(
                    id,
                    Sample {
                        data,
                        time: to_100ns(time),
                        duration: to_100ns(s.duration as i64),
                        key,
                        encryption,
                    },
                ));
            }
            self.next_decode.insert(id, decode);
        }
    }
}

/// The media time of a track's first edit that is not empty (`edts`/`elst`), if it has one.
fn edit_start(b: &[u8], start: usize, end: usize) -> Option<i64> {
    let (es, ee, eh) = child(b, start, end, b"edts")?;
    let (ls, _, lh) = child(b, es + eh, ee, b"elst")?;
    let version = *b.get(ls + lh)?;
    let count = be32(b, ls + lh + 4)?;
    let mut q = ls + lh + 8;
    for _ in 0..count {
        let media_time = if version == 1 {
            let t = be64(b, q + 8)? as i64;
            q += 20;
            t
        } else {
            let t = be32(b, q + 4)? as i32 as i64;
            q += 12;
            t
        };
        if media_time >= 0 {
            return Some(media_time);
        }
    }
    None
}

/// An `avcC` body (AVCDecoderConfigurationRecord, ISO/IEC 14496-15 5.3.3.1).
fn read_avcc(c: &[u8]) -> Option<Codec> {
    let length_size = (c.get(4)? & 3) + 1;
    let mut parameter_sets = Vec::new();
    let mut p = 5;
    for mask in [31u8, 255] {
        let n = c.get(p)? & mask;
        p += 1;
        for _ in 0..n {
            let len = be16(c, p)? as usize;
            parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
            parameter_sets.extend_from_slice(c.get(p + 2..p + 2 + len)?);
            p += 2 + len;
        }
    }
    Some(Codec::Avc {
        parameter_sets,
        length_size,
    })
}

/// An MPEG-4 descriptor at `p` (ISO/IEC 14496-1 8.3.3): its tag and body.
fn descriptor(b: &[u8], mut p: usize) -> Option<(u8, usize, usize)> {
    let tag = *b.get(p)?;
    p += 1;
    let mut size = 0usize;
    for _ in 0..4 {
        let byte = *b.get(p)?;
        p += 1;
        size = (size << 7) | (byte & 0x7f) as usize;
        if byte & 0x80 == 0 {
            break;
        }
    }
    let end = p.checked_add(size)?;
    (end <= b.len()).then_some((tag, p, end))
}

/// The `AudioSpecificConfig` of an `esds` body whose stream is MPEG-4 audio (object type 0x40).
fn read_esds(b: &[u8]) -> Option<Vec<u8>> {
    // (version and flags, then the ES_Descriptor)
    let (tag, mut p, end) = descriptor(b, 4)?;
    if tag != 3 {
        return None;
    }
    let flags = *b.get(p + 2)?;
    p += 3;
    if flags & 0x80 != 0 {
        p += 2;
    }
    if flags & 0x40 != 0 {
        p += 1 + *b.get(p)? as usize;
    }
    if flags & 0x20 != 0 {
        p += 2;
    }
    let (tag, q, dend) = descriptor(&b[..end], p)?;
    if tag != 4 || *b.get(q)? != 0x40 {
        return None;
    }
    let (tag, s, send) = descriptor(&b[..dend], q + 13)?;
    (tag == 5).then(|| b[s..send].to_vec())
}

/// Whether a length-prefixed H.264 sample holds a slice of an IDR picture (NAL unit type 5);
/// `None` when its lengths do not add up.
fn avc_idr(data: &[u8], length_size: usize) -> Option<bool> {
    if !matches!(length_size, 1 | 2 | 4) {
        return None;
    }
    let mut p = 0usize;
    let mut idr = false;
    while p < data.len() {
        let prefix = data.get(p..p + length_size)?;
        let len = prefix.iter().fold(0usize, |n, b| (n << 8) | *b as usize);
        let nal = data.get(p + length_size..p + length_size + len)?;
        idr |= nal.first().is_some_and(|h| h & 0x1f == 5);
        p += length_size + len;
    }
    Some(idr)
}

/// An H.264 sample as Annex B: each NAL unit's length prefix becomes a start code, and `params`
/// (Annex B already) go first. The subsample map grows by every byte added to a clear range; when
/// a prefix is not in the clear, or the lengths do not add up, the sample stays as it is.
fn avc_sample(
    data: &[u8],
    length_size: usize,
    params: &[u8],
    encryption: Option<&mut SampleEncryption>,
) -> Vec<u8> {
    let mut grow = Vec::new();
    let subsamples = encryption.as_ref().map(|e| &e.subsamples[..]);
    if length_size == 0 || length_size == 3 || subsamples.is_some_and(|s| s.is_empty()) {
        // (an encrypted sample without subsamples has its prefixes encrypted too)
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(params.len() + data.len() + 16);
    out.extend_from_slice(params);
    let mut p = 0usize;
    while p < data.len() {
        let Some(prefix) = data.get(p..p + length_size) else {
            return data.to_vec();
        };
        let len = prefix.iter().fold(0usize, |n, b| (n << 8) | *b as usize);
        if let Some(subs) = subsamples {
            // The subsample whose clear bytes hold the prefix.
            let mut start = 0usize;
            let mut found = None;
            for (i, &(clear, enc)) in subs.iter().enumerate() {
                if p >= start && p + length_size <= start + clear as usize {
                    found = Some(i);
                    break;
                }
                start += clear as usize + enc as usize;
            }
            let Some(i) = found else {
                return data.to_vec();
            };
            grow.push(i);
        }
        let Some(nal) = data.get(p + length_size..p + length_size + len) else {
            return data.to_vec();
        };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        p += length_size + len;
    }
    if let Some(e) = encryption {
        for i in grow {
            e.subsamples[i].0 += (4 - length_size) as u32;
        }
        if let Some(first) = e.subsamples.first_mut() {
            first.0 += params.len() as u32;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    fn full(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut b = vec![version];
        b.extend_from_slice(&flags.to_be_bytes()[1..]);
        b.extend_from_slice(body);
        bx(typ, &b)
    }

    #[test]
    fn length_prefixes_become_start_codes_and_the_clear_bytes_follow() {
        // Two NAL units with 2-byte prefixes, each prefix and a byte of header in the clear.
        let data = [0, 3, 0x65, 1, 2, 0, 2, 0x41, 9];
        let mut e = SampleEncryption {
            cbc: false,
            crypt_skip: (0, 0),
            key_id: [0; 16],
            iv: vec![0; 8],
            subsamples: vec![(3, 2), (3, 1)],
        };
        let params = [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68];
        let out = avc_sample(&data, 2, &params, Some(&mut e));
        let mut want = params.to_vec();
        want.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2, 0, 0, 0, 1, 0x41, 9]);
        assert_eq!(out, want);
        assert_eq!(e.subsamples, vec![(3 + 2 + 10, 2), (3 + 2, 1)]);
        // A prefix inside encrypted bytes: left as it is.
        let mut bad = e.clone();
        bad.subsamples = vec![(1, 8)];
        assert_eq!(avc_sample(&data, 2, &[], Some(&mut bad)), data.to_vec());
        assert_eq!(bad.subsamples, vec![(1, 8)]);
    }

    /// A one-track (video, id 1, timescale 1000) initialization segment, `encv` with `cenc` when
    /// `protected`.
    fn init(protected: bool) -> Vec<u8> {
        let tkhd = full(
            b"tkhd",
            0,
            3,
            &[&[0u8; 8][..], &1u32.to_be_bytes(), &[0; 68]].concat(),
        );
        let mdhd = full(
            b"mdhd",
            0,
            0,
            &[&[0u8; 8][..], &1000u32.to_be_bytes(), &[0; 8]].concat(),
        );
        let hdlr = full(b"hdlr", 0, 0, &[&[0u8; 4][..], b"vide", &[0; 13]].concat());
        let avcc = bx(
            b"avcC",
            &[1, 0x64, 0, 0x1e, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 1, 0x68],
        );
        let mut entry = vec![0u8; 78];
        entry[24..26].copy_from_slice(&640u16.to_be_bytes());
        entry[26..28].copy_from_slice(&360u16.to_be_bytes());
        entry.extend_from_slice(&avcc);
        let typ = if protected {
            let frma = bx(b"frma", b"avc1");
            let schm = full(
                b"schm",
                0,
                0,
                &[&b"cenc"[..], &0x10000u32.to_be_bytes()].concat(),
            );
            let tenc = full(b"tenc", 0, 0, &[&[0u8, 0, 1, 8][..], &[7; 16]].concat());
            let sinf = bx(b"sinf", &[frma, schm, bx(b"schi", &tenc)].concat());
            entry.extend_from_slice(&sinf);
            b"encv"
        } else {
            b"avc1"
        };
        let stsd = full(
            b"stsd",
            0,
            0,
            &[&1u32.to_be_bytes()[..], &bx(typ, &entry)].concat(),
        );
        let stbl = bx(b"stbl", &stsd);
        let minf = bx(b"minf", &stbl);
        let elst = full(
            b"elst",
            0,
            0,
            &[
                &1u32.to_be_bytes()[..],
                &0u32.to_be_bytes(),
                &40i32.to_be_bytes(),
                &0x10000u32.to_be_bytes(),
            ]
            .concat(),
        );
        let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
        let trak = bx(b"trak", &[tkhd, bx(b"edts", &elst), mdia].concat());
        let trex = full(
            b"trex",
            0,
            0,
            &[1u32, 1, 40, 0, SAMPLE_IS_NON_SYNC]
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect::<Vec<_>>(),
        );
        let moov = bx(b"moov", &[trak, bx(b"mvex", &trex)].concat());
        [bx(b"ftyp", b"isom\0\0\0\0"), moov].concat()
    }

    /// A media segment of two samples (a key frame, then another), decode time 1000; `senc` with
    /// one subsample each when `protected`.
    fn segment(protected: bool) -> Vec<u8> {
        let samples: [&[u8]; 2] = [&[0, 0, 0, 3, 0x65, 0xaa, 0xbb], &[0, 0, 0, 2, 0x41, 0xcc]];
        let tfhd = full(b"tfhd", 0, 0x2_0000, &1u32.to_be_bytes());
        let tfdt = full(b"tfdt", 1, 0, &1000u64.to_be_bytes());
        let senc = full(
            b"senc",
            0,
            2,
            &[
                &2u32.to_be_bytes()[..],
                &[1; 8],
                &1u16.to_be_bytes(),
                &5u16.to_be_bytes(),
                &2u32.to_be_bytes(),
                &[2; 8],
                &1u16.to_be_bytes(),
                &5u16.to_be_bytes(),
                &1u32.to_be_bytes(),
            ]
            .concat(),
        );
        // trun: data offset, first sample flags (a sync sample), sizes and composition offsets.
        let trun_len = 12 + 4 + 4 + 4 + 2 * 8;
        let mut traf_body = [tfhd, tfdt].concat();
        if protected {
            traf_body.extend_from_slice(&senc);
        }
        let traf_len = 8 + traf_body.len() + trun_len;
        let moof_len = 8 + traf_len;
        let mut trun = Vec::new();
        trun.extend_from_slice(&2u32.to_be_bytes());
        trun.extend_from_slice(&((moof_len + 8) as u32).to_be_bytes());
        trun.extend_from_slice(&0u32.to_be_bytes());
        for (s, cto) in samples.iter().zip([80u32, 0]) {
            trun.extend_from_slice(&(s.len() as u32).to_be_bytes());
            trun.extend_from_slice(&cto.to_be_bytes());
        }
        traf_body.extend_from_slice(&full(b"trun", 0, 0x1 | 0x4 | 0x200 | 0x800, &trun));
        let moof = bx(b"moof", &bx(b"traf", &traf_body));
        assert_eq!(moof.len(), moof_len);
        [moof, bx(b"mdat", &samples.concat())].concat()
    }

    #[test]
    fn tracks_and_samples_in_any_pieces() {
        let stream = [init(true), segment(true)].concat();
        for piece in [stream.len(), 1, 7, 100] {
            let mut r = SampleReader::new();
            let mut out = Vec::new();
            for c in stream.chunks(piece) {
                out.extend(r.push(c));
            }
            assert_eq!(out.len(), 3, "pieces of {piece}");
            let Output::Tracks(tracks) = &out[0] else {
                panic!("tracks first");
            };
            let t = &tracks[0];
            assert_eq!(
                (t.id, t.video, t.timescale, t.width, t.height),
                (1, true, 1000, 640, 360)
            );
            assert_eq!(t.start, 40);
            assert_eq!(
                t.encryption,
                Some(TrackEncryption {
                    scheme: Scheme::Cenc,
                    key_id: [7; 16]
                })
            );
            let params = vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68];
            assert_eq!(
                t.codec,
                Codec::Avc {
                    parameter_sets: params.clone(),
                    length_size: 4
                }
            );
            let Output::Sample(1, first) = &out[1] else {
                panic!("a sample of track 1");
            };
            let Output::Sample(1, second) = &out[2] else {
                panic!("a sample of track 1");
            };
            // Times: (1000 + 80 - 40) ms, then (1040 + 0 - 40) ms; 40 ms each.
            assert_eq!(
                (first.time, first.duration, first.key),
                (10_400_000, 400_000, true)
            );
            assert_eq!((second.time, second.key), (10_000_000, false));
            assert_eq!(
                first.data,
                [&params[..], &[0, 0, 0, 1, 0x65, 0xaa, 0xbb]].concat()
            );
            assert_eq!(second.data, vec![0, 0, 0, 1, 0x41, 0xcc]);
            let e = first.encryption.as_ref().unwrap();
            assert_eq!((e.iv.clone(), e.key_id), (vec![1; 8], [7; 16]));
            assert_eq!(e.subsamples, vec![(5 + params.len() as u32, 2)]);
            assert_eq!(second.encryption.as_ref().unwrap().subsamples, vec![(5, 1)]);
        }
    }

    #[test]
    fn clear_tracks_have_no_encryption() {
        let mut r = SampleReader::new();
        let out = r.push(&[init(false), segment(false)].concat());
        let Output::Sample(_, s) = &out[1] else {
            panic!("a sample");
        };
        assert_eq!(r.tracks()[0].encryption, None);
        assert_eq!(s.encryption, None);
        assert!(s.key);
    }

    /// The WPT `encrypted-media` H.264 and AAC files (`SNOWLARK_WPT`): every sample of the
    /// encrypted file, decrypted with its key, equals the clear file's sample at the same time.
    #[test]
    fn wpt_media_decrypts_to_the_clear_samples() {
        let Some(wpt) = std::env::var_os("SNOWLARK_WPT") else {
            eprintln!("SNOWLARK_WPT is not set: skipped");
            return;
        };
        let dir = std::path::Path::new(&wpt).join("encrypted-media/content");
        let keys: HashMap<KeyId, [u8; 16]> = [
            (
                [
                    0xad, 0x13, 0xf9, 0xea, 0x2b, 0xe6, 0x98, 0xb8, 0x75, 0xf5, 0x04, 0xa8, 0xe3,
                    0xcc, 0xea, 0x64,
                ],
                [
                    0xbe, 0x7d, 0xf8, 0xa3, 0x66, 0x7a, 0x6a, 0x8f, 0xd5, 0x64, 0xd0, 0xed, 0x81,
                    0x33, 0x9a, 0x95,
                ],
            ),
            (
                [
                    0x55, 0x8e, 0xe5, 0x41, 0xb9, 0x0a, 0xb2, 0xf3, 0x95, 0x0d, 0x00, 0xad, 0xe3,
                    0x76, 0x0d, 0x45,
                ],
                [
                    0x91, 0x03, 0x92, 0x63, 0x01, 0x6d, 0xa6, 0x35, 0x77, 0x0d, 0x57, 0xdb, 0x92,
                    0xf9, 0x8b, 0xd0,
                ],
            ),
        ]
        .into_iter()
        .collect();
        let read = |name: &str| {
            let bytes = std::fs::read(dir.join(name)).unwrap();
            let mut r = SampleReader::new();
            let mut out = Vec::new();
            for c in bytes.chunks(65_536) {
                out.extend(r.push(c));
            }
            (r.tracks().to_vec(), out)
        };
        for (enc, clear, video) in [
            (
                "video_512x288_h264-360k_enc_dashinit.mp4",
                "video_512x288_h264-360k_clear_dashinit.mp4",
                true,
            ),
            (
                "audio_aac-lc_128k_enc_dashinit.mp4",
                "audio_aac-lc_128k_dashinit.mp4",
                false,
            ),
        ] {
            let (tracks, enc) = read(enc);
            let (clear_tracks, clear) = read(clear);
            assert_eq!(tracks.len(), 1);
            assert_eq!(tracks[0].video, video);
            assert!(tracks[0].encryption.is_some());
            assert_eq!(clear_tracks[0].encryption, None);
            assert_eq!(tracks[0].codec, clear_tracks[0].codec);
            match &tracks[0].codec {
                Codec::Avc { .. } => assert!(video),
                Codec::Aac { config } => assert!(!video && !config.is_empty()),
                c => panic!("codec {c:?}"),
            }
            let samples = |out: &[Output]| -> Vec<Sample> {
                out.iter()
                    .filter_map(|o| match o {
                        Output::Sample(_, s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect()
            };
            let (enc, clear) = (samples(&enc), samples(&clear));
            assert!(enc.len() > 100, "{} samples", enc.len());
            assert_eq!(enc.len(), clear.len());
            let mut keyframes = 0;
            for (e, c) in enc.iter().zip(&clear) {
                let mut data = e.data.clone();
                let x = e.encryption.as_ref().expect("encrypted");
                assert!(!x.cbc);
                let key = keys[&x.key_id];
                let cipher = <aes::Aes128 as aes::cipher::KeyInit>::new(
                    aes::cipher::generic_array::GenericArray::from_slice(&key),
                );
                let mut iv = [0u8; 16];
                iv[..x.iv.len()].copy_from_slice(&x.iv);
                let subs: Vec<(usize, usize)> = x
                    .subsamples
                    .iter()
                    .map(|&(c, e)| (c as usize, e as usize))
                    .collect();
                cenc::decrypt_cenc(&cipher, &iv, &subs, &mut data);
                assert_eq!(data, c.data, "sample at {}", c.time);
                assert_eq!((e.time, e.duration, e.key), (c.time, c.duration, c.key));
                if e.key {
                    keyframes += 1;
                }
            }
            // (every picture of the video is an IDR picture, as is every AAC frame a sync sample)
            assert_eq!(keyframes, enc.len());
        }
    }
}
