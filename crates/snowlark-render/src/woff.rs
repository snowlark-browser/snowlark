// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web font containers back to plain sfnt (TrueType/OpenType) data, which DirectWrite loads.
//!
//! - WOFF 1.0 (W3C WOFF File Format 1.0 §4–5): each table zlib-compressed.
//! - WOFF 2.0 (W3C WOFF File Format 2.0 §4–5): all tables in one Brotli stream, with `glyf`/`loca`
//!   in the transformed form of §5.1 (and optionally `hmtx` in that of §5.4), rebuilt here.
//!
//! Font data is hostile input: every read is bounds-checked and sizes are capped, a malformed file is
//! an error, never a panic.

use std::io::Read;

/// Fonts larger than this (decompressed) are refused.
const MAX_SFNT: usize = 64 << 20;

const fn tag(t: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*t)
}

const GLYF: u32 = tag(b"glyf");
const LOCA: u32 = tag(b"loca");
const HMTX: u32 = tag(b"hmtx");
const HHEA: u32 = tag(b"hhea");
const HEAD: u32 = tag(b"head");
const NAME: u32 = tag(b"name");

/// Why a font file couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontError(pub &'static str);

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

type Result<T> = std::result::Result<T, FontError>;

fn err<T>(msg: &'static str) -> Result<T> {
    Err(FontError(msg))
}

/// TrueType/OpenType bytes from a font file: WOFF and WOFF2 are unwrapped, and a font that lacks
/// the standard names gets them (see [`with_standard_names`]).
pub fn to_sfnt(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 12 {
        return err("too short for a font");
    }
    let sfnt = match u32::from_be_bytes([data[0], data[1], data[2], data[3]]) {
        0x774F_4646 => woff1(data)?,
        0x774F_4632 => woff2(data)?,
        0x0001_0000 | 0x4F54_544F | 0x7472_7565 | 0x7474_6366 => data.to_vec(),
        _ => return err("not a font format this engine reads"),
    };
    Ok(with_standard_names(&sfnt).unwrap_or(sfnt))
}

/// The names every font needs (OpenType `name` IDs 1 family, 2 subfamily, 4 full name, 5 version
/// and 6 PostScript name), with the text given to one a font lacks.
const STANDARD_NAMES: [(u16, &str); 5] = [
    (1, "Web Font"),
    (2, "Regular"),
    (4, "Web Font"),
    (5, "Version 1.000"),
    (6, "WebFont"),
];

/// A copy of sfnt `data` whose `name` table has a Macintosh and a Windows record for each standard
/// name that has neither. The text engine takes a font's names from those two platforms only and
/// refuses a font without them (one named on the Unicode platform alone, or not at all), though the
/// font is otherwise valid. `None` when no name is missing, and for a collection or a font without
/// a `name` table of format 0.
pub fn with_standard_names(data: &[u8]) -> Option<Vec<u8>> {
    let mut r = Reader::new(data);
    let flavor = r.u32().ok()?;
    if flavor == 0x7474_6366 {
        return None;
    }
    let n = r.u16().ok()? as usize;
    let mut tables = Vec::with_capacity(n);
    let mut name = None;
    for i in 0..n {
        let entry = data.get(12 + 16 * i..28 + 16 * i)?;
        let field = |at: usize| {
            u32::from_be_bytes([entry[at], entry[at + 1], entry[at + 2], entry[at + 3]])
        };
        let (tag, offset, length) = (field(0), field(8) as usize, field(12) as usize);
        let body = data.get(offset..offset.checked_add(length)?)?;
        if tag == NAME {
            name = Some(i);
        }
        tables.push((tag, body.to_vec()));
    }
    let i = name?;
    tables[i].1 = with_names_added(&tables[i].1)?;
    build_sfnt(flavor, tables).ok()
}

