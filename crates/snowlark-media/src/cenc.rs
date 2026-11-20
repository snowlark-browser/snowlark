// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Common Encryption (ISO/IEC 23001-7) of fragmented MP4 appended through Media Source Extensions,
//! decrypted with keys the page holds (the Clear Key key system of Encrypted Media Extensions) before
//! Media Foundation parses the bytes.
//!
//! A [`Decryptor`] follows the byte stream of one `SourceBuffer` box by box. An initialization
//! segment's `moov` is read for the protected tracks (`sinf`: `frma`, `schm`, `tenc`), the defaults of
//! their fragments (`trex`) and the `pssh` boxes (the `cenc` initialization data), and rewritten in
//! place so that it describes clear media: each `encv`/`enca` sample entry takes its original format
//! from `frma`, and `sinf` and `pssh` become `free` boxes of the same size, so no offset moves. Each
//! media segment (`moof` and its `mdat`) of a protected track is decrypted in place from the sample
//! auxiliary information (`senc`, or `saiz` and `saio`), whose boxes become `free` too. A segment
//! whose key is not known yet is held, with everything after it, until [`Decryptor::retry`] finds the
//! key: Encrypted Media Extensions' "wait for key".
//!
//! Schemes: `cenc` (AES-128 CTR over the protected ranges of each subsample, one counter run per
//! sample) and `cbcs` (AES-128 CBC with a crypt/skip block pattern, restarted at each subsample, the
//! trailing partial block in the clear), the two that Encrypted Media Extensions' Clear Key streams
//! use.

use std::borrow::Cow;
use std::collections::HashMap;

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;

/// A key id (`KID`).
pub type KeyId = [u8; 16];

/// The Common SystemID of `pssh` boxes that list key ids (W3C "ISO Common Encryption 'cenc'
/// Protection Scheme" initialization data format).
pub const COMMON_SYSTEM_ID: [u8; 16] = [
    0x10, 0x77, 0xef, 0xec, 0xc0, 0xb2, 0x4d, 0x02, 0xac, 0xe3, 0x3c, 0x1e, 0x52, 0xe2, 0xfb, 0x4b,
];

/// Where decryption keys come from.
pub trait Keys {
    fn key(&self, kid: &KeyId) -> Option<[u8; 16]>;
}

impl Keys for HashMap<KeyId, [u8; 16]> {
    fn key(&self, kid: &KeyId) -> Option<[u8; 16]> {
        self.get(kid).copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Cenc,
    Cbcs,
    /// A scheme this does not decrypt (`cens`, `cbc1`, others): left as it is.
    Other,
}

/// A protected track's `tenc` and fragment defaults.
#[derive(Clone, Debug)]
struct Track {
    scheme: Scheme,
    protected: bool,
    kid: KeyId,
    iv_size: u8,
    constant_iv: Option<Vec<u8>>,
    crypt: u8,
    skip: u8,
    /// `trex` default_sample_size.
    default_size: u32,
}

/// What [`Decryptor::push`] found besides the bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The `cenc` initialization data of an initialization segment that has protected tracks: its
    /// `pssh` boxes, whole and in order (EME §? "Initialization Data Encountered").
    pub init_data: Vec<Vec<u8>>,
    /// The stream started waiting for a key.
    pub waiting: bool,
}

/// One `SourceBuffer`'s byte stream (see the module).
#[derive(Default)]
pub struct Decryptor {
    /// Protected tracks of the last initialization segment, by track id.
    tracks: HashMap<u32, Track>,
    /// `trex` default sample sizes of the last initialization segment.
    defaults: HashMap<u32, u32>,
    /// Bytes not released yet: an incomplete box this must see whole, or (while waiting for a key)
    /// everything from the segment that waits.
    pending: Vec<u8>,
    /// Bytes still to pass of a box released before it ended.
    skip: u64,
    /// The key the held segment waits for.
    missing: Option<KeyId>,
    /// The start of a box header split across appends, passed already (clear streams).
    head: Vec<u8>,
}

/// A box header: its size (0 when it runs to the end), type and header length.
fn header(b: &[u8]) -> Option<(u64, [u8; 4], usize)> {
    if b.len() < 8 {
        return None;
    }
    let size = u32::from_be_bytes(b[0..4].try_into().unwrap()) as u64;
    let typ: [u8; 4] = b[4..8].try_into().unwrap();
    if size == 1 {
        if b.len() < 16 {
            return None;
        }
        let large = u64::from_be_bytes(b[8..16].try_into().unwrap());
        return Some((large, typ, 16));
    }
    Some((size, typ, 8))
}

