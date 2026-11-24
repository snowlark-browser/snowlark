// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web font files go through OTS before the text engine reads them (`web_font`): the formats it
//! reads, what it refuses and why, what it keeps and drops, and that what it makes loads and
//! measures as the original does.

use std::io::Write;

use snowlark_paint::raster_tables::{FontFile, FontTable, MAX_FONT_BYTES};
use snowlark_paint::web_font::{sanitize, FontLoadError};
use snowlark_paint::*;

const AHEM: &[u8] = include_bytes!("../../snowlark-layout/tests/fixtures/Ahem.ttf");
const NOTO_WOFF2: &[u8] = include_bytes!("fixtures/noto-sans-latin-400.woff2");

fn tag(t: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*t)
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

/// An sfnt's flavor and tables (tag, bytes), in directory order.
fn tables(sfnt: &[u8]) -> (u32, Vec<(u32, Vec<u8>)>) {
    let n = u16_at(sfnt, 4) as usize;
    let tables = (0..n)
        .map(|i| {
            let e = 12 + 16 * i;
            let (offset, length) = (u32_at(sfnt, e + 8) as usize, u32_at(sfnt, e + 12) as usize);
            (u32_at(sfnt, e), sfnt[offset..offset + length].to_vec())
        })
        .collect();
    (u32_at(sfnt, 0), tables)
}

fn tags(sfnt: &[u8]) -> Vec<u32> {
    tables(sfnt).1.into_iter().map(|(t, _)| t).collect()
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, c| {
        let mut word = [0; 4];
        word[..c.len()].copy_from_slice(c);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

/// An sfnt of these tables (sorted by tag, as the directory must be).
fn sfnt(flavor: u32, mut tables: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|t| t.0);
    let n = tables.len();
    let mut out = Vec::new();
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&(n as u16).to_be_bytes());
    let pow = 1u16 << (15 - (n as u16).leading_zeros());
    for v in [
        pow * 16,
        15 - pow.leading_zeros() as u16,
        n as u16 * 16 - pow * 16,
    ] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    let mut offset = 12 + 16 * n;
    let mut body = Vec::new();
    for (t, data) in &tables {
        out.extend_from_slice(&t.to_be_bytes());
        out.extend_from_slice(&checksum(data).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        while body.len() % 4 != 0 {
            body.push(0);
        }
        offset = 12 + 16 * n + body.len();
    }
    out.extend_from_slice(&body);
    out
}

/// A WOFF 1.0 file of an sfnt's tables, each zlib-compressed where that makes it smaller.
fn woff1(sfnt: &[u8]) -> Vec<u8> {
    let (flavor, tables) = tables(sfnt);
    let n = tables.len();
    let mut woff = Vec::new();
    woff.extend_from_slice(b"wOFF");
    woff.extend_from_slice(&flavor.to_be_bytes());
    woff.extend_from_slice(&[0; 4]);
    woff.extend_from_slice(&(n as u16).to_be_bytes());
    woff.extend_from_slice(&[0; 2]);
    woff.extend_from_slice(&(sfnt.len() as u32).to_be_bytes());
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
        woff[e + 16..e + 20].copy_from_slice(&checksum(d).to_be_bytes());
        data.extend_from_slice(&stored);
        while data.len() % 4 != 0 {
            data.push(0);
        }
    }
    woff.extend_from_slice(&data);
    let len = woff.len() as u32;
    woff[8..12].copy_from_slice(&len.to_be_bytes());
    woff
}

fn list(engine: &TextEngine, family: &str) -> FontListId {
    engine
        .font_list(&FontQuery {
            families: &[family],
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::NORMAL,
        })
        .unwrap()
}

/// Ahem's glyphs are 1em squares: "XX" at 10 px is 20 px wide.
fn assert_ahem(engine: &TextEngine, family: &str) {
    let width = engine.measure("XX", list(engine, family), 10.0).unwrap();
    assert!((width - 20.0).abs() < 0.01, "{width}");
}

#[test]
fn truetype_woff_and_woff2_load_and_measure() {
    let engine = TextEngine::new().unwrap();
    let ttf = engine.add_font_data(AHEM).unwrap();
    assert_ahem(&engine, &ttf);
    let woff = engine.add_font_data(&woff1(AHEM)).unwrap();
    assert_ahem(&engine, &woff);
    // WOFF 1.0 unwraps to the font OTS makes of the TrueType file itself.
    assert_eq!(sanitize(&woff1(AHEM)), sanitize(AHEM));

    // WOFF 2.0 with a transformed `glyf`/`loca`, rebuilt as TrueType.
    let noto = sanitize(NOTO_WOFF2).unwrap();
    assert_eq!(&noto[..4], &[0, 1, 0, 0]);
    let tags = tags(&noto);
    assert!(tags.contains(&tag(b"glyf")) && tags.contains(&tag(b"loca")));
    let name = engine.add_font_data(NOTO_WOFF2).unwrap();
    let list = list(&engine, &name);
    let font = engine.primary_font(list);
    assert_eq!(engine.family_name(font).as_deref(), Some("Noto Sans"));
    // Noto Sans' advances: "ii" is narrower than "MM", and both are sane at 100 px.
    let ii = engine.measure("ii", list, 100.0).unwrap();
    let mm = engine.measure("MM", list, 100.0).unwrap();
    assert!(ii > 20.0 && ii < 80.0, "{ii}");
    assert!(mm > 120.0 && mm < 220.0, "{mm}");
}

