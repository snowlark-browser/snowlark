// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inline formatting contexts (CSS 2 §9.4.2, §10.8; css-text-3; css-inline-3).
//!
//! **Preparation** flattens a block container's inline-level descendants into [`Item`]s (text, inline
//! box start/end, atomic inlines, floats, out-of-flow boxes, forced breaks), applying `text-transform`
//! and white-space processing phase I (CSS Text 3 §4.1.1: collapsing across inline boundaries,
//! preserved segment breaks become forced breaks, preserved tabs become tab items), and shapes each text
//! item through the paint crate's [`TextEngine`]. Shaped text is reused when an item's text and font
//! are unchanged, so editing one text node reshapes one item; and that item is shaped through the
//! tree's [`TextCache`]: a run shaped before (in this document or another of its origin) is
//! taken whole, and otherwise only its words not shaped before are (`snowlark_paint::text_cache`).
//!
//! **Line breaking** is greedy over clusters: soft wrap opportunities come from the shaper's UAX #14
//! analysis within an item and from the junction of adjacent items, as `white-space`, `word-break` and
//! `overflow-wrap` allow; phase II removes collapsible spaces at line starts and hangs them at line
//! ends. Floats are placed as they're met (on the current line if they fit), lines shortened beside
//! them, and moved down past them when not even one word fits.
//!
//! **Line boxes** align inline boxes and atomic inlines vertically (CSS 2 §10.8.1 with half-leading),
//! reorder bidi runs (UAX #9 L2 per nesting level), apply `text-align`/`-last`, `text-indent`,
//! justification and `text-overflow: ellipsis`, and become line fragments holding inline box pieces and
//! text fragments.
//!
//! Hooks not implemented yet: `::first-line` would restyle the items of the first line built in
//! [`layout_lines`] (re-measuring them before the line is decided); `::first-letter` would be an inline
//! box generated around the first typographic letter unit during preparation. Both styles are
//! available from `StyleSource::pseudo_style`. Bidi levels come from shaping each item with the
//! paragraph's base direction (not one UAX #9 pass over the whole paragraph), and soft wrap
//! opportunities between items use a simplified UAX #14 pair table.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use smallvec::SmallVec;
use snowlark_paint::{
    visual_order, BreakAfter, FontFeature, FontId, FontListId, FontMetrics, FontQuery, Point, Rect,
    ShapeKey, ShapeOptions, ShapedText, Sides, Size, TextCache, TextEngine, VerticalText,
};

use crate::block::layout_float;
use crate::boxes::*;
use crate::floats::Exclusions;
use crate::fragment::*;
use crate::layout::*;
use crate::sizing::{ContentSizes, Edges};
use crate::style::*;

// ----- items -----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Text,
    /// A preserved tab.
    Tab,
    /// The start of an inline box.
    Open,
    /// The end of an inline box.
    Close,
    Atomic,
    Float,
    OutOfFlow,
    /// A forced line break (`<br>`, a preserved newline).
    Break,
}

/// A resolved font: the list, its primary face's metrics at the size, the size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FontInfo {
    pub list: FontListId,
    /// The list's first available font (whose metrics these are).
    pub primary: FontId,
    pub metrics: FontMetrics,
    pub size: f32,
    /// How the text is set if the line is vertical.
    pub vertical: VerticalText,
    /// How far the alphabetic baseline is below the baseline `metrics` are measured from (the
    /// central one in vertical text, §4.2).
    pub alphabetic: f32,
}

pub(crate) struct Item<S: Style> {
    pub kind: ItemKind,
    pub box_id: BoxId,
    pub style: S,
    /// Text items: the processed text.
    pub text: String,
    /// The byte offset of the processed text's start in the box's text.
    pub dom_start: u32,
    /// Where white space collapsing removed characters: (processed offset, bytes removed up to it),
    /// so processed offsets map back to the box's text exactly.
    pub collapsed: Option<Arc<[(u32, u32)]>>,
    pub shaped: Option<Arc<ShapedText>>,
    pub font: Option<FontInfo>,
    pub wrap: bool,
    /// Spaces collapse (and so are removed at line starts and ends).
    pub collapsible: bool,
    /// `white-space-collapse: break-spaces`: a soft wrap opportunity after every preserved white
    /// space character (CSS Text 3 §5.1).
    pub break_spaces: bool,
    pub break_all: bool,
    pub keep_all: bool,
    /// `line-break` (CSS Text 3 §5.3).
    pub line_break: LineBreak,
    /// The content language's writing system is Chinese or Japanese (for `line-break`'s rules
    /// that apply only to them).
    pub cj_language: bool,
    /// `hyphens` (CSS Text 3 §6.1): `none` takes away the soft hyphens' wrap opportunities.
    pub hyphens: Hyphens,
    pub anywhere: bool,
    pub break_word: bool,
    /// Which edges of an inline box are present (split continuations lack one).
    pub has_start_edge: bool,
    pub has_end_edge: bool,
    /// Text set as one upright composition (`text-combine-upright`).
    pub combine: bool,
    /// Text and atomic items of a paragraph analyzed across its inline boxes: the bidi level of each
    /// UTF-16 unit (of an atomic inline: its level; of an isolating box: its initiator's).
    pub levels: Option<Arc<[u8]>>,
    /// Tab items: the font's space advance (tab stops are multiples of it, CSS Text 3 §4.2).
    pub space: f32,
    /// Text items: the used `letter-spacing` and `word-spacing` (px), read once: line breaking
    /// measures every cluster with them. Both are layout damage, so a change of either prepares
    /// the content again. (0 for other items.)
    pub letter_spacing: f32,
    pub word_spacing: f32,
    /// Spaces and other space separators hang at line ends (CSS Text 3 §4.1.2): white space
    /// collapses, or is preserved but wraps.
    pub can_hang: bool,
}

/// A block container's prepared inline content.
pub(crate) struct InlineContent<S: Style> {
    pub items: Vec<Item<S>>,
    /// `unicode-bidi: plaintext` on the container: per item, whether its paragraph is right-to-left.
    pub plaintext: Option<Vec<bool>>,
    /// The container's own font (for the root inline box's strut).
    pub root_font: FontInfo,
}

impl<S: Style> InlineContent<S> {
    /// Whether the paragraph of item `item` is right-to-left.
    fn para_rtl(&self, style: &S, item: usize) -> bool {
        match &self.plaintext {
            Some(p) => p.get(item).copied().unwrap_or(false),
            None => style.direction() == Direction::Rtl,
        }
    }
}

/// Resolves a style's font through the text engine, by way of the tree's [`FontCache`]: a style's
/// font properties are resolved once while some box keeps them.
pub(crate) fn tree_font_info<S: Style>(
    tree: &BoxTree<S>,
    text: &TextEngine,
    style: &S,
) -> FontInfo {
    let font = match style.font_key() {
        Some(key) => tree.fonts.get(key, style, || resolve_font(text, style)),
        None => resolve_font(text, style),
    };
    in_line_frame(style, font)
}

/// The resolved fonts of a box tree by [`Style::font_key`], each kept with a style handle that keeps
/// its key's state alive (so an address in a key is never reused while the entry exists). Font
/// list ids are the text engine's: a tree is laid out with one engine, as its text cache is.
pub(crate) struct FontCache<S> {
    map: std::sync::Mutex<rustc_hash::FxHashMap<(usize, usize), (S, FontInfo)>>,
    /// Fonts resolved through the cache (statistics, and for tests).
    pub(crate) resolved: std::sync::atomic::AtomicU64,
}

/// Entries kept before the cache starts again: a page has a few dozen distinct fonts; the bound
/// keeps a page that keeps making new ones from keeping their styles alive.
const FONT_CACHE_MAX: usize = 512;

impl<S> Default for FontCache<S> {
    fn default() -> FontCache<S> {
        FontCache {
            map: Default::default(),
            resolved: Default::default(),
        }
    }
}

impl<S: Style> FontCache<S> {
    fn get(&self, key: (usize, usize), style: &S, resolve: impl FnOnce() -> FontInfo) -> FontInfo {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, font)) = map.get(&key) {
            return *font;
        }
        let font = resolve();
        self.resolved
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if map.len() >= FONT_CACHE_MAX {
            map.clear();
        }
        map.insert(key, (style.clone(), font));
        font
    }
}

/// A font resolved in the horizontal frame (what does not depend on the writing mode).
fn resolve_font<S: Style>(text: &TextEngine, style: &S) -> FontInfo {
    let families: SmallVec<[&str; 8]> = style.font_family().collect();
    let query = FontQuery {
        families: &families,
        weight: style.font_weight(),
        style: style.font_style(),
        stretch: style.font_stretch(),
    };
    let size = style.font_size();
    let list = text
        .font_list(&query)
        .or_else(|_| {
            text.font_list(&FontQuery {
                families: &["sans-serif"],
                ..query
            })
        })
        .expect("a system font");
    let primary = text.primary_font(list);
    FontInfo {
        list,
        primary,
        metrics: line_metrics(text.metrics(primary, size)),
        size,
        vertical: VerticalText::None,
        alphabetic: 0.0,
    }
}

/// A font resolved in the horizontal frame ([`resolve_font`]), as `style`'s lines see it.
fn in_line_frame<S: Style>(style: &S, mut font: FontInfo) -> FontInfo {
    if central_baseline(style) {
        font.vertical = match style.text_orientation() {
            TextOrientation::Upright => VerticalText::Upright,
            _ => VerticalText::Mixed,
        };
        // CSS Writing Modes 4 §4.2: in vertical typographic mode with `text-orientation: mixed` or
        // `upright` the dominant baseline is the central one, halfway between the em box's over
        // and under edges: line layout sees the font's ascent and descent around it.
        let a = font.metrics.ascent;
        font.metrics = centered(font.metrics);
        font.alphabetic = a - font.metrics.ascent;
    }
    font
}

/// Whether text in `style` aligns on the central baseline (CSS Writing Modes 4 §4.2): vertical
/// typographic mode (`vertical-rl`, `vertical-lr`) with `text-orientation` `mixed` or `upright`.
pub(crate) fn central_baseline<S: Style>(style: &S) -> bool {
    // (by the view: the root's used writing mode can be its body's, §8)
    !style.view().is_identity()
        && !matches!(
            style.writing_mode(),
            WritingMode::SidewaysRl | WritingMode::SidewaysLr
        )
        && style.text_orientation() != TextOrientation::Sideways
}

/// The frame a line of a block container with flow frame `view` is laid out in: y runs from
/// line-over to line-under (CSS Writing Modes 4 §6.3), which is the block direction except in
/// `vertical-lr`, whose line-over is the block-end side. There a line is laid out in that mirrored
/// frame (the one `vertical-rl` flows in), so that ascents, `vertical-align` and the block-axis
/// edges of inline boxes come out on their line-relative sides, and its fragments are mapped back
/// ([`crate::flow::map_line_children`]).
pub(crate) fn line_view(view: Flip) -> Flip {
    if view == Flip::VERTICAL_LR {
        Flip::MIRROR_Y.then(view)
    } else {
        view
    }
}

/// Metrics measured from the central baseline.
pub(crate) fn centered(mut m: FontMetrics) -> FontMetrics {
    let h = m.ascent + m.descent;
    // (whole pixels, as the line metrics are: the extra one goes over the baseline)
    m.descent = (h / 2.0).floor();
    m.ascent = h - m.descent;
    m
}

/// A face's metrics as line layout uses them: the ascent, descent and line gap each rounded to a
/// whole CSS pixel. CSS Inline 3 §5.1 leaves the source of the ascent and descent metrics to the UA;
/// every engine rounds them so that baselines and content areas land on pixels (Blink's
/// `SimpleFontData::PlatformInit` rounds the ascent and descent separately and makes `normal`
/// line-height the sum of the rounded ascent, descent and line gap), and pages' line boxes are
/// measured against that.
pub(crate) fn line_metrics(mut m: FontMetrics) -> FontMetrics {
    let round = |v: f32| (v + 0.5).floor();
    m.ascent = round(m.ascent);
    m.descent = round(m.descent);
    m.line_gap = round(m.line_gap);
    m
}

/// Lengths in line layout are kept in 1/64 px, the layout unit engines use, so that line boxes
/// stacked from fractional line-heights add up as they do elsewhere.
pub(crate) fn layout_unit(v: f32) -> f32 {
    (v * 64.0).round() / 64.0
}

/// The used line height (CSS 2 §10.8.1; `normal` from the font's ascent, descent and line gap).
pub(crate) fn line_height<S: Style>(style: &S, font: &FontInfo) -> f32 {
    match style.line_height() {
        LineHeight::Normal => font.metrics.ascent + font.metrics.descent + font.metrics.line_gap,
        LineHeight::Number(n) => layout_unit(n * font.size),
        LineHeight::Length(l) => layout_unit(l),
    }
}

fn apply_transform(text: &str, t: TextTransform, word_start: &mut bool) -> String {
    match t {
        TextTransform::None => text.to_string(),
        TextTransform::Uppercase => text.to_uppercase(),
        TextTransform::Lowercase => text.to_lowercase(),
        TextTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            for ch in text.chars() {
                if ch.is_alphanumeric() {
                    if *word_start {
                        out.extend(ch.to_uppercase());
                    } else {
                        out.push(ch);
                    }
                    *word_start = false;
                } else {
                    *word_start = ch.is_whitespace() || ch == '-';
                    out.push(ch);
                }
            }
            out
        }
    }
}

struct Prepare<'p, S: Style> {
    /// The container's flow frame.
    view: Flip,
    items: Vec<Item<S>>,
    /// The last character emitted was a collapsible space (collapsing continues across inline boxes).
    after_space: bool,
    word_start: bool,
    _text: std::marker::PhantomData<&'p TextEngine>,
}

impl<S: Style> Prepare<'_, S> {
    /// A style read in the line frame (an atomic inline in another writing mode as seen from it).
    fn in_line_frame(&self, s: &S) -> S {
        if s.view() == self.view {
            s.clone()
        } else {
            s.with_view(self.view)
        }
    }

    fn item(&self, kind: ItemKind, box_id: BoxId, style: S) -> Item<S> {
        let wrap = style.text_wrap_mode() == TextWrapMode::Wrap;
        let wb = style.word_break();
        let ow = style.overflow_wrap();
        let ws = style.white_space_collapse();
        let collapsible = matches!(
            ws,
            WhiteSpaceCollapse::Collapse | WhiteSpaceCollapse::PreserveBreaks
        );
        // (only text is measured cluster by cluster)
        let (letter_spacing, word_spacing) = if kind == ItemKind::Text {
            (style.letter_spacing(), style.word_spacing())
        } else {
            (0.0, 0.0)
        };
        Item {
            kind,
            box_id,
            text: String::new(),
            dom_start: 0,
            collapsed: None,
            shaped: None,
            font: None,
            wrap,
            collapsible,
            break_spaces: ws == WhiteSpaceCollapse::BreakSpaces,
            break_all: wb == WordBreak::BreakAll,
            keep_all: wb == WordBreak::KeepAll,
            line_break: style.line_break(),
            cj_language: is_chinese_or_japanese(style.locale()),
            hyphens: style.hyphens(),
            anywhere: ow == OverflowWrap::Anywhere || wb == WordBreak::BreakWord,
            break_word: ow == OverflowWrap::BreakWord,
            has_start_edge: true,
            has_end_edge: true,
            combine: false,
            levels: None,
            space: 0.0,
            letter_spacing,
            word_spacing,
            can_hang: collapsible || ws == WhiteSpaceCollapse::Preserve && wrap,
            style,
        }
    }

    fn push_text(&mut self, box_id: BoxId, style: &S, text: String, dom_start: u32) {
        self.push_text_collapsed(box_id, style, text, dom_start, Vec::new());
    }

    fn push_text_collapsed(
        &mut self,
        box_id: BoxId,
        style: &S,
        text: String,
        dom_start: u32,
        collapsed: Vec<(u32, u32)>,
    ) {
        if text.is_empty() {
            return;
        }
        let mut it = self.item(ItemKind::Text, box_id, style.clone());
        it.text = text;
        it.dom_start = dom_start;
        if !collapsed.is_empty() {
            it.collapsed = Some(collapsed.into());
        }
        self.items.push(it);
    }

    /// CSS Text 3 §4.1.1 phase I for one text box.
    fn text_box(&mut self, box_id: BoxId, style: &S, raw: &str) {
        let raw = apply_transform(raw, style.text_transform(), &mut self.word_start);
        let collapse = style.white_space_collapse();
        match collapse {
            WhiteSpaceCollapse::Collapse | WhiteSpaceCollapse::PreserveBreaks => {
                let keep_breaks = collapse == WhiteSpaceCollapse::PreserveBreaks;
                let mut cur = String::with_capacity(raw.len());
                let mut start = 0u32;
                // Where collapsing removed characters (processed offset, bytes removed so far).
                let mut collapsed: Vec<(u32, u32)> = Vec::new();
                let mut removed = 0u32;
                // Step 1: spaces and tabs around a segment break are removed, bidi formatting
                // characters ignored. Without such characters, collapsing (step 4) gives the same.
                let around_breaks = spaces_around_segment_breaks(&raw, !keep_breaks);
                for (i, ch) in raw.char_indices() {
                    if around_breaks
                        .as_ref()
                        .is_some_and(|r| r.contains(&(i as u32)))
                    {
                        removed += 1;
                        let at = cur.len() as u32;
                        match collapsed.last_mut() {
                            Some(last) if last.0 == at => last.1 = removed,
                            _ => collapsed.push((at, removed)),
                        }
                        continue;
                    }
                    match ch {
                        '\n' if keep_breaks => {
                            // Spaces before a preserved break are removed (they'd hang anyway).
                            while cur.ends_with(' ') {
                                cur.pop();
                            }
                            let t = std::mem::take(&mut cur);
                            let c = std::mem::take(&mut collapsed);
                            self.push_text_collapsed(box_id, style, t, start, c);
                            removed = 0;
                            let br = self.item(ItemKind::Break, box_id, style.clone());
                            self.items.push(br);
                            self.after_space = true;
                            start = i as u32 + 1;
                        }
                        ' ' | '\t' | '\n' | '\r' => {
                            if !self.after_space {
                                cur.push(' ');
                                self.after_space = true;
                            } else {
                                removed += 1;
                                let at = cur.len() as u32;
                                match collapsed.last_mut() {
                                    Some(last) if last.0 == at => last.1 = removed,
                                    _ => collapsed.push((at, removed)),
                                }
                            }
                        }
                        _ => {
                            cur.push(ch);
                            self.after_space = false;
                        }
                    }
                }
                self.push_text_collapsed(box_id, style, cur, start, collapsed);
            }
            WhiteSpaceCollapse::Preserve
            | WhiteSpaceCollapse::BreakSpaces
            | WhiteSpaceCollapse::PreserveSpaces => {
                let breaks = collapse != WhiteSpaceCollapse::PreserveSpaces;
                let mut cur = String::with_capacity(raw.len());
                let mut start = 0u32;
                for (i, ch) in raw.char_indices() {
                    match ch {
                        '\n' | '\r' if breaks => {
                            if ch == '\r' && raw[i + 1..].starts_with('\n') {
                                continue;
                            }
                            let t = std::mem::take(&mut cur);
                            self.push_text(box_id, style, t, start);
                            let br = self.item(ItemKind::Break, box_id, style.clone());
                            self.items.push(br);
                            start = i as u32 + 1;
                        }
                        '\t' if breaks => {
                            let t = std::mem::take(&mut cur);
                            self.push_text(box_id, style, t, start);
                            let tab = self.item(ItemKind::Tab, box_id, style.clone());
                            self.items.push(tab);
                            start = i as u32 + 1;
                        }
                        '\n' | '\r' | '\t' => cur.push(' '),
                        _ => cur.push(ch),
                    }
                }
                self.push_text(box_id, style, cur, start);
                self.after_space = false;
            }
        }
    }
}

