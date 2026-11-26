// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shaping per word: a [`WordCache`] that [`TextEngine::shape_cached`] uses so that text shaped again
//! (a paragraph after an edit, the same words elsewhere in a document) costs a hash lookup per word
//! instead of DirectWrite's shaping.
//!
//! **Where text is split.** Only runs whose script DirectWrite's analysis reports as Latin, Greek,
//! Cyrillic or Common, shaped left to right, are split; Arabic, Indic and other scripts whose shaping
//! crosses word boundaries (joining, reordering, conjuncts) are shaped as whole runs, as before. A run
//! is split after a space (U+0020) and before the next character `c` only where shaping cannot cross
//! that boundary: `c` is a base character (not a combining mark, joiner or other format character)
//! and the font shapes `" " + c` exactly as `" "` followed by `c` (no kerning between the space and
//! `c`, no substitution of either). That is probed once per font, size, script, locale, features and
//! character, so fonts that kern spaces (Arial and Times New Roman kern `" A"`, `" T"`, `" Y"`) keep
//! those pairs in one segment. A segment is a word with its trailing spaces.
//!
//! **What is cached.** Per segment: the glyphs, advances, offsets and cluster map, keyed by the
//! segment's text and its context (font face, size, script, locale, features). Line-break analysis
//! stays per paragraph (break opportunities between words depend on both words). Missed segments of a
//! run are shaped together in one DirectWrite call (their boundaries are safe ones, so the result
//! splits exactly) and stored.
//!
//! **Simple text.** Text of one of those scripts (or with no letter, all Common, such as a number)
//! in a left-to-right paragraph whose characters the
//! list's first font all has needs neither DirectWrite's script and bidi analysis nor font fallback:
//! it is one run, at level 0, in the first font, whose script analysis is known
//! ([`TextEngine::simple_run`]).
//!
//! **Memory.** Two generations of at most [`WordCache::GENERATION_BYTES`] each; a hit in the older one
//! moves the entry to the newer, and when the newer is full the older is dropped.
//!
//! **Runs.** Above the words, a [`ShapeCache`] keeps whole shaped runs (a paragraph's text with its
//! font list, size, direction, locale and features), so text shaped before is not even split again.
//! Both make a [`TextCache`].
//!
//! **Sharing.** A layout tree has its own [`TextCache`] by default. [`TextEngine::shared_text_cache`]
//! hands documents of one origin on one thread the same cache (as a site's documents share a
//! renderer process's shaping caches in Chromium), so a reloaded or navigated same-origin document
//! finds its text shaped. Caches are never shared across origins or threads (tabs): whether a word
//! is cached is observable through timing, and that must not reveal one site's text to another.

use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use rustc_hash::{FxBuildHasher, FxHashMap};
use windows::core::{Interface, Result};
use windows::Win32::Graphics::DirectWrite::*;
use windows_core::ComObject;

pub use snowlark_graphics::text_cache::*;

use crate::display_list::{FontId, GlyphOffset};
use crate::text::{Analysis, FontListId, GlyphScratch, RunShape, RunSpec, ShapedText, TextEngine};

/// One shaping context: everything but the text that a segment's shaping depends on.
struct Context {
    font: FontId,
    size: u32,
    script: u16,
    shapes: i32,
    /// NUL-terminated.
    locale: Box<[u16]>,
    features: Box<[(u32, u32)]>,
    /// Runs in this context are split into segments.
    words: bool,
    /// Per code unit after a space: whether a segment may start there (below [`COVERAGE_END`]: 0
    /// not probed yet, 1 yes, 2 no).
    starts: Box<[u8]>,
    starts_other: FxHashMap<u16, bool>,
}

impl Context {
    fn start_state(&self, u: u16) -> Option<bool> {
        if u < COVERAGE_END {
            match self.starts[u as usize] {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            }
        } else {
            self.starts_other.get(&u).copied()
        }
    }

    fn set_start_state(&mut self, u: u16, safe: bool) {
        if u < COVERAGE_END {
            self.starts[u as usize] = if safe { 1 } else { 2 };
        } else {
            self.starts_other.insert(u, safe);
        }
    }
}

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Entry {
    ctx: u32,
    /// Start in `Gen::units` of the text, followed by the cluster map (`len` units each).
    units: u32,
    len: u32,
    /// Start in `Gen::glyphs`, `Gen::advances` and `Gen::offsets`.
    glyphs: u32,
    count: u32,
    /// The next entry with the same hash.
    next: u32,
}

#[derive(Default)]
struct Gen {
    map: FxHashMap<u64, u32>,
    entries: Vec<Entry>,
    units: Vec<u16>,
    glyphs: Vec<u16>,
    advances: Vec<f32>,
    offsets: Vec<GlyphOffset>,
}

