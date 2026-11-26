// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The text types every text engine shares: what a computed font asks for, how to shape, and the
//! shaped result (runs, glyphs, clusters, break opportunities) that layout measures and paints. The
//! engine that fills them in (`snowlark-paint`'s `TextEngine`) is the platform's.

use std::ops::Range;

use crate::display_list::{DisplayListBuilder, FontId, GlyphOffset};
use crate::geom::{Color, Point};

/// `font-style`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
}

/// `font-stretch` as OpenType's nine `usWidthClass` steps: 1 ultra-condensed … 5 normal … 9 ultra-expanded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontStretch(pub u8);

impl FontStretch {
    pub const NORMAL: FontStretch = FontStretch(5);
    pub const CONDENSED: FontStretch = FontStretch(3);
    pub const EXPANDED: FontStretch = FontStretch(7);
}

impl Default for FontStretch {
    fn default() -> FontStretch {
        FontStretch::NORMAL
    }
}

/// What a computed font asks for: the `font-family` list in order (names, generic families such as
/// `sans-serif`, or names returned by `TextEngine::add_font_data` (`snowlark-paint`)) and the face descriptors.
#[derive(Clone, Copy, Debug)]
pub struct FontQuery<'a> {
    pub families: &'a [&'a str],
    /// 1..=1000 (400 normal, 700 bold).
    pub weight: u16,
    pub style: FontStyle,
    pub stretch: FontStretch,
}

/// A resolved font list (the families that exist, in order, with the descriptors), from
/// `TextEngine::font_list` (`snowlark-paint`). Cheap to copy; resolve once per computed font and keep it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontListId(pub u32);

/// An OpenType feature setting (`font-feature-settings`, `font-kerning`, `font-variant-ligatures`):
/// e.g. `FontFeature::new(b"liga", 0)` turns standard ligatures off, `(b"kern", 0)` kerning off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontFeature {
    pub tag: [u8; 4],
    pub value: u32,
}

impl FontFeature {
    pub const fn new(tag: &[u8; 4], value: u32) -> FontFeature {
        FontFeature { tag: *tag, value }
    }
}

/// The paragraph's base direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Ltr,
    Rtl,
}

/// How to shape: em size in CSS px, BCP 47 language (affects glyph choice, e.g. CJK forms), base
/// direction, and feature settings applied to the whole text.
#[derive(Clone, Copy, Debug)]
pub struct ShapeOptions<'a> {
    pub size: f32,
    pub locale: &'a str,
    pub direction: Direction,
    pub features: &'a [FontFeature],
    pub vertical: VerticalText,
    /// The bidi level of each UTF-16 unit, when the text is part of a paragraph analyzed as a whole.
    /// `None`: the text is analyzed on its own, as a paragraph of `direction`.
    pub levels: Option<&'a [u8]>,
}

/// How text is set in a vertical line (CSS Writing Modes 4 §5.1). Rotated text is shaped as
/// horizontal text and drawn turned; upright text is shaped for vertical layout (vertical glyph
/// forms and advances) in runs of its own ([`ShapedRun::upright`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VerticalText {
    /// Horizontal text, or text set sideways.
    #[default]
    None,
    /// `text-orientation: mixed`: per character, by Unicode's Vertical_Orientation (UAX #50).
    Mixed,
    /// `text-orientation: upright`: every character upright, all strong left-to-right.
    Upright,
}

/// A character's orientation in a vertical line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharOrientation {
    Rotated,
    Upright,
    /// Vertical_Orientation `Tr`: a vertical alternate when the font has them, else rotated.
    UprightIfVariant,
}

/// Per UTF-16 unit of `text`, its orientation; `None` when the whole text is rotated.
pub fn orientations(text: &str, vertical: VerticalText) -> Option<Vec<CharOrientation>> {
    if vertical == VerticalText::None {
        return None;
    }
    if vertical == VerticalText::Mixed && text.is_ascii() {
        return None;
    }
    let mut out = Vec::with_capacity(text.len());
    let any_vertical_script = text.chars().any(is_vertical_script);
    let mut any = false;
    let mut last = CharOrientation::Rotated;
    for c in text.chars() {
        use unicode_vo::Orientation as O;
        let o = if is_vertical_script(c) {
            // Scripts written vertically have no upright form of their rotated glyphs: the font's
            // horizontal glyphs are their vertical ones turned (§5.1.1).
            CharOrientation::Rotated
        } else if c.is_whitespace() && last == CharOrientation::Rotated && any_vertical_script {
            // (the spaces of such text are part of it)
            CharOrientation::Rotated
        } else if vertical == VerticalText::Upright {
            CharOrientation::Upright
        } else if is_mark(c) {
            // UAX #50 §4: a combining mark takes the orientation of its base.
            last
        } else {
            match unicode_vo::char_orientation(c) {
                O::Rotated => CharOrientation::Rotated,
                O::Upright | O::TransformedOrUpright => CharOrientation::Upright,
                O::TransformedOrRotated => CharOrientation::UprightIfVariant,
            }
        };
        last = o;
        any |= o != CharOrientation::Rotated;
        out.extend(std::iter::repeat_n(o, c.len_utf16()));
    }
    any.then_some(out)
}