#[test]
fn what_the_sanitizer_makes_it_takes_unchanged() {
    // The raster side sanitizes the renderer's bytes again: that must change nothing.
    for file in [AHEM.to_vec(), woff1(AHEM), NOTO_WOFF2.to_vec()] {
        let once = sanitize(&file).unwrap();
        assert_eq!(sanitize(&once).unwrap(), once);
    }
}

#[test]
fn malformed_files_are_refused_with_the_reason() {
    let engine = TextEngine::new().unwrap();
    let mut truncated_woff = woff1(AHEM);
    truncated_woff.truncate(truncated_woff.len() / 2);
    let mut bad_zlib = woff1(AHEM);
    let first = u32_at(&bad_zlib, 44 + 4) as usize;
    bad_zlib[first..first + 8].fill(0xFF);
    let mut bad_brotli = NOTO_WOFF2.to_vec();
    let at = bad_brotli.len() / 2;
    bad_brotli[at..at + 64].fill(0x55);
    let cases: [(&str, Vec<u8>); 7] = [
        ("empty", Vec::new()),
        ("not a font", b"not a font at all, just some text".to_vec()),
        (
            "a WOFF 2.0 header alone",
            b"wOF2\0\0\0\0\0\0\0\0\0\0\0\0".to_vec(),
        ),
        ("half of a TrueType file", AHEM[..AHEM.len() / 2].to_vec()),
        ("half of a WOFF 1.0 file", truncated_woff),
        ("a WOFF 1.0 table that is not zlib", bad_zlib),
        ("a WOFF 2.0 stream that is not Brotli", bad_brotli),
    ];
    for (what, file) in cases {
        let why = sanitize(&file).expect_err(what);
        assert!(!why.0.is_empty(), "{what}: no reason");
        match engine.add_font_data(&file) {
            Err(FontLoadError::Rejected(r)) => assert_eq!(r, why, "{what}"),
            other => panic!("{what}: {other:?}"),
        }
    }
    let shown = format!("{}", engine.add_font_data(b"junk").unwrap_err());
    assert!(shown.starts_with("OTS parsing error: "), "{shown}");
}

#[test]
fn a_file_too_large_is_refused_before_it_is_read() {
    let mut huge = AHEM.to_vec();
    huge.resize(MAX_FONT_BYTES + 1, 0);
    assert!(sanitize(&huge).unwrap_err().0.contains("more than 64MB"));
}

/// A font whose `loca` points past the end of `glyf` is refused: the glyph table is required.
#[test]
fn glyph_offsets_outside_the_glyph_table_are_refused() {
    let (flavor, mut t) = tables(AHEM);
    let glyf_len = t.iter().find(|(g, _)| *g == tag(b"glyf")).unwrap().1.len();
    let head = &t.iter().find(|(g, _)| *g == tag(b"head")).unwrap().1;
    let long = u16_at(head, 50) == 1;
    let loca = &mut t.iter_mut().find(|(g, _)| *g == tag(b"loca")).unwrap().1;
    // The last offset (the end of the last glyph) far past the table.
    let n = loca.len();
    if long {
        loca[n - 4..].copy_from_slice(&((glyf_len as u32 + 4096) * 2).to_be_bytes());
    } else {
        loca[n - 2..].copy_from_slice(&((glyf_len as u16 / 2).wrapping_add(4096)).to_be_bytes());
    }
    let bad = sfnt(flavor, t);
    assert!(sanitize(&bad).unwrap_err().0.contains("glyf"));
}

#[test]
fn tables_the_sanitizer_does_not_know_are_dropped_and_color_bitmaps_kept() {
    let (flavor, mut t) = tables(AHEM);
    for (name, body) in [
        (b"ZZZZ", vec![1u8; 40]),
        (b"SVG ", vec![0u8; 40]),
        (b"CBDT", vec![0, 3, 0, 0]),
        (b"CBLC", vec![0, 3, 0, 0, 0, 0, 0, 0]),
    ] {
        t.push((tag(name), body));
    }
    let font = sfnt(flavor, t);
    let out = tags(&sanitize(&font).unwrap());
    assert!(!out.contains(&tag(b"ZZZZ")) && !out.contains(&tag(b"SVG ")));
    assert!(out.contains(&tag(b"CBDT")) && out.contains(&tag(b"CBLC")));

    // What the text engine registers, and sends to the raster side, is the sanitized font.
    let engine = TextEngine::new().unwrap();
    let family = engine.add_font_data(&font).unwrap();
    assert_ahem(&engine, &family);
    let id = engine.primary_font(list(&engine, &family));
    let source = engine.font_source(id).unwrap();
    let FontFile::Data(bytes) = &source.file else {
        panic!("a web font goes as its bytes");
    };
    assert!(!tags(bytes).contains(&tag(b"ZZZZ")));
    let mut table = FontTable::new().unwrap();
    table.add(id, &source).unwrap();
}