/// A cached segment's shaping: its cluster map (relative to its first glyph), glyphs, advances and
/// offsets.
type Shaping<'a> = (&'a [u16], &'a [u16], &'a [f32], &'a [GlyphOffset]);

impl Gen {
    fn bytes(&self) -> usize {
        self.units.len() * 2
            + self.glyphs.len() * 2
            + self.advances.len() * 4
            + self.offsets.len() * 8
            + self.entries.len() * std::mem::size_of::<Entry>()
            + self.map.len() * 16
    }

    fn find(&self, hash: u64, ctx: u32, text: &[u16]) -> Option<u32> {
        let mut i = *self.map.get(&hash)?;
        while i != NONE {
            let e = &self.entries[i as usize];
            if e.ctx == ctx
                && e.len as usize == text.len()
                && &self.units[e.units as usize..(e.units + e.len) as usize] == text
            {
                return Some(i);
            }
            i = e.next;
        }
        None
    }

    fn insert(&mut self, hash: u64, ctx: u32, text: &[u16], shaping: Shaping) -> u32 {
        let (map, glyphs, advances, offsets) = shaping;
        let i = self.entries.len() as u32;
        let units = self.units.len() as u32;
        self.units.extend_from_slice(text);
        self.units.extend_from_slice(map);
        let g = self.glyphs.len() as u32;
        self.glyphs.extend_from_slice(glyphs);
        self.advances.extend_from_slice(advances);
        self.offsets.extend_from_slice(offsets);
        let next = self.map.insert(hash, i).unwrap_or(NONE);
        self.entries.push(Entry {
            ctx,
            units,
            len: text.len() as u32,
            glyphs: g,
            count: glyphs.len() as u32,
            next,
        });
        i
    }

    fn entry_data(&self, i: u32) -> Shaping<'_> {
        let e = &self.entries[i as usize];
        let map = &self.units[(e.units + e.len) as usize..(e.units + 2 * e.len) as usize];
        let g = e.glyphs as usize..(e.glyphs + e.count) as usize;
        (
            map,
            &self.glyphs[g.clone()],
            &self.advances[g.clone()],
            &self.offsets[g],
        )
    }
}

/// Where a segment's shaping comes from when a run is assembled.
#[derive(Clone, Copy)]
enum Piece {
    /// An entry of the current generation.
    Cached(u32),
    /// Code units `a..b` of the missed text shaped for this run.
    Shaped(u32, u32),
}

/// Shaped words by context and text: see the module documentation.
pub struct WordCache {
    contexts: Vec<Context>,
    context_index: FxHashMap<u64, Vec<u32>>,
    current: Gen,
    previous: Gen,
    /// Per font: whether it has each character below [`COVERAGE_END`] (0 unknown, 1 yes, 2 no).
    coverage: FxHashMap<u32, Box<[u8]>>,
    stats: WordCacheStats,
    // Scratch, kept for its capacity.
    segments: Vec<(u32, u32)>,
    pieces: Vec<Piece>,
    missed: Vec<u16>,
    shaped: ShapedText,
    shaped_map: Vec<u32>,
    map16: Vec<u16>,
}

impl Default for WordCache {
    fn default() -> WordCache {
        WordCache::new()
    }
}

/// Segments longer than this many code units are shaped every time (they are rarely repeated).
const MAX_SEGMENT: usize = 64;
/// The characters [`WordCache`] tracks font coverage for (the simple-text ranges).
const COVERAGE_END: u16 = 0x20C1;

fn hash_context(run: &RunShape) -> u64 {
    let mut h = FxBuildHasher.build_hasher();
    run.font.0.hash(&mut h);
    run.size.to_bits().hash(&mut h);
    run.script.script.hash(&mut h);
    run.script.shapes.0.hash(&mut h);
    run.locale.hash(&mut h);
    for f in run.features {
        f.nameTag.0.hash(&mut h);
        f.parameter.hash(&mut h);
    }
    h.finish()
}

fn hash_segment(ctx: u32, text: &[u16]) -> u64 {
    let mut h = FxBuildHasher.build_hasher();
    ctx.hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

/// Whether a segment may start with this code unit: a base character, not a space, combining mark,
/// joiner, variation selector, control or other format character, or half of a surrogate pair.
fn plain_start(u: u16) -> bool {
    !matches!(u,
        0..=0x20 | 0x7F..=0x9F | 0xAD | 0x300..=0x36F | 0x483..=0x489 | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F
        | 0x20D0..=0x20FF | 0xD800..=0xDFFF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0xFEFF)
}

/// The script class of a code unit for simple text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Letter(usize),
    /// Script Common (spaces, digits, punctuation, symbols).
    Common,
    /// Script Inherited (combining diacritical marks).
    Inherited,
    /// Anything else: the text is not simple.
    Other,
}