/// Mongolian and Phags-pa, the scripts written top to bottom whose glyphs are stored turned.
pub fn is_vertical_script(c: char) -> bool {
    matches!(c as u32, 0x1800..=0x18AF | 0xA840..=0xA87F | 0x11660..=0x1167F)
}

/// The combining marks and format characters that stay with their base in a vertical line.
pub fn is_mark(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200D | 0x20D0..=0x20FF
        | 0x302A..=0x302F | 0x3099..=0x309A | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
        | 0xE0100..=0xE01EF | 0x1F3FB..=0x1F3FF)
}

impl Default for ShapeOptions<'_> {
    fn default() -> Self {
        ShapeOptions {
            size: 16.0,
            locale: "en-us",
            direction: Direction::Ltr,
            features: &[],
            vertical: VerticalText::None,
            levels: None,
        }
    }
}

/// Font metrics scaled to a size (CSS px). `ascent` and `descent` are both positive distances from
/// the baseline; `underline_offset` is positive below the baseline.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub x_height: f32,
    pub cap_height: f32,
    pub underline_offset: f32,
    pub underline_thickness: f32,
    pub strikeout_offset: f32,
    pub strikeout_thickness: f32,
    pub units_per_em: u16,
}

/// Break opportunity after a cluster (UAX #14).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BreakAfter {
    #[default]
    Never,
    Allowed,
    /// A hard break (a newline character).
    Mandatory,
}

/// The smallest unit that can't be split: one or more characters drawn by one or more glyphs
/// (a ligature, a base with combining marks, an emoji ZWJ sequence).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cluster {
    /// Byte range in the shaped `&str`.
    pub text: (u32, u32),
    /// Glyph range in [`ShapedText::glyphs`].
    pub glyphs: (u32, u32),
    pub advance: f32,
    pub break_after: BreakAfter,
    pub whitespace: bool,
    pub soft_hyphen: bool,
}

/// A run of glyphs that share font, script and bidi level, in logical order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedRun {
    pub font: FontId,
    pub size: f32,
    /// Byte range in the shaped `&str`.
    pub text: (u32, u32),
    pub glyphs: (u32, u32),
    pub clusters: (u32, u32),
    pub bidi_level: u8,
    /// The text engine's script id (opaque, equal for equal scripts).
    pub script: u16,
    pub advance: f32,
    /// Whether the face has color glyphs (emoji); the rasterizer draws those layer by layer.
    pub color: bool,
    /// Shaped for vertical layout, upright in a vertical line (see [`VerticalText`]).
    pub upright: bool,
}

/// The result of shaping: runs, then per-glyph arrays and per-cluster data. Reuse one across calls to
/// keep its vectors' capacity.
#[derive(Clone, Debug, Default)]
pub struct ShapedText {
    pub runs: Vec<ShapedRun>,
    pub glyphs: Vec<u16>,
    pub advances: Vec<f32>,
    pub offsets: Vec<GlyphOffset>,
    pub clusters: Vec<Cluster>,
}

impl ShapedText {
    pub fn clear(&mut self) {
        self.runs.clear();
        self.glyphs.clear();
        self.advances.clear();
        self.offsets.clear();
        self.clusters.clear();
    }

    /// The total advance.
    pub fn width(&self) -> f32 {
        self.runs.iter().map(|r| r.advance).sum()
    }

    /// The advance of a range of clusters (a line's worth).
    pub fn clusters_width(&self, clusters: Range<usize>) -> f32 {
        self.clusters[clusters].iter().map(|c| c.advance).sum()
    }