/// The child boxes of `b[start..end]`: (start, end, type, header length).
fn children(b: &[u8], start: usize, end: usize) -> Vec<(usize, usize, [u8; 4], usize)> {
    let mut out = Vec::new();
    let mut p = start;
    while p + 8 <= end {
        let Some((size, typ, h)) = header(&b[p..end]) else {
            break;
        };
        let size = if size == 0 { (end - p) as u64 } else { size };
        if size < h as u64 || p as u64 + size > end as u64 {
            break;
        }
        let e = p + size as usize;
        out.push((p, e, typ, h));
        p = e;
    }
    out
}

fn child(b: &[u8], start: usize, end: usize, typ: &[u8; 4]) -> Option<(usize, usize, usize)> {
    children(b, start, end)
        .into_iter()
        .find(|c| &c.2 == typ)
        .map(|(s, e, _, h)| (s, e, h))
}

fn be32(b: &[u8], p: usize) -> Option<u32> {
    b.get(p..p + 4)
        .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

fn be16(b: &[u8], p: usize) -> Option<u16> {
    b.get(p..p + 2)
        .map(|s| u16::from_be_bytes(s.try_into().unwrap()))
}

fn retype(b: &mut [u8], at: usize, typ: &[u8; 4]) {
    b[at + 4..at + 8].copy_from_slice(typ);
}

/// The key ids of the Common SystemID `pssh` boxes (version 1) in `cenc` initialization data, in
/// order and without repeats; `None` when the data is not a sequence of whole `pssh` boxes.
pub fn cenc_key_ids(init_data: &[u8]) -> Option<Vec<KeyId>> {
    let mut kids = Vec::new();
    let mut p = 0;
    if init_data.is_empty() {
        return None;
    }
    while p < init_data.len() {
        let (size, typ, h) = header(&init_data[p..])?;
        let size = size as usize;
        if &typ != b"pssh" || size < h + 4 + 16 + 4 || p + size > init_data.len() {
            return None;
        }
        let b = &init_data[p..p + size];
        let version = b[h];
        let system: [u8; 16] = b[h + 4..h + 20].try_into().unwrap();
        let mut q = h + 20;
        if version > 0 {
            let n = be32(b, q)? as usize;
            q += 4;
            if q + n * 16 + 4 > size {
                return None;
            }
            if system == COMMON_SYSTEM_ID {
                for i in 0..n {
                    let kid: KeyId = b[q + i * 16..q + i * 16 + 16].try_into().unwrap();
                    if !kids.contains(&kid) {
                        kids.push(kid);
                    }
                }
            }
            q += n * 16;
        }
        let data = be32(b, q)? as usize;
        if q + 4 + data != size {
            return None;
        }
        p += size;
    }
    Some(kids)
}

impl Decryptor {
    pub fn new() -> Decryptor {
        Decryptor::default()
    }

    /// Whether the last initialization segment had a protected track.
    pub fn encrypted(&self) -> bool {
        !self.tracks.is_empty()
    }

    /// The key the stream waits for, if any.
    pub fn waiting_for(&self) -> Option<KeyId> {
        self.missing
    }

    /// Forgets a partly received box and anything held (the `SourceBuffer`'s `abort()` and
    /// "reset parser state"); the tracks of the last initialization segment stay.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.skip = 0;
        self.missing = None;
        self.head.clear();
    }

    /// Takes appended bytes; returns the bytes for the media side now (`Cow::Borrowed(data)` when
    /// they pass unchanged) and what was found in them.
    pub fn push<'a>(&mut self, data: &'a [u8], keys: &dyn Keys) -> (Cow<'a, [u8]>, Found) {
        if self.pending.is_empty() && self.missing.is_none() && self.passes(data) {
            return (Cow::Borrowed(data), Found::default());
        }
        self.pending.extend_from_slice(data);
        let (out, found) = self.process(keys);
        (Cow::Owned(out), found)
    }

    /// After a key was added: the held bytes that can now be released.
    pub fn retry(&mut self, keys: &dyn Keys) -> (Vec<u8>, Found) {
        match self.missing {
            Some(kid) if keys.key(&kid).is_some() => {
                self.missing = None;
                self.process(keys)
            }
            _ => (Vec::new(), Found::default()),
        }
    }

    /// Whether `data` passes as it is (no box in it this must see whole), moving past it if so.
    fn passes(&mut self, data: &[u8]) -> bool {
        // A header split across appends, its start already passed: completed from these bytes.
        if !self.head.is_empty() {
            let mut hdr = std::mem::take(&mut self.head);
            let had = hdr.len() as u64;
            let take = (16 - hdr.len()).min(data.len());
            hdr.extend_from_slice(&data[..take]);
            match header(&hdr) {
                None => {
                    self.head = hdr;
                    return true;
                }
                Some((0, _, _)) => self.skip = u64::MAX,
                Some((size, _, _)) => self.skip = size.saturating_sub(had),
            }
        }
        let mut skip = self.skip;
        let mut p = 0usize;
        loop {
            let left = (data.len() - p) as u64;
            if skip >= left {
                self.skip = skip - left;
                return true;
            }
            p += skip as usize;
            let Some((size, typ, _)) = header(&data[p..]) else {
                // A clear stream's bytes are never held back: a split header passes, and a copy
                // is kept to find the box's end (an encrypted stream must see its segments whole).
                if self.encrypted() {
                    return false;
                }
                self.skip = 0;
                self.head = data[p..].to_vec();
                return true;
            };
            if &typ == b"moov" || (&typ == b"moof" && self.encrypted()) || size == 0 {
                return false;
            }
            skip = size;
        }
    }

    fn process(&mut self, keys: &dyn Keys) -> (Vec<u8>, Found) {
        let mut found = Found::default();
        let mut buf = std::mem::take(&mut self.pending);
        let mut out = Vec::with_capacity(buf.len());
        let mut p = 0usize;
        while p < buf.len() {
            if self.skip > 0 {
                let n = self.skip.min((buf.len() - p) as u64) as usize;
                out.extend_from_slice(&buf[p..p + n]);
                p += n;
                self.skip -= n as u64;
                continue;
            }
            if self.missing.is_some() {
                break;
            }
            let Some((size, typ, h)) = header(&buf[p..]) else {
                if !self.encrypted() {
                    // (as in `passes`: a clear stream's split header goes on now)
                    self.head = buf[p..].to_vec();
                    out.extend_from_slice(&buf[p..]);
                    p = buf.len();
                }
                break;
            };
            let avail = buf.len() - p;
            if size == 0 {
                // (runs to the end of the stream: passed as it comes)
                out.extend_from_slice(&buf[p..]);
                p = buf.len();
                self.skip = u64::MAX;
                break;
            }
            if size < h as u64 {
                // Not a box: the media side reports the error.
                out.extend_from_slice(&buf[p..]);
                p = buf.len();
                break;
            }
            match &typ {
                b"moov" => {
                    if (avail as u64) < size {
                        break;
                    }
                    let e = p + size as usize;
                    if let Some(init) = self.read_moov(&mut buf[p..e]) {
                        found.init_data.push(init);
                    }
                    out.extend_from_slice(&buf[p..e]);
                    p = e;
                }
                b"moof" if self.encrypted() => {
                    if (avail as u64) < size {
                        break;
                    }
                    let m = p + size as usize;
                    let Some((msize, mtyp, _)) = header(&buf[m..]) else {
                        break;
                    };
                    if &mtyp != b"mdat" || msize == 0 {
                        // (no media data after it: nothing to decrypt here)
                        out.extend_from_slice(&buf[p..m]);
                        p = m;
                        continue;
                    }
                    if ((buf.len() - m) as u64) < msize {
                        break;
                    }
                    let e = m + msize as usize;
                    match self.decrypt_fragment(&mut buf[p..e], size as usize, keys) {
                        Err(kid) => {
                            self.missing = Some(kid);
                            found.waiting = true;
                            break;
                        }
                        Ok(()) => {
                            out.extend_from_slice(&buf[p..e]);
                            p = e;
                        }
                    }
                }
                _ => {
                    let n = (size.min(avail as u64)) as usize;
                    out.extend_from_slice(&buf[p..p + n]);
                    p += n;
                    self.skip = size - n as u64;
                }
            }
        }
        buf.drain(..p);
        self.pending = buf;
        (out, found)
    }

    /// Reads and rewrites an initialization segment's `moov` (see the module); returns its `pssh`
    /// boxes when a track is protected.
    fn read_moov(&mut self, b: &mut [u8]) -> Option<Vec<u8>> {
        self.tracks.clear();
        self.defaults.clear();
        let (_, _, h) = header(b)?;
        let end = b.len();
        let mut pssh = Vec::new();
        let mut psshs = Vec::new();
        for (s, e, typ, ch) in children(b, h, end) {
            match &typ {
                b"pssh" => {
                    pssh.extend_from_slice(&b[s..e]);
                    psshs.push(s);
                }
                b"mvex" => {
                    for (ts, _, ttyp, th) in children(b, s + ch, e) {
                        if &ttyp == b"trex" {
                            if let (Some(id), Some(size)) =
                                (be32(b, ts + th + 4), be32(b, ts + th + 16))
                            {
                                self.defaults.insert(id, size);
                            }
                        }
                    }
                }
                b"trak" => self.read_trak(b, s + ch, e),
                _ => {}
            }
        }
        for (id, size) in &self.defaults {
            if let Some(t) = self.tracks.get_mut(id) {
                t.default_size = *size;
            }
        }
        if self.tracks.is_empty() {
            return None;
        }
        for s in psshs {
            retype(b, s, b"free");
        }
        Some(pssh)
    }

    fn read_trak(&mut self, b: &mut [u8], start: usize, end: usize) {
        let Some((ks, _, kh)) = child(b, start, end, b"tkhd") else {
            return;
        };
        let version = b[ks + kh];
        let Some(id) = be32(b, ks + kh + if version == 1 { 20 } else { 12 }) else {
            return;
        };
        let mut at = (start, end);
        for typ in [b"mdia", b"minf", b"stbl", b"stsd"] {
            let Some((s, e, h)) = child(b, at.0, at.1, typ) else {
                return;
            };
            at = (s + h, e);
        }
        // (stsd: version and flags, entry_count, then the sample entries)
        let (s, e) = (at.0 + 8, at.1);
        for (es, ee, etyp, eh) in children(b, s, e) {
            let video = &etyp == b"encv";
            if !video && &etyp != b"enca" {
                continue;
            }
            // The sample entry's own fields: VisualSampleEntry 78 bytes, AudioSampleEntry 28
            // (more for QuickTime sound descriptions version 1 and 2).
            let fields = if video {
                78
            } else {
                match be16(b, es + eh + 8) {
                    Some(1) => 44,
                    Some(2) => 64,
                    _ => 28,
                }
            };
            let Some((ss, se, sh)) = child(b, es + eh + fields, ee, b"sinf") else {
                continue;
            };
            let Some((fs, _, fh)) = child(b, ss + sh, se, b"frma") else {
                continue;
            };
            let format: [u8; 4] = b[fs + fh..fs + fh + 4].try_into().unwrap();
            let scheme = match child(b, ss + sh, se, b"schm") {
                Some((cs, _, ch)) => match &b[cs + ch + 4..cs + ch + 8] {
                    b"cenc" => Scheme::Cenc,
                    b"cbcs" => Scheme::Cbcs,
                    _ => Scheme::Other,
                },
                None => Scheme::Cenc,
            };
            let Some((is, ie, ih)) = child(b, ss + sh, se, b"schi") else {
                continue;
            };
            let Some((ts, te, th)) = child(b, is + ih, ie, b"tenc") else {
                continue;
            };
            let t = &b[ts + th..te];
            if t.len() < 24 {
                continue;
            }
            let (crypt, skip) = if t[0] > 0 {
                (t[5] >> 4, t[5] & 15)
            } else {
                (0, 0)
            };
            let protected = t[6] != 0;
            let iv_size = t[7];
            let kid: KeyId = t[8..24].try_into().unwrap();
            let constant_iv = if protected && iv_size == 0 && t.len() > 24 {
                let n = t[24] as usize;
                t.get(25..25 + n).map(|v| v.to_vec())
            } else {
                None
            };
            self.tracks.insert(
                id,
                Track {
                    scheme,
                    protected,
                    kid,
                    iv_size,
                    constant_iv,
                    crypt,
                    skip,
                    default_size: 0,
                },
            );
            b[es + 4..es + 8].copy_from_slice(&format);
            retype(b, ss, b"free");
        }
    }

    /// Decrypts a media segment (`moof` of `moof_len` bytes, then its `mdat`) in place; `Err` names
    /// a key it needs that `keys` does not have (and leaves the bytes as they were).
    fn decrypt_fragment(
        &self,
        b: &mut [u8],
        moof_len: usize,
        keys: &dyn Keys,
    ) -> Result<(), KeyId> {
        let mut jobs: Vec<(Track, Vec<Sample>)> = Vec::new();
        let mut retyped = Vec::new();
        let mut prev_end: Option<usize> = None;
        for (ts, te, typ, th) in children(b, 8, moof_len) {
            if &typ == b"pssh" {
                retyped.push(ts);
            }
            if &typ != b"traf" {
                continue;
            }
            let Some(traf) = self.read_traf(b, ts + th, te, prev_end, &mut retyped) else {
                continue;
            };
            prev_end = traf.data_end;
            if let Some(track) = traf.track {
                jobs.push((track, traf.samples));
            }
        }
        // Every key first: a segment is released whole or not at all.
        for (track, _) in &jobs {
            if track.protected && track.scheme != Scheme::Other && keys.key(&track.kid).is_none() {
                return Err(track.kid);
            }
        }
        for (track, samples) in &jobs {
            if !track.protected || track.scheme == Scheme::Other {
                continue;
            }
            let key = keys.key(&track.kid).unwrap();
            let cipher = Aes128::new(GenericArray::from_slice(&key));
            for s in samples {
                let Some(data) = b.get_mut(s.offset..s.offset + s.size) else {
                    continue;
                };
                match track.scheme {
                    Scheme::Cenc => decrypt_cenc(&cipher, &s.iv, &s.subsamples, data),
                    Scheme::Cbcs => decrypt_cbcs(
                        &cipher,
                        &s.iv,
                        &s.subsamples,
                        (track.crypt, track.skip),
                        data,
                    ),
                    Scheme::Other => {}
                }
            }
        }
        for at in retyped {
            retype(b, at, b"free");
        }
        Ok(())
    }

    /// A track fragment's samples: where each is in the segment, with its IV and subsamples.
    fn read_traf(
        &self,
        b: &[u8],
        start: usize,
        end: usize,
        prev_end: Option<usize>,
        retyped: &mut Vec<usize>,
    ) -> Option<Traf> {
        let kids = children(b, start, end);
        let (hs, _, hh) = kids
            .iter()
            .find(|c| &c.2 == b"tfhd")
            .map(|c| (c.0, c.1, c.3))?;
        let flags = be32(b, hs + hh)? & 0xff_ffff;
        let id = be32(b, hs + hh + 4)?;
        let mut q = hs + hh + 8;
        if flags & 1 != 0 {
            // (base-data-offset counts from the file's start, which a byte stream does not know:
            // read from the moof, as segments made for MSE place it)
            q += 8;
        }
        if flags & 2 != 0 {
            q += 4;
        }
        if flags & 8 != 0 {
            q += 4;
        }
        let tfhd_size = if flags & 0x10 != 0 { be32(b, q) } else { None };
        let track = self.tracks.get(&id).cloned();
        let default_size = tfhd_size
            .or(track.as_ref().map(|t| t.default_size))
            .or(self.defaults.get(&id).copied())
            .unwrap_or(0);
        let base = if flags & 0x2_0000 != 0 {
            0
        } else {
            prev_end.unwrap_or(0)
        };
        // Samples in order: offset and size, from each trun.
        let mut samples: Vec<Sample> = Vec::new();
        let mut data_end = None;
        let mut next = base;
        for &(rs, _, ref typ, rh) in &kids {
            if typ != b"trun" {
                continue;
            }
            let rflags = be32(b, rs + rh)? & 0xff_ffff;
            let count = be32(b, rs + rh + 4)?;
            let mut q = rs + rh + 8;
            let mut offset = next;
            if rflags & 1 != 0 {
                let d = be32(b, q)? as i32;
                offset = (base as i64 + d as i64).max(0) as usize;
                q += 4;
            }
            if rflags & 4 != 0 {
                q += 4;
            }
            for _ in 0..count {
                if rflags & 0x100 != 0 {
                    q += 4;
                }
                let size = if rflags & 0x200 != 0 {
                    let s = be32(b, q)?;
                    q += 4;
                    s
                } else {
                    default_size
                };
                if rflags & 0x400 != 0 {
                    q += 4;
                }
                if rflags & 0x800 != 0 {
                    q += 4;
                }
                samples.push(Sample {
                    offset,
                    size: size as usize,
                    iv: [0; 16],
                    subsamples: Vec::new(),
                });
                offset += size as usize;
            }
            next = offset;
            data_end = Some(offset);
        }
        let Some(track) = track else {
            return Some(Traf {
                track: None,
                samples,
                data_end,
            });
        };
        // The sample auxiliary information: `senc`, else `saiz` with `saio`.
        let senc = kids.iter().find(|c| &c.2 == b"senc");
        let saiz = kids.iter().find(|c| &c.2 == b"saiz");
        let saio = kids.iter().find(|c| &c.2 == b"saio");
        for c in kids
            .iter()
            .filter(|c| matches!(&c.2, b"senc" | b"saiz" | b"saio"))
        {
            retyped.push(c.0);
        }
        let iv_size = track.iv_size as usize;
        let read_entry = |b: &[u8], q: &mut usize, subs: bool, s: &mut Sample| -> Option<()> {
            if iv_size > 0 {
                s.iv[..iv_size].copy_from_slice(b.get(*q..*q + iv_size)?);
                *q += iv_size;
            } else if let Some(c) = &track.constant_iv {
                s.iv[..c.len().min(16)].copy_from_slice(&c[..c.len().min(16)]);
            }
            if subs {
                let n = be16(b, *q)? as usize;
                *q += 2;
                for _ in 0..n {
                    let clear = be16(b, *q)? as usize;
                    let enc = be32(b, *q + 2)? as usize;
                    *q += 6;
                    s.subsamples.push((clear, enc));
                }
            }
            Some(())
        };
        if let Some(&(ss, _, _, sh)) = senc {
            let sflags = be32(b, ss + sh)? & 0xff_ffff;
            let count = be32(b, ss + sh + 4)? as usize;
            let mut q = ss + sh + 8;
            for s in samples.iter_mut().take(count) {
                read_entry(b, &mut q, sflags & 2 != 0, s)?;
            }
        } else if let (Some(&(zs, _, _, zh)), Some(&(os, _, _, oh))) = (saiz, saio) {
            let zflags = be32(b, zs + zh)? & 0xff_ffff;
            let mut zq = zs + zh + 4;
            if zflags & 1 != 0 {
                zq += 8;
            }
            let default = *b.get(zq)? as usize;
            let count = be32(b, zq + 1)? as usize;
            let sizes_at = zq + 5;
            let oflags = be32(b, os + oh)?;
            let mut oq = os + oh + 4;
            if oflags & 1 != 0 {
                oq += 8;
            }
            let entries = be32(b, oq)?;
            if entries != 1 {
                return None;
            }
            let at = if oflags >> 24 == 1 {
                u64::from_be_bytes(b.get(oq + 4..oq + 12)?.try_into().unwrap()) as usize
            } else {
                be32(b, oq + 4)? as usize
            };
            let mut q = base + at;
            for (i, s) in samples.iter_mut().take(count).enumerate() {
                let size = if default == 0 {
                    *b.get(sizes_at + i)? as usize
                } else {
                    default
                };
                let mut r = q;
                read_entry(b, &mut r, size > iv_size, s)?;
                q += size;
            }
        } else if track.protected && track.constant_iv.is_some() {
            // (cbcs with a constant IV and whole samples encrypted: no auxiliary information)
            for s in samples.iter_mut() {
                let mut q = 0;
                read_entry(b, &mut q, false, s)?;
            }
        }
        Some(Traf {
            track: Some(track),
            samples,
            data_end,
        })
    }
}