const LATIN: usize = 0;
const GREEK: usize = 1;
const CYRILLIC: usize = 2;
/// Text with no letter: all of it script Common (numbers, punctuation, symbols).
const COMMON: usize = 3;

/// Unicode's Script property for the simple-text ranges (Scripts.txt), conservatively: code points
/// outside these ranges, unassigned ones and format characters are `Other`.
fn classify(u: u16) -> Class {
    match u {
        0x41..=0x5A | 0x61..=0x7A => Class::Letter(LATIN),
        0x20..=0x7E => Class::Common,
        0xAA | 0xBA => Class::Letter(LATIN),
        0xAD => Class::Other,
        0xA0..=0xBF | 0xD7 | 0xF7 => Class::Common,
        0xC0..=0x24F => Class::Letter(LATIN),
        0x300..=0x36F => Class::Inherited,
        0x386 | 0x388..=0x38A | 0x38C | 0x38E..=0x3A1 | 0x3A3..=0x3E1 | 0x3F0..=0x3FF => {
            Class::Letter(GREEK)
        }
        0x400..=0x482 | 0x48A..=0x52F => Class::Letter(CYRILLIC),
        0x1E00..=0x1EFF => Class::Letter(LATIN),
        0x2010..=0x2027 | 0x2030..=0x205E | 0x20A0..=0x20C0 => Class::Common,
        _ => Class::Other,
    }
}

impl WordCache {
    /// The most memory one generation holds (an estimate, in bytes).
    pub const GENERATION_BYTES: usize = 4 << 20;

    pub fn new() -> WordCache {
        WordCache {
            contexts: Vec::new(),
            context_index: FxHashMap::default(),
            current: Gen::default(),
            previous: Gen::default(),
            coverage: FxHashMap::default(),
            stats: WordCacheStats::default(),
            segments: Vec::new(),
            pieces: Vec::new(),
            missed: Vec::new(),
            shaped: ShapedText::default(),
            shaped_map: Vec::new(),
            map16: Vec::new(),
        }
    }

    pub fn stats(&self) -> WordCacheStats {
        self.stats
    }