/// Builds (or refreshes) the inline content of block container `id`, reusing shaped text from `old`.
pub(crate) fn prepare<S: Style>(
    tree: &BoxTree<S>,
    text: &TextEngine,
    id: BoxId,
    old: Option<Box<InlineContent<S>>>,
    stats: &mut LayoutStats,
) -> Box<InlineContent<S>> {
    let container = tree.b(id);
    let root_font = tree_font_info(tree, text, &container.style);
    let mut p = Prepare {
        view: line_view(container.style.view()),
        items: Vec::new(),
        after_space: true,
        word_start: true,
        _text: std::marker::PhantomData,
    };
    fn walk<S: Style>(tree: &BoxTree<S>, p: &mut Prepare<'_, S>, id: BoxId) {
        for &c in &tree.b(id).children {
            let b = tree.b(c);
            match b.kind {
                BoxKind::Text => {
                    if let BoxData::Text(t) = &b.data {
                        p.text_box(c, &b.style, t);
                    }
                }
                BoxKind::LineBreak => {
                    let it = p.item(ItemKind::Break, c, b.style.clone());
                    p.items.push(it);
                    p.after_space = true;
                }
                BoxKind::Inline => {
                    let mut open = p.item(ItemKind::Open, c, p.in_line_frame(&b.style));
                    open.has_start_edge = !b.has(flags::NO_START_EDGE);
                    open.has_end_edge = !b.has(flags::NO_END_EDGE);
                    p.items.push(open);
                    walk(tree, p, c);
                    let mut close = p.item(ItemKind::Close, c, p.in_line_frame(&b.style));
                    close.has_start_edge = !b.has(flags::NO_START_EDGE);
                    close.has_end_edge = !b.has(flags::NO_END_EDGE);
                    p.items.push(close);
                }
                _ if b.is_float() => {
                    let it = p.item(ItemKind::Float, c, b.style.clone());
                    p.items.push(it);
                }
                _ if b.is_out_of_flow() => {
                    let it = p.item(ItemKind::OutOfFlow, c, b.style.clone());
                    p.items.push(it);
                }
                _ => {
                    let mut it = p.item(ItemKind::Atomic, c, p.in_line_frame(&b.style));
                    // An atomic inline is a character of its parent's content: whether a line may
                    // wrap next to it is the parent's `white-space` (CSS Text 3 §5: the nearest
                    // common ancestor of the two sides controls breaking), not its own, which
                    // applies inside it.
                    it.wrap = tree.b(id).style.text_wrap_mode() == TextWrapMode::Wrap;
                    p.items.push(it);
                    p.after_space = false;
                }
            }
        }
    }
    walk(tree, &mut p, id);
    let mut items = p.items;
    // (`unicode-bidi` is not inherited: an anonymous block's text is that of the block around it)
    let mut bidi = container;
    while let Some(p) = bidi.parent.filter(|_| bidi.is_anonymous()) {
        bidi = tree.b(p);
    }
    if bidi.level != Level::Block {
        bidi = container;
    }
    let plaintext = bidi_paragraphs(&mut items, &bidi.style, &container.style);

    // Reuse shaping from the previous preparation: same box, same text, same font.
    type Reused<'o> = (Arc<ShapedText>, FontInfo, Option<&'o [u8]>);
    let mut reuse: HashMap<(BoxId, &str), Reused<'_>> = HashMap::new();
    if let Some(old) = &old {
        for it in &old.items {
            if let (Some(s), Some(f)) = (&it.shaped, it.font) {
                reuse.insert(
                    (it.box_id, it.text.as_str()),
                    (s.clone(), f, it.levels.as_deref()),
                );
            }
        }
    }
    let mut buffer = ShapedText::default();
    // The tree's text cache, locked when the first item needs shaping.
    let mut cache: Option<std::sync::MutexGuard<'_, TextCache>> = None;
    let mut words_before = None;
    for it in &mut items {
        match it.kind {
            ItemKind::Text | ItemKind::Tab | ItemKind::Open | ItemKind::Close | ItemKind::Break => {
                let font = tree_font_info(tree, text, &it.style);
                it.font = Some(font);
                if it.kind == ItemKind::Tab {
                    it.space = text
                        .measure(" ", font.list, font.size)
                        .unwrap_or(font.size * 0.25)
                        + it.style.letter_spacing()
                        + it.style.word_spacing();
                }
                if it.kind != ItemKind::Text {
                    continue;
                }
                it.combine = central_baseline(&it.style) && it.style.text_combine_upright();
                if let Some((s, f, l)) = reuse.get(&(it.box_id, it.text.as_str())) {
                    if *f == font && *l == it.levels.as_deref() {
                        it.shaped = Some(s.clone());
                        continue;
                    }
                }
                let vertical = if it.combine {
                    VerticalText::None
                } else {
                    font.vertical
                };
                let features = it.style.font_features();
                let direction = container.style.direction();
                let locale = it.style.locale();
                let key = ShapeKey {
                    text: &it.text,
                    list: font.list,
                    size: font.size,
                    rtl: direction == Direction::Rtl,
                    vertical,
                    locale,
                    features: &features,
                };
                let cache = &mut **cache.get_or_insert_with(|| {
                    let g = tree.text_cache.lock().unwrap_or_else(|e| e.into_inner());
                    words_before = Some(g.words.stats());
                    g
                });
                let levels = it.levels.clone();
                if it.combine {
                    let options = ShapeOptions {
                        size: font.size,
                        locale,
                        direction,
                        features: &features,
                        vertical,
                        levels: levels.as_deref(),
                    };
                    let s = shape_composition(text, &it.text, font.list, &options);
                    it.shaped = Some(combined(&s, font.size));
                    stats.shaped += 1;
                    continue;
                }
                // (levels from the whole paragraph are not part of the run cache's key)
                if let Some(s) = cache.runs.get(&key).filter(|_| levels.is_none()) {
                    it.shaped = Some(s);
                    stats.shape_cache_hits += 1;
                    continue;
                }
                let options = ShapeOptions {
                    size: font.size,
                    locale,
                    direction,
                    features: &features,
                    vertical,
                    levels: levels.as_deref(),
                };
                let shaped = if text
                    .shape_cached(
                        &it.text,
                        font.list,
                        &options,
                        Some(&mut cache.words),
                        &mut buffer,
                    )
                    .is_ok()
                {
                    Arc::new(std::mem::take(&mut buffer))
                } else {
                    Arc::new(ShapedText::default())
                };
                stats.shaped += 1;
                if levels.is_none() {
                    cache.runs.insert(&key, shaped.clone());
                }
                it.shaped = Some(shaped);
            }
            _ => {}
        }
    }
    if let (Some(g), Some(before)) = (&cache, words_before) {
        let after = g.words.stats();
        stats.words_shaped += (after.misses - before.misses) as u32;
        stats.words_cached += (after.hits - before.hits) as u32;
    }
    drop(cache);
    drop(reuse);
    Box::new(InlineContent {
        items,
        plaintext,
        root_font,
    })
}

const LRE: char = '\u{202A}';
const RLE: char = '\u{202B}';
const PDF: char = '\u{202C}';
const LRO: char = '\u{202D}';
const RLO: char = '\u{202E}';
const LRI: char = '\u{2066}';
const RLI: char = '\u{2067}';
const FSI: char = '\u{2068}';
const PDI: char = '\u{2069}';

/// The bidi controls an inline box's `unicode-bidi` and `direction` put at its start and end (CSS
/// Writing Modes 4 §2.4.2).
fn bidi_controls(bidi: UnicodeBidi, rtl: bool) -> (&'static [char], &'static [char]) {
    match (bidi, rtl) {
        (UnicodeBidi::Normal, _) => (&[], &[]),
        (UnicodeBidi::Embed, false) => (&[LRE], &[PDF]),
        (UnicodeBidi::Embed, true) => (&[RLE], &[PDF]),
        (UnicodeBidi::Isolate, false) => (&[LRI], &[PDI]),
        (UnicodeBidi::Isolate, true) => (&[RLI], &[PDI]),
        (UnicodeBidi::BidiOverride, false) => (&[LRO], &[PDF]),
        (UnicodeBidi::BidiOverride, true) => (&[RLO], &[PDF]),
        (UnicodeBidi::IsolateOverride, false) => (&[LRI, LRO], &[PDF, PDI]),
        (UnicodeBidi::IsolateOverride, true) => (&[RLI, RLO], &[PDF, PDI]),
        (UnicodeBidi::Plaintext, _) => (&[FSI], &[PDI]),
    }
}

/// Whether `s` has characters of a right-to-left script or explicit bidi formatting.
fn has_rtl(s: &str) -> bool {
    !s.is_ascii()
        && s.chars().any(|c| {
            matches!(c as u32,
                0x0590..=0x08FF | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069 | 0xFB1D..=0xFDFF
                | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
        })
}

/// Resolves the bidi levels of an inline formatting context as whole paragraphs (CSS Writing Modes
/// 4 §2.4): inline box boundaries add bidi controls, atomic inlines count as U+FFFC, forced line
/// breaks end a paragraph. Without explicit controls text is analyzed item by item, which only
/// differs when right-to-left text meets an item boundary. For `unicode-bidi: plaintext` on the
/// container gives each item's paragraph direction.
fn bidi_paragraphs<S: Style>(items: &mut [Item<S>], bidi: &S, container: &S) -> Option<Vec<bool>> {
    let container_bidi = bidi.unicode_bidi();
    let base_rtl = container.direction() == Direction::Rtl;
    let plaintext = container_bidi == UnicodeBidi::Plaintext;
    let mut explicit = !matches!(
        container_bidi,
        UnicodeBidi::Normal | UnicodeBidi::Embed | UnicodeBidi::Isolate
    );
    let mut texts = 0;
    let mut rtl_text = base_rtl;
    for it in items.iter() {
        match it.kind {
            ItemKind::Text => {
                texts += 1;
                rtl_text = rtl_text || has_rtl(&it.text);
            }
            ItemKind::Open => explicit = explicit || it.style.unicode_bidi() != UnicodeBidi::Normal,
            ItemKind::Atomic => texts += 1,
            _ => {}
        }
    }
    if !explicit && !(rtl_text && texts > 1) {
        return None;
    }
    // On a block container `bidi-override` and `isolate-override` apply to its inline content.
    let (outer_open, outer_close): (&[char], &[char]) = match container_bidi {
        UnicodeBidi::BidiOverride | UnicodeBidi::IsolateOverride => {
            bidi_controls(UnicodeBidi::BidiOverride, base_rtl)
        }
        _ => (&[], &[]),
    };
    let mut para_rtl = vec![base_rtl; items.len()];
    // The controls of the boxes open at the current point, reopened after a paragraph break.
    let mut open: Vec<(&[char], &[char])> = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let end = items[start..]
            .iter()
            .position(|i| i.kind == ItemKind::Break)
            .map_or(items.len(), |k| start + k + 1);
        let mut para = String::new();
        para.extend(outer_open);
        for (o, _) in &open {
            para.extend(*o);
        }
        let mut spans: Vec<(usize, usize, usize)> = Vec::new();
        // Isolating boxes, each at its initiator: to the text around it the box is that character.
        let mut isolates: Vec<(usize, usize)> = Vec::new();
        for (i, it) in items.iter().enumerate().take(end).skip(start) {
            let at = para.len();
            match it.kind {
                ItemKind::Text => {
                    para.push_str(&it.text);
                    spans.push((i, at, para.len()));
                }
                ItemKind::Atomic => {
                    para.push('\u{FFFC}');
                    spans.push((i, at, para.len()));
                }
                ItemKind::Tab => para.push('\t'),
                ItemKind::Open => {
                    let c = bidi_controls(
                        it.style.unicode_bidi(),
                        it.style.direction() == Direction::Rtl,
                    );
                    if c.0.first().is_some_and(|c| matches!(*c, LRI | RLI | FSI)) {
                        isolates.push((i, at));
                    }
                    para.extend(c.0);
                    open.push(c);
                }
                ItemKind::Close => {
                    if let Some((_, close)) = open.pop() {
                        para.extend(close);
                    }
                }
                _ => {}
            }
        }
        for (_, c) in open.iter().rev() {
            para.extend(*c);
        }
        para.extend(outer_close);
        let rtl = if plaintext {
            // UAX #9 P2, P3: the first strong character; without one, the box's `direction`.
            match unicode_bidi::get_base_direction(para.as_str()) {
                unicode_bidi::Direction::Ltr => false,
                unicode_bidi::Direction::Rtl => true,
                unicode_bidi::Direction::Mixed => base_rtl,
            }
        } else {
            base_rtl
        };
        para_rtl[start..end].fill(rtl);
        let base = if rtl {
            unicode_bidi::Level::rtl()
        } else {
            unicode_bidi::Level::ltr()
        };
        let info = unicode_bidi::BidiInfo::new(&para, Some(base));
        // A left-to-right paragraph whose text and isolates all resolve to even levels (left-to-right
        // isolates such as an inline `li` or `[dir=ltr]` box around left-to-right text: the UA sheet
        // isolates them, and their contents sit at level 2) is unidirectional: UAX #9 L2 reorders
        // nothing in it and all of it is shaped left to right. It keeps no levels: its items are
        // analyzed one by one, as text in a paragraph without bidi controls is, and keep the
        // shaping caches, which items with paragraph levels bypass.
        let even = |at: usize| info.levels[at].number().is_multiple_of(2);
        if !rtl
            && spans.iter().all(|&(_, a, b)| (a..b).all(even))
            && isolates.iter().all(|&(_, at)| even(at))
        {
            start = end;
            continue;
        }
        for (i, a, b) in spans {
            // Per UTF-16 unit of the item, as its text is shaped.
            let mut levels = Vec::with_capacity(b - a);
            for (o, c) in para[a..b].char_indices() {
                let l = info.levels[a + o].number();
                levels.extend(std::iter::repeat_n(l, c.len_utf16()));
            }
            items[i].levels = Some(levels.into());
        }
        for (i, at) in isolates {
            items[i].levels = Some([info.levels[at].number()].into());
        }
        start = end;
    }
    plaintext.then_some(para_rtl)
}

/// The text of a composition (CSS Writing Modes 4 §9.1.3). Wider than 1em, it first tries narrower
/// glyphs: full-width characters as their ASCII forms, then the font's half-, third- or quarter-width
/// forms for two, three or four characters. What is still too wide is scaled when painted.
fn shape_composition(
    engine: &TextEngine,
    text: &str,
    list: FontListId,
    options: &ShapeOptions<'_>,
) -> ShapedText {
    let shape = |text: &str, options: &ShapeOptions<'_>| {
        let mut s = ShapedText::default();
        let _ = engine.shape(text, list, options, &mut s);
        s
    };
    let em = options.size;
    let mut shaped = shape(text, options);
    if shaped.width() <= em {
        return shaped;
    }
    let narrow: String = text
        .chars()
        .map(|c| match c {
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '\u{3000}' => ' ',
            c => c,
        })
        .collect();
    if narrow != text {
        shaped = shape(&narrow, options);
        if shaped.width() <= em {
            return shaped;
        }
    }
    let tag = match narrow.chars().filter(|c| !c.is_whitespace()).count() {
        2 => b"hwid",
        3 => b"twid",
        4 => b"qwid",
        _ => return shaped,
    };
    let mut features = options.features.to_vec();
    features.push(FontFeature::new(tag, 1));
    let narrower = shape(
        &narrow,
        &ShapeOptions {
            features: &features,
            ..*options
        },
    );
    if narrower.width() < shaped.width() {
        shaped = narrower;
    }
    shaped
}

/// Shaped text as one composition 1em wide on the line (CSS Writing Modes 4 §9.1.2): a single
/// cluster, which nothing breaks and which breaks around like an object (§9.1.4).
fn combined(s: &ShapedText, em: f32) -> Arc<ShapedText> {
    let mut out = s.clone();
    // (collapsible spaces at its ends are at the composition's line edges, §9.1.2)
    let visible = || s.clusters.iter().filter(|c| !c.whitespace);
    let (Some(first), Some(last)) = (visible().next(), visible().next_back()) else {
        return Arc::new(out);
    };
    let mut cluster = *first;
    cluster.text.1 = last.text.1;
    cluster.glyphs.1 = last.glyphs.1;
    cluster.advance = em;
    cluster.break_after = last.break_after;
    cluster.whitespace = false;
    cluster.soft_hyphen = false;
    out.clusters = vec![cluster];
    let mut run = s.runs[0];
    run.text.1 = s.runs[s.runs.len() - 1].text.1;
    run.glyphs.1 = s.runs[s.runs.len() - 1].glyphs.1;
    run.clusters = (0, 1);
    run.advance = em;
    out.runs = vec![run];
    Arc::new(out)
}

// ----- measuring -----

/// A position in the item list: an item and, for text items, a cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
struct Pos {
    item: usize,
    cluster: usize,
}

/// The inline-start and -end edges (margin + border + padding) of an inline box, physical left/right.
fn inline_edges<S: Style>(style: &S, cb_width: f32) -> (f32, f32) {
    let e = Edges::of(style, cb_width);
    (
        e.ml() + e.border.left + e.padding.left,
        e.mr() + e.border.right + e.padding.right,
    )
}

/// The inline-start and -end edges (margin + border + padding) of an inline box, in logical
/// order: the start edge is the line-left one in a left-to-right box and the line-right one in a
/// right-to-left box (CSS Writing Modes 4 §6.4, the box's own `direction`).
fn start_end_edges<S: Style>(style: &S, cb_width: f32) -> (f32, f32) {
    let (left, right) = inline_edges(style, cb_width);
    if style.direction() == Direction::Rtl {
        (right, left)
    } else {
        (left, right)
    }
}