    /// Appends the glyph runs of `clusters` (a line) to a display list, in visual order (bidi
    /// reordering per UAX #9 rule L2 over the runs' levels), starting at `origin` (left end of the
    /// baseline). Returns the line's advance.
    pub fn paint(
        &self,
        clusters: Range<usize>,
        origin: Point,
        color: Color,
        out: &mut DisplayListBuilder,
    ) -> f32 {
        if clusters.is_empty() {
            return 0.0;
        }
        let (c0, c1) = (clusters.start as u32, clusters.end as u32);
        // The runs on the line, cut to it.
        let mut parts: Vec<(usize, u32, u32)> = Vec::new();
        for (i, run) in self.runs.iter().enumerate() {
            let a = run.clusters.0.max(c0);
            let b = run.clusters.1.min(c1);
            if a < b {
                parts.push((i, a, b));
            }
        }
        let levels: Vec<u8> = parts.iter().map(|p| self.runs[p.0].bidi_level).collect();
        let mut x = origin.x;
        for k in visual_order(&levels) {
            let (i, a, b) = parts[k];
            let run = &self.runs[i];
            let g0 = self.clusters[a as usize].glyphs.0 as usize;
            let g1 = self.clusters[b as usize - 1].glyphs.1 as usize;
            let width: f32 = self.advances[g0..g1].iter().sum();
            out.text(
                &self.glyphs[g0..g1],
                &self.advances[g0..g1],
                &self.offsets[g0..g1],
                Point::new(x, origin.y),
                run.font,
                run.size,
                color,
                run.bidi_level,
            );
            x += width;
        }
        x - origin.x
    }

    /// Splits the shaping of a concatenation of texts into the shaping of each part, `ends` being the
    /// byte offset in the shaped text where each part ends (ascending, the last one the text's
    /// length). Each part keeps the glyphs and advances shaped with its neighbours (kerning,
    /// contextual forms across the boundary), its clusters' text offsets made relative to its own
    /// start, and a wrap opportunity after its last cluster as if it ended the text (the caller
    /// decides breaks at the boundaries). `None` when a cluster spans a boundary (a ligature of
    /// characters on both sides) or a run is right-to-left: such text is shaped part by part.
    pub fn split(&self, ends: &[u32]) -> Option<Vec<ShapedText>> {
        if self.runs.iter().any(|r| r.bidi_level % 2 == 1) {
            return None;
        }
        let mut parts = Vec::with_capacity(ends.len());
        let mut c = 0usize;
        let mut start = 0u32;
        for &end in ends {
            let c0 = c;
            while c < self.clusters.len() && self.clusters[c].text.1 <= end {
                c += 1;
            }
            if c < self.clusters.len() && self.clusters[c].text.0 < end {
                return None;
            }
            let mut part = ShapedText::default();
            if c > c0 {
                let g0 = self.clusters[c0].glyphs.0;
                let g1 = self.clusters[c - 1].glyphs.1;
                let (gs, ge) = (g0 as usize, g1 as usize);
                part.glyphs.extend_from_slice(&self.glyphs[gs..ge]);
                part.advances.extend_from_slice(&self.advances[gs..ge]);
                part.offsets.extend_from_slice(&self.offsets[gs..ge]);
                part.clusters
                    .extend(self.clusters[c0..c].iter().map(|k| Cluster {
                        text: (k.text.0 - start, k.text.1 - start),
                        glyphs: (k.glyphs.0 - g0, k.glyphs.1 - g0),
                        ..*k
                    }));
                if let Some(last) = part.clusters.last_mut() {
                    if last.break_after != BreakAfter::Mandatory {
                        last.break_after = BreakAfter::Allowed;
                    }
                }
                for run in &self.runs {
                    let a = (run.clusters.0 as usize).max(c0);
                    let b = (run.clusters.1 as usize).min(c);
                    if a >= b {
                        continue;
                    }
                    let (rg0, rg1) = (self.clusters[a].glyphs.0, self.clusters[b - 1].glyphs.1);
                    part.runs.push(ShapedRun {
                        text: (
                            self.clusters[a].text.0 - start,
                            self.clusters[b - 1].text.1 - start,
                        ),
                        glyphs: (rg0 - g0, rg1 - g0),
                        clusters: ((a - c0) as u32, (b - c0) as u32),
                        advance: self.advances[rg0 as usize..rg1 as usize].iter().sum(),
                        ..*run
                    });
                }
            }
            parts.push(part);
            start = end;
        }
        Some(parts)
    }
}

/// UAX #9 rule L2: the visual order of items with the given embedding levels (indices into `levels`).
pub fn visual_order(levels: &[u8]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..levels.len()).collect();
    let Some(&max) = levels.iter().max() else {
        return order;
    };
    let min_odd = levels.iter().copied().filter(|l| l % 2 == 1).min();
    let Some(min_odd) = min_odd else {
        return order;
    };
    for level in (min_odd..=max).rev() {
        let mut i = 0;
        while i < order.len() {
            if levels[order[i]] >= level {
                let start = i;
                while i < order.len() && levels[order[i]] >= level {
                    i += 1;
                }
                order[start..i].reverse();
            } else {
                i += 1;
            }
        }
    }
    order
}