    /// Segments held (both generations).
    pub fn len(&self) -> usize {
        self.current.entries.len() + self.previous.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The estimated bytes held (both generations).
    pub fn bytes(&self) -> usize {
        self.current.bytes() + self.previous.bytes()
    }

    fn context(&mut self, engine: &TextEngine, run: &RunShape) -> u32 {
        let hash = hash_context(run);
        if let Some(ids) = self.context_index.get(&hash) {
            for &id in ids {
                let c = &self.contexts[id as usize];
                if c.font == run.font
                    && c.size == run.size.to_bits()
                    && c.script == run.script.script
                    && c.shapes == run.script.shapes.0
                    && *c.locale == *run.locale
                    && c.features.len() == run.features.len()
                    && c.features
                        .iter()
                        .zip(run.features)
                        .all(|(a, b)| a.0 == b.nameTag.0 && a.1 == b.parameter)
                {
                    return id;
                }
            }
        }
        let id = self.contexts.len() as u32;
        self.contexts.push(Context {
            font: run.font,
            size: run.size.to_bits(),
            script: run.script.script,
            shapes: run.script.shapes.0,
            locale: run.locale.into(),
            features: run
                .features
                .iter()
                .map(|f| (f.nameTag.0, f.parameter))
                .collect(),
            words: run.script.shapes == DWRITE_SCRIPT_SHAPES_DEFAULT
                && engine.splits_words(run.script),
            starts: vec![0u8; COVERAGE_END as usize].into_boxed_slice(),
            starts_other: FxHashMap::default(),
        });
        self.context_index.entry(hash).or_default().push(id);
        id
    }

    /// Whether the font has every character of `units` (each below [`COVERAGE_END`]).
    fn covers_all(&mut self, engine: &TextEngine, font: FontId, units: &[u16]) -> bool {
        let table = self
            .coverage
            .entry(font.0)
            .or_insert_with(|| vec![0u8; COVERAGE_END as usize].into_boxed_slice());
        for &u in units {
            let has = match table[u as usize] {
                1 => true,
                2 => false,
                _ => {
                    let has = engine.has_character(font, u32::from(u));
                    table[u as usize] = if has { 1 } else { 2 };
                    has
                }
            };
            if !has {
                return false;
            }
        }
        true
    }

    /// Whether a segment may start with `u` after a space in this context (see the module
    /// documentation), probing the font the first time.
    fn safe_start(
        &mut self,
        engine: &TextEngine,
        ctx: u32,
        run: &RunShape,
        u: u16,
        scratch: &mut GlyphScratch,
    ) -> Result<bool> {
        if !plain_start(u) {
            return Ok(false);
        }
        if let Some(safe) = self.contexts[ctx as usize].start_state(u) {
            return Ok(safe);
        }
        self.stats.probes += 1;
        let mut shape = |units: &[u16]| -> Result<(ShapedText, Vec<u32>)> {
            let mut out = ShapedText::default();
            let mut map = Vec::new();
            engine.shape_units(&RunShape { units, ..*run }, scratch, &mut out, &mut map)?;
            Ok((out, map))
        };
        let (pair, pair_map) = shape(&[0x20, u])?;
        let (space, space_map) = shape(&[0x20])?;
        let (single, single_map) = shape(&[u])?;
        let k = space.glyphs.len();
        let joined_map: Vec<u32> = space_map
            .iter()
            .copied()
            .chain(single_map.iter().map(|g| g + k as u32))
            .collect();
        let safe = pair.glyphs.len() == k + single.glyphs.len()
            && pair.glyphs[..k] == space.glyphs[..]
            && pair.glyphs[k..] == single.glyphs[..]
            && pair.advances[..k] == space.advances[..]
            && pair.advances[k..] == single.advances[..]
            && pair.offsets[..k] == space.offsets[..]
            && pair.offsets[k..] == single.offsets[..]
            && pair_map == joined_map;
        self.contexts[ctx as usize].set_start_state(u, safe);
        Ok(safe)
    }

    /// Room for about `bytes` more in the current generation, retiring it when full.
    fn reserve(&mut self, bytes: usize) {
        if self.current.bytes() + bytes > Self::GENERATION_BYTES {
            self.previous = std::mem::take(&mut self.current);
        }
    }

    /// Shapes one run per segment (see the module documentation): appends its glyphs, advances and
    /// offsets to `out` and its cluster map to `map`, as [`TextEngine::shape_units`] does. Returns
    /// `false`, having appended nothing, when the run is not one to split.
    pub(crate) fn shape_run(
        &mut self,
        engine: &TextEngine,
        run: &RunShape,
        scratch: &mut GlyphScratch,
        out: &mut ShapedText,
        map: &mut Vec<u32>,
    ) -> Result<bool> {
        let ctx = self.context(engine, run);
        if !self.contexts[ctx as usize].words {
            self.stats.runs_whole += 1;
            return Ok(false);
        }
        let units = run.units;
        let g_start = out.glyphs.len();
        let mut segments = std::mem::take(&mut self.segments);
        segments.clear();
        let mut start = 0usize;
        for i in 1..units.len() {
            if units[i - 1] == 0x20
                && units[i] != 0x20
                && self.safe_start(engine, ctx, run, units[i], scratch)?
            {
                segments.push((start as u32, i as u32));
                start = i;
            }
        }
        segments.push((start as u32, units.len() as u32));

        // Look the segments up; collect the missed ones' text.
        self.reserve(units.len() * 24 + 256);
        let mut pieces = std::mem::take(&mut self.pieces);
        pieces.clear();
        let mut missed = std::mem::take(&mut self.missed);
        missed.clear();
        for &(a, b) in &segments {
            let text = &units[a as usize..b as usize];
            let hash = hash_segment(ctx, text);
            if let Some(i) = self.current.find(hash, ctx, text) {
                self.stats.hits += 1;
                pieces.push(Piece::Cached(i));
            } else if let Some(old) = self.previous.find(hash, ctx, text) {
                self.stats.hits += 1;
                let i = self
                    .current
                    .insert(hash, ctx, text, self.previous.entry_data(old));
                pieces.push(Piece::Cached(i));
            } else {
                self.stats.misses += 1;
                let m0 = missed.len() as u32;
                missed.extend_from_slice(text);
                pieces.push(Piece::Shaped(m0, missed.len() as u32));
            }
        }

        // Shape the missed segments together: every boundary between them is a space followed by a
        // safe start (only a run's last segment may not end with a space, and it stays last).
        let mut shaped = std::mem::take(&mut self.shaped);
        let mut shaped_map = std::mem::take(&mut self.shaped_map);
        shaped.clear();
        shaped_map.clear();
        let mut ok = true;
        if !missed.is_empty() {
            self.stats.shaped_units += missed.len() as u64;
            engine.shape_units(
                &RunShape {
                    units: &missed,
                    ..*run
                },
                scratch,
                &mut shaped,
                &mut shaped_map,
            )?;
            // Each missed segment must start a cluster, and glyphs must follow the text's order.
            ok = shaped_map.windows(2).all(|w| w[0] <= w[1]);
            for p in &pieces {
                if let Piece::Shaped(a, _) = *p {
                    if a > 0 && shaped_map[a as usize] == shaped_map[a as usize - 1] {
                        ok = false;
                    }
                }
            }
            if ok {
                let count = shaped.glyphs.len() as u32;
                let glyph_end = |b: u32| -> u32 {
                    if (b as usize) < missed.len() {
                        shaped_map[b as usize]
                    } else {
                        count
                    }
                };
                let mut map16 = std::mem::take(&mut self.map16);
                for p in &mut pieces {
                    let Piece::Shaped(a, b) = *p else {
                        continue;
                    };
                    if (b - a) as usize > MAX_SEGMENT {
                        continue;
                    }
                    let (g0, g1) = (shaped_map[a as usize], glyph_end(b));
                    let text = &missed[a as usize..b as usize];
                    map16.clear();
                    map16.extend(
                        shaped_map[a as usize..b as usize]
                            .iter()
                            .map(|g| (g - g0) as u16),
                    );
                    let g = g0 as usize..g1 as usize;
                    let hash = hash_segment(ctx, text);
                    let i = self.current.insert(
                        hash,
                        ctx,
                        text,
                        (
                            &map16,
                            &shaped.glyphs[g.clone()],
                            &shaped.advances[g.clone()],
                            &shaped.offsets[g],
                        ),
                    );
                    *p = Piece::Cached(i);
                }
                self.map16 = map16;
            }
        }

        if ok {
            // Assemble the run in order.
            let count = shaped.glyphs.len() as u32;
            for p in &pieces {
                let run_glyphs = (out.glyphs.len() - g_start) as u32;
                match *p {
                    Piece::Cached(i) => {
                        let (m, g, a, o) = self.current.entry_data(i);
                        map.extend(m.iter().map(|&x| u32::from(x) + run_glyphs));
                        out.glyphs.extend_from_slice(g);
                        out.advances.extend_from_slice(a);
                        out.offsets.extend_from_slice(o);
                    }
                    Piece::Shaped(a, b) => {
                        let g0 = shaped_map[a as usize];
                        let g1 = if (b as usize) < missed.len() {
                            shaped_map[b as usize]
                        } else {
                            count
                        };
                        map.extend(
                            shaped_map[a as usize..b as usize]
                                .iter()
                                .map(|&x| x - g0 + run_glyphs),
                        );
                        out.glyphs
                            .extend_from_slice(&shaped.glyphs[g0 as usize..g1 as usize]);
                        out.advances
                            .extend_from_slice(&shaped.advances[g0 as usize..g1 as usize]);
                        out.offsets
                            .extend_from_slice(&shaped.offsets[g0 as usize..g1 as usize]);
                    }
                }
            }
            self.stats.runs_by_word += 1;
        } else {
            self.stats.runs_whole += 1;
        }
        self.segments = segments;
        self.pieces = pieces;
        self.missed = missed;
        self.shaped = shaped;
        self.shaped_map = shaped_map;
        Ok(ok)
    }
}

impl TextEngine {
    /// Whether runs of this script are split into words: Latin, Greek, Cyrillic and Common (ISO
    /// 15924 numbers 215, 200, 220, 998), and the analysis DirectWrite gives text of Common
    /// characters only (999: digits, punctuation and symbols with no letter to take the script of).
    pub(crate) fn splits_words(&self, script: DWRITE_SCRIPT_ANALYSIS) -> bool {
        let Ok(analyzer) = self.analyzer().cast::<IDWriteTextAnalyzer1>() else {
            return false;
        };
        let mut props = DWRITE_SCRIPT_PROPERTIES::default();
        // SAFETY: `props` is a local the call writes.
        if unsafe { analyzer.GetScriptProperties(script, &mut props) }.is_err() {
            return false;
        }
        matches!(props.isoScriptNumber, 215 | 200 | 220 | 998 | 999)
    }