/// Whether a piece of an inline box has its line-left and line-right edges, from whether it has
/// its start and end edges: the start edge is the right one in a right-to-left box (CSS Writing
/// Modes 4 §6.4), so a box broken across lines right to left has its right padding, border and
/// margin on its first piece (CSS 2 §8.6).
fn line_sides<S: Style>(style: &S, start: bool, end: bool) -> (bool, bool) {
    if style.direction() == Direction::Rtl {
        (end, start)
    } else {
        (start, end)
    }
}

/// Whether an inline box start or end item has a non-zero margin, border or padding on its side.
fn has_inline_edge<S: Style>(it: &Item<S>, cb_width: f32) -> bool {
    let e = Edges::of(&it.style, cb_width);
    let left = e.ml() != 0.0 || e.border.left + e.padding.left != 0.0;
    let right = e.mr() != 0.0 || e.border.right + e.padding.right != 0.0;
    let (start, end) = if it.style.direction() == Direction::Rtl {
        (right, left)
    } else {
        (left, right)
    };
    match it.kind {
        ItemKind::Open => it.has_start_edge && start,
        ItemKind::Close => it.has_end_edge && end,
        _ => false,
    }
}

struct Measure<'m, S: Style> {
    items: &'m [Item<S>],
    /// Atomic inline and float results, by item index.
    atomics: &'m HashMap<usize, LayoutResult<S>>,
    cb_width: f32,
    /// Lines are laid out in a mirrored frame (see [`line_view`]).
    mirrored: bool,
    /// The dominant baseline of the lines is the central one.
    central: bool,
}

impl<S: Style> Measure<'_, S> {
    /// A box of height `h` without a baseline: where its baseline is synthesized (CSS Inline 3 §4.1:
    /// the under edge of the margin box for the alphabetic baseline, its middle for the central
    /// one), from the top.
    fn synthesized_baseline(&self, h: f32) -> f32 {
        if self.central {
            h - h / 2.0
        } else {
            h
        }
    }

    /// A distance from the top of a box of height `h` in the block frame, from the top of the box
    /// in the line frame.
    fn line_y(&self, y: f32, h: f32) -> f32 {
        if self.mirrored {
            h - y
        } else {
            y
        }
    }

    fn clusters(&self, i: usize) -> usize {
        self.items[i]
            .shaped
            .as_ref()
            .map_or(0, |s| s.clusters.len())
    }

    /// Units in an item (clusters for text, 1 otherwise).
    fn units(&self, i: usize) -> usize {
        if self.items[i].kind == ItemKind::Text {
            self.clusters(i)
        } else {
            1
        }
    }

    fn cluster_width(&self, i: usize, c: usize) -> f32 {
        let it = &self.items[i];
        let s = it.shaped.as_ref().unwrap();
        let cl = &s.clusters[c];
        if it.combine {
            // (`letter-spacing` does not apply in a composition, §9.1.2)
            return cl.advance;
        }
        let mut w = cl.advance + it.letter_spacing;
        if cl.whitespace {
            w += it.word_spacing;
        }
        w
    }

    /// Whether cluster `c` of item `i` is a space (U+0020): the white space that collapses, is
    /// removed at line starts and hangs at line ends (CSS Text 3 §4.1). Other characters the shaper
    /// counts as white space, such as U+00A0 NO-BREAK SPACE, are not.
    fn is_space(&self, i: usize, c: usize) -> bool {
        let it = &self.items[i];
        it.kind == ItemKind::Text && is_space_cluster(it, c)
    }

    /// Whether cluster `c` of item `i` hangs at the end of a line (CSS Text 3 §4.1.2): a space, or
    /// one of the other space separators (Unicode Zs but U+0020 and U+00A0, such as U+3000 and
    /// U+1680), where white space collapses (or is preserved but wraps, as `pre-wrap`).
    fn hangs(&self, i: usize, c: usize) -> bool {
        let it = &self.items[i];
        it.kind == ItemKind::Text
            && it.can_hang
            && (is_space_cluster(it, c) || is_other_space_separator(it, c))
    }

    /// Width of a unit at line position `x` (tabs depend on it).
    fn unit_width(&self, p: Pos, x: f32) -> f32 {
        let it = &self.items[p.item];
        match it.kind {
            ItemKind::Text => self.cluster_width(p.item, p.cluster),
            ItemKind::Tab => {
                // The next tab stop at least half a space away (CSS Text 3 §4.2).
                let stop = match it.style.tab_size() {
                    TabSize::Spaces(n) => n * it.space,
                    TabSize::Length(l) => l,
                }
                .max(1.0);
                let mut next = ((x / stop).floor() + 1.0) * stop;
                if next - x < it.space * 0.5 {
                    next += stop;
                }
                next - x
            }
            ItemKind::Open => {
                if it.has_start_edge {
                    start_end_edges(&it.style, self.cb_width).0
                } else {
                    0.0
                }
            }
            ItemKind::Close => {
                if it.has_end_edge {
                    start_end_edges(&it.style, self.cb_width).1
                } else {
                    0.0
                }
            }
            ItemKind::Atomic => self.atomics.get(&p.item).map_or(0.0, |r| {
                r.fragment.size.width + r.margin.left + r.margin.right
            }),
            _ => 0.0,
        }
    }

    fn first_char(&self, i: usize) -> Option<char> {
        self.items[i].text.chars().next()
    }

    fn last_char(&self, i: usize) -> Option<char> {
        self.items[i].text.chars().next_back()
    }

    /// The next item after `i` that holds content, skipping inline box edges.
    fn next_content(&self, i: usize) -> Option<usize> {
        (i + 1..self.items.len()).find(|&j| {
            !matches!(
                self.items[j].kind,
                ItemKind::Open | ItemKind::Close | ItemKind::OutOfFlow | ItemKind::Float
            )
        })
    }

    /// Whether a soft wrap opportunity follows unit `p`.
    fn break_after(&self, p: Pos) -> bool {
        let it = &self.items[p.item];
        match it.kind {
            ItemKind::Text => {
                let s = it.shaped.as_ref().unwrap();
                let last = p.cluster + 1 >= s.clusters.len();
                if !last {
                    if !it.wrap {
                        return false;
                    }
                    let c = &s.clusters[p.cluster];
                    if it.line_break == LineBreak::Anywhere
                        || it.break_spaces && is_space_cluster(it, p.cluster)
                    {
                        return true;
                    }
                    if it.keep_all {
                        return c.whitespace
                            || c.break_after == BreakAfter::Allowed
                                && is_space_like(
                                    self.items[p.item].text[c.text.0 as usize..].chars().next(),
                                );
                    }
                    if it.break_all && !c.whitespace {
                        return true;
                    }
                    if c.soft_hyphen && it.hyphens == Hyphens::None {
                        return false;
                    }
                    if c.break_after != BreakAfter::Never {
                        return true;
                    }
                    if matches!(it.line_break, LineBreak::Normal | LineBreak::Loose) {
                        let next = &s.clusters[p.cluster + 1];
                        // (every character `relaxed_break` allows a break before is outside ASCII)
                        if it
                            .text
                            .as_bytes()
                            .get(next.text.0 as usize)
                            .is_none_or(|&b| b < 0x80)
                        {
                            return false;
                        }
                        let before = it.text[c.text.0 as usize..c.text.1 as usize]
                            .chars()
                            .next_back();
                        let after = it.text[next.text.0 as usize..].chars().next();
                        if let (Some(a), Some(b)) = (before, after) {
                            return relaxed_break(it.line_break, it.cj_language, a, b);
                        }
                    }
                    return false;
                }
                self.junction(p.item, it.wrap, self.last_char(p.item))
            }
            ItemKind::Atomic | ItemKind::Tab => self.junction(p.item, it.wrap, Some('\u{FFFC}')),
            ItemKind::Close | ItemKind::Open => false,
            _ => false,
        }
    }

    /// Whether a line that ends after unit `p` ends with a hyphen: `p` is a soft hyphen and
    /// `hyphens` is not `none` (CSS Text 3 §6.1; `line-break: anywhere` hyphenates nothing).
    fn hyphenates_after(&self, p: Pos) -> bool {
        let it = &self.items[p.item];
        it.kind == ItemKind::Text
            && it.hyphens != Hyphens::None
            && it.line_break != LineBreak::Anywhere
            && it
                .shaped
                .as_ref()
                .and_then(|s| s.clusters.get(p.cluster))
                .is_some_and(|c| c.soft_hyphen)
    }

    /// The soft wrap opportunity between item `i` (ending with `last`) and the next content.
    fn junction(&self, i: usize, wrap: bool, last: Option<char>) -> bool {
        let Some(j) = self.next_content(i) else {
            return false;
        };
        let next = &self.items[j];
        // The wrap opportunity after a space follows the space's own `white-space`, so a space
        // before a `nowrap` box still breaks (CSS Text 3 §5.1: the nearest common ancestor's
        // `white-space` controls it, and that is the wrapping one here); elsewhere both sides must
        // allow wrapping.
        if !wrap || !next.wrap && next.kind == ItemKind::Text && !is_space_like(last) {
            return false;
        }
        // A soft hyphen ending a text item (CSS Text 3 §6.1).
        if last == Some('\u{AD}') && self.items[i].hyphens != Hyphens::None {
            return matches!(next.kind, ItemKind::Text | ItemKind::Atomic | ItemKind::Tab);
        }
        match next.kind {
            ItemKind::Text => junction_break(last, self.first_char(j)),
            ItemKind::Atomic | ItemKind::Tab => junction_break(last, Some('\u{FFFC}')),
            _ => false,
        }
    }
}

/// Whether a text item's cluster `c` is a space character (U+0020).
fn is_space_cluster<S: Style>(it: &Item<S>, c: usize) -> bool {
    let cl = &it.shaped.as_ref().unwrap().clusters[c];
    cl.whitespace && it.text.as_bytes().get(cl.text.0 as usize) == Some(&b' ')
}

/// Whether a text item's cluster `c` starts with one of CSS Text's other space separators.
fn is_other_space_separator<S: Style>(it: &Item<S>, c: usize) -> bool {
    let cl = &it.shaped.as_ref().unwrap().clusters[c];
    cl.whitespace
        && matches!(
            it.text[cl.text.0 as usize..].chars().next(),
            Some('\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
        )
}

fn is_space_like(c: Option<char>) -> bool {
    matches!(c, Some(' ' | '\u{3000}'))
}

/// UAX #14 at an item boundary, simplified: after spaces and before/after ideographs and object
/// replacement characters, unless a closing punctuation follows or an opening one precedes.
fn junction_break(prev: Option<char>, next: Option<char>) -> bool {
    let (Some(a), Some(b)) = (prev, next) else {
        return false;
    };
    if matches!(b, ' ' | '\u{A0}' | '\u{202F}' | '\u{2060}' | '\u{FEFF}') {
        return false;
    }
    if matches!(
        b,
        ',' | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | ')'
            | ']'
            | '}'
            | '\u{3001}'
            | '\u{3002}'
            | '\u{FF0C}'
            | '\u{FF0E}'
            | '%'
    ) {
        return false;
    }
    if matches!(a, '(' | '[' | '{' | '\u{A0}' | '\u{2060}' | '\u{FEFF}') {
        return false;
    }
    if a == ' ' || a == '\u{200B}' || a == '\u{3000}' {
        return true;
    }
    if a == '-' || a == '\u{2010}' || a == '\u{2013}' {
        return b.is_alphanumeric();
    }
    is_ideographic(a) || is_ideographic(b) || a == '\u{FFFC}' || b == '\u{FFFC}'
}

fn is_ideographic(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFF60 | 0x20000..=0x3FFFF)
}