/// A `name` table (format 0) with the missing standard names added on the Macintosh (Roman,
/// English) and Windows (Unicode BMP, US English) platforms; `None` when none is missing.
fn with_names_added(table: &[u8]) -> Option<Vec<u8>> {
    let mut r = Reader::new(table);
    if r.u16().ok()? != 0 {
        return None;
    }
    let count = r.u16().ok()? as usize;
    let storage = r.u16().ok()? as usize;
    // (platform, encoding, language, name id, the string's bytes)
    let mut records: Vec<(u16, u16, u16, u16, Vec<u8>)> = Vec::with_capacity(count + 10);
    for _ in 0..count {
        let mut field = || r.u16().ok();
        let (platform, encoding, language, id) = (field()?, field()?, field()?, field()?);
        let (length, offset) = (field()? as usize, field()? as usize);
        // (a record whose string is outside the table is dropped)
        let start = storage + offset;
        if let Some(text) = table.get(start..start + length) {
            records.push((platform, encoding, language, id, text.to_vec()));
        }
    }
    let missing: Vec<&(u16, &str)> = STANDARD_NAMES
        .iter()
        .filter(|(id, _)| !records.iter().any(|r| r.3 == *id && (r.0 == 1 || r.0 == 3)))
        .collect();
    if missing.is_empty() {
        return None;
    }
    for (id, text) in missing {
        records.push((1, 0, 0, *id, text.as_bytes().to_vec()));
        let utf16 = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        records.push((3, 1, 0x409, *id, utf16));
    }
    // OpenType `name`: records sorted by platform, encoding, language and name ID.
    records.sort_by_key(|r| (r.0, r.1, r.2, r.3));
    let count = u16::try_from(records.len()).ok()?;
    let mut out = Vec::new();
    put16(&mut out, 0);
    put16(&mut out, count);
    put16(&mut out, 6u16.checked_add(count.checked_mul(12)?)?);
    let mut strings = Vec::new();
    for (platform, encoding, language, id, text) in &records {
        for v in [*platform, *encoding, *language, *id] {
            put16(&mut out, v);
        }
        put16(&mut out, u16::try_from(text.len()).ok()?);
        put16(&mut out, u16::try_from(strings.len()).ok()?);
        strings.extend_from_slice(text);
    }
    out.extend_from_slice(&strings);
    Some(out)
}

/// A bounds-checked big-endian reader.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(FontError("overflow"))?;
        let s = self.data.get(self.pos..end).ok_or(FontError("truncated"))?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// WOFF2 §4.1 UIntBase128.
    fn base128(&mut self) -> Result<u32> {
        let mut acc: u32 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            if i == 0 && b == 0x80 {
                return err("UIntBase128 with a leading zero");
            }
            if acc & 0xFE00_0000 != 0 {
                return err("UIntBase128 overflow");
            }
            acc = (acc << 7) | (b & 0x7F) as u32;
            if b & 0x80 == 0 {
                return Ok(acc);
            }
        }
        err("UIntBase128 too long")
    }

    /// WOFF2 §4.2 255UInt16.
    fn u255(&mut self) -> Result<u16> {
        Ok(match self.u8()? {
            253 => self.u16()?,
            255 => self.u8()? as u16 + 253,
            254 => self.u8()? as u16 + 253 * 2,
            c => c as u16,
        })
    }
}

/// Builds an sfnt from tables (tag, data), computing the directory and checksums.
fn build_sfnt(flavor: u32, mut tables: Vec<(u32, Vec<u8>)>) -> Result<Vec<u8>> {
    tables.sort_by_key(|t| t.0);
    let n = tables.len();
    if n == 0 || n > 4096 {
        return err("bad table count");
    }
    let mut pow = 1u16;
    let mut log = 0u16;
    while (pow as usize) * 2 <= n {
        pow *= 2;
        log += 1;
    }
    let mut out = Vec::new();
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&(n as u16).to_be_bytes());
    out.extend_from_slice(&(pow * 16).to_be_bytes());
    out.extend_from_slice(&log.to_be_bytes());
    out.extend_from_slice(&((n as u16) * 16 - pow * 16).to_be_bytes());
    let mut offset = 12 + 16 * n;
    let mut head_offset = None;
    for (tag, data) in &tables {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&checksum(data).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        if *tag == HEAD {
            head_offset = Some(offset);
        }
        offset += (data.len() + 3) & !3;
    }
    for (_, data) in &tables {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    if out.len() > MAX_SFNT {
        return err("font too large");
    }
    // head.checkSumAdjustment (OpenType `head`): 0xB1B0AFBA minus the whole font's checksum.
    if let Some(h) = head_offset {
        if out.len() >= h + 12 {
            out[h + 8..h + 12].copy_from_slice(&[0; 4]);
            let adj = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
            out[h + 8..h + 12].copy_from_slice(&adj.to_be_bytes());
        }
    }
    Ok(out)
}

fn checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in data.chunks(4) {
        let mut b = [0u8; 4];
        b[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(b));
    }
    sum
}