    /// DirectWrite's script analysis of a Latin, a Greek and a Cyrillic letter, and of a text with
    /// no letter (script Common).
    fn simple_scripts(&self) -> Option<[DWRITE_SCRIPT_ANALYSIS; 4]> {
        *self.simple_scripts.get_or_init(|| {
            let analyze = |text: &str| {
                let text: Vec<u16> = text.encode_utf16().collect();
                let len = text.len() as u32;
                let analysis = ComObject::new(Analysis::new(text, "en-us", false));
                let source: IDWriteTextAnalysisSource = analysis.to_interface();
                let sink: IDWriteTextAnalysisSink = analysis.to_interface();
                // SAFETY: the source and sink are our COM object, alive for the call.
                unsafe { self.analyzer().AnalyzeScript(&source, 0, len, &sink) }.ok()?;
                let scripts = analysis.scripts.borrow().clone();
                Some(scripts)
            };
            let letters = analyze("a\u{3b1}\u{430}")?;
            let at = |pos: u32| {
                letters
                    .iter()
                    .find(|s| s.0 <= pos && pos < s.0 + s.1)
                    .map(|s| s.2)
            };
            // (a text of Common characters is one run of one script analysis)
            let common = match analyze("1 .")?.as_slice() {
                [(0, 3, script)] => *script,
                _ => return None,
            };
            Some([at(0)?, at(1)?, at(2)?, common])
        })
    }