/// The bidi formatting characters (UAX #9 §2), which CSS Text 3 §4.1.1 ignores when it removes
/// the spaces around a segment break.
fn is_bidi_formatting(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// CSS Text 3 §4.1.1 phase I step 1 for collapsible white space: the byte offsets of the spaces
/// and tabs immediately before or after a segment break, bidi formatting characters ignored, and,
/// when segment breaks collapse (`merge_breaks`), of a segment break that immediately follows
/// another (§4.1.2); `None` when the text has no bidi formatting character (collapsing alone then
/// removes the same).
fn spaces_around_segment_breaks(text: &str, merge_breaks: bool) -> Option<Vec<u32>> {
    // Every bidi formatting character starts with byte 0xD8 or 0xE2 in UTF-8.
    if !text.bytes().any(|b| b == 0xD8 || b == 0xE2) || !text.chars().any(is_bidi_formatting) {
        return None;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    for (k, &(_, ch)) in chars.iter().enumerate() {
        if ch != '\n' {
            continue;
        }
        let mut walk = |range: &mut dyn Iterator<Item = usize>, forward: bool| {
            for j in range {
                let (at, c) = chars[j];
                match c {
                    ' ' | '\t' => out.push(at as u32),
                    '\n' if forward && merge_breaks => {
                        out.push(at as u32);
                        break;
                    }
                    c if is_bidi_formatting(c) => {}
                    _ => break,
                }
            }
        };
        walk(&mut (0..k).rev(), false);
        walk(&mut (k + 1..chars.len()), true);
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// Whether a content language (BCP 47) is Chinese or Japanese: `line-break`'s rules that apply
/// only "if the writing system is Chinese or Japanese" (CSS Text 3 §5.3).
fn is_chinese_or_japanese(lang: &str) -> bool {
    let primary = lang.split(['-', '_']).next().unwrap_or("");
    primary.eq_ignore_ascii_case("ja") || primary.eq_ignore_ascii_case("zh")
}

/// Whether `line-break: normal` or `loose` allows a soft wrap opportunity between `a` and `b`
/// that the UAX #14 rules (the `strict` behaviour the shaper reports) forbid. CSS Text 3 §5.3:
///
/// - `normal` and `loose`: before conditional Japanese starters (class CJ, which `strict` treats
///   as NS and the others as ID), and, for Chinese or Japanese, before the hyphen-like 〜 and ゠.
/// - `loose` also: before iteration marks and inseparable characters (IN), and, for Chinese or
///   Japanese, before centered punctuation marks and before suffixes (PO with East Asian Width
///   A, F or W), when the preceding character is an ideographic letter (class ID).
fn relaxed_break(mode: LineBreak, cj_language: bool, a: char, b: char) -> bool {
    let loose = mode == LineBreak::Loose;
    if !is_line_break_id(a) {
        return false;
    }
    if is_conditional_japanese_starter(b) || cj_language && matches!(b, '\u{301C}' | '\u{30A0}') {
        return true;
    }
    if !loose {
        return false;
    }
    if matches!(
        b,
        // Iteration marks.
        '\u{3005}' | '\u{303B}' | '\u{309D}' | '\u{309E}' | '\u{30FD}' | '\u{30FE}'
        // Inseparable characters (UAX #14 class IN).
        | '\u{2024}' | '\u{2025}' | '\u{2026}' | '\u{22EF}' | '\u{FE19}'
    ) {
        return true;
    }
    cj_language
        && matches!(
            b,
            // Centered punctuation marks.
            '\u{30FB}' | '\u{FF1A}' | '\u{FF1B}' | '\u{FF65}' | '\u{203C}' | '\u{2047}' | '\u{2048}'
            | '\u{2049}' | '\u{FF01}' | '\u{FF1F}'
            // Suffixes: class PO with East Asian Width A, F or W.
            | '\u{00B0}' | '\u{2030}' | '\u{2032}' | '\u{2033}' | '\u{2035}' | '\u{2103}'
            | '\u{2109}' | '\u{FE6A}' | '\u{FF05}' | '\u{FFE0}'
        )
}

/// UAX #14 class CJ: small kana and the prolonged sound marks.
fn is_conditional_japanese_starter(c: char) -> bool {
    matches!(
        c as u32,
        0x3041 | 0x3043 | 0x3045 | 0x3047 | 0x3049 | 0x3063 | 0x3083 | 0x3085 | 0x3087 | 0x308E
            | 0x3095 | 0x3096 | 0x30A1 | 0x30A3 | 0x30A5 | 0x30A7 | 0x30A9 | 0x30C3 | 0x30E3
            | 0x30E5 | 0x30E7 | 0x30EE | 0x30F5 | 0x30F6 | 0x30FC | 0x31F0..=0x31FF
            | 0xFF67..=0xFF70
    )
}

/// UAX #14 class ID (ideographs, kana, Hangul syllables and the CJK symbols of the class), with
/// the conditional Japanese starters that `normal` and `loose` resolve to ID.
fn is_line_break_id(c: char) -> bool {
    matches!(
        c as u32,
        0x2E80..=0x2FFF
            | 0x3003 | 0x3004 | 0x3006 | 0x3007 | 0x3012 | 0x3013 | 0x3020..=0x3029
            | 0x3030..=0x303A | 0x303C..=0x303F
            | 0x3041..=0x3096 | 0x309F | 0x30A1..=0x30FA | 0x30FC | 0x30FF
            | 0x3105..=0x31FF | 0x3200..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFF66..=0xFF9D | 0x20000..=0x3FFFD
    )
}

// ----- intrinsic sizes -----

/// Min-content and max-content widths of an inline formatting context (css-sizing-3 §5.1).
pub(crate) fn content_sizes<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) -> ContentSizes {
    let content = take_content(cx, id);
    let n = content.items.len();
    let cb_width = 0.0;
    // Atomic inlines contribute their own min/max; floats too.
    let mut atomic_sizes: HashMap<usize, ContentSizes> = HashMap::new();
    for (i, it) in content.items.iter().enumerate() {
        if matches!(it.kind, ItemKind::Atomic | ItemKind::Float) {
            atomic_sizes.insert(i, crate::intrinsic::contribution(cx, it.box_id));
        }
    }
    let empty = HashMap::new();
    let m = Measure {
        items: &content.items,
        atomics: &empty,
        cb_width,
        mirrored: false,
        central: false,
    };
    let style = cx.tree.b(id).style.clone();
    let indent = style.text_indent().resolve(0.0);
    let mut max = 0.0f32;
    let mut min = 0.0f32;
    let mut line = indent;
    let mut line_trailing = 0.0f32;
    let mut word = indent;
    // Preserved spaces at the end of `word` that hang if a line ends after them (CSS Text 3
    // §4.1.3: `pre-wrap`'s hang unconditionally; hanging glyphs do not count in min-content).
    let mut word_hanging = 0.0f32;
    let mut floats_line = 0.0f32;
    for i in 0..n {
        let it = &content.items[i];
        match it.kind {
            ItemKind::Break => {
                max = max.max(line - line_trailing + floats_line);
                min = min.max(word - word_hanging);
                line = 0.0;
                line_trailing = 0.0;
                word = 0.0;
                word_hanging = 0.0;
            }
            ItemKind::Float => {
                let s = atomic_sizes[&i];
                // A float that clears goes below the floats before it (CSS 2 §9.5.2), so they no
                // longer add up side by side.
                if cx.tree.b(it.box_id).style.clear() != Clear::None {
                    max = max.max(line - line_trailing + floats_line);
                    floats_line = 0.0;
                }
                floats_line += s.max;
                min = min.max(s.min);
            }
            ItemKind::OutOfFlow => {}
            ItemKind::Atomic => {
                let s = atomic_sizes[&i];
                line += s.max;
                line_trailing = 0.0;
                word_hanging = 0.0;
                if m.junction(i, it.wrap, Some('\u{FFFC}')) || !it.wrap {
                    word += s.min;
                    min = min.max(word);
                    if it.wrap {
                        word = 0.0;
                    }
                } else {
                    word += s.min;
                }
                // A break before an atomic inline is usually allowed.
                min = min.max(s.min);
            }
            ItemKind::Text => {
                let clusters = m.clusters(i);
                // (a cluster inside a word only adds its width, see `word_run`)
                let words = word_clusters(it).filter(|_| !(it.anywhere && it.wrap));
                let mut c = 0;
                while c < clusters {
                    if let Some(s) = words {
                        if c + 1 < clusters && inside_word(it, s, c) {
                            let w = s.clusters[c].advance + it.letter_spacing;
                            line += w;
                            line_trailing = 0.0;
                            word += w;
                            word_hanging = 0.0;
                            c += 1;
                            continue;
                        }
                    }
                    let p = Pos {
                        item: i,
                        cluster: c,
                    };
                    let w = m.cluster_width(i, c);
                    // (hanging white space at a line end does not count, CSS Text 3 §4.1.2)
                    let space =
                        m.is_space(i, c) || it.collapsible && is_other_space_separator(it, c);
                    line += w;
                    // (preserved spaces stay in the max-content size; only collapsible ones go)
                    if space && it.collapsible {
                        line_trailing += w;
                    } else {
                        line_trailing = 0.0;
                    }
                    let breaks = it.anywhere && it.wrap || m.break_after(p);
                    if !space || !it.collapsible {
                        word += w;
                        if !it.collapsible && m.hangs(i, c) {
                            word_hanging += w;
                        } else {
                            word_hanging = 0.0;
                        }
                    } else if !breaks {
                        // A collapsible space with no soft wrap opportunity after it (`nowrap`)
                        // stays inside the unbreakable run; it is removed only where a line
                        // ends right after it (CSS Text 3 §4.1.2), so it counts as hanging.
                        word += w;
                        word_hanging += w;
                    }
                    if breaks {
                        min = min.max(word - word_hanging);
                        word = 0.0;
                        word_hanging = 0.0;
                    }
                    c += 1;
                }
            }
            ItemKind::Tab => {
                let w = m.unit_width(
                    Pos {
                        item: i,
                        cluster: 0,
                    },
                    line,
                );
                line += w;
                word += w;
                word_hanging = 0.0;
            }
            ItemKind::Open | ItemKind::Close => {
                let w = m.unit_width(
                    Pos {
                        item: i,
                        cluster: 0,
                    },
                    line,
                );
                line += w;
                word += w;
            }
        }
    }
    max = max.max(line - line_trailing + floats_line);
    min = min.max(word - word_hanging);
    put_content(cx, id, content);
    ContentSizes {
        min: min.max(0.0),
        max: max.max(min).max(0.0),
    }
}

fn take_content<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) -> Box<InlineContent<S>> {
    let cache = &mut cx.tree.b_mut(id).cache;
    let stale = cache.inline_stale;
    let restyled = std::mem::take(&mut cache.inline_restyled);
    let existing = cache.inline.take();
    match existing {
        Some(mut c) if !stale => {
            if restyled {
                restyle_items(cx.tree, id, &mut c);
            }
            c
        }
        old => {
            let started = std::time::Instant::now();
            let mut stats = cx.stats;
            let c = prepare(cx.tree, cx.text, id, old, &mut stats);
            cx.stats = stats;
            cx.stats.prepare_us += started.elapsed().as_micros() as u64;
            cx.stats.prepared += 1;
            cx.tree.b_mut(id).cache.inline_stale = false;
            c
        }
    }
}

/// Gives prepared items their boxes' current style handles, as [`prepare`] would: the boxes were
/// restyled in ways that prepare the content the same ([`Style::same_inline_preparation`]), so
/// only what layout reads from the items' styles changed.
fn restyle_items<S: Style>(tree: &BoxTree<S>, id: BoxId, content: &mut InlineContent<S>) {
    let view = line_view(tree.b(id).style.view());
    for it in &mut content.items {
        let s = &tree.b(it.box_id).style;
        it.style = match it.kind {
            ItemKind::Open | ItemKind::Close | ItemKind::Atomic if s.view() != view => {
                s.with_view(view)
            }
            _ => s.clone(),
        };
    }
}

fn put_content<S: Style>(cx: &mut Cx<'_, S>, id: BoxId, content: Box<InlineContent<S>>) {
    cx.tree.b_mut(id).cache.inline = Some(content);
}

// ----- layout -----

/// What an inline formatting context's layout gives its block container (offsets relative to the
/// container's content box).
pub(crate) struct InlineOutput<S: Style> {
    /// Line boxes and floats.
    pub children: Vec<FragmentChild<S>>,
    pub height: f32,
    pub first_baseline: Option<f32>,
    pub last_baseline: Option<f32>,
    /// At least one line isn't phantom (CSS 2 §9.4.2).
    pub has_content: bool,
    pub oof: Vec<Oof>,
    /// How much `text-box-trim` took off the top of the first line.
    pub start_trim: f32,
}

/// One decided line: its units and where it starts.
struct LineSpec {
    start: Pos,
    end: Pos,
    /// Line ends with a forced break (or the end of the content): `text-align-last` applies.
    last: bool,
    left: f32,
    avail: f32,
    /// The block overflow ellipsis (CSS Overflow 4 §4.3) at the end of the line: the line before a
    /// line-clamp container's clamp point.
    ellipsis: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_inline_content<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    content_width: f32,
    cb_height: Option<f32>,
    bfc: &mut Exclusions,
    origin: Point,
    marker: Option<BoxId>,
    trim: TrimRequest,
) -> InlineOutput<S> {
    let content = take_content(cx, id);
    let floats_mark = bfc.len();
    let clamp_before = cx.clamp.clone();
    let clipped_inlines_before = std::mem::take(&mut cx.clipped_inlines);
    let striped_inlines_before = std::mem::take(&mut cx.striped_inlines);
    let mut out = layout_lines(
        cx,
        id,
        &content,
        content_width,
        cb_height,
        bfc,
        origin,
        marker,
        trim,
    );
    if out.start_trim != 0.0 && bfc.len() > floats_mark {
        // text-box-trim on the first line moves everything after its top up, floats too: lay the
        // lines out again from a content box that starts that much higher, untrimmed, and put
        // them back (the trimmed part of the first line is then above the content box).
        let d = out.start_trim;
        bfc.truncate(floats_mark);
        cx.clamp = clamp_before;
        let shifted = Point::new(origin.x, origin.y - d);
        let again = TrimRequest {
            start: false,
            end: trim.end,
        };
        out = layout_lines(
            cx,
            id,
            &content,
            content_width,
            cb_height,
            bfc,
            shifted,
            marker,
            again,
        );
        for c in &mut out.children {
            c.offset.y -= d;
        }
        for o in &mut out.oof {
            *o = o.shifted(0.0, -d);
        }
        out.first_baseline = out.first_baseline.map(|b| b - d);
        out.last_baseline = out.last_baseline.map(|b| b - d);
        out.height = (out.height - d).max(0.0);
        out.start_trim = d;
    }
    if std::mem::replace(&mut cx.clipped_inlines, clipped_inlines_before) {
        relate_clipped_pieces(&out.children);
    }
    if std::mem::replace(&mut cx.striped_inlines, striped_inlines_before) {
        strip_pieces(&out.children);
    }
    put_content(cx, id, content);
    out
}

/// Places the pieces of each inline box that is on several lines in their strip (see
/// [`BoxFragment::strip`]): the pieces side by side in line order as one box, with the box's
/// edges at the strip's ends. A box laid out in one piece here has none.
fn strip_pieces<S: Style>(lines: &[FragmentChild<S>]) {
    fn walk<'a, S: Style>(
        children: &'a [FragmentChild<S>],
        pieces: &mut HashMap<u32, SmallVec<[&'a Fragment<S>; 2]>>,
    ) {
        for c in children {
            let f = &*c.fragment;
            match &f.kind {
                FragmentKind::Line { .. } => walk(&f.children, pieces),
                FragmentKind::Box(b) if b.flags & fragment_flags::INLINE_BOX != 0 => {
                    pieces.entry(b.order).or_default().push(f);
                    walk(&f.children, pieces);
                }
                _ => {}
            }
        }
    }
    let mut pieces = HashMap::new();
    walk(lines, &mut pieces);
    for list in pieces.values() {
        let boxes = || list.iter().filter_map(|f| f.as_box());
        let Some(first) = boxes().next() else {
            continue;
        };
        // (in vertical writing modes the strip runs down the lines: not done, each piece is its
        // own area as before)
        if list.len() < 2 || !first.style.view().is_identity() {
            for b in boxes() {
                b.strip.set(None);
            }
            continue;
        }
        let total: f32 = list.iter().map(|f| f.size.width).sum();
        // (the box's left and right edges are on one piece each)
        let left = (
            boxes().map(|b| b.border.left).sum::<f32>(),
            boxes().map(|b| b.padding.left).sum::<f32>(),
        );
        let right = (
            boxes().map(|b| b.border.right).sum::<f32>(),
            boxes().map(|b| b.padding.right).sum::<f32>(),
        );
        let rtl = first.style.direction() == Direction::Rtl;
        let mut before = 0.0;
        for f in list {
            let Some(b) = f.as_box() else { continue };
            // (right to left, the first piece is at the strip's right end)
            let x = if rtl {
                -(total - before - f.size.width)
            } else {
                -before
            };
            let border = Rect::new(x, 0.0, total, f.size.height);
            let padding = Rect::new(
                border.x + left.0,
                b.border.top,
                (total - left.0 - right.0).max(0.0),
                (f.size.height - b.border.top - b.border.bottom).max(0.0),
            );
            let content = Rect::new(
                padding.x + left.1,
                padding.y + b.padding.top,
                (padding.width - left.1 - right.1).max(0.0),
                (padding.height - b.padding.top - b.padding.bottom).max(0.0),
            );
            b.strip.set(Some([border, padding, content]));
            before += f.size.width;
        }
    }
}

/// Relates the pieces of each inline box with a `clip-path` that is on several lines: every
/// piece after the first clips to the first's reference boxes (see
/// [`BoxFragment::clip_reference`]), which the walk finds in line order.
fn relate_clipped_pieces<S: Style>(lines: &[FragmentChild<S>]) {
    fn walk<S: Style>(
        children: &[FragmentChild<S>],
        at: Point,
        first: &mut HashMap<u32, ReferenceBoxes>,
    ) {
        for c in children {
            let here = Point::new(at.x + c.offset.x, at.y + c.offset.y);
            let f = &*c.fragment;
            match &f.kind {
                FragmentKind::Line { .. } => walk(&f.children, here, first),
                FragmentKind::Box(b) if b.flags & fragment_flags::INLINE_BOX != 0 => {
                    if b.style.has_clip_path() {
                        match first.get(&b.order) {
                            None => {
                                first.insert(b.order, ReferenceBoxes::of(f, b, here));
                            }
                            Some(r) => b.clip_reference.set(r.translate(-here.x, -here.y)),
                        }
                    }
                    walk(&f.children, here, first);
                }
                _ => {}
            }
        }
    }
    walk(lines, Point::ZERO, &mut HashMap::new());
}

#[allow(clippy::too_many_arguments)]
fn layout_lines<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    content: &InlineContent<S>,
    content_width: f32,
    cb_height: Option<f32>,
    bfc: &mut Exclusions,
    origin: Point,
    marker: Option<BoxId>,
    trim: TrimRequest,
) -> InlineOutput<S> {
    let style = cx.tree.b(id).style.clone();
    let rtl = style.direction() == Direction::Rtl;
    let items = &content.items;
    let n = items.len();

    let mirrored = line_view(style.view()) != style.view();
    // Lay out atomic inlines (shrink-to-fit against the containing block).
    let mut atomics: HashMap<usize, LayoutResult<S>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.kind == ItemKind::Atomic {
            let space = Space::shrink(content_width, cb_height, content_width);
            let mut r = cx.layout(it.box_id, &space, None, Point::ZERO);
            if mirrored {
                std::mem::swap(&mut r.margin.top, &mut r.margin.bottom);
            }
            atomics.insert(i, r);
        }
    }
    let marker_result = marker.map(|m| {
        let space = Space::shrink(content_width, None, f32::INFINITY);
        cx.layout(m, &space, None, Point::ZERO)
    });
    let m = Measure {
        items,
        atomics: &atomics,
        cb_width: content_width,
        mirrored,
        central: central_baseline(&style),
    };
    let strut_height = line_height(&style, &content.root_font);
    let indent = style.text_indent().resolve(content_width);
    // CSS Text 3 §7.1: the first formatted line is indented, or, with `each-line`, also every line
    // after a forced break; `hanging` inverts which lines are.
    let (hanging, each_line) = (style.text_indent_hanging(), style.text_indent_each_line());
    let mut indented = !hanging;

    let mut out = InlineOutput {
        children: Vec::new(),
        height: 0.0,
        first_baseline: None,
        last_baseline: None,
        has_content: false,
        oof: Vec::new(),
        start_trim: 0.0,
    };
    let mut y = 0.0f32;
    let mut pos = Pos::default();
    let mut open_stack: Vec<usize> = Vec::new();
    let mut first_line = true;
    let mut deferred_floats: Vec<usize> = Vec::new();
    // Floats already placed or deferred (a line retried lower down meets them again).
    let mut floats_done = vec![false; n];
    let left_edge = origin.x;
    let right_edge = origin.x + content_width;
    // `text-wrap: balance` and `pretty` choose breaks for lines of one width: not beside floats,
    // nor in a line-clamp container (whose last line keeps room for its ellipsis).
    let wrap_style = match style.text_wrap_style() {
        s @ (TextWrapStyle::Balance | TextWrapStyle::Pretty)
            if cx.clamp.is_none()
                && content_width.is_finite()
                && !items.iter().any(|it| it.kind == ItemKind::Float)
                && bfc
                    .band(origin.y, strut_height, left_edge, right_edge)
                    .width()
                    >= content_width
                && bfc.next_edge_below(origin.y).is_none() =>
        {
            s
        }
        // (`stable` breaks as `auto` does: lines are laid out one after another anyway)
        _ => TextWrapStyle::Auto,
    };
    // `balance` and `pretty` take a paragraph (the lines up to a forced break) at a time: those
    // from `paragraph_next` on are not looked at yet. A balanced paragraph's lines break
    // `balance_shrink` narrower; `planned` holds the ends `pretty` chose for the lines from
    // `planned_from` on, the next one last.
    let mut paragraph_next = Pos::default();
    let mut balance_shrink = 0.0f32;
    let mut planned: Vec<Pos> = Vec::new();
    let mut planned_from = Pos::default();

    let place_float = |cx: &mut Cx<'_, S>,
                       bfc: &mut Exclusions,
                       out: &mut InlineOutput<S>,
                       item: usize,
                       top: f32| {
        let (p, frag, f_oof) = layout_float(
            cx,
            items[item].box_id,
            content_width,
            cb_height,
            rtl,
            bfc,
            left_edge,
            right_edge,
            origin.y + top,
        );
        let off = Point::new(p.x - origin.x, p.y - origin.y);
        for o in f_oof {
            out.oof.push(o.shifted(off.x, off.y));
        }
        out.children.push(FragmentChild {
            offset: off,
            fragment: frag,
            stacking_owner: 0,
        });
    };

    // An empty IFC with an outside marker still makes one line for the marker.
    let mut marker_pending = marker_result.is_some();
    while pos.item < n || marker_pending {
        // Past a line-clamp container's clamp point (it can be before all content), only
        // out-of-flow boxes get their candidates.
        if cx.clamp.as_ref().is_some_and(|c| c.reached) {
            for it in &items[pos.item.min(n)..] {
                if it.kind == ItemKind::OutOfFlow {
                    let c = cx.oof_candidate(it.box_id, StaticPos::at(0.0), StaticPos::at(y));
                    out.oof.push(c);
                }
            }
            deferred_floats.clear();
            break;
        }
        // Floats deferred from the previous line go at this line's top.
        for f in std::mem::take(&mut deferred_floats) {
            place_float(cx, bfc, &mut out, f, y);
        }
        let mut band = bfc.band(origin.y + y, strut_height, left_edge, right_edge);
        let mut left = band.left - origin.x;
        let mut avail = band.width();
        if indented {
            if rtl {
                avail -= indent;
            } else {
                left += indent;
                avail -= indent;
            }
        }
        // The line before a clamp point keeps room for the block overflow ellipsis: it ends at
        // the last soft wrap opportunity that lets the ellipsis fit (CSS Overflow 4 §4.3).
        let ellipsis = cx
            .clamp
            .as_ref()
            .and_then(|c| c.ellipsis_for_next_line())
            .map(str::to_string);
        let ellipsis_width = match ellipsis.as_deref() {
            Some(e) if !e.is_empty() => shape_ellipsis(cx, &content.root_font, e, rtl).width(),
            _ => 0.0,
        };
        if wrap_style != TextWrapStyle::Auto && planned.is_empty() && pos >= paragraph_next {
            // (the paragraph's lines after its first are not indented, unless `hanging`)
            let later = content_width - if hanging { indent } else { 0.0 };
            if wrap_style == TextWrapStyle::Balance {
                (paragraph_next, balance_shrink) =
                    balance_paragraph(cx, &m, pos, avail, later, rtl);
            } else {
                let (end, plan) = pretty_breaks(cx, &m, pos, avail, later, rtl);
                paragraph_next = end;
                if let Some((from, mut ends)) = plan {
                    ends.reverse();
                    planned = ends;
                    planned_from = from;
                }
            }
        }
        // Decide the line.
        let start = pos;
        let stop = planned.last().copied().filter(|_| pos >= planned_from);
        if stop.is_some() {
            planned.pop();
        }
        let decided = break_line(
            cx,
            &m,
            start,
            avail,
            if stop.is_some() {
                f32::NEG_INFINITY
            } else {
                ellipsis_width + balance_shrink
            },
            rtl,
            stop,
            None,
            &mut |cx, event| match event {
                LineEvent::Float { item, .. } if floats_done[item] => LineAnswer::Nothing,
                LineEvent::Float {
                    item,
                    room,
                    empty,
                    avail,
                } => {
                    floats_done[item] = true;
                    let r = cx.layout(
                        items[item].box_id,
                        &Space::shrink(content_width, cb_height, content_width),
                        None,
                        Point::ZERO,
                    );
                    let fw = r.fragment.size.width + r.margin.left + r.margin.right;
                    if room + fw <= avail + 0.01 || empty {
                        place_float(cx, bfc, &mut out, item, y);
                        band = bfc.band(origin.y + y, strut_height, left_edge, right_edge);
                        left = band.left - origin.x + if indented && !rtl { indent } else { 0.0 };
                        LineAnswer::Avail(band.width() - if indented { indent } else { 0.0 })
                    } else {
                        deferred_floats.push(item);
                        LineAnswer::Nothing
                    }
                }
                // Not even one word fits beside floats: move below them (CSS 2 §9.5).
                LineEvent::NoFit if band.width() < content_width => {
                    // (the new position must be strictly lower, despite rounding between
                    // coordinate systems)
                    let below = bfc
                        .next_edge_below(origin.y + y)
                        .map(|next| next - origin.y)
                        .filter(|ny| *ny > y + 0.001);
                    match below {
                        Some(ny) => {
                            y = ny;
                            LineAnswer::MovedDown
                        }
                        None => LineAnswer::Nothing,
                    }
                }
                LineEvent::NoFit => LineAnswer::Nothing,
            },
        );
        if decided.moved_down {
            // Retry the line lower down.
            continue;
        }
        let LineEnd {
            mut end,
            forced,
            has_content,
            avail,
            ..
        } = decided;
        let last = forced || end.item >= n;
        // Phantom lines (no content, nothing with inline mbp, no forced break from preserved text).
        let mut real = has_content || marker_pending;
        if forced && !has_content {
            // A line ended by <br> or a preserved newline is not phantom.
            real = true;
        }
        let spec = LineSpec {
            start,
            end,
            last,
            left,
            avail,
            ellipsis: ellipsis.filter(|e| !e.is_empty()),
        };
        let line_marker = if marker_pending {
            marker_pending = false;
            marker_result.as_ref()
        } else {
            None
        };
        // text-box-trim: the first formatted line's over side and the last one's under side (CSS
        // Inline 3 §6.1); the last is the one no other line with content follows.
        let edge = style.text_box_edge();
        let line_trim = (
            if real && first_line && trim.start {
                Some(edge.over)
            } else {
                None
            },
            if real && trim.end && !content_after(&m, end) {
                Some(edge.under)
            } else {
                None
            },
        );
        // (in the mirrored line of `vertical-lr` the block-start side is the line-under one)
        let line_trim = if mirrored {
            (
                if real && trim.end && !content_after(&m, end) {
                    Some(edge.over)
                } else {
                    None
                },
                if real && first_line && trim.start {
                    Some(edge.under)
                } else {
                    None
                },
            )
        } else {
            line_trim
        };
        let built = build_line(
            cx,
            &style,
            content,
            &m,
            &spec,
            &mut open_stack,
            real,
            line_trim,
            content_width,
            cb_height,
            line_marker,
            y,
            &mut out.oof,
        );
        if real {
            let line_top = y;
            if first_line {
                out.start_trim = built.trimmed;
            }
            if out.first_baseline.is_none() {
                out.first_baseline = Some(line_top + built.baseline);
            }
            out.last_baseline = Some(line_top + built.baseline);
            out.has_content = true;
            out.children.push(FragmentChild {
                offset: Point::new(spec.left, line_top),
                fragment: built.fragment,
                stacking_owner: 0,
            });
            y += built.height;
            first_line = false;
            indented = (each_line && forced) != hanging;
            if let Some(c) = cx.clamp.as_mut() {
                if c.point(true, origin.y + y) {
                    // The clamp point: what follows is not laid out, but out-of-flow boxes still
                    // get their candidates (their containing block may come before the clamp
                    // point or contain it).
                    for it in &items[end.item.min(n)..] {
                        if it.kind == ItemKind::OutOfFlow {
                            let c = cx.oof_candidate(
                                it.box_id,
                                StaticPos::at(spec.left),
                                StaticPos::at(y),
                            );
                            out.oof.push(c);
                        }
                    }
                    deferred_floats.clear();
                    break;
                }
            }
            // A <br> with `clear` (HTML's `clear` attribute maps to it, HTML §15.3.10): the next
            // line goes below the floats it clears, as for a block (CSS 2 §9.5.2).
            if forced {
                let br = (start.item..end.item.min(n))
                    .rev()
                    .find(|&i| items[i].kind == ItemKind::Break)
                    .filter(|&i| cx.tree.b(items[i].box_id).kind == BoxKind::LineBreak);
                if let Some(side) = br.and_then(|i| crate::block::clear_side(&items[i].style)) {
                    if let Some(cy) = bfc.clearance_y(side) {
                        y = y.max(cy - origin.y);
                    }
                }
            }
        } else if !built.fragment.children.is_empty() {
            out.children.push(FragmentChild {
                offset: Point::new(spec.left, y),
                fragment: built.fragment,
                stacking_owner: 0,
            });
        }
        if end == pos && !real {
            // No progress (only possible with zero units): stop.
            if end.item >= n {
                break;
            }
            end = Pos {
                item: end.item + 1,
                cluster: 0,
            };
        }
        pos = end;
    }
    for f in deferred_floats {
        place_float(cx, bfc, &mut out, f, y);
    }
    out.height = y;
    out
}