struct Sample {
    offset: usize,
    size: usize,
    iv: [u8; 16],
    /// (clear bytes, protected bytes) pairs; empty: the whole sample is protected.
    subsamples: Vec<(usize, usize)>,
}

struct Traf {
    track: Option<Track>,
    samples: Vec<Sample>,
    data_end: Option<usize>,
}

/// The protected byte ranges of a sample.
fn protected_ranges(subsamples: &[(usize, usize)], len: usize) -> Vec<(usize, usize)> {
    if subsamples.is_empty() {
        return vec![(0, len)];
    }
    let mut out = Vec::with_capacity(subsamples.len());
    let mut p = 0;
    for &(clear, enc) in subsamples {
        p += clear;
        let s = p.min(len);
        p += enc;
        out.push((s, p.min(len)));
    }
    out
}

/// `cenc`: AES-128 CTR, the counter block the IV (8-byte IVs padded with zeros) counting up in its
/// low 64 bits, run continuously over the protected ranges of the sample.
fn decrypt_cenc(cipher: &Aes128, iv: &[u8; 16], subsamples: &[(usize, usize)], data: &mut [u8]) {
    let mut counter = *iv;
    let mut stream = [0u8; 16];
    let mut used = 16;
    for (s, e) in protected_ranges(subsamples, data.len()) {
        for byte in &mut data[s..e] {
            if used == 16 {
                stream = counter;
                cipher.encrypt_block(GenericArray::from_mut_slice(&mut stream));
                let low = u64::from_be_bytes(counter[8..16].try_into().unwrap()).wrapping_add(1);
                counter[8..16].copy_from_slice(&low.to_be_bytes());
                used = 0;
            }
            *byte ^= stream[used];
            used += 1;
        }
    }
}