    /// The single run of simple text (see the module documentation), or `None` when `units` is not
    /// simple text the list's first font covers.
    pub(crate) fn simple_run(
        &self,
        cache: &mut WordCache,
        units: &[u16],
        list: FontListId,
    ) -> Option<RunSpec> {
        let mut script = None;
        for (i, &u) in units.iter().enumerate() {
            match classify(u) {
                Class::Other => return None,
                Class::Letter(s) => {
                    if script.is_some_and(|t| t != s) {
                        return None;
                    }
                    script = Some(s);
                }
                Class::Inherited if i == 0 => return None,
                _ => {}
            }
        }
        let script = script.unwrap_or(COMMON);
        let font = self.primary_font(list);
        if !cache.covers_all(self, font, units) {
            return None;
        }
        let analysis = self.simple_scripts()?[script];
        cache.stats.simple_texts += 1;
        Some(RunSpec {
            start: 0,
            end: units.len(),
            font,
            level: 0,
            script: analysis,
            upright: false,
        })
    }

    /// The text cache shared by the documents of `partition` (an origin) on the calling thread; see
    /// the module documentation. An empty partition (an opaque origin) gets a cache of its own.
    pub fn shared_text_cache(&self, partition: &str) -> Arc<Mutex<TextCache>> {
        /// Partitions kept, least recently used dropped first.
        const KEEP: usize = 8;
        if partition.is_empty() || partition == "null" {
            return Arc::default();
        }
        let thread = std::thread::current().id();
        let mut shared = self.shared_text.lock().unwrap();
        if let Some(i) = shared
            .iter()
            .position(|s| s.0 == thread && s.1 == partition)
        {
            let entry = shared.remove(i);
            let cache = entry.2.clone();
            shared.push(entry);
            return cache;
        }
        if shared.len() >= KEEP {
            shared.remove(0);
        }
        let cache: Arc<Mutex<TextCache>> = Arc::default();
        shared.push((thread, partition.to_string(), cache.clone()));
        cache
    }
}

/// The text caches [`TextEngine::shared_text_cache`] hands out: (thread, partition, cache).
pub(crate) type SharedTextCaches = Mutex<Vec<(ThreadId, String, Arc<Mutex<TextCache>>)>>;

/// What a layout tree shapes through: whole runs, then words.
#[derive(Default)]
pub struct TextCache {
    pub runs: ShapeCache,
    pub words: WordCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{Direction, FontFeature, FontQuery, FontStretch, FontStyle, ShapeOptions};

    /// Text covering what splitting must not change: kerning around spaces, ligatures, combining
    /// marks, no-break and soft hyphens, punctuation, digits, Greek, Cyrillic, scripts shaped whole
    /// (Arabic, Devanagari, Hebrew, CJK) and emoji, runs of spaces.
    pub(crate) const FIXTURES: &[&str] = &[
        "Hello wide world, says the quick brown fox.",
        "AVATAR Tom Yes \u{201C}You\u{201D} T. Y. A. 'quoted' \u{2018}single\u{2019} (paren) [brack]",
        "office affine fluffy  ff fi fl  waffle   triple   spaces ",
        " leading space and trailing  ",
        "na\u{EF}ve caf\u{E9} cafe\u{301} r\u{E9}sum\u{E9} \u{C5}ngstr\u{F6}m \u{153}uvre \u{1E9E}",
        "non\u{A0}breaking 100\u{A0}km soft\u{AD}hyphen co-operate \u{2014} dash \u{2013} 1\u{2009}000",
        "12,345.67 $99 \u{20AC}5 50% #1 @home a/b x*y=z 3\u{D7}4 1\u{2026} \u{2022} bullet",
        "\u{39A}\u{3B1}\u{3BB}\u{3B7}\u{3BC}\u{3AD}\u{3C1}\u{3B1} \u{3BA}\u{3CC}\u{3C3}\u{3BC}\u{3B5} \u{3AC}\u{3BB}\u{3C6}\u{3B1}",
        "\u{41F}\u{440}\u{438}\u{432}\u{435}\u{442} \u{43C}\u{438}\u{440} \u{42F}\u{43D}\u{432}\u{430}\u{440}\u{44C} \u{451}\u{436}",
        "Latin and \u{3B5}\u{3BB}\u{3BB}\u{3B7}\u{3BD}\u{3B9}\u{3BA}\u{3AC} and \u{440}\u{443}\u{441}\u{441}\u{43A}\u{438}\u{439} mixed",
        "\u{645}\u{631}\u{62D}\u{628}\u{627} \u{628}\u{627}\u{644}\u{639}\u{627}\u{644}\u{645} \u{627}\u{644}\u{639}\u{631}\u{628}\u{64A}\u{629}",
        "\u{928}\u{92E}\u{938}\u{94D}\u{924}\u{947} \u{926}\u{941}\u{928}\u{93F}\u{92F}\u{93E} \u{915}\u{94D}\u{937}\u{924}\u{94D}\u{930}\u{93F}\u{92F}",
        "\u{5E9}\u{5DC}\u{5D5}\u{5DD} \u{5E2}\u{5D5}\u{5DC}\u{5DD} and English",
        "\u{65E5}\u{672C}\u{8A9E}\u{306E} \u{6587}\u{7AE0} mixed with Latin \u{1F600} emoji \u{1F44D}\u{1F3FD}",
        "Mixed abc \u{645}\u{631}\u{62D}\u{628}\u{627} def 123 \u{928}\u{92E}\u{938}\u{94D}\u{924}\u{947} end.",
        "WAVE LT Ty Yo Te P. F. V, W. Wa We Yo Av Aw Ay",
        // (no letter: script Common only, as chart axes label)
        "12,345.67 $99 50% 3\u{D7}4 1\u{2026} -0.25",
        "1/14",
        "  ",
    ];