fn woff1(data: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader::new(data);
    r.u32()?;
    let flavor = r.u32()?;
    r.u32()?; // length
    let num_tables = r.u16()? as usize;
    r.pos = 44;
    let mut tables = Vec::with_capacity(num_tables);
    let mut total = 0usize;
    for _ in 0..num_tables {
        let tag = r.u32()?;
        let offset = r.u32()? as usize;
        let comp_length = r.u32()? as usize;
        let orig_length = r.u32()? as usize;
        r.u32()?; // checksum
        total = total.saturating_add(orig_length);
        if total > MAX_SFNT {
            return err("font too large");
        }
        let raw = data
            .get(offset..offset.saturating_add(comp_length))
            .ok_or(FontError("table outside the file"))?;
        let table = if comp_length < orig_length {
            let mut out = Vec::with_capacity(orig_length);
            flate2::read::ZlibDecoder::new(raw)
                .take(orig_length as u64)
                .read_to_end(&mut out)
                .map_err(|_| FontError("bad zlib data"))?;
            if out.len() != orig_length {
                return err("table length mismatch");
            }
            out
        } else {
            raw.to_vec()
        };
        tables.push((tag, table));
    }
    build_sfnt(flavor, tables)
}

/// WOFF2 §4.1 known table tags, by index.
const KNOWN_TAGS: [&[u8; 4]; 63] = [
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post", b"cvt ", b"fpgm",
    b"glyf", b"loca", b"prep", b"CFF ", b"VORG", b"EBDT", b"EBLC", b"gasp", b"hdmx", b"kern",
    b"LTSH", b"PCLT", b"VDMX", b"vhea", b"vmtx", b"BASE", b"GDEF", b"GPOS", b"GSUB", b"EBSC",
    b"JSTF", b"MATH", b"CBDT", b"CBLC", b"COLR", b"CPAL", b"SVG ", b"sbix", b"acnt", b"avar",
    b"bdat", b"bloc", b"bsln", b"cvar", b"fdsc", b"feat", b"fmtx", b"fvar", b"gvar", b"hsty",
    b"just", b"lcar", b"mort", b"morx", b"opbd", b"prop", b"trak", b"Zapf", b"Silf", b"Glat",
    b"Gloc", b"Feat", b"Sill",
];

struct Entry {
    tag: u32,
    orig_length: u32,
    /// Bytes in the decompressed stream.
    length: u32,
    transformed: bool,
}

fn woff2(data: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader::new(data);
    r.u32()?;
    let flavor = r.u32()?;
    if flavor == tag(b"ttcf") {
        return err("WOFF2 font collections are not supported");
    }
    r.u32()?; // length
    let num_tables = r.u16()? as usize;
    r.u16()?; // reserved
    r.u32()?; // totalSfntSize
    let compressed_size = r.u32()? as usize;
    r.pos = 48;
    let mut entries = Vec::with_capacity(num_tables);
    for _ in 0..num_tables {
        let flags = r.u8()?;
        let tag = match flags & 0x3F {
            63 => r.u32()?,
            i => u32::from_be_bytes(*KNOWN_TAGS[i as usize]),
        };
        let version = flags >> 6;
        let orig_length = r.base128()?;
        // §4.1: glyf and loca are transformed at version 0 (3 is the null transform); every other
        // table is transformed at a non-zero version.
        let transformed = if tag == GLYF || tag == LOCA {
            version == 0
        } else {
            version != 0
        };
        let length = if transformed {
            r.base128()?
        } else {
            orig_length
        };
        entries.push(Entry {
            tag,
            orig_length,
            length,
            transformed,
        });
    }
    let total: usize = entries.iter().map(|e| e.length as usize).sum();
    if total > MAX_SFNT {
        return err("font too large");
    }
    let compressed = r.bytes(compressed_size)?;
    let mut stream = Vec::with_capacity(total);
    brotli_decompressor::Decompressor::new(compressed, 4096)
        .take(total as u64 + 1)
        .read_to_end(&mut stream)
        .map_err(|_| FontError("bad Brotli data"))?;
    if stream.len() < total {
        return err("Brotli stream too short");
    }
    let mut pos = 0;
    let mut raw: Vec<(&Entry, &[u8])> = Vec::with_capacity(entries.len());
    for e in &entries {
        let end = pos + e.length as usize;
        raw.push((e, &stream[pos..end]));
        pos = end;
    }
    let find = |t: u32| raw.iter().find(|(e, _)| e.tag == t).map(|(e, d)| (*e, *d));
    let mut tables: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut glyf_out: Option<(Vec<u8>, Vec<u8>, Vec<i16>)> = None;
    if let Some((e, d)) = find(GLYF) {
        if e.transformed {
            let head = find(HEAD).ok_or(FontError("no head table"))?.1;
            glyf_out = Some(reconstruct_glyf(d, head)?);
        }
    }
    for (e, d) in &raw {
        let table = match e.tag {
            GLYF if e.transformed => glyf_out.as_ref().map(|g| g.0.clone()).unwrap_or_default(),
            LOCA if e.transformed => match &glyf_out {
                Some(g) => g.1.clone(),
                None => return err("transformed loca without glyf"),
            },
            HMTX if e.transformed => {
                let x_mins = glyf_out
                    .as_ref()
                    .map(|g| g.2.as_slice())
                    .ok_or(FontError("transformed hmtx without glyf"))?;
                let hhea = find(HHEA).ok_or(FontError("no hhea table"))?.1;
                reconstruct_hmtx(d, hhea, x_mins)?
            }
            _ if e.transformed => return err("unknown table transform"),
            _ => d.to_vec(),
        };
        if !e.transformed && table.len() != e.orig_length as usize {
            return err("table length mismatch");
        }
        tables.push((e.tag, table));
    }
    // A transformed loca's index format comes from head.indexToLocFormat, which the rebuilt glyf
    // honours; nothing else to patch.
    build_sfnt(flavor, tables)
}