/// `cbcs`: AES-128 CBC from the IV at each protected range, over `crypt` blocks of every
/// `crypt + skip` (all blocks when both are 0), the chain carried across the skipped blocks; the
/// trailing partial block stays in the clear.
fn decrypt_cbcs(
    cipher: &Aes128,
    iv: &[u8; 16],
    subsamples: &[(usize, usize)],
    (crypt, skip): (u8, u8),
    data: &mut [u8],
) {
    for (s, e) in protected_ranges(subsamples, data.len()) {
        let blocks = (e - s) / 16;
        let (crypt, skip) = if crypt == 0 && skip == 0 {
            (blocks.max(1), 0)
        } else {
            (crypt as usize, skip as usize)
        };
        let mut chain = *iv;
        let mut i = 0;
        while i < blocks {
            for _ in 0..crypt.min(blocks - i) {
                let at = s + i * 16;
                let block = &mut data[at..at + 16];
                let cipher_text: [u8; 16] = block.try_into().unwrap();
                cipher.decrypt_block(GenericArray::from_mut_slice(block));
                for (b, c) in block.iter_mut().zip(chain.iter()) {
                    *b ^= c;
                }
                chain = cipher_text;
                i += 1;
            }
            i += skip;
        }
    }
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

    const KID: KeyId = [7; 16];
    const KEY: [u8; 16] = [
        0x6b, 0x2a, 0x4e, 0x55, 0x01, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
        0xcc,
    ];

    /// The CTR encryption of `data` (its own inverse).
    fn ctr(iv: &[u8; 16], subs: &[(usize, usize)], data: &[u8]) -> Vec<u8> {
        let mut d = data.to_vec();
        decrypt_cenc(
            &Aes128::new(GenericArray::from_slice(&KEY)),
            iv,
            subs,
            &mut d,
        );
        d
    }

    fn pssh() -> Vec<u8> {
        let mut body = COMMON_SYSTEM_ID.to_vec();
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&KID);
        body.extend_from_slice(&0u32.to_be_bytes());
        full(b"pssh", 1, 0, &body)
    }

    fn init() -> Vec<u8> {
        let tkhd = full(
            b"tkhd",
            0,
            0,
            &[[0u8; 8].as_slice(), &1u32.to_be_bytes(), &[0u8; 68]].concat(),
        );
        let mut tenc = vec![0, 0, 1, 8];
        tenc.extend_from_slice(&KID);
        let sinf = bx(
            b"sinf",
            &[
                bx(b"frma", b"avc1"),
                full(b"schm", 0, 0, b"cenc\x00\x01\x00\x00"),
                bx(b"schi", &full(b"tenc", 0, 0, &tenc)),
            ]
            .concat(),
        );
        let encv = bx(
            b"encv",
            &[[0u8; 78].as_slice(), &bx(b"avcC", &[1, 2, 3]), &sinf].concat(),
        );
        let stsd = full(
            b"stsd",
            0,
            0,
            &[1u32.to_be_bytes().as_slice(), &encv].concat(),
        );
        let stbl = bx(b"stbl", &stsd);
        let trak = bx(b"trak", &[tkhd, bx(b"mdia", &bx(b"minf", &stbl))].concat());
        let trex = full(
            b"trex",
            0,
            0,
            &[1u32, 1, 0, 0, 0]
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect::<Vec<_>>(),
        );
        let moov = bx(b"moov", &[pssh(), trak, bx(b"mvex", &trex)].concat());
        [bx(b"ftyp", b"isom"), moov].concat()
    }

    /// A segment of two samples (5 and 20 bytes; the second with a clear 3-byte header).
    fn segment(clear: &[u8], plain: bool) -> Vec<u8> {
        let iv1 = [1u8; 8];
        let iv2 = [2u8; 8];
        let mut pad1 = [0u8; 16];
        pad1[..8].copy_from_slice(&iv1);
        let mut pad2 = [0u8; 16];
        pad2[..8].copy_from_slice(&iv2);
        let s1 = &clear[..5];
        let s2 = &clear[5..25];
        let mdat_body = if plain {
            clear.to_vec()
        } else {
            [ctr(&pad1, &[], s1), ctr(&pad2, &[(3, 17)], s2)].concat()
        };
        let tfhd = full(b"tfhd", 0, 0x2_0000, &1u32.to_be_bytes());
        let mut senc = 2u32.to_be_bytes().to_vec();
        senc.extend_from_slice(&iv1);
        senc.extend_from_slice(&0u16.to_be_bytes());
        senc.extend_from_slice(&iv2);
        senc.extend_from_slice(&1u16.to_be_bytes());
        senc.extend_from_slice(&3u16.to_be_bytes());
        senc.extend_from_slice(&17u32.to_be_bytes());
        let senc = full(b"senc", 0, 2, &senc);
        // trun: data offset and sample sizes; the offset is fixed after the moof's size is known.
        let trun = |off: u32| {
            full(
                b"trun",
                0,
                0x201,
                &[2u32, off, 5, 20]
                    .iter()
                    .flat_map(|v| v.to_be_bytes())
                    .collect::<Vec<_>>(),
            )
        };
        let moof_len =
            |off| 8 + bx(b"traf", &[tfhd.clone(), trun(off), senc.clone()].concat()).len();
        let off = (moof_len(0) + 8) as u32;
        let moof = bx(
            b"moof",
            &bx(b"traf", &[tfhd.clone(), trun(off), senc.clone()].concat()),
        );
        [moof, bx(b"mdat", &mdat_body)].concat()
    }

    fn clear() -> Vec<u8> {
        (0..25u8).map(|i| i.wrapping_mul(37)).collect()
    }

    fn mdat_of(b: &[u8]) -> Vec<u8> {
        let moof = u32::from_be_bytes(b[0..4].try_into().unwrap()) as usize;
        b[moof + 8..].to_vec()
    }

    #[test]
    fn clear_streams_pass_unchanged() {
        let mut d = Decryptor::new();
        let keys = HashMap::new();
        let clear_init = [bx(b"ftyp", b"isom"), bx(b"moov", &bx(b"mvhd", &[0; 4]))].concat();
        let (out, found) = d.push(&clear_init, &keys);
        assert_eq!(&*out, &clear_init[..]);
        assert_eq!(found, Found::default());
        assert!(!d.encrypted());
        let seg = segment(&clear(), true);
        // (in pieces, as appends may split boxes anywhere)
        let (a, b) = seg.split_at(11);
        let (o1, _) = d.push(a, &keys);
        assert!(matches!(o1, Cow::Borrowed(_)));
        let (o2, _) = d.push(b, &keys);
        assert!(matches!(o2, Cow::Borrowed(_)));
    }

    #[test]
    fn clear_streams_in_small_pieces_are_never_held() {
        let mut d = Decryptor::new();
        let keys = HashMap::new();
        let clear_init = [bx(b"ftyp", b"isom"), bx(b"moov", &bx(b"mvhd", &[0; 4]))].concat();
        let _ = d.push(&clear_init, &keys);
        let stream = [segment(&clear(), true), segment(&clear(), true)].concat();
        for piece in stream.chunks(3) {
            let (out, _) = d.push(piece, &keys);
            assert!(matches!(out, Cow::Borrowed(_)));
        }
    }
    #[test]
    fn init_segment_is_rewritten_and_reports_pssh() {
        let mut d = Decryptor::new();
        let init = init();
        let (out, found) = d.push(&init, &HashMap::new());
        assert!(d.encrypted());
        assert_eq!(found.init_data, vec![pssh()]);
        assert_eq!(cenc_key_ids(&found.init_data[0]), Some(vec![KID]));
        let out = out.into_owned();
        let has = |t: &[u8]| out.windows(4).any(|w| w == t);
        assert!(has(b"avc1") && !has(b"encv") && !has(b"sinf") && !has(b"pssh"));
        assert_eq!(out.len(), init.len());
    }

    #[test]
    fn segments_decrypt_once_the_key_is_known() {
        let mut d = Decryptor::new();
        let mut keys = HashMap::new();
        let _ = d.push(&init(), &keys);
        let seg = segment(&clear(), false);
        let (out, found) = d.push(&seg, &keys);
        assert!(out.is_empty());
        assert!(found.waiting);
        assert_eq!(d.waiting_for(), Some(KID));
        // Later bytes wait behind it.
        let free = bx(b"free", &[]);
        let (out, _) = d.push(&free, &keys);
        assert!(out.is_empty());
        keys.insert(KID, KEY);
        let (out, _) = d.retry(&keys);
        assert_eq!(out.len(), seg.len() + 8);
        assert_eq!(mdat_of(&out[..seg.len()]), clear());
        assert!(!out.windows(4).any(|w| w == b"senc"));
        // With the key, a segment goes through at once, in any pieces.
        let (a, b) = seg.split_at(40);
        let (o1, _) = d.push(a, &keys);
        assert!(o1.is_empty());
        let (o2, _) = d.push(b, &keys);
        assert_eq!(mdat_of(&o2), clear());
    }

    #[test]
    fn cbcs_pattern_and_partial_block() {
        let cipher = Aes128::new(GenericArray::from_slice(&KEY));
        let iv = [9u8; 16];
        // 1 crypt : 1 skip over 4 whole blocks and 5 trailing bytes.
        let plain: Vec<u8> = (0..69u8).collect();
        let mut enc = plain.clone();
        let mut chain = iv;
        for blk in [0usize, 2] {
            let b = &mut enc[blk * 16..blk * 16 + 16];
            for (x, c) in b.iter_mut().zip(chain.iter()) {
                *x ^= c;
            }
            cipher.encrypt_block(GenericArray::from_mut_slice(b));
            chain = b.try_into().unwrap();
        }
        let mut out = enc.clone();
        decrypt_cbcs(&cipher, &iv, &[], (1, 1), &mut out);
        assert_eq!(out, plain);
    }
}