    pub(crate) const FAMILIES: &[&str] = &[
        "Segoe UI",
        "Arial",
        "Times New Roman",
        "Verdana",
        "Calibri",
        "Georgia",
        "Consolas",
    ];

    fn list(e: &TextEngine, family: &str, weight: u16, style: FontStyle) -> FontListId {
        e.font_list(&FontQuery {
            families: &[family],
            weight,
            style,
            stretch: FontStretch::NORMAL,
        })
        .unwrap()
    }

    pub(crate) fn assert_same(a: &ShapedText, b: &ShapedText, what: &str) {
        assert_eq!(a.runs, b.runs, "{what}: runs");
        assert_eq!(a.glyphs, b.glyphs, "{what}: glyphs");
        assert_eq!(a.advances, b.advances, "{what}: advances");
        assert_eq!(a.offsets, b.offsets, "{what}: offsets");
        assert_eq!(a.clusters, b.clusters, "{what}: clusters");
    }

    #[test]
    fn word_shaping_matches_whole_run_shaping() {
        let e = TextEngine::new().unwrap();
        let liga_off = [FontFeature::new(b"liga", 0)];
        let kern_off = [FontFeature::new(b"kern", 0)];
        let features: [&[FontFeature]; 3] = [&[], &liga_off, &kern_off];
        let mut cache = WordCache::new();
        for family in FAMILIES {
            for (weight, style) in [(400, FontStyle::Normal), (700, FontStyle::Italic)] {
                let l = list(&e, family, weight, style);
                for size in [13.0, 16.0, 21.5] {
                    for f in features {
                        for dir in [Direction::Ltr, Direction::Rtl] {
                            let options = ShapeOptions {
                                size,
                                features: f,
                                direction: dir,
                                ..Default::default()
                            };
                            for text in FIXTURES {
                                let what =
                                    format!("{family} {weight} {size} {f:?} {dir:?} {text:?}");
                                let mut whole = ShapedText::default();
                                e.shape(text, l, &options, &mut whole).unwrap();
                                // Twice: shaped and stored, then from the cache.
                                for _ in 0..2 {
                                    let mut words = ShapedText::default();
                                    e.shape_cached(text, l, &options, Some(&mut cache), &mut words)
                                        .unwrap();
                                    assert_same(&words, &whole, &what);
                                }
                            }
                        }
                    }
                }
            }
        }
        let s = cache.stats();
        assert!(s.hits > s.misses, "{s:?}");
        assert!(s.runs_by_word > 0 && s.runs_whole > 0, "{s:?}");
        assert!(s.simple_texts > 0, "{s:?}");
    }

    /// A number (script Common, no letter) is simple text: one run in the first font, without
    /// DirectWrite's script analysis or font fallback, its words cached.
    #[test]
    fn text_with_no_letter_is_simple() {
        let e = TextEngine::new().unwrap();
        let l = list(&e, "Arial", 400, FontStyle::Normal);
        let options = ShapeOptions {
            size: 12.0,
            ..Default::default()
        };
        let mut cache = WordCache::new();
        let mut out = ShapedText::default();
        for _ in 0..2 {
            e.shape_cached("-12.5 3/14", l, &options, Some(&mut cache), &mut out)
                .unwrap();
        }
        let s = cache.stats();
        assert_eq!(s.simple_texts, 2, "{s:?}");
        assert!(s.hits > 0, "{s:?}");
    }