/// What deciding a line asks of the caller.
enum LineEvent {
    /// A float met on the line: `room` is the line's width so far without its hanging end,
    /// `empty` whether the line has nothing yet, `avail` its available width.
    Float {
        item: usize,
        room: f32,
        empty: bool,
        avail: f32,
    },
    /// Not even one unit fits on the line.
    NoFit,
}

enum LineAnswer {
    Nothing,
    /// The float went beside the line, which now has this available width.
    Avail(f32),
    /// The line moves below floats: decide it again there.
    MovedDown,
}

/// A decided line.
struct LineEnd {
    end: Pos,
    /// Ended by a forced break.
    forced: bool,
    has_content: bool,
    /// The available width, after floats placed on the line.
    avail: f32,
    moved_down: bool,
    /// The line breaks inside a word (`overflow-wrap`) or overflows: no soft wrap opportunity
    /// let it fit.
    emergency: bool,
}

/// Decides the line that starts at `start`: greedy over units, it ends at the last soft wrap
/// opportunity before the content overflows `avail - reserve`, or after the first unit that
/// overflows when there is none (CSS Text 3 §5); or at `stop`, a soft wrap opportunity chosen
/// before. `breaks` gets every place the line could end without overflowing, with the width of
/// the line ending there (hanging spaces left out, the hyphen of a soft hyphen in).
#[allow(clippy::too_many_arguments)]
fn break_line<'a, S: Style>(
    cx: &mut Cx<'a, S>,
    m: &Measure<'_, S>,
    start: Pos,
    mut avail: f32,
    reserve: f32,
    rtl: bool,
    stop: Option<Pos>,
    mut breaks: Option<&mut Vec<(Pos, f32)>>,
    events: &mut dyn FnMut(&mut Cx<'a, S>, LineEvent) -> LineAnswer,
) -> LineEnd {
    let items = m.items;
    let n = items.len();
    let mut p = start;
    let mut width = 0.0f32;
    let mut trailing = 0.0f32;
    let mut has_content = false;
    let mut last_break: Option<Pos> = None;
    let mut overflowing = false;
    let mut end: Option<Pos> = None;
    let mut forced = false;
    let mut emergency = false;
    let mut at_line_start = true;
    while p.item < n {
        let it = &items[p.item];
        match it.kind {
            ItemKind::Break => {
                p = Pos {
                    item: p.item + 1,
                    cluster: 0,
                };
                has_content = true;
                forced = true;
                end = Some(p);
                if let Some(b) = breaks.as_deref_mut().filter(|_| !overflowing) {
                    b.push((p, width - trailing));
                }
                break;
            }
            ItemKind::Float => {
                let event = LineEvent::Float {
                    item: p.item,
                    room: width - trailing,
                    empty: !has_content && width == 0.0,
                    avail,
                };
                if let LineAnswer::Avail(a) = events(cx, event) {
                    avail = a;
                }
                p = Pos {
                    item: p.item + 1,
                    cluster: 0,
                };
                continue;
            }
            ItemKind::OutOfFlow => {
                p = Pos {
                    item: p.item + 1,
                    cluster: 0,
                };
                continue;
            }
            _ => {}
        }
        let units = m.units(p.item);
        if units == 0 {
            p = Pos {
                item: p.item + 1,
                cluster: 0,
            };
            continue;
        }
        // Phase II: collapsible spaces at the line start go away.
        if at_line_start
            && it.kind == ItemKind::Text
            && it.collapsible
            && m.is_space(p.item, p.cluster)
        {
            p = advance(m, p);
            continue;
        }
        // The clusters inside a word, in one go: what the steps below do for each of them comes
        // down to adding its width (see `word_run`).
        if it.kind == ItemKind::Text {
            let c = word_run(
                it,
                p.cluster,
                avail - reserve + 0.01,
                overflowing,
                &mut width,
                &mut has_content,
            );
            if c > p.cluster {
                trailing = 0.0;
                at_line_start = false;
                p.cluster = c;
                continue;
            }
        }
        let w = m.unit_width(p, width);
        let space = it.kind == ItemKind::Text && m.is_space(p.item, p.cluster);
        let hangs = m.hangs(p.item, p.cluster);
        if !matches!(it.kind, ItemKind::Open | ItemKind::Close) {
            at_line_start = false;
        }
        let content_unit = match it.kind {
            ItemKind::Text => !(space && it.collapsible),
            // CSS Inline 3 §2.4: non-zero inline-axis margins (negative ones too), borders
            // or padding keep the line from being a phantom line box.
            ItemKind::Open | ItemKind::Close => w != 0.0 || has_inline_edge(it, m.cb_width),
            _ => true,
        };
        // Would this unit overflow?
        let new_width = width + w;
        let fit_width = if hangs { width - trailing } else { new_width };
        if fit_width > avail - reserve + 0.01 && has_content && !overflowing {
            if let Some(bp) = last_break {
                end = Some(bp);
                break;
            }
            // overflow-wrap: break inside the word.
            if (it.anywhere || it.break_word) && it.kind == ItemKind::Text && it.wrap {
                end = Some(p);
                emergency = true;
                break;
            }
            if let LineAnswer::MovedDown = events(cx, LineEvent::NoFit) {
                return LineEnd {
                    end: start,
                    forced: false,
                    has_content: false,
                    avail,
                    moved_down: true,
                    emergency: false,
                };
            }
            overflowing = true;
            emergency = true;
        }
        width = new_width;
        if hangs {
            trailing += w;
        } else {
            trailing = 0.0;
        }
        if content_unit {
            has_content = true;
        }
        let next = advance(m, p);
        if m.break_after(p) {
            // A break at a soft hyphen needs room for the hyphen it adds.
            let hyphen = if m.hyphenates_after(p) {
                shape_hyphen(cx, it, rtl).map_or(0.0, |h| h.width())
            } else {
                0.0
            };
            if hyphen == 0.0 || width + hyphen <= avail - reserve + 0.01 {
                last_break = Some(next);
                if let Some(b) = breaks.as_deref_mut().filter(|_| !overflowing) {
                    b.push((next, width - trailing + hyphen));
                }
            }
            // (an end chosen before is a soft wrap opportunity, past the inline box ends after it)
            if overflowing || stop.is_some_and(|s| past_closes(m, next) == s) {
                end = Some(next);
                break;
            }
        }
        p = next;
    }
    if end.is_none() && !overflowing {
        if let Some(b) = breaks {
            if b.last().map(|l| l.0) != Some(p) {
                b.push((p, width - trailing));
            }
        }
    }
    // Close tags right after the break stay on this line.
    let end = past_closes(m, end.unwrap_or(p));
    LineEnd {
        end,
        forced,
        has_content,
        avail,
        moved_down: false,
        emergency,
    }
}

/// [`break_line`]'s steps over the clusters of text item `it` from cluster `c` that are not its last,
/// not white space, and have no soft wrap opportunity after them ([`Measure::break_after`]): such a
/// cluster neither hangs nor ends a line, so each only adds its width to `width` (in the same order of
/// additions) and makes the line have content, unless it would overflow `limit` (a line with content,
/// not yet overflowing), which the per-cluster steps then handle. Returns the cluster it stopped at.
/// The line breaker so measures text a word at a time, between soft wrap opportunities.
fn word_run<S: Style>(
    it: &Item<S>,
    mut c: usize,
    limit: f32,
    overflowing: bool,
    width: &mut f32,
    has_content: &mut bool,
) -> usize {
    let Some(s) = word_clusters(it) else {
        return c;
    };
    let last = s.clusters.len().saturating_sub(1);
    let mut w = *width;
    let mut has = *has_content;
    while c < last && inside_word(it, s, c) {
        let cl = &s.clusters[c];
        let next = w + (cl.advance + it.letter_spacing);
        if next > limit && has && !overflowing {
            break;
        }
        w = next;
        has = true;
        c += 1;
    }
    *width = w;
    *has_content = has;
    c
}

/// The shaped text of text item `it` when [`inside_word`] decides its clusters: each cluster's width
/// is its advance plus the letter spacing (`combine` has none), and whether a soft wrap opportunity
/// follows a cluster comes from the cluster alone (`anywhere` and `break-all` put one after every
/// letter, and `keep-all` decides from the next character).
fn word_clusters<S: Style>(it: &Item<S>) -> Option<&ShapedText> {
    if it.combine
        || it.wrap && (it.line_break == LineBreak::Anywhere || it.break_all || it.keep_all)
    {
        return None;
    }
    it.shaped.as_deref()
}

/// Whether cluster `c` of `it` (not its last; `s` from [`word_clusters`]) is inside a word: not
/// white space (so it neither collapses nor hangs), with no soft wrap opportunity after it, as
/// [`Measure::break_after`] decides.
#[inline]
fn inside_word<S: Style>(it: &Item<S>, s: &ShapedText, c: usize) -> bool {
    let cl = &s.clusters[c];
    if cl.whitespace {
        return false;
    }
    if !it.wrap {
        return true;
    }
    if cl.soft_hyphen || cl.break_after != BreakAfter::Never {
        return false;
    }
    // `line-break: normal | loose` allows breaks before some characters outside ASCII.
    !(matches!(it.line_break, LineBreak::Normal | LineBreak::Loose)
        && it
            .text
            .as_bytes()
            .get(s.clusters[c + 1].text.0 as usize)
            .is_some_and(|&b| b >= 0x80))
}

/// The most lines `text-wrap: balance` balances; longer paragraphs wrap as `auto` (CSS Text 4
/// §6.2 leaves the limit to the UA).
const MAX_LINES_TO_BALANCE: usize = 6;

/// `text-wrap: balance` (CSS Text 4 §6.2) for the paragraph that starts at `start` (its lines run
/// to the next forced break): returns where the paragraph ends and how much narrower than the
/// available width its lines break, so that it takes as many lines as it would otherwise with the
/// least width each, which evens out the space left at their ends. The line boxes keep their width
/// (alignment uses it). Zero when the paragraph takes one line or more than
/// [`MAX_LINES_TO_BALANCE`]. `first` is the available width of its first line, `later` that of
/// the others.
fn balance_paragraph<'a, S: Style>(
    cx: &mut Cx<'a, S>,
    m: &Measure<'_, S>,
    start: Pos,
    first: f32,
    later: f32,
    rtl: bool,
) -> (Pos, f32) {
    let n = m.items.len();
    // The paragraph's end, its line count (`None` past `max` lines) and how many of its lines
    // break inside a word or overflow.
    let count = |cx: &mut Cx<'a, S>, shrink: f32, max: usize| -> (Pos, Option<usize>, usize) {
        let mut pos = start;
        let mut lines = 0;
        let mut emergencies = 0;
        loop {
            let avail = if lines == 0 { first } else { later };
            let line = break_line(cx, m, pos, avail, shrink, rtl, None, None, &mut |_, _| {
                LineAnswer::Nothing
            });
            if line.has_content {
                lines += 1;
            }
            emergencies += usize::from(line.emergency);
            pos = if line.end == pos {
                Pos {
                    item: pos.item + 1,
                    cluster: 0,
                }
            } else {
                line.end
            };
            if line.forced || pos.item >= n {
                return (pos, Some(lines), emergencies);
            }
            if lines > max {
                return (pos, None, emergencies);
            }
        }
    };
    let (end, lines, emergencies) = count(cx, 0.0, MAX_LINES_TO_BALANCE);
    let lines = match lines {
        Some(l) if (2..=MAX_LINES_TO_BALANCE).contains(&l) => l,
        // (past the limit: the paragraph ends after its forced break, or with the content)
        None => {
            let end = (start.item..n)
                .find(|&i| m.items[i].kind == ItemKind::Break)
                .map_or(n, |i| i + 1);
            return (
                past_closes(
                    m,
                    Pos {
                        item: end,
                        cluster: 0,
                    },
                ),
                0.0,
            );
        }
        Some(_) => return (end, 0.0),
    };
    // The least narrowing (to a pixel) at which the paragraph takes more lines, or breaks inside
    // more words: the counts only grow as lines narrow.
    let (mut fits, mut too_much) = (0.0f32, first.max(later));
    while too_much - fits > 1.0 {
        let middle = (fits + too_much) / 2.0;
        let (_, l, e) = count(cx, middle, lines);
        if l.is_some_and(|l| l <= lines) && e <= emergencies {
            fits = middle;
        } else {
            too_much = middle;
        }
    }
    (end, fits)
}

/// `p`, or past the ends of inline boxes right after it: they stay on the line that ends at `p`.
fn past_closes<S: Style>(m: &Measure<'_, S>, mut p: Pos) -> Pos {
    while p.item < m.items.len() && m.items[p.item].kind == ItemKind::Close && p.cluster == 0 {
        p.item += 1;
    }
    p
}

/// The most lines at a paragraph's end that `text-wrap: pretty` chooses breaks for.
const MAX_LINES_PRETTY: usize = 4;

/// What a last line of one short word costs a `pretty` choice of breaks, against the squares of
/// the space left on the other lines.
const ORPHAN_PENALTY: f32 = 10_000.0;