#[test]
fn the_raster_side_refuses_a_malformed_font_from_the_renderer() {
    let engine = TextEngine::new().unwrap();
    let family = engine.add_font_data(AHEM).unwrap();
    let id = engine.primary_font(list(&engine, &family));
    let mut source = engine.font_source(id).unwrap();
    // A renderer that sends other bytes for the font than the sanitizer made.
    source.file = FontFile::Data(AHEM[..AHEM.len() / 2].to_vec());
    let mut table = FontTable::new().unwrap();
    assert!(table.add(id, &source).is_err());
}

/// Ahem with its `name` table cut to the first `keep` records (they are sorted by platform: ten
/// Unicode, then thirteen Macintosh, then thirteen Windows).
fn ahem_with_names(keep: u16) -> Vec<u8> {
    let mut data = AHEM.to_vec();
    let n = u16_at(&data, 4) as usize;
    let at = (0..n)
        .map(|i| 12 + 16 * i)
        .find(|&at| &data[at..at + 4] == b"name")
        .unwrap();
    let offset = u32_at(&data, at + 8) as usize;
    data[offset + 2..offset + 4].copy_from_slice(&keep.to_be_bytes());
    data
}

/// The text engine takes a font's names from the Macintosh and Windows platforms only; the
/// sanitizer gives a font without them the standard ones.
#[test]
fn fonts_named_on_the_unicode_platform_only_or_not_at_all_load() {
    let engine = TextEngine::new().unwrap();
    for keep in [10, 0] {
        let name = engine.add_font_data(&ahem_with_names(keep)).unwrap();
        assert_ahem(&engine, &name);
    }
}

/// Mutated fonts: whatever the sanitizer accepts, the text engine loads; nothing crashes.
#[test]
fn mutated_fonts_are_refused_or_load() {
    let engine = TextEngine::new().unwrap();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let (mut accepted, mut refused) = (0, 0);
    for base in [AHEM.to_vec(), woff1(AHEM), NOTO_WOFF2.to_vec()] {
        for _ in 0..400 {
            let mut file = base.clone();
            for _ in 0..1 + next() % 8 {
                let at = (next() as usize) % file.len();
                file[at] = next() as u8;
            }
            match sanitize(&file) {
                Ok(font) => {
                    accepted += 1;
                    if let Err(e) = engine.add_font_data(&font) {
                        panic!("accepted but not loaded: {e}");
                    }
                }
                Err(_) => refused += 1,
            }
        }
    }
    assert!(
        accepted > 0 && refused > 0,
        "{accepted} accepted, {refused} refused"
    );
}

/// The layout tables pass through unchanged, as other engines pass them: OTS's checks of them are
/// stricter than the text engines', and fonts in wide use fail them (here a GPOS lookup whose
/// subtable offset points at the lookup list itself, as in a font a large streaming site serves).
#[test]
fn layout_tables_pass_through_unchanged() {
    let (flavor, mut t) = tables(AHEM);
    #[rustfmt::skip]
    let gpos: Vec<u8> = vec![
        0, 1, 0, 0, // version 1.0
        0, 10, 0, 12, 0, 14, // script list, feature list, lookup list
        0, 0, // script list: no scripts
        0, 0, // feature list: no features
        0, 1, 0, 4, // lookup list: one lookup, at 4
        0, 1, 0, 0, 0, 1, 0, 8, // type 1, flags 0, one subtable at 8 (the lookup list)
    ];
    t.push((tag(b"GPOS"), gpos.clone()));
    let font = sfnt(flavor, t);
    let (_, out) = tables(&sanitize(&font).unwrap());
    let kept = out.iter().find(|(g, _)| *g == tag(b"GPOS")).map(|(_, d)| d);
    assert_eq!(kept, Some(&gpos));
    let engine = TextEngine::new().unwrap();
    let family = engine.add_font_data(&font).unwrap();
    assert_ahem(&engine, &family);
}

/// `SNOWLARK_FONT_FILE=<file> ... --ignored font_file`: what the sanitizer makes of a font file.
#[test]
#[ignore]
fn font_file() {
    let path = std::env::var("SNOWLARK_FONT_FILE").expect("SNOWLARK_FONT_FILE");
    let data = std::fs::read(path).unwrap();
    match sanitize(&data) {
        Ok(font) => {
            println!("accepted: {} bytes in, {} out", data.len(), font.len());
            TextEngine::new().unwrap().add_font_data(&data).unwrap();
        }
        Err(e) => panic!("refused: {e}"),
    }
}