    #[test]
    fn fonts_that_kern_spaces_keep_those_pairs_in_one_segment() {
        let e = TextEngine::new().unwrap();
        for family in ["Arial", "Times New Roman"] {
            let l = list(&e, family, 400, FontStyle::Normal);
            let options = ShapeOptions::default();
            let mut cache = WordCache::new();
            let mut out = ShapedText::default();
            e.shape_cached("x Ab Tc Yd ef", l, &options, Some(&mut cache), &mut out)
                .unwrap();
            let ctx = &cache.contexts[0];
            // These fonts kern the space before A, T and Y; not before e.
            assert_eq!(ctx.start_state(0x41), Some(false), "{family}");
            assert_eq!(ctx.start_state(0x65), Some(true), "{family}");
            // So "x Ab Tc Yd " is one segment and "ef" another.
            assert_eq!(cache.stats().misses, 2, "{family}");
        }
    }

    #[test]
    fn an_edit_reshapes_only_the_changed_word() {
        let e = TextEngine::new().unwrap();
        let l = list(&e, "Verdana", 400, FontStyle::Normal);
        let options = ShapeOptions::default();
        let mut cache = WordCache::new();
        let mut out = ShapedText::default();
        let before = "The quick brown fox jumps over the lazy dog and keeps running";
        e.shape_cached(before, l, &options, Some(&mut cache), &mut out)
            .unwrap();
        let s0 = cache.stats();
        // Twelve segments ("The " and "the " differ).
        assert_eq!((s0.misses, s0.hits), (12, 0), "{s0:?}");
        assert_eq!(s0.simple_texts, 1);
        let after = "The quick brown fox leaps over the lazy dog and keeps running";
        e.shape_cached(after, l, &options, Some(&mut cache), &mut out)
            .unwrap();
        let s1 = cache.stats();
        assert_eq!((s1.misses - s0.misses, s1.hits - s0.hits), (1, 11));
        assert_eq!(s1.shaped_units - s0.shaped_units, "leaps ".len() as u64);
        let mut whole = ShapedText::default();
        e.shape(after, l, &options, &mut whole).unwrap();
        assert_same(&out, &whole, after);
    }

    #[test]
    fn complex_scripts_are_shaped_as_whole_runs() {
        let e = TextEngine::new().unwrap();
        let l = list(&e, "Segoe UI", 400, FontStyle::Normal);
        let mut cache = WordCache::new();
        let mut out = ShapedText::default();
        // Arabic joins and Devanagari forms conjuncts: their runs are never split.
        for text in [
            "\u{645}\u{631}\u{62D}\u{628}\u{627} \u{628}\u{627}\u{644}\u{639}\u{627}\u{644}\u{645}",
            "\u{928}\u{92E}\u{938}\u{94D}\u{924}\u{947} \u{926}\u{941}\u{928}\u{93F}\u{92F}\u{93E}",
        ] {
            e.shape_cached(
                text,
                l,
                &ShapeOptions::default(),
                Some(&mut cache),
                &mut out,
            )
            .unwrap();
        }
        let s = cache.stats();
        assert_eq!(s.runs_by_word, 0, "{s:?}");
        assert!(s.runs_whole >= 2, "{s:?}");
        assert!(cache.is_empty());
    }

    #[test]
    fn generations_bound_the_memory_held() {
        let e = TextEngine::new().unwrap();
        let l = list(&e, "Segoe UI", 400, FontStyle::Normal);
        let mut cache = WordCache::new();
        let mut out = ShapedText::default();
        let mut text = String::new();
        for i in 0..200_000u32 {
            text.push_str(&format!("w{i:x} "));
            if text.len() > 4000 {
                e.shape_cached(
                    &text,
                    l,
                    &ShapeOptions::default(),
                    Some(&mut cache),
                    &mut out,
                )
                .unwrap();
                text.clear();
            }
        }
        assert!(
            cache.bytes() <= 2 * WordCache::GENERATION_BYTES + 200_000,
            "{}",
            cache.bytes()
        );
        assert!(cache.len() > 10_000);
    }

    #[test]
    fn shared_caches_are_per_origin_and_thread() {
        let e = TextEngine::new().unwrap();
        let a = e.shared_text_cache("https://a.example");
        let a2 = e.shared_text_cache("https://a.example");
        let b = e.shared_text_cache("https://b.example");
        assert!(Arc::ptr_eq(&a, &a2));
        assert!(!Arc::ptr_eq(&a, &b));
        // Opaque origins share with nobody.
        assert!(!Arc::ptr_eq(
            &e.shared_text_cache("null"),
            &e.shared_text_cache("null")
        ));
        // Another thread (another tab) gets its own.
        let other = std::thread::scope(|s| {
            s.spawn(|| e.shared_text_cache("https://a.example"))
                .join()
                .unwrap()
        });
        assert!(!Arc::ptr_eq(&a, &other));
    }
}