/// `text-wrap: pretty` (CSS Text 4 §6.2) for the paragraph that starts at `start` (its lines run
/// to the next forced break): returns where the paragraph ends and, when its last line is a single
/// word shorter than a third of the line and the paragraph has more than one line, where its last
/// lines (up to [`MAX_LINES_PRETTY`]) should end instead. Those breaks keep the line count and
/// minimize the sum of the squares of the space left at the ends of all but the last line, plus a
/// penalty for leaving the last word alone again. `first` is the available width of the
/// paragraph's first line, `later` that of the others.
fn pretty_breaks<'a, S: Style>(
    cx: &mut Cx<'a, S>,
    m: &Measure<'_, S>,
    start: Pos,
    first: f32,
    later: f32,
    rtl: bool,
) -> (Pos, Option<(Pos, Vec<Pos>)>) {
    let n = m.items.len();
    let mut nothing = |_: &mut Cx<'a, S>, _: LineEvent| LineAnswer::Nothing;
    // The greedy lines: their starts and ends.
    let mut lines: Vec<(Pos, Pos)> = Vec::new();
    let mut pos = start;
    let mut phantom = false;
    let para_end = loop {
        let avail = if lines.is_empty() { first } else { later };
        let line = break_line(cx, m, pos, avail, 0.0, rtl, None, None, &mut nothing);
        let end = if line.end == pos {
            Pos {
                item: pos.item + 1,
                cluster: 0,
            }
        } else {
            line.end
        };
        if line.has_content {
            lines.push((pos, end));
        } else if !(line.forced || end.item >= n) {
            phantom = true;
        }
        pos = end;
        if line.forced || pos.item >= n {
            break pos;
        }
    };
    let count = lines.len();
    if phantom || count < 2 {
        return (para_end, None);
    }
    let avail_at = |at: Pos| if at == start { first } else { later };
    // Does the last line hold a single word shorter than a third of the line?
    let last_start = lines[count - 1].0;
    let mut last = Vec::new();
    break_line(
        cx,
        m,
        last_start,
        avail_at(last_start),
        0.0,
        rtl,
        None,
        Some(&mut last),
        &mut nothing,
    );
    let internal = last
        .iter()
        .any(|&(end, _)| end < para_end && content_after(m, end));
    match last.iter().find(|e| e.0 == para_end) {
        Some(&(_, w)) if !internal && w < avail_at(last_start) / 3.0 => {}
        _ => return (para_end, None),
    }
    // Choose the breaks of the last `k` lines: the cheapest ways to reach each candidate break
    // with one line more, layer by layer.
    let k = count.min(MAX_LINES_PRETTY);
    let from = lines[count - k].0;
    let mut layer: Vec<(Pos, f32, usize)> = vec![(from, 0.0, 0)];
    let mut layers: Vec<Vec<(Pos, f32, usize)>> = Vec::new();
    for line in 0..k {
        let final_line = line + 1 == k;
        let mut next: Vec<(Pos, f32, usize)> = Vec::new();
        for (index, &(at, cost, _)) in layer.iter().enumerate() {
            let avail = avail_at(at);
            let mut ends = Vec::new();
            break_line(
                cx,
                m,
                at,
                avail,
                0.0,
                rtl,
                None,
                Some(&mut ends),
                &mut nothing,
            );
            for (end, width) in ends {
                let end = past_closes(m, end);
                if (end == para_end) != final_line || (!final_line && !content_after(m, end)) {
                    continue;
                }
                let cost = cost
                    + if final_line {
                        0.0
                    } else {
                        (avail - width).powi(2)
                            + if end == last_start {
                                ORPHAN_PENALTY
                            } else {
                                0.0
                            }
                    };
                match next.iter_mut().find(|e| e.0 == end) {
                    Some(e) if e.1 <= cost => {}
                    Some(e) => *e = (end, cost, index),
                    None => next.push((end, cost, index)),
                }
            }
        }
        if next.is_empty() {
            return (para_end, None);
        }
        layers.push(std::mem::replace(&mut layer, next));
    }
    // (the last layer holds the paragraph's end alone)
    let mut ends = Vec::with_capacity(k);
    let (mut at, _, mut index) = layer[0];
    for l in layers.iter().rev() {
        ends.push(at);
        (at, _, index) = l[index];
    }
    ends.reverse();
    // The last line ends where it did; the other ends move to where the choice put them.
    let greedy: Vec<Pos> = lines[count - k..].iter().map(|l| l.1).collect();
    if ends[..k - 1] != greedy[..k - 1] {
        (para_end, Some((from, ends)))
    } else {
        (para_end, None)
    }
}

/// How far above the baseline a font's `text-box-edge` over metric is (CSS Inline 3 §6.2; the
/// ideographic baselines are taken as the text ones, which is what they are synthesized from when
/// the font has no BASE table).
fn over_edge(font: &FontInfo, metric: TextEdgeMetric) -> f32 {
    match metric {
        TextEdgeMetric::Cap => layout_unit(font.metrics.cap_height - font.alphabetic),
        TextEdgeMetric::Ex => layout_unit(font.metrics.x_height - font.alphabetic),
        _ => font.metrics.ascent,
    }
}

/// How far below the baseline a font's `text-box-edge` under metric is.
fn under_edge(font: &FontInfo, metric: TextEdgeMetric) -> f32 {
    match metric {
        TextEdgeMetric::Alphabetic => font.alphabetic,
        _ => font.metrics.descent,
    }
}

/// Whether anything from `p` on would make a line that is not phantom (CSS Inline 3 §2.4): text other
/// than collapsible spaces, an atomic inline, a tab, a forced break, an inline box edge with margin,
/// border or padding.
fn content_after<S: Style>(m: &Measure<'_, S>, p: Pos) -> bool {
    (p.item..m.items.len()).any(|i| {
        let it = &m.items[i];
        match it.kind {
            ItemKind::Text => {
                let from = if i == p.item { p.cluster } else { 0 };
                (from..m.clusters(i)).any(|c| !(it.collapsible && m.is_space(i, c)))
            }
            ItemKind::Atomic | ItemKind::Tab | ItemKind::Break => true,
            ItemKind::Open | ItemKind::Close => has_inline_edge(it, m.cb_width),
            ItemKind::Float | ItemKind::OutOfFlow => false,
        }
    })
}

fn advance<S: Style>(m: &Measure<'_, S>, p: Pos) -> Pos {
    if m.items[p.item].kind == ItemKind::Text && p.cluster + 1 < m.clusters(p.item) {
        Pos {
            item: p.item,
            cluster: p.cluster + 1,
        }
    } else {
        Pos {
            item: p.item + 1,
            cluster: 0,
        }
    }
}

// ----- line boxes -----

/// A node of a line's inline tree.
enum LNode {
    Box {
        item: usize,
        starts: bool,
        ends: bool,
        children: Vec<LNode>,
        width: f32,
        level: u8,
    },
    Text {
        item: usize,
        run: usize,
        clusters: Range<usize>,
        width: f32,
        spaces: usize,
        level: u8,
    },
    Atomic {
        item: usize,
        width: f32,
        level: u8,
    },
    Tab {
        width: f32,
        level: u8,
    },
    /// A forced line break (`<br>`, a preserved newline): no width, but in quirks mode it can give
    /// its box a strut, and it has a position.
    Break {
        item: usize,
        level: u8,
    },
    /// The ellipsis (`text-overflow`), or, with the item whose style it takes, the hyphen a line
    /// ends with at a soft hyphen (CSS Text 3 §6.1).
    Ellipsis {
        shaped: Arc<ShapedText>,
        width: f32,
        level: u8,
        /// The hyphen's text item and the soft hyphen's cluster (its shaped text range): the hyphen
        /// stands for that character in the DOM (ranges, selection and hit testing).
        item: Option<(usize, (u32, u32))>,
    },
}

impl LNode {
    fn width(&self) -> f32 {
        match self {
            LNode::Box { width, .. }
            | LNode::Text { width, .. }
            | LNode::Atomic { width, .. }
            | LNode::Tab { width, .. }
            | LNode::Ellipsis { width, .. } => *width,
            LNode::Break { .. } => 0.0,
        }
    }

    fn level(&self) -> u8 {
        match self {
            LNode::Box { level, .. }
            | LNode::Text { level, .. }
            | LNode::Atomic { level, .. }
            | LNode::Tab { level, .. }
            | LNode::Break { level, .. }
            | LNode::Ellipsis { level, .. } => *level,
        }
    }
}

struct BuiltLine<S: Style> {
    fragment: Arc<Fragment<S>>,
    height: f32,
    baseline: f32,
    /// What `text-box-trim` took off its top.
    trimmed: f32,
}

/// Vertical metrics of an inline box on a line: its baseline offset from the root baseline (positive
/// down) and the top/bottom of its layout bounds relative to the root baseline.
#[derive(Clone, Copy, Default)]
struct VBox {
    baseline: f32,
    top: f32,
    bottom: f32,
}