/// WOFF2 §5.2 triplet decoding: the point delta for a flag and its data bytes.
fn triplet(flag: u8, r: &mut Reader) -> Result<(i32, i32)> {
    let f = (flag & 0x7F) as i32;
    let sign = |bit: i32, v: i32| if bit & 1 == 1 { v } else { -v };
    Ok(if f < 10 {
        let b0 = r.u8()? as i32;
        (0, sign(f, ((f & 14) << 7) + b0))
    } else if f < 20 {
        let b0 = r.u8()? as i32;
        (sign(f, (((f - 10) & 14) << 7) + b0), 0)
    } else if f < 84 {
        let b0 = f - 20;
        let b1 = r.u8()? as i32;
        (
            sign(f, 1 + (b0 & 0x30) + (b1 >> 4)),
            sign(f >> 1, 1 + ((b0 & 0x0C) << 2) + (b1 & 0x0F)),
        )
    } else if f < 120 {
        let b0 = f - 84;
        let (b1, b2) = (r.u8()? as i32, r.u8()? as i32);
        (
            sign(f, 1 + ((b0 / 12) << 8) + b1),
            sign(f >> 1, 1 + (((b0 % 12) >> 2) << 8) + b2),
        )
    } else if f < 124 {
        let (b1, b2, b3) = (r.u8()? as i32, r.u8()? as i32, r.u8()? as i32);
        (
            sign(f, (b1 << 4) + (b2 >> 4)),
            sign(f >> 1, ((b2 & 0x0F) << 8) + b3),
        )
    } else {
        let (b1, b2, b3, b4) = (
            r.u8()? as i32,
            r.u8()? as i32,
            r.u8()? as i32,
            r.u8()? as i32,
        );
        (sign(f, (b1 << 8) + b2), sign(f >> 1, (b3 << 8) + b4))
    })
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// A simple glyph's flags and coordinates (OpenType `glyf`, simple glyph description): one-byte
/// vectors with sign bits where the delta fits, nothing for a zero delta, runs of equal flags as
/// REPEAT_FLAG.
fn encode_points(out: &mut Vec<u8>, on: &[bool], xs: &[i32], ys: &[i32], overlap: bool) {
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const REPEAT: u8 = 0x08;
    const X_SAME_OR_POSITIVE: u8 = 0x10;
    const Y_SAME_OR_POSITIVE: u8 = 0x20;
    const OVERLAP_SIMPLE: u8 = 0x40;
    let mut flags = Vec::with_capacity(on.len());
    let mut xb = Vec::with_capacity(on.len() * 2);
    let mut yb = Vec::with_capacity(on.len() * 2);
    let (mut px, mut py) = (0i32, 0i32);
    for k in 0..on.len() {
        let mut f = if on[k] { ON_CURVE } else { 0 };
        if k == 0 && overlap {
            f |= OVERLAP_SIMPLE;
        }
        let dx = xs[k] - px;
        let dy = ys[k] - py;
        px = xs[k];
        py = ys[k];
        if dx == 0 {
            f |= X_SAME_OR_POSITIVE;
        } else if dx.abs() < 256 {
            f |= X_SHORT;
            if dx > 0 {
                f |= X_SAME_OR_POSITIVE;
            }
            xb.push(dx.unsigned_abs() as u8);
        } else {
            xb.extend_from_slice(&(dx as i16).to_be_bytes());
        }
        if dy == 0 {
            f |= Y_SAME_OR_POSITIVE;
        } else if dy.abs() < 256 {
            f |= Y_SHORT;
            if dy > 0 {
                f |= Y_SAME_OR_POSITIVE;
            }
            yb.push(dy.unsigned_abs() as u8);
        } else {
            yb.extend_from_slice(&(dy as i16).to_be_bytes());
        }
        flags.push(f);
    }
    let mut k = 0;
    while k < flags.len() {
        let f = flags[k];
        let mut run = 1;
        while k + run < flags.len() && flags[k + run] == f && run < 256 {
            run += 1;
        }
        if run > 1 {
            out.push(f | REPEAT);
            out.push((run - 1) as u8);
        } else {
            out.push(f);
        }
        k += run;
    }
    out.extend_from_slice(&xb);
    out.extend_from_slice(&yb);
}

/// WOFF2 §5.1: rebuilds `glyf` and `loca` from the transformed `glyf`. Also returns each glyph's xMin
/// (for a transformed `hmtx`).
fn reconstruct_glyf(data: &[u8], head: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<i16>)> {
    let mut h = Reader::new(data);
    h.u16()?; // reserved
    let option_flags = h.u16()?;
    let num_glyphs = h.u16()? as usize;
    let index_format = h.u16()?;
    let sizes: Vec<usize> = (0..7)
        .map(|_| h.u32().map(|v| v as usize))
        .collect::<Result<_>>()?;
    let mut offset = h.pos;
    let mut streams = Vec::with_capacity(7);
    for s in &sizes {
        let end = offset.checked_add(*s).ok_or(FontError("overflow"))?;
        streams.push(
            data.get(offset..end)
                .ok_or(FontError("glyf stream outside the table"))?,
        );
        offset = end;
    }
    let overlap = if option_flags & 1 != 0 {
        data.get(offset..offset + num_glyphs.div_ceil(8))
    } else {
        None
    };
    let mut n_contours = Reader::new(streams[0]);
    let mut n_points = Reader::new(streams[1]);
    let mut flags = Reader::new(streams[2]);
    let mut glyphs = Reader::new(streams[3]);
    let mut composites = Reader::new(streams[4]);
    let bbox_bitmap_len = 4 * num_glyphs.div_ceil(32);
    let bbox_bitmap = streams[5]
        .get(..bbox_bitmap_len)
        .ok_or(FontError("bbox bitmap truncated"))?;
    let mut bboxes = Reader::new(&streams[5][bbox_bitmap_len..]);
    let mut instructions = Reader::new(streams[6]);
    // head.indexToLocFormat must agree with the transform's index format.
    let long_loca = index_format != 0;
    if head.len() >= 52 && (u16::from_be_bytes([head[50], head[51]]) != 0) != long_loca {
        return err("loca format disagrees with head");
    }

    let mut glyf = Vec::new();
    let mut loca = Vec::with_capacity((num_glyphs + 1) * 4);
    let mut x_mins = vec![0i16; num_glyphs];
    let push_loca = |loca: &mut Vec<u8>, off: usize| {
        if long_loca {
            loca.extend_from_slice(&(off as u32).to_be_bytes());
        } else {
            put16(loca, (off / 2) as u16);
        }
    };
    for (i, x_min_out) in x_mins.iter_mut().enumerate() {
        push_loca(&mut loca, glyf.len());
        let has_bbox = bbox_bitmap[i / 8] & (0x80 >> (i % 8)) != 0;
        let nc = n_contours.i16()?;
        if nc == 0 {
            if has_bbox {
                return err("empty glyph with a bbox");
            }
            continue;
        }
        let start = glyf.len();
        if nc > 0 {
            let nc = nc as usize;
            let mut end_points = Vec::with_capacity(nc);
            let mut total = 0usize;
            for _ in 0..nc {
                total += n_points.u255()? as usize;
                if total == 0 || total > 0xFFFF {
                    return err("bad point count");
                }
                end_points.push((total - 1) as u16);
            }
            let mut xs = Vec::with_capacity(total);
            let mut ys = Vec::with_capacity(total);
            let mut on = Vec::with_capacity(total);
            let (mut x, mut y) = (0i32, 0i32);
            for _ in 0..total {
                let f = flags.u8()?;
                let (dx, dy) = triplet(f, &mut glyphs)?;
                x += dx;
                y += dy;
                xs.push(x);
                ys.push(y);
                on.push(f & 0x80 == 0);
            }
            let instruction_len = glyphs.u255()? as usize;
            let code = instructions.bytes(instruction_len)?;
            let (x_min, y_min, x_max, y_max) = if has_bbox {
                (bboxes.i16()?, bboxes.i16()?, bboxes.i16()?, bboxes.i16()?)
            } else {
                let clamp = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                (
                    clamp(*xs.iter().min().unwrap()),
                    clamp(*ys.iter().min().unwrap()),
                    clamp(*xs.iter().max().unwrap()),
                    clamp(*ys.iter().max().unwrap()),
                )
            };
            *x_min_out = x_min;
            put16(&mut glyf, nc as u16);
            for v in [x_min, y_min, x_max, y_max] {
                put16(&mut glyf, v as u16);
            }
            for e in &end_points {
                put16(&mut glyf, *e);
            }
            put16(&mut glyf, instruction_len as u16);
            glyf.extend_from_slice(code);
            // Flags and coordinates in the compact TrueType encoding (short vectors, "same" bits,
            // repeated flags), so the glyphs fit the font's loca format as the original did.
            let overlaps = overlap.is_some_and(|o| o[i / 8] & (0x80 >> (i % 8)) != 0);
            encode_points(&mut glyf, &on, &xs, &ys, overlaps);
        } else {
            // Composite: the components verbatim from the composite stream.
            if !has_bbox {
                return err("composite glyph without a bbox");
            }
            let bbox = [bboxes.i16()?, bboxes.i16()?, bboxes.i16()?, bboxes.i16()?];
            *x_min_out = bbox[0];
            put16(&mut glyf, nc as u16);
            for v in bbox {
                put16(&mut glyf, v as u16);
            }
            let mut have_instructions = false;
            loop {
                let f = composites.u16()?;
                let glyph = composites.u16()?;
                let mut len = if f & 1 != 0 { 4 } else { 2 };
                if f & 0x08 != 0 {
                    len += 2;
                } else if f & 0x40 != 0 {
                    len += 4;
                } else if f & 0x80 != 0 {
                    len += 8;
                }
                put16(&mut glyf, f);
                put16(&mut glyf, glyph);
                glyf.extend_from_slice(composites.bytes(len)?);
                have_instructions |= f & 0x100 != 0;
                if f & 0x20 == 0 {
                    break;
                }
            }
            if have_instructions {
                let n = glyphs.u255()? as usize;
                put16(&mut glyf, n as u16);
                glyf.extend_from_slice(instructions.bytes(n)?);
            }
        }
        while (glyf.len() - start) % 4 != 0 {
            glyf.push(0);
        }
        if glyf.len() > MAX_SFNT {
            return err("font too large");
        }
    }
    push_loca(&mut loca, glyf.len());
    if !long_loca && glyf.len() / 2 > 0xFFFF {
        return err("glyf too large for short loca");
    }
    Ok((glyf, loca, x_mins))
}

/// WOFF2 §5.4: rebuilds `hmtx`; absent left side bearings are the glyphs' xMin.
fn reconstruct_hmtx(data: &[u8], hhea: &[u8], x_mins: &[i16]) -> Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let flags = r.u8()?;
    let num_h = hhea
        .get(34..36)
        .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
        .ok_or(FontError("hhea truncated"))?;
    let num_glyphs = x_mins.len();
    if num_h == 0 || num_h > num_glyphs {
        return err("bad numberOfHMetrics");
    }
    let advances: Vec<u16> = (0..num_h).map(|_| r.u16()).collect::<Result<_>>()?;
    let lsbs: Vec<i16> = if flags & 1 == 0 {
        (0..num_h).map(|_| r.i16()).collect::<Result<_>>()?
    } else {
        x_mins[..num_h].to_vec()
    };
    let rest: Vec<i16> = if flags & 2 == 0 {
        (num_h..num_glyphs)
            .map(|_| r.i16())
            .collect::<Result<_>>()?
    } else {
        x_mins[num_h..].to_vec()
    };
    let mut out = Vec::with_capacity(num_h * 4 + rest.len() * 2);
    for (a, l) in advances.iter().zip(&lsbs) {
        put16(&mut out, *a);
        put16(&mut out, *l as u16);
    }
    for l in rest {
        put16(&mut out, l as u16);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base128_and_255uint16() {
        assert_eq!(Reader::new(&[0x3F]).base128(), Ok(63));
        assert_eq!(Reader::new(&[0x81, 0x00]).base128(), Ok(128));
        assert!(Reader::new(&[0x80, 0x01]).base128().is_err());
        assert_eq!(Reader::new(&[252]).u255(), Ok(252));
        assert_eq!(Reader::new(&[255, 0]).u255(), Ok(253));
        assert_eq!(Reader::new(&[254, 0]).u255(), Ok(506));
        assert_eq!(Reader::new(&[253, 0x12, 0x34]).u255(), Ok(0x1234));
    }

    #[test]
    fn triplets() {
        // Flag 0: dx 0, dy = -(b0); flag 1: +b0.
        assert_eq!(triplet(0, &mut Reader::new(&[5])), Ok((0, -5)));
        assert_eq!(triplet(1, &mut Reader::new(&[5])), Ok((0, 5)));
        assert_eq!(triplet(11, &mut Reader::new(&[7])), Ok((7, 0)));
        // 4-byte form: flag 127 (both positive).
        assert_eq!(
            triplet(127, &mut Reader::new(&[0x01, 0x00, 0x00, 0x10])),
            Ok((256, 16))
        );
    }

    #[test]
    fn sfnt_passes_through_and_garbage_is_refused() {
        let ahem = include_bytes!("../../snowlark-layout/tests/fixtures/Ahem.ttf");
        assert_eq!(to_sfnt(ahem).unwrap(), ahem.to_vec());
        assert!(to_sfnt(b"not a font at all").is_err());
        assert!(to_sfnt(b"wOF2\0\0\0\0\0\0\0\0\0\0\0\0").is_err());
    }

    #[test]
    fn woff1_roundtrip() {
        // Wrap Ahem's tables in a WOFF 1.0 container (zlib-compressed) and unwrap it.
        use std::io::Write;
        let ahem = include_bytes!("../../snowlark-layout/tests/fixtures/Ahem.ttf");
        let mut r = Reader::new(ahem);
        let flavor = r.u32().unwrap();
        let n = r.u16().unwrap() as usize;
        let mut tables = Vec::new();
        for i in 0..n {
            let e = &ahem[12 + 16 * i..];
            let t = u32::from_be_bytes([e[0], e[1], e[2], e[3]]);
            let off = u32::from_be_bytes([e[8], e[9], e[10], e[11]]) as usize;
            let len = u32::from_be_bytes([e[12], e[13], e[14], e[15]]) as usize;
            tables.push((t, ahem[off..off + len].to_vec()));
        }
        let mut woff = Vec::new();
        woff.extend_from_slice(b"wOFF");
        woff.extend_from_slice(&flavor.to_be_bytes());
        woff.extend_from_slice(&[0; 4]);
        woff.extend_from_slice(&(n as u16).to_be_bytes());
        woff.resize(44 + 20 * n, 0);
        let mut data = Vec::new();
        for (i, (t, d)) in tables.iter().enumerate() {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            z.write_all(d).unwrap();
            let c = z.finish().unwrap();
            let stored = if c.len() < d.len() { c } else { d.clone() };
            let off = woff.len() + data.len();
            let e = 44 + 20 * i;
            woff[e..e + 4].copy_from_slice(&t.to_be_bytes());
            woff[e + 4..e + 8].copy_from_slice(&(off as u32).to_be_bytes());
            woff[e + 8..e + 12].copy_from_slice(&(stored.len() as u32).to_be_bytes());
            woff[e + 12..e + 16].copy_from_slice(&(d.len() as u32).to_be_bytes());
            data.extend_from_slice(&stored);
            while data.len() % 4 != 0 {
                data.push(0);
            }
        }
        woff.extend_from_slice(&data);
        let sfnt = to_sfnt(&woff).unwrap();
        // Same tables in the rebuilt file.
        let mut r = Reader::new(&sfnt);
        r.u32().unwrap();
        assert_eq!(r.u16().unwrap() as usize, n);
        let engine = snowlark_paint::TextEngine::new().unwrap();
        assert!(engine.add_font_data(&sfnt).is_ok());
    }

    #[test]
    fn woff2_with_transformed_glyf_loads_and_shapes() {
        let woff2 = include_bytes!("../tests/fixtures/noto-sans-latin-400.woff2");
        let sfnt = to_sfnt(woff2).unwrap();
        // The rebuilt font starts with a TrueType sfnt header and has glyf and loca tables.
        assert_eq!(&sfnt[..4], &[0, 1, 0, 0]);
        let n = u16::from_be_bytes([sfnt[4], sfnt[5]]) as usize;
        let tags: Vec<&[u8]> = (0..n).map(|i| &sfnt[12 + 16 * i..16 + 16 * i]).collect();
        assert!(tags.contains(&&b"glyf"[..]) && tags.contains(&&b"loca"[..]));
        let engine = snowlark_paint::TextEngine::new().unwrap();
        let name = engine.add_font_data(&sfnt).unwrap();
        let list = engine
            .font_list(&snowlark_paint::FontQuery {
                families: &[&name],
                weight: 400,
                style: snowlark_paint::FontStyle::Normal,
                stretch: snowlark_paint::FontStretch::NORMAL,
            })
            .unwrap();
        let font = engine.primary_font(list);
        assert_eq!(engine.family_name(font).as_deref(), Some("Noto Sans"));
        // Noto Sans' advances: "ii" is narrower than "MM", and both are sane at 100 px.
        let ii = engine.measure("ii", list, 100.0).unwrap();
        let mm = engine.measure("MM", list, 100.0).unwrap();
        assert!(ii > 20.0 && ii < 80.0, "{ii}");
        assert!(mm > 120.0 && mm < 220.0, "{mm}");
    }

    /// `SNOWLARK_WOFF=<file> cargo test -- --ignored woff_file`: decodes a font file and loads it.
    #[test]
    #[ignore]
    fn woff_file() {
        let path = std::env::var("SNOWLARK_WOFF").expect("SNOWLARK_WOFF");
        let data = std::fs::read(path).unwrap();
        let sfnt = to_sfnt(&data).unwrap();
        let engine = snowlark_paint::TextEngine::new().unwrap();
        engine.add_font_data(&sfnt).unwrap();
    }

    /// Ahem with its `name` table cut to the first `keep` records (they are sorted by platform:
    /// ten Unicode, then thirteen Macintosh, then thirteen Windows).
    fn ahem_with_names(keep: u16) -> Vec<u8> {
        let mut data = include_bytes!("../../snowlark-layout/tests/fixtures/Ahem.ttf").to_vec();
        let n = u16::from_be_bytes([data[4], data[5]]) as usize;
        let at = (0..n)
            .map(|i| 12 + 16 * i)
            .find(|&at| &data[at..at + 4] == b"name")
            .unwrap();
        let offset = u32::from_be_bytes(data[at + 8..at + 12].try_into().unwrap()) as usize;
        data[offset + 2..offset + 4].copy_from_slice(&keep.to_be_bytes());
        data
    }

    #[test]
    fn fonts_named_on_the_unicode_platform_only_or_not_at_all_load() {
        let engine = snowlark_paint::TextEngine::new().unwrap();
        for keep in [10, 0] {
            let sfnt = to_sfnt(&ahem_with_names(keep)).unwrap();
            let name = engine.add_font_data(&sfnt).unwrap();
            let list = engine
                .font_list(&snowlark_paint::FontQuery {
                    families: &[&name],
                    weight: 400,
                    style: snowlark_paint::FontStyle::Normal,
                    stretch: snowlark_paint::FontStretch::NORMAL,
                })
                .unwrap();
            // Ahem's glyphs are 1em squares.
            let width = engine.measure("XX", list, 10.0).unwrap();
            assert!((width - 20.0).abs() < 0.01, "{keep} records: {width}");
        }
        // A font with Macintosh or Windows names is left as it is.
        let ahem = ahem_with_names(36);
        assert_eq!(with_standard_names(&ahem), None);
        assert_eq!(with_standard_names(&ahem_with_names(23)), None);
        assert_eq!(to_sfnt(&ahem).unwrap(), ahem);
    }
}