/// The last unit of the line from `start` to `end` that stays at its end: a cluster of text other
/// than a collapsible space or other space separator (those hang and are dropped, CSS Text 3 §4.1.2,
/// phase II), an atomic inline or a tab. Found from the end, past the trailing spaces only.
fn last_line_unit<S: Style>(m: &Measure<'_, S>, start: Pos, end: Pos) -> Option<Pos> {
    let items = m.items;
    if items.is_empty() || end <= start {
        return None;
    }
    let top = end.item.min(items.len() - 1);
    for i in (start.item..=top).rev() {
        let it = &items[i];
        let from = if i == start.item { start.cluster } else { 0 };
        match it.kind {
            ItemKind::Text => {
                let to = if i == end.item {
                    end.cluster.min(m.clusters(i))
                } else {
                    m.clusters(i)
                };
                for c in (from..to).rev() {
                    let trailing_space =
                        it.collapsible && (m.is_space(i, c) || is_other_space_separator(it, c));
                    if !trailing_space {
                        return Some(Pos {
                            item: i,
                            cluster: c,
                        });
                    }
                }
            }
            ItemKind::Atomic | ItemKind::Tab => {
                let p = Pos {
                    item: i,
                    cluster: from,
                };
                if p < end {
                    return Some(p);
                }
            }
            _ => {}
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn build_line<S: Style>(
    cx: &mut Cx<'_, S>,
    style: &S,
    content: &InlineContent<S>,
    m: &Measure<'_, S>,
    spec: &LineSpec,
    open_stack: &mut Vec<usize>,
    real: bool,
    (trim_start, trim_end): (Option<TextEdgeMetric>, Option<TextEdgeMetric>),
    content_width: f32,
    cb_height: Option<f32>,
    marker: Option<&LayoutResult<S>>,
    line_y: f32,
    oof: &mut Vec<Oof>,
) -> BuiltLine<S> {
    let items = m.items;
    let para_rtl = content.para_rtl(style, spec.start.item);
    let base_level = u8::from(para_rtl);
    // 1. The logical tree, with phase II trimming of trailing collapsible spaces.
    let last_unit = last_line_unit(m, spec.start, spec.end);
    let last_content = last_unit.map_or(spec.start, |u| advance(m, u));
    // A line that a soft hyphen ends shows the hyphenation character (CSS Text 3 §6.1).
    let hyphen_after = last_unit.filter(|&u| !spec.last && m.hyphenates_after(u));
    let mut stack: Vec<LNode> = Vec::new();
    for &open in open_stack.iter() {
        stack.push(LNode::Box {
            item: open,
            starts: false,
            ends: false,
            children: Vec::new(),
            width: 0.0,
            level: base_level,
        });
    }
    let mut root: Vec<LNode> = Vec::new();
    fn push<S2>(stack: &mut [LNode], root: &mut Vec<LNode>, node: LNode, _s: &S2) {
        match stack.last_mut() {
            Some(LNode::Box { children, .. }) => children.push(node),
            _ => root.push(node),
        }
    }
    let mut x = 0.0f32;
    let mut p = spec.start;
    let mut leading = true;
    let mut oof_here: Vec<(usize, f32)> = Vec::new();
    while p < spec.end && p.item < items.len() {
        let it = &items[p.item];
        match it.kind {
            ItemKind::Text => {
                let shaped = it.shaped.as_ref().unwrap();
                let nclusters = shaped.clusters.len();
                if nclusters == 0 {
                    p = Pos {
                        item: p.item + 1,
                        cluster: 0,
                    };
                    continue;
                }
                let end_c = if spec.end.item == p.item {
                    spec.end.cluster
                } else {
                    nclusters
                };
                let mut c0 = p.cluster;
                // leading collapsible spaces
                if leading && it.collapsible {
                    while c0 < end_c && is_space_cluster(it, c0) {
                        c0 += 1;
                    }
                }
                // trailing collapsible spaces at the line end
                let mut c1 = end_c;
                if it.collapsible {
                    if last_content.item == p.item {
                        c1 = c1.min(last_content.cluster.max(c0));
                    } else if last_content.item < p.item {
                        c1 = c0;
                    }
                }
                if c0 < c1 {
                    leading = false;
                    // split at run boundaries
                    let mut c = c0;
                    while c < c1 {
                        let run = shaped
                            .runs
                            .iter()
                            .position(|r| (r.clusters.0 as usize) <= c && c < r.clusters.1 as usize)
                            .unwrap_or(0);
                        let run_end = (shaped.runs[run].clusters.1 as usize).min(c1);
                        let mut w = 0.0;
                        let mut spaces = 0;
                        for k in c..run_end {
                            w += m.cluster_width(p.item, k);
                            if shaped.clusters[k].whitespace {
                                spaces += 1;
                            }
                        }
                        x += w;
                        let level = shaped.runs[run].bidi_level;
                        push(
                            &mut stack,
                            &mut root,
                            LNode::Text {
                                item: p.item,
                                run,
                                clusters: c..run_end,
                                width: w,
                                spaces,
                                level,
                            },
                            &(),
                        );
                        c = run_end;
                    }
                    if hyphen_after
                        == Some(Pos {
                            item: p.item,
                            cluster: c1 - 1,
                        })
                    {
                        // (at the bidi level of the text it ends)
                        let level = shaped
                            .runs
                            .iter()
                            .find(|r| (r.clusters.0 as usize) < c1 && c1 <= r.clusters.1 as usize)
                            .map_or(base_level, |r| r.bidi_level);
                        if let Some(hyphen) = shape_hyphen(cx, it, level % 2 == 1) {
                            let width = hyphen.width();
                            x += width;
                            push(
                                &mut stack,
                                &mut root,
                                LNode::Ellipsis {
                                    shaped: hyphen,
                                    width,
                                    level,
                                    item: Some((p.item, shaped.clusters[c1 - 1].text)),
                                },
                                &(),
                            );
                        }
                    }
                }
                p = Pos {
                    item: p.item + 1,
                    cluster: 0,
                };
                if end_c < nclusters {
                    break;
                }
                continue;
            }
            ItemKind::Open => {
                let w = m.unit_width(p, x);
                x += w;
                stack.push(LNode::Box {
                    item: p.item,
                    starts: true,
                    ends: false,
                    children: Vec::new(),
                    width: 0.0,
                    level: base_level,
                });
                open_stack.push(p.item);
            }
            ItemKind::Close => {
                let w = m.unit_width(p, x);
                x += w;
                // Close the matching box (the innermost open one).
                if let Some(mut node) = stack.pop() {
                    if let LNode::Box { ends, .. } = &mut node {
                        *ends = true;
                    }
                    push(&mut stack, &mut root, node, &());
                }
                open_stack.pop();
            }
            ItemKind::Atomic => {
                leading = false;
                let w = m.unit_width(p, x);
                x += w;
                push(
                    &mut stack,
                    &mut root,
                    LNode::Atomic {
                        item: p.item,
                        width: w,
                        level: it
                            .levels
                            .as_ref()
                            .and_then(|l| l.first().copied())
                            .unwrap_or(base_level),
                    },
                    &(),
                );
            }
            ItemKind::Tab => {
                leading = false;
                let w = m.unit_width(p, x);
                x += w;
                push(
                    &mut stack,
                    &mut root,
                    LNode::Tab {
                        width: w,
                        level: base_level,
                    },
                    &(),
                );
            }
            ItemKind::OutOfFlow => {
                oof_here.push((p.item, x));
            }
            ItemKind::Break => {
                push(
                    &mut stack,
                    &mut root,
                    LNode::Break {
                        item: p.item,
                        level: base_level,
                    },
                    &(),
                );
            }
            ItemKind::Float => {}
        }
        p = advance(m, p);
    }
    // Boxes still open continue on the next line.
    while let Some(node) = stack.pop() {
        push(&mut stack, &mut root, node, &());
    }

    // 2. Widths.
    fn measure_widths<S: Style>(nodes: &mut [LNode], m: &Measure<'_, S>) -> f32 {
        let mut total = 0.0;
        for n in nodes.iter_mut() {
            if let LNode::Box {
                item,
                starts,
                ends,
                children,
                width,
                level,
            } = n
            {
                let (s, e) = start_end_edges(&m.items[*item].style, m.cb_width);
                let it = &m.items[*item];
                let inner = measure_widths(children, m);
                *width = inner
                    + if *starts && it.has_start_edge { s } else { 0.0 }
                    + if *ends && it.has_end_edge { e } else { 0.0 };
                *level = match &it.levels {
                    Some(l) => l[0],
                    None => children.iter().map(|c| c.level()).min().unwrap_or(*level),
                };
            }
            total += n.width();
        }
        total
    }
    let mut total = measure_widths(&mut root, m);

    // text-overflow (CSS UI 4 §5.2) on a clipping container: an ellipsis or a string.
    let overflow_string = if style.overflow_x() != Overflow::Visible && total > spec.avail + 0.01 {
        style.text_overflow_string()
    } else {
        None
    };
    if let Some(text) = overflow_string {
        let shaped = shape_ellipsis(cx, &content.root_font, &text, para_rtl);
        let ew = shaped.width();
        let limit = (spec.avail - ew).max(0.0);
        truncate(&mut root, m, limit, &mut 0.0, &mut true);
        total = measure_widths(&mut root, m);
        root.push(LNode::Ellipsis {
            shaped,
            width: ew,
            level: base_level,
            item: None,
        });
        total += ew;
    }

    // The block overflow ellipsis (CSS Overflow 4 §4.3), after the line's content.
    if let Some(e) = &spec.ellipsis {
        let shaped = shape_ellipsis(cx, &content.root_font, e, para_rtl);
        let ew = shaped.width();
        root.push(LNode::Ellipsis {
            shaped,
            width: ew,
            level: base_level,
            item: None,
        });
        total += ew;
    }

    // 3. Justification and alignment.
    let align = effective_align(style, spec.last, para_rtl);
    let free = spec.avail - total;
    let mut justify = 0.0f32;
    if align == Align1::Justify && free > 0.0 {
        let spaces = count_spaces(&root);
        if spaces > 0 {
            justify = free / spaces as f32;
            total = spec.avail;
        }
    }
    let rtl = base_level == 1;
    let offset = if free <= 0.0 {
        if rtl {
            free
        } else {
            0.0
        }
    } else {
        match align {
            Align1::Left | Align1::Justify if !rtl => 0.0,
            Align1::Left => 0.0,
            Align1::Right => free,
            Align1::Center => free / 2.0,
            Align1::Justify => {
                if justify > 0.0 {
                    0.0
                } else {
                    free
                }
            }
        }
    };

    // 4. Vertical alignment.
    let root_font = content.root_font;
    let root_lh = line_height(style, &root_font);
    let root_vb = strut(&root_font, root_lh, 0.0);
    // The root inline box's strut (CSS 2 §10.8.1). In quirks mode the blocks-ignore-line-height
    // quirk drops it unless the root inline box itself holds text (Quirks Mode §3.3, §3.4), or the
    // line holds the list marker.
    let quirk = cx.tree.line_height_quirk;
    let root_contributes = !quirk || has_text(&root) || marker.is_some();
    let (mut top, mut bottom) = if real && root_contributes {
        (root_vb.top, root_vb.bottom)
    } else {
        // (empty: nothing has contributed yet)
        (f32::INFINITY, f32::NEG_INFINITY)
    };
    let mut deferred: Vec<(f32, f32, bool)> = Vec::new();
    let mut vinfo: Vec<VBox> = Vec::new();
    compute_vertical(
        cx.text,
        &root,
        m,
        &root_font,
        root_vb,
        quirk,
        root_contributes,
        &mut top,
        &mut bottom,
        &mut deferred,
        &mut vinfo,
    );
    if let Some(mr) = marker {
        let f = &mr.fragment;
        let mb = f
            .baseline
            .map_or(f.size.height, |b| m.line_y(b, f.size.height));
        top = top.min(-mb);
        bottom = bottom.max(f.size.height - mb);
    }
    if top > bottom {
        // Nothing on the line has layout bounds (a quirks-mode line of empty inline boxes).
        top = 0.0;
        bottom = 0.0;
    }
    // top/bottom aligned subtrees may enlarge the line.
    for (h, _, _) in &deferred {
        if bottom - top < *h {
            bottom = top + *h;
        }
    }
    if !real {
        // A phantom line box (CSS Inline 3 §2.4) is treated as zero-height: so are the inline
        // boxes on it, at its top.
        top = 0.0;
        bottom = 0.0;
    }
    let mut trimmed = 0.0;
    if let (true, Some(metric)) = (real, trim_start) {
        // text-box-trim: the first formatted line's top is its root inline box's over edge.
        let over = -over_edge(&root_font, metric);
        trimmed = over - top;
        top = over;
        bottom = bottom.max(top);
    }
    if let (true, Some(metric)) = (real, trim_end) {
        // ...and the last one's bottom its under edge.
        bottom = under_edge(&root_font, metric).max(top);
    }
    let line_height_used = (bottom - top).max(0.0);
    let baseline = -top;

    // 5. Fragments, in visual order.
    let mut children: Vec<FragmentChild<S>> = Vec::new();
    let mut atomic_oof: Vec<Oof> = Vec::new();
    let mut vi = 0usize;
    let mut cursor = offset;
    place_nodes(
        cx,
        &root,
        m,
        &mut vi,
        &vinfo,
        &mut cursor,
        baseline,
        line_height_used,
        !real,
        justify,
        &mut children,
        &mut atomic_oof,
        content_width,
        cb_height,
        Point::ZERO,
        line_end_leaf(&root, base_level),
    );
    // Out-of-flow descendants of atomic inlines whose containing block is further up, from the
    // line's coordinates to the formatting context's.
    let mirror_dy = if m.mirrored { line_height_used } else { 0.0 };
    for o in atomic_oof {
        oof.push(o.shifted(spec.left, line_y + mirror_dy));
    }
    if let Some(mr) = marker {
        let f = &mr.fragment;
        let mb = f
            .baseline
            .map_or(f.size.height, |b| m.line_y(b, f.size.height));
        let x = if rtl { spec.avail } else { -f.size.width };
        children.push(FragmentChild {
            offset: Point::new(x, baseline - mb),
            fragment: f.clone(),
            stacking_owner: 0,
        });
    }
    // Out-of-flow static positions: inline-level ones at their place on the line, block-level ones at
    // the start of the next line when this one has content before them (CSS 2 §10.3.7).
    for (item, x) in oof_here {
        let b = cx.tree.b(items[item].box_id);
        let originally_inline = b.has(flags::ORIGINALLY_INLINE);
        // (a block-level one starts at the content box's start edge, whatever the floats and
        // the indent of the line: CSS 2 §10.3.7's hypothetical box is a block)
        let (sx, sy) = if originally_inline {
            // (`x` counts from the line's start: its right end in a right-to-left paragraph)
            let at = if rtl { offset + total - x } else { offset + x };
            (StaticPos::at(spec.left + at), line_y)
        } else if x > 0.0 && real {
            (
                StaticPos::block_level(content_width),
                line_y + line_height_used,
            )
        } else {
            (StaticPos::block_level(content_width), line_y)
        };
        let c = cx.oof_candidate(items[item].box_id, sx, StaticPos::at(sy));
        oof.push(c);
    }
    let width = spec.avail.max(0.0);
    let baseline = if m.mirrored {
        crate::flow::map_line_children(&mut children, line_height_used, style.view());
        line_height_used - baseline
    } else {
        baseline
    };
    let mut overflow = Rect::new(0.0, 0.0, width, line_height_used);
    let mut bounds = overflow;
    for c in &children {
        add_overflow(&mut overflow, &mut bounds, c);
    }
    let fragment = Arc::new(Fragment {
        size: Size::new(width, line_height_used),
        kind: FragmentKind::Line { baseline },
        children,
        overflow,
        paint_bounds: bounds,
        // (the content's extent on the line, atomic inlines' margins included)
        inflow: real.then(|| Rect::new(offset, 0.0, total.max(0.0), line_height_used)),
        baseline: Some(baseline),
        last_baseline: Some(baseline),
    });
    BuiltLine {
        fragment,
        height: line_height_used,
        baseline,
        trimmed,
    }
}

/// An ellipsis string shaped in the root inline box's font, through the tree's text cache as the
/// line's own text is: a line that overflows its box asks for the same ellipsis at each of its
/// layouts (and the line breaking of a clamped block before that), each a DirectWrite analysis,
/// font fallback and shaping of the string otherwise.
/// The hyphenation character (`hyphenate-character`, CSS Text 4 §6.2) in a text item's font: for
/// `auto`, U+2010 HYPHEN when the item's first available font has it, else U+002D HYPHEN-MINUS.
fn shape_hyphen<S: Style>(cx: &mut Cx<'_, S>, it: &Item<S>, rtl: bool) -> Option<Arc<ShapedText>> {
    let font = it.font.as_ref()?;
    match it.style.hyphenate_character() {
        Some(s) if s.is_empty() => None,
        Some(s) => Some(shape_ellipsis(cx, font, &s, rtl)),
        None => {
            let hyphen = shape_ellipsis(cx, font, "\u{2010}", rtl);
            if hyphen.runs.first().is_some_and(|r| r.font == font.primary) {
                Some(hyphen)
            } else {
                Some(shape_ellipsis(cx, font, "-", rtl))
            }
        }
    }
}

fn shape_ellipsis<S: Style>(
    cx: &mut Cx<'_, S>,
    font: &FontInfo,
    text: &str,
    rtl: bool,
) -> Arc<ShapedText> {
    let options = ShapeOptions {
        size: font.size,
        // (its bidi runs order in the paragraph's direction)
        direction: if rtl { Direction::Rtl } else { Direction::Ltr },
        ..Default::default()
    };
    let key = ShapeKey {
        text,
        list: font.list,
        size: font.size,
        rtl,
        vertical: options.vertical,
        locale: options.locale,
        features: options.features,
    };
    let mut cache = cx.tree.text_cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = cache.runs.get(&key) {
        drop(cache);
        cx.stats.shape_cache_hits += 1;
        return s;
    }
    let mut shaped = ShapedText::default();
    let _ = cx.text.shape_cached(
        text,
        font.list,
        &options,
        Some(&mut cache.words),
        &mut shaped,
    );
    let shaped = Arc::new(shaped);
    cache.runs.insert(&key, shaped.clone());
    drop(cache);
    cx.stats.shaped += 1;
    shaped
}

/// Drops content past `limit` (logical order), cutting text at clusters. The line's first
/// character or atomic inline stays even when it does not fit (CSS UI 4 §5.2: it is clipped rather
/// than ellipsed); `first` is whether none was kept yet.
fn truncate<S: Style>(
    nodes: &mut Vec<LNode>,
    m: &Measure<'_, S>,
    limit: f32,
    x: &mut f32,
    first: &mut bool,
) -> bool {
    let mut cut_at = None;
    for (i, n) in nodes.iter_mut().enumerate() {
        match n {
            LNode::Box { children, .. } => {
                if truncate(children, m, limit, x, first) {
                    cut_at = Some(i + 1);
                    break;
                }
            }
            LNode::Text {
                item,
                clusters,
                width,
                spaces,
                ..
            } => {
                if *x + *width <= limit {
                    *x += *width;
                    if *width > 0.0 {
                        *first = false;
                    }
                    continue;
                }
                let mut w = 0.0;
                let mut end = clusters.start;
                let mut sp = 0;
                for k in clusters.clone() {
                    let cw = m.cluster_width(*item, k);
                    if *x + w + cw > limit && !*first {
                        break;
                    }
                    *first = false;
                    w += cw;
                    if m.is_space(*item, k) {
                        sp += 1;
                    }
                    end = k + 1;
                }
                clusters.end = end;
                *width = w;
                *spaces = sp;
                *x += w;
                // (nothing of this text fits: drop it)
                cut_at = Some(if end > clusters.start { i + 1 } else { i });
                break;
            }
            other => {
                if *x + other.width() > limit && !(*first && other.width() > 0.0) {
                    cut_at = Some(i);
                    break;
                }
                if other.width() > 0.0 {
                    *first = false;
                }
                *x += other.width();
            }
        }
    }
    if let Some(c) = cut_at {
        nodes.truncate(c);
        true
    } else {
        false
    }
}

fn count_spaces(nodes: &[LNode]) -> usize {
    nodes
        .iter()
        .map(|n| match n {
            LNode::Box { children, .. } => count_spaces(children),
            LNode::Text { spaces, .. } => *spaces,
            _ => 0,
        })
        .sum()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align1 {
    Left,
    Right,
    Center,
    Justify,
}

/// `text-align` / `text-align-last` for a line, physical.
fn effective_align<S: Style>(style: &S, last: bool, rtl: bool) -> Align1 {
    let start = if rtl { Align1::Right } else { Align1::Left };
    let end = if rtl { Align1::Left } else { Align1::Right };
    let from_align = |a: TextAlign| match a {
        TextAlign::Start | TextAlign::MatchParent => start,
        TextAlign::End => end,
        TextAlign::Left => Align1::Left,
        TextAlign::Right => Align1::Right,
        TextAlign::Center => Align1::Center,
        TextAlign::Justify => Align1::Justify,
    };
    if !last {
        return from_align(style.text_align());
    }
    match style.text_align_last() {
        TextAlignLast::Auto => match style.text_align() {
            TextAlign::Justify => start,
            a => from_align(a),
        },
        TextAlignLast::Start | TextAlignLast::MatchParent => start,
        TextAlignLast::End => end,
        TextAlignLast::Left => Align1::Left,
        TextAlignLast::Right => Align1::Right,
        TextAlignLast::Center => Align1::Center,
        TextAlignLast::Justify => Align1::Justify,
    }
}

/// An inline box's layout bounds around its baseline (CSS 2 §10.8.1, CSS Inline 3 §5.2: the leading
/// `line-height - (A + D)` is split in half above and below). The half above is floored to a whole
/// pixel and the rest goes below, so that with whole-pixel ascents (see [`line_metrics`]) baselines
/// sit on pixels, as in other engines.
fn strut(font: &FontInfo, line_height: f32, baseline: f32) -> VBox {
    let a = font.metrics.ascent;
    let d = font.metrics.descent;
    let leading = line_height - (a + d);
    // (in 1/64 px, as layout units, before flooring)
    let above = ((leading * 32.0).round() / 64.0).floor();
    VBox {
        baseline,
        top: baseline - (a + above),
        bottom: baseline + d + (leading - above),
    }
}

/// The baseline shift of a box with `va` inside a parent with font `parent` (positive down), for a box
/// whose layout bounds extend `above` / `below` its own baseline. `None` for top/bottom.
fn baseline_shift(
    va: VerticalAlign,
    parent: &FontInfo,
    above: f32,
    below: f32,
    line_height: f32,
) -> Option<f32> {
    let pm = &parent.metrics;
    Some(layout_unit(match va {
        VerticalAlign::Baseline => 0.0,
        VerticalAlign::Sub => parent.size / 5.0 + 1.0,
        VerticalAlign::Super => -(parent.size / 3.0 + 1.0),
        VerticalAlign::TextTop => -pm.ascent + above,
        VerticalAlign::TextBottom => pm.descent - below,
        VerticalAlign::Middle => -(pm.x_height / 2.0) + (above - below) / 2.0,
        VerticalAlign::LengthPercentage(lp) => -lp.resolve(line_height),
        VerticalAlign::Top | VerticalAlign::Bottom => return None,
    }))
}

/// Whether `nodes` hold text directly (not inside a nested inline box): text that the line
/// height calculation quirk counts (Quirks Mode §3.3).
fn has_text(nodes: &[LNode]) -> bool {
    nodes
        .iter()
        .any(|n| matches!(n, LNode::Text { .. } | LNode::Tab { .. }))
}

/// Whether an inline box counts for the line's height in quirks mode (Quirks Mode §3.3, "the line
/// height calculation quirk"): it holds text directly, or it has inline-axis borders or padding
/// and one of its edges is on this line. Otherwise it is treated as having a zero line-height,
/// that is, it adds nothing to the line box.
fn quirk_box_counts<S: Style>(
    it: &Item<S>,
    starts: bool,
    ends: bool,
    children: &[LNode],
    cb_width: f32,
) -> bool {
    if has_text(children) {
        return true;
    }
    let e = Edges::of(&it.style, cb_width);
    let inline_bp =
        e.border.left + e.padding.left != 0.0 || e.border.right + e.padding.right != 0.0;
    inline_bp && (starts || ends)
}

/// Computes each box's baseline and bounds (pre-order into `out`), growing the line's extent.
/// `has_metrics` says whether the parent box already contributes layout bounds; the result says
/// whether it does after `nodes` (quirks mode: a forced break gives a box that has none its own
/// strut, so a line holding only a `<br>` keeps its height).
#[allow(clippy::too_many_arguments)]
fn compute_vertical<S: Style>(
    text: &TextEngine,
    nodes: &[LNode],
    m: &Measure<'_, S>,
    parent_font: &FontInfo,
    parent: VBox,
    quirk: bool,
    mut has_metrics: bool,
    top: &mut f32,
    bottom: &mut f32,
    deferred: &mut Vec<(f32, f32, bool)>,
    out: &mut Vec<VBox>,
) -> bool {
    for n in nodes {
        match n {
            LNode::Box {
                item,
                starts,
                ends,
                children,
                ..
            } => {
                let it = &m.items[*item];
                let font = it.font.unwrap();
                let lh = line_height(&it.style, &font);
                let s = strut(&font, lh, 0.0);
                let above = -s.top;
                let below = s.bottom;
                let va = it.style.vertical_align();
                let counts = !quirk || quirk_box_counts(it, *starts, *ends, children, m.cb_width);
                let idx = out.len();
                match baseline_shift(va, parent_font, above, below, lh) {
                    Some(shift) => {
                        let bl = parent.baseline + shift;
                        let vb = VBox {
                            baseline: bl,
                            top: bl - above,
                            bottom: bl + below,
                        };
                        out.push(vb);
                        if counts {
                            *top = top.min(vb.top);
                            *bottom = bottom.max(vb.bottom);
                        }
                        let child = compute_vertical(
                            text, children, m, &font, vb, quirk, counts, top, bottom, deferred, out,
                        );
                        has_metrics |= child;
                    }
                    None => {
                        // Aligned to the line box's top or bottom: measured on its own.
                        let vb = VBox {
                            baseline: 0.0,
                            top: -above,
                            bottom: below,
                        };
                        out.push(vb);
                        let (mut t, mut b) = if counts {
                            (vb.top, vb.bottom)
                        } else {
                            (f32::INFINITY, f32::NEG_INFINITY)
                        };
                        let mut d2 = Vec::new();
                        let child = compute_vertical(
                            text, children, m, &font, vb, quirk, counts, &mut t, &mut b, &mut d2,
                            out,
                        );
                        if t > b {
                            t = 0.0;
                            b = 0.0;
                        }
                        deferred.push((b - t, -t, va == VerticalAlign::Top));
                        out[idx] = VBox {
                            baseline: f32::NAN,
                            top: t,
                            bottom: b,
                        };
                        has_metrics |= child;
                    }
                }
            }
            LNode::Atomic { item, .. } => {
                has_metrics = true;
                let r = &m.atomics[item];
                let f = &r.fragment;
                let h = f.size.height + r.margin.top + r.margin.bottom;
                // The baseline: an inline-block's last line box (unless it clips), else the bottom
                // margin edge (CSS 2 §10.8.1). (The clipping rule is the inline-block's: a flex or
                // grid container keeps its items' baseline, css-flexbox-1 §8.5, css-grid-2 §11.8.)
                let b = m.items[*item].box_id;
                let clips = f.as_box().is_some_and(|bf| {
                    bf.flags & (fragment_flags::CLIP_X | fragment_flags::CLIP_Y) != 0
                        && matches!(
                            bf.style.display(),
                            Display::Box {
                                inside: DisplayInside::Flow | DisplayInside::FlowRoot,
                                ..
                            }
                        )
                });
                let is_replaced = matches!(
                    f.as_box().map(|bf| &bf.content),
                    Some(BoxContent::Replaced { .. })
                );
                let _ = b;
                let control = matches!(
                    f.as_box().map(|bf| &bf.content),
                    Some(BoxContent::Replaced {
                        control: Some(_),
                        ..
                    })
                );
                let fb = if control {
                    // A form control's baseline is its text's (as an inline-block's line box).
                    f.baseline
                } else if clips || is_replaced {
                    None
                } else if f.as_box().is_some_and(|bf| {
                    matches!(
                        bf.style.display(),
                        Display::Box {
                            inside: DisplayInside::Flex | DisplayInside::Grid,
                            ..
                        }
                    )
                }) {
                    // (only an inline-block exports its last baseline, CSS 2 §10.8.1)
                    f.baseline
                } else {
                    f.last_baseline.or(f.baseline)
                };
                let fb = fb.map(|b| m.line_y(b, f.size.height));
                let above = match fb {
                    Some(v) => v + r.margin.top,
                    // (a central baseline is that of the border box, css-align-3 §9.1)
                    None if m.central => r.margin.top + m.synthesized_baseline(f.size.height),
                    None => m.synthesized_baseline(h),
                };
                let below = h - above;
                let va = m.items[*item].style.vertical_align();
                match baseline_shift(va, parent_font, above, below, h) {
                    Some(shift) => {
                        let bl = parent.baseline + shift;
                        let vb = VBox {
                            baseline: bl,
                            top: bl - above,
                            bottom: bl + below,
                        };
                        out.push(vb);
                        *top = top.min(vb.top);
                        *bottom = bottom.max(vb.bottom);
                    }
                    None => {
                        out.push(VBox {
                            baseline: f32::NAN,
                            top: -above,
                            bottom: below,
                        });
                        deferred.push((h, above, va == VerticalAlign::Top));
                    }
                }
            }
            LNode::Break { item, .. } => {
                out.push(parent);
                if quirk && !has_metrics {
                    // The break's own line-height with its box's font.
                    let lh = line_height(&m.items[*item].style, parent_font);
                    let s = strut(parent_font, lh, parent.baseline);
                    *top = top.min(s.top);
                    *bottom = bottom.max(s.bottom);
                    has_metrics = true;
                }
            }
            LNode::Text { item, run, .. } => {
                has_metrics = true;
                out.push(parent);
                // With `line-height: normal`, the fonts the text falls back to count with their
                // own ascent, descent and line gap (CSS Inline 3 §5.1: "normal" uses the metrics
                // of the fonts used; as Chrome does, each around the box's baseline).
                let it = &m.items[*item];
                if let (LineHeight::Normal, Some(shaped), Some(font)) =
                    (it.style.line_height(), &it.shaped, it.font)
                {
                    if let Some(r) = shaped.runs.get(*run) {
                        if r.font != font.primary {
                            let used = FontInfo {
                                list: font.list,
                                primary: r.font,
                                metrics: if font.vertical == VerticalText::None {
                                    line_metrics(text.metrics(r.font, r.size))
                                } else {
                                    centered(line_metrics(text.metrics(r.font, r.size)))
                                },
                                size: r.size,
                                vertical: font.vertical,
                                alphabetic: font.alphabetic,
                            };
                            let lh = line_height(&it.style, &used);
                            let s = strut(&used, lh, parent.baseline);
                            *top = top.min(s.top);
                            *bottom = bottom.max(s.bottom);
                        }
                    }
                }
            }
            LNode::Tab { .. } => {
                has_metrics = true;
                out.push(parent);
            }
            LNode::Ellipsis { .. } => {
                out.push(parent);
            }
        }
    }
    has_metrics
}

/// Positions nodes left to right in visual order and makes their fragments.
#[allow(clippy::too_many_arguments)]
fn place_nodes<S: Style>(
    cx: &mut Cx<'_, S>,
    nodes: &[LNode],
    m: &Measure<'_, S>,
    vi: &mut usize,
    vinfo: &[VBox],
    cursor: &mut f32,
    line_baseline: f32,
    line_height: f32,
    empty_line: bool,
    justify: f32,
    out: &mut Vec<FragmentChild<S>>,
    oof: &mut Vec<Oof>,
    cb_width: f32,
    cb_height: Option<f32>,
    origin: Point,
    line_end: Option<usize>,
) {
    // Visual order of this level (UAX #9 L2).
    let levels: SmallVec<[u8; 16]> = nodes.iter().map(|n| n.level()).collect();
    let order: SmallVec<[usize; 16]> = if levels.iter().any(|l| l % 2 == 1) {
        visual_order(&levels).into_iter().collect()
    } else {
        (0..nodes.len()).collect()
    };
    // vinfo is in logical pre-order; compute each node's index first.
    let mut starts: SmallVec<[usize; 16]> = SmallVec::new();
    let mut k = *vi;
    for n in nodes {
        starts.push(k);
        k += subtree_len(n);
    }
    *vi = k;
    for &i in &order {
        let n = &nodes[i];
        let v = vinfo[starts[i]];
        // Resolve top/bottom-aligned boxes against the line box.
        let resolve_tb = |v: VBox, top_aligned: Option<bool>| -> f32 {
            // baseline of the box in line coordinates
            match top_aligned {
                Some(true) => -v.top,
                Some(false) => line_height - v.bottom,
                None => line_baseline + v.baseline,
            }
        };
        match n {
            LNode::Box {
                item,
                starts: st,
                ends,
                children,
                width,
                ..
            } => {
                let it = &m.items[*item];
                let e = Edges::of(&it.style, cb_width);
                let tb = if v.baseline.is_nan() {
                    Some(it.style.vertical_align() == VerticalAlign::Top)
                } else {
                    None
                };
                let bl = if tb.is_some() {
                    resolve_tb(VBox { baseline: 0.0, ..v }, tb)
                } else {
                    resolve_tb(v, None)
                };
                let font = it.font.unwrap();
                // (on a phantom line the box has no content area; text-box-trim moves its edges to
                // the text-box-edge metrics, CSS Inline 3 §6.1)
                let (a, d) = if empty_line {
                    (0.0, 0.0)
                } else {
                    let t = it.style.text_box_trim();
                    let edge = it.style.text_box_edge();
                    // (block-start is the under side in `vertical-lr`'s line frame)
                    let (over, under) = if m.mirrored {
                        (t.end, t.start)
                    } else {
                        (t.start, t.end)
                    };
                    (
                        if over {
                            over_edge(&font, edge.over)
                        } else {
                            font.metrics.ascent
                        },
                        if under {
                            under_edge(&font, edge.under)
                        } else {
                            font.metrics.descent
                        },
                    )
                };
                let has_start = *st && it.has_start_edge;
                let has_end = *ends && it.has_end_edge;
                let (has_left, has_right) = line_sides(&it.style, has_start, has_end);
                let ml = if has_left { e.ml() } else { 0.0 };
                let mr = if has_right { e.mr() } else { 0.0 };
                let border = Sides::new(
                    e.border.top,
                    if has_right { e.border.right } else { 0.0 },
                    e.border.bottom,
                    if has_left { e.border.left } else { 0.0 },
                );
                let padding = Sides::new(
                    e.padding.top,
                    if has_right { e.padding.right } else { 0.0 },
                    e.padding.bottom,
                    if has_left { e.padding.left } else { 0.0 },
                );
                let x0 = *cursor + ml;
                let content_top = bl - a;
                let frag_top = content_top - border.top - padding.top;
                let extra = justify * count_spaces(children) as f32;
                let frag_w = (width + extra - ml - mr).max(0.0);
                let frag_h = a + d + border.top + padding.top + border.bottom + padding.bottom;
                let mut inner = Vec::new();
                let mut inner_cursor = x0 + border.left + padding.left;
                let mut sub_vi = starts[i] + 1;
                // Children's vertical positions are relative to the root baseline, or to this
                // box's own baseline when it is aligned to the line's top or bottom.
                let child_ref = if tb.is_some() { bl } else { line_baseline };
                place_nodes(
                    cx,
                    children,
                    m,
                    &mut sub_vi,
                    vinfo,
                    &mut inner_cursor,
                    child_ref,
                    line_height,
                    empty_line,
                    justify,
                    &mut inner,
                    oof,
                    cb_width,
                    cb_height,
                    Point::new(x0, frag_top),
                    line_end,
                );
                // children were placed relative to the line: shift into the box
                let rel = cx.relative_offset(&it.style, cb_width, cb_height);
                let b = cx.tree.b(it.box_id);
                let mut overflow = Rect::new(0.0, 0.0, frag_w, frag_h);
                let mut bounds = overflow;
                for c in &inner {
                    add_overflow(&mut overflow, &mut bounds, c);
                }
                // (a blur or a drop shadow shows past what it filters: Filter Effects 1 §10)
                if it.style.has_filter() {
                    let border_box = Rect::new(0.0, 0.0, frag_w, frag_h);
                    bounds =
                        snowlark_paint::filter::map_rect(&it.style.filter_ops(border_box), bounds);
                }
                let flags = box_flags(b) | fragment_flags::INLINE_BOX;
                if it.style.has_clip_path() {
                    cx.clipped_inlines = true;
                }
                if !has_start || !has_end {
                    cx.striped_inlines = true;
                }
                let frag = Arc::new(Fragment {
                    size: Size::new(frag_w, frag_h),
                    kind: FragmentKind::Box(BoxFragment {
                        style: it.style.clone(),
                        box_id: Some(it.box_id),
                        node: b.node,
                        pseudo: b.pseudo,
                        pseudo_of: b.pseudo_of,
                        border,
                        padding,
                        flags,
                        order: b.order,
                        subtree_end: cx.tree.subtree_end(it.box_id),
                        content: BoxContent::None,
                        containing: Default::default(),
                        clip_reference: Default::default(),
                        strip: Default::default(),
                        stitched: None,
                    }),
                    children: inner,
                    overflow,
                    paint_bounds: bounds,
                    inflow: None,
                    baseline: Some(bl - frag_top),
                    last_baseline: Some(bl - frag_top),
                });
                out.push(FragmentChild {
                    offset: Point::new(x0 - origin.x + rel.x, frag_top - origin.y + rel.y),
                    fragment: frag,
                    stacking_owner: 0,
                });
                *cursor += width + extra;
            }
            LNode::Text {
                item,
                run,
                clusters,
                width,
                spaces,
                ..
            } => {
                if clusters.is_empty() {
                    continue;
                }
                let it = &m.items[*item];
                let font = it.font.unwrap();
                let shaped = it.shaped.as_ref().unwrap();
                let a = font.metrics.ascent;
                let d = font.metrics.descent;
                let bl = line_baseline + v.baseline;
                let w = width + justify * *spaces as f32;
                let b = cx.tree.b(it.box_id);
                let t0 = shaped.clusters[clusters.start].text.0;
                let t1 = shaped.clusters[clusters.end - 1].text.1;
                let frag = Arc::new(Fragment {
                    size: Size::new(w, a + d),
                    kind: FragmentKind::Text(TextFragment {
                        style: it.style.clone(),
                        box_id: Some(it.box_id),
                        node: b.node,
                        shaped: shaped.clone(),
                        run: *run as u32,
                        clusters: clusters.start as u32..clusters.end as u32,
                        baseline: a,
                        justify,
                        letter_spacing: it.style.letter_spacing(),
                        decorations: b.decorations.clone(),
                        text_range: dom_offset(it.dom_start, it.collapsed.as_deref(), t0)
                            ..dom_offset(it.dom_start, it.collapsed.as_deref(), t1),
                        dom_start: it.dom_start,
                        collapsed: it.collapsed.clone(),
                        flip: Flip::IDENTITY,
                        central: central_baseline(&it.style),
                        combined: it.combine.then(|| {
                            let c = shaped.clusters[clusters.start];
                            shaped.advances[c.glyphs.0 as usize..c.glyphs.1 as usize]
                                .iter()
                                .sum::<f32>()
                        }),
                        line: (a - bl, line_height + a - bl),
                        line_end: line_end == Some(starts[i]),
                    }),
                    children: Vec::new(),
                    overflow: Rect::new(0.0, 0.0, w, a + d),
                    paint_bounds: Rect::new(0.0, 0.0, w, a + d),
                    inflow: None,
                    baseline: Some(a),
                    last_baseline: Some(a),
                });
                out.push(FragmentChild {
                    offset: Point::new(*cursor - origin.x, bl - a - origin.y),
                    fragment: frag,
                    stacking_owner: 0,
                });
                *cursor += w;
            }
            LNode::Atomic { item, width, .. } => {
                let r = &m.atomics[item];
                let f = &r.fragment;
                let h = f.size.height + r.margin.top + r.margin.bottom;
                let above = if v.baseline.is_nan() {
                    -v.top
                } else {
                    v.baseline - v.top
                };
                let bl = if v.baseline.is_nan() {
                    let top_aligned = m.items[*item].style.vertical_align() == VerticalAlign::Top;
                    if top_aligned {
                        above
                    } else {
                        line_height - (h - above)
                    }
                } else {
                    line_baseline + v.baseline
                };
                let margin_top = bl - above;
                let rel = cx.relative_offset(&m.items[*item].style, cb_width, cb_height);
                // (in the line's coordinates)
                let at = Point::new(
                    *cursor + r.margin.left + rel.x,
                    margin_top + r.margin.top + rel.y,
                );
                // (in a mirrored line the box's own coordinates stay the block frame's: its top is
                // where the line's mirror puts it, `build_line` adds the line's height)
                let dy = if m.mirrored {
                    -(at.y + f.size.height)
                } else {
                    at.y
                };
                oof.extend(r.oof.iter().map(|o| o.shifted(at.x, dy)));
                out.push(FragmentChild {
                    offset: Point::new(at.x - origin.x, at.y - origin.y),
                    fragment: f.clone(),
                    stacking_owner: 0,
                });
                *cursor += width;
            }
            LNode::Tab { width, .. } => {
                *cursor += width;
            }
            LNode::Break { item, .. } => {
                // A <br> has a position (CSSOM View: its client rect is its content area, empty
                // inline-wise, where the line ends); it paints nothing.
                let it = &m.items[*item];
                let b = cx.tree.b(it.box_id);
                if b.kind == BoxKind::LineBreak && !empty_line {
                    if let Some(font) = it.font {
                        let (a, d) = (font.metrics.ascent, font.metrics.descent);
                        let bl = line_baseline + v.baseline;
                        let frag = Arc::new(Fragment {
                            size: Size::new(0.0, a + d),
                            kind: FragmentKind::Box(BoxFragment {
                                style: it.style.clone(),
                                box_id: Some(it.box_id),
                                node: b.node,
                                pseudo: b.pseudo,
                                pseudo_of: b.pseudo_of,
                                border: Sides::default(),
                                padding: Sides::default(),
                                flags: fragment_flags::INLINE_BOX | fragment_flags::HIDE_EMPTY,
                                order: b.order,
                                subtree_end: cx.tree.subtree_end(it.box_id),
                                content: BoxContent::None,
                                containing: Default::default(),
                                clip_reference: Default::default(),
                                strip: Default::default(),
                                stitched: None,
                            }),
                            children: Vec::new(),
                            overflow: Rect::new(0.0, 0.0, 0.0, a + d),
                            paint_bounds: Rect::new(0.0, 0.0, 0.0, 0.0),
                            inflow: None,
                            baseline: Some(a),
                            last_baseline: Some(a),
                        });
                        out.push(FragmentChild {
                            offset: Point::new(*cursor - origin.x, bl - a - origin.y),
                            fragment: frag,
                            stacking_owner: 0,
                        });
                    }
                }
            }
            LNode::Ellipsis {
                shaped,
                width,
                item,
                ..
            } => {
                let first = m
                    .items
                    .get(item.map_or(0, |(i, _)| i))
                    .map(|i| (i.style.clone(), i.box_id));
                // A hyphen covers its soft hyphen's DOM text (`TextFragment::dom_offset` maps the
                // hyphen's ends to the soft hyphen's).
                let owner = item.map(|(i, (p0, p1))| {
                    let it = &m.items[i];
                    let b = cx.tree.b(it.box_id);
                    let d0 = dom_offset(it.dom_start, it.collapsed.as_deref(), p0);
                    let d1 = dom_offset(it.dom_start, it.collapsed.as_deref(), p1);
                    (b.node, b.decorations.clone(), d0..d1, d0)
                });
                if let Some((style, style_box)) = first {
                    // Each run of the string on its own, in visual order (it can be bidirectional).
                    let levels: SmallVec<[u8; 4]> =
                        shaped.runs.iter().map(|r| r.bidi_level).collect();
                    let mut x = *cursor;
                    for i in visual_order(&levels) {
                        let r = shaped.runs[i];
                        let fm = line_metrics(cx.text.metrics(r.font, r.size));
                        let (a, d) = (fm.ascent, fm.descent);
                        let w = r.advance;
                        let bl = line_baseline + v.baseline;
                        let frag = Arc::new(Fragment {
                            size: Size::new(w, a + d),
                            kind: FragmentKind::Text(TextFragment {
                                flip: Flip::IDENTITY,
                                central: central_baseline(&style),
                                combined: None,
                                style: style.clone(),
                                box_id: Some(style_box),
                                node: owner.as_ref().and_then(|o| o.0),
                                shaped: shaped.clone(),
                                run: i as u32,
                                clusters: r.clusters.0..r.clusters.1,
                                baseline: a,
                                justify: 0.0,
                                letter_spacing: 0.0,
                                decorations: owner
                                    .as_ref()
                                    .map(|o| o.1.clone())
                                    .unwrap_or_default(),
                                text_range: owner.as_ref().map_or(0..0, |o| o.2.clone()),
                                dom_start: owner.as_ref().map_or(0, |o| o.3),
                                collapsed: None,
                                line: (a - bl, line_height + a - bl),
                                line_end: owner.is_some() && line_end == Some(starts[i]),
                            }),
                            children: Vec::new(),
                            overflow: Rect::new(0.0, 0.0, w, a + d),
                            paint_bounds: Rect::new(0.0, 0.0, w, a + d),
                            inflow: None,
                            baseline: Some(a),
                            last_baseline: Some(a),
                        });
                        out.push(FragmentChild {
                            offset: Point::new(x - origin.x, bl - a - origin.y),
                            fragment: frag,
                            stacking_owner: 0,
                        });
                        x += w;
                    }
                }
                *cursor += width;
            }
        }
    }
}

fn subtree_len(n: &LNode) -> usize {
    match n {
        LNode::Box { children, .. } => 1 + children.iter().map(subtree_len).sum::<usize>(),
        _ => 1,
    }
}

/// The line's last leaf in logical order, skipping a forced break at its end, by its index in the
/// logical pre-order of `nodes`: when it is text (or a hyphen) in the line's direction, a
/// selection that goes on past it covers the end of the line (CSS Pseudo-Elements 4 §3.4 leaves
/// the highlighted area to the UA; a space's width there shows that the line break is selected).
fn line_end_leaf(nodes: &[LNode], base_level: u8) -> Option<usize> {
    // (index, is text in the base direction) of the last leaf, and of the one before a break
    fn walk(
        nodes: &[LNode],
        i: &mut usize,
        base: u8,
        last: &mut Option<(usize, bool)>,
        before: &mut Option<(usize, bool)>,
    ) {
        for n in nodes {
            let at = *i;
            *i += 1;
            match n {
                LNode::Box { children, .. } if !children.is_empty() => {
                    walk(children, i, base, last, before)
                }
                LNode::Text { clusters, .. } if clusters.is_empty() => {}
                LNode::Break { .. } => {
                    *before = *last;
                    *last = None;
                }
                _ => {
                    let text = matches!(
                        n,
                        LNode::Text { .. } | LNode::Ellipsis { item: Some(_), .. }
                    );
                    *last = Some((at, text && n.level() % 2 == base % 2));
                    *before = None;
                }
            }
        }
    }
    let (mut i, mut last, mut before) = (0, None, None);
    walk(nodes, &mut i, base_level, &mut last, &mut before);
    match last.or(before) {
        Some((at, true)) => Some(at),
        _ => None,
    }
}
