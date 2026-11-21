// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The layout driver: [`Cx`] (the tree, the text engine, the pass), the constraint [`Space`] a box is laid
//! out in, the [`LayoutResult`] it returns (fragment, margin struts for collapsing, escaping floats,
//! out-of-flow candidates), the per-box cache check, and the dispatch to each formatting context.

use std::sync::Arc;

use snowlark_paint::{Point, Rect, Sides, Size, TextEngine};

use crate::boxes::*;
use crate::cache::CachedLayout;
use crate::floats::{Exclusions, PlacedFloat};
use crate::fragment::*;
use crate::sizing::ContentSizes;
use crate::style::*;

/// How an `auto` width is resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WidthMode {
    /// Block-level in normal flow: fill the available space (CSS 2 §10.3.3).
    Fill,
    /// Floats, inline-blocks, absolutely positioned boxes: shrink-to-fit (§10.3.5).
    ShrinkToFit,
}

/// The constraints a box is laid out under; equal spaces give equal layouts of a clean box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Space {
    /// The containing block's width (the percentage basis for widths, margins and padding).
    pub cb_width: f32,
    /// The containing block's height when definite (the percentage basis for heights).
    pub cb_height: Option<f32>,
    /// The inline space available to the margin box.
    pub available: f32,
    pub mode: WidthMode,
    /// An imposed border-box width (flex/grid items, table cells, abspos with resolved width).
    pub fixed_width: Option<f32>,
    /// An imposed border-box height.
    pub fixed_height: Option<f32>,
    /// Whether `fixed_height` is definite for the box's children's percentages.
    pub fixed_height_definite: bool,
    /// A table cell's border in the collapsing border model (half of each resolved edge).
    pub collapsed_border: Option<Sides>,
    /// Lay the box out for its content height: its own `height`, `min-height` and `max-height` count
    /// as `auto` (a flex item measured for its content size: css-flexbox-1 §9.2 step 3E and the
    /// content size suggestion of §4.5, where the item's preferred and min/max sizes are applied by
    /// the flex algorithm, not by the measurement).
    pub content_height: bool,
    /// Trimming an ancestor block container asks of this box's first/last formatted line.
    pub trim: TrimRequest,
    /// The basis of percentage margins and padding when it is not `cb_width`: in an orthogonal
    /// flow, the containing block's inline size in its own writing mode.
    pub edge_basis: Option<f32>,
    /// The box is measured for its parent's intrinsic sizes across axes, with `cb_width` only the
    /// fallback constraint: its percentage inline size behaves as `auto`.
    pub inline_pct_auto: bool,
    /// Lay the box out for its content width: what `content_height` means for the parent's width,
    /// which an orthogonal flow's block size is.
    pub content_width: bool,
}

/// `text-box-trim` passed down to the first or last formatted line (CSS Inline 3 §6.1), per side. A
/// block container passes the request to its first (last) in-flow block-level child when no border
/// or padding intervenes, and not into flex, grid or table formatting contexts. The line is trimmed
/// to the `text-box-edge` metrics of its own containing block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrimRequest {
    pub start: bool,
    pub end: bool,
}

impl Space {
    pub fn fill(cb_width: f32, cb_height: Option<f32>) -> Space {
        Space {
            cb_width,
            cb_height,
            available: cb_width,
            mode: WidthMode::Fill,
            fixed_width: None,
            fixed_height: None,
            fixed_height_definite: true,
            collapsed_border: None,
            content_height: false,
            trim: TrimRequest::default(),
            edge_basis: None,
            inline_pct_auto: false,
            content_width: false,
        }
    }

    pub fn shrink(cb_width: f32, cb_height: Option<f32>, available: f32) -> Space {
        Space {
            cb_width,
            cb_height,
            available,
            mode: WidthMode::ShrinkToFit,
            fixed_width: None,
            fixed_height: None,
            fixed_height_definite: true,
            collapsed_border: None,
            content_height: false,
            trim: TrimRequest::default(),
            edge_basis: None,
            inline_pct_auto: false,
            content_width: false,
        }
    }

    /// The basis of the box's percentage margins and padding.
    #[inline]
    pub fn edges_basis(&self) -> f32 {
        self.edge_basis.unwrap_or(self.cb_width)
    }
}

/// A set of adjoining margins being collapsed (CSS 2 §8.3.1): the largest positive and the most
/// negative.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Strut {
    pub pos: f32,
    pub neg: f32,
}

impl Strut {
    pub fn of(m: f32) -> Strut {
        let mut s = Strut::default();
        s.add(m);
        s
    }

    pub fn add(&mut self, m: f32) {
        if m >= 0.0 {
            self.pos = self.pos.max(m);
        } else {
            self.neg = self.neg.min(m);
        }
    }

    pub fn join(mut self, other: Strut) -> Strut {
        self.pos = self.pos.max(other.pos);
        self.neg = self.neg.min(other.neg);
        self
    }

    /// The collapsed margin.
    pub fn resolve(&self) -> f32 {
        self.pos + self.neg
    }
}

/// How an out-of-flow box's margin box aligns in its static-position area, per axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaticAlign {
    /// Physical start (left, top), center and end of the area, as a flex or grid container's
    /// layout decided.
    Start,
    Center,
    End,
    /// As the box's own self-alignment says (block and inline static positions:
    /// css-position-3 §3.5.1, its `justify-self`, `auto` being the parent's `justify-items`).
    SelfAlign,
}

/// The static position in one axis (css-position-3 §4.2): an area and an alignment (a point for
/// block and inline layout, an area for flex and grid).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StaticPos {
    pub start: f32,
    pub end: f32,
    pub align: StaticAlign,
    /// `safe` alignment (css-align-3 §4.3): the alignment used instead when the box overflows the
    /// area.
    pub safe_fallback: Option<StaticAlign>,
}

impl StaticPos {
    pub fn at(v: f32) -> StaticPos {
        StaticPos {
            start: v,
            end: v,
            align: StaticAlign::SelfAlign,
            safe_fallback: None,
        }
    }

    /// The static position of a block-level box in a block container whose content box is
    /// `width` wide: its margin box starts at the content box's start edge (left in ltr, right in
    /// rtl).
    pub fn block_level(width: f32) -> StaticPos {
        StaticPos {
            start: 0.0,
            end: width,
            align: StaticAlign::SelfAlign,
            safe_fallback: None,
        }
    }

    fn shift(&mut self, d: f32) {
        self.start += d;
        self.end += d;
    }
}

/// An absolutely or fixed positioned box waiting for its containing block, with its static position
/// relative to the fragment currently holding it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Oof {
    pub id: BoxId,
    /// The containing block box; `None` for the initial containing block.
    pub cb: Option<BoxId>,
    pub x: StaticPos,
    pub y: StaticPos,
}

impl Oof {
    pub fn shifted(mut self, dx: f32, dy: f32) -> Oof {
        self.x.shift(dx);
        self.y.shift(dy);
        self
    }
}

/// What laying out a box gives its parent.
pub(crate) struct LayoutResult<S: Style> {
    pub fragment: Arc<Fragment<S>>,
    /// Used margins (horizontal ones resolved; vertical ones as specified, `auto` as 0).
    pub margin: Sides,
    /// The margins adjoining the box's top edge (its own and, through it, its first children's).
    pub start: Strut,
    /// The margins adjoining its bottom edge.
    pub end: Strut,
    /// Its top and bottom margins are adjoining (it is empty): `start` and `end` collapse together.
    pub through: bool,
    /// Floats it added to its parent's BFC, relative to its border-box origin (re-added on cache hits).
    pub floats: Vec<PlacedFloat>,
    /// Out-of-flow descendants whose containing block is an ancestor, relative to its border box.
    pub oof: Vec<Oof>,
}

impl<S: Style> Clone for LayoutResult<S> {
    fn clone(&self) -> Self {
        LayoutResult {
            fragment: self.fragment.clone(),
            margin: self.margin,
            start: self.start,
            end: self.end,
            through: self.through,
            floats: self.floats.clone(),
            oof: self.oof.clone(),
        }
    }
}

/// Counters for one layout pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutStats {
    /// Boxes whose layout ran.
    pub laid_out: u32,
    /// Boxes whose cached fragment was reused.
    pub reused: u32,
    /// Of `laid_out`, layouts of a box already laid out in this pass in another space (a flex or
    /// grid item measured and then laid out, say).
    pub relaid: u32,
    /// Flex and grid containers whose fragment was built from taffy's layouts of their items.
    pub containers_built: u32,
    /// Flex and grid algorithm runs (taffy's, for a measurement or a layout; a cache hit is none).
    pub algorithm_runs: u32,
    /// Text items shaped (not reused).
    pub shaped: u32,
    /// Text items whose shaping came from the tree's shaping cache (a run shaped before, in this
    /// formatting context or another).
    pub shape_cache_hits: u32,
    /// Words (segments of simple-script runs) shaped by DirectWrite, and taken from the word cache
    /// (see `snowlark_paint::text_cache`), for the text items shaped.
    pub words_shaped: u32,
    pub words_cached: u32,
    /// Time spent preparing inline content (white space, font resolution, shaping), in µs.
    pub prepare_us: u64,
    /// Inline formatting contexts whose content was prepared (not kept from before).
    pub prepared: u32,
}

/// The line clamping of the line-clamp container being laid out (CSS Overflow 4 §4). Its possible
/// clamp points are after each line box of its block formatting context and after each in-flow
/// block-level box in it, in order, through the block containers that take part in it (not inside
/// independent formatting contexts, which take this away while they are laid out). A first pass
/// records them; when the container has more lines than `max-lines` (or content past its block
/// size, for `auto`), a second pass puts the clamp point after the chosen one: a line there gets
/// the block overflow ellipsis, and what follows is not laid out (out-of-flow boxes aside).
#[derive(Clone, Debug, Default)]
pub(crate) struct Clamp {
    /// The possible clamp points seen so far: whether each follows a line box (else a block-level
    /// box), and its bottom edge from the container's content-box top.
    pub points: Vec<(bool, f32)>,
    /// The container's content-box top in the coordinates of its block formatting context.
    pub content_top: f32,
    /// The second pass: the clamp point is after this many possible clamp points, and the first
    /// pass's points (the same up to there).
    pub after: Option<usize>,
    pub planned: Vec<(bool, f32)>,
    /// The block-end borders and paddings of the boxes open around the content being laid out
    /// (inside the container): a clamp point there ends below them.
    pub end_bp: f32,
    /// The block overflow ellipsis on the line before the clamp point.
    pub ellipsis: Option<String>,
    /// The clamp point has been passed.
    pub reached: bool,
}

impl Clamp {
    /// The block overflow ellipsis to put on the next line, when the clamp point follows it.
    pub fn ellipsis_for_next_line(&self) -> Option<&str> {
        let next = self.points.len();
        match self.after {
            Some(a)
                if !self.reached
                    && next + 1 == a
                    && self.planned.get(next).is_some_and(|p| p.0) =>
            {
                Some(self.ellipsis.as_deref().unwrap_or(""))
            }
            _ => None,
        }
    }

    /// Records a possible clamp point after a line box (`line`) or a block-level box whose bottom
    /// edge is at `bottom` (BFC coordinates); returns whether it is the clamp point.
    pub fn point(&mut self, line: bool, bottom: f32) -> bool {
        self.points
            .push((line, bottom + self.end_bp - self.content_top));
        if self.after == Some(self.points.len()) {
            self.reached = true;
        }
        self.reached
    }

    /// The line boxes recorded.
    pub fn lines(&self) -> usize {
        self.points.iter().filter(|p| p.0).count()
    }

    /// Where the clamp point goes after a first pass: after the `max_lines`th line when there are
    /// more, and for `auto` (a `limit` on the content height) after the last point that fits when
    /// the content does not (0: before all content). `None`: no clamping.
    pub fn clamp_point(&self, max_lines: Option<u32>, limit: Option<f32>) -> Option<usize> {
        let after = max_lines.and_then(|n| {
            let n = n as usize;
            if self.lines() <= n {
                return None;
            }
            let mut seen = 0;
            self.points.iter().position(|p| {
                seen += p.0 as usize;
                p.0 && seen == n
            })
        });
        // (from an index to a count)
        let mut after = after.map(|i| i + 1);
        if let Some(limit) = limit {
            let overflows = self.points.iter().any(|p| p.1 > limit + 0.01);
            if overflows {
                // (none fits: the clamp point is before all content)
                let fit = self
                    .points
                    .iter()
                    .rposition(|p| p.1 <= limit + 0.01)
                    .map_or(0, |i| i + 1);
                after = Some(after.map_or(fit, |a| a.min(fit)));
            }
        }
        after
    }
}

/// The layout context of one pass.
pub(crate) struct Cx<'a, S: Style> {
    pub tree: &'a mut BoxTree<S>,
    pub text: &'a TextEngine,
    pub pass: u32,
    pub stats: LayoutStats,
    /// The line clamping in effect ([`Clamp`]).
    pub clamp: Option<Clamp>,
    /// An inline box with a `clip-path` has been placed on a line since this was last cleared
    /// (the inline layout then relates its pieces' reference boxes, see
    /// [`crate::fragment::BoxFragment::clip_reference`]).
    pub clipped_inlines: bool,
    /// The same for an inline box with background images (the inline layout then places its
    /// pieces in their strip, see [`crate::fragment::BoxFragment::strip`]).
    pub striped_inlines: bool,
    /// While the intrinsic inline sizes of a box are computed: its content-box height when that is
    /// definite, which percentage heights of its children resolve against (CSS 2 §10.5), so that a
    /// replaced child's percentage height transfers through its ratio (css-sizing-3 §5.2: a grid
    /// item whose height resolves against its grid area, css-grid-2 §11.5). `None` otherwise.
    pub pct_height_basis: Option<f32>,
    /// The flow frame layout is running in: the map from its coordinates to physical ones (see
    /// [`crate::logical`]; identity in `horizontal-tb`).
    pub frame: crate::logical::Flip,
    /// The initial containing block's physical size (CSS Writing Modes 4 §7.3.1's fallback).
    pub icb: Size,
    /// An orthogonal flow is being measured for its parent's intrinsic sizes.
    pub measuring_across: bool,
    /// The box being laid out is in a multi-column container's flow, where an in-flow block-level
    /// child with `column-span: all` is a spanner (see [`crate::multicol`]).
    pub column_flow: bool,
    /// Multi-column containers being laid out inside an outer multi-column container's columns,
    /// each in rows of columns as tall as the space its outer columns leave (see
    /// [`crate::multicol`]).
    pub nested: Vec<crate::multicol::Nested>,
    /// Those boxes and their ancestors in the outer flow: their cached layouts are not theirs here.
    pub nested_path: Vec<BoxId>,
}

impl<'a, S: Style> Cx<'a, S> {
    /// Lays out box `id` in `space`. Boxes that don't establish an independent formatting context take
    /// part in their parent's BFC: `bfc` holds its floats and `at` is the estimated position, in the BFC
    /// root's coordinates, of the containing block's content-box left edge and the box's border-box top.
    pub fn layout(
        &mut self,
        id: BoxId,
        space: &Space,
        bfc: Option<&mut Exclusions>,
        at: Point,
    ) -> LayoutResult<S> {
        // (a layout is not part of an intrinsic size computation's height context)
        let basis = self.pct_height_basis.take();
        let r = self.layout_box(id, space, bfc, at);
        self.pct_height_basis = basis;
        r
    }

    fn layout_box(
        &mut self,
        id: BoxId,
        space: &Space,
        bfc: Option<&mut Exclusions>,
        at: Point,
    ) -> LayoutResult<S> {
        let pass = self.pass;
        let b = self.tree.b(id);
        // A box below every float of its BFC lays out the same wherever it is placed. (Under line
        // clamping, a box that takes part in the clamped formatting context depends on the lines
        // before it.)
        let independent = b.is_independent();
        let cacheable = bfc.as_ref().is_none_or(|e| !e.any_below(at.y))
            && (independent || self.clamp.is_none())
            && (self.nested_path.is_empty() || !self.nested_path.contains(&id));
        if cacheable {
            if let Some(c) = &b.cache.layout {
                if (!b.is_dirty() || c.pass == pass)
                    && (c.space == *space || same_layout_in(self.tree, self.frame, id, c, space))
                {
                    let result = c.result.clone();
                    self.stats.reused += 1;
                    if let Some(ex) = bfc {
                        for f in &result.floats {
                            let mut f = *f;
                            f.rect = f.rect.translate(at.x, at.y);
                            ex.push(f);
                        }
                    }
                    return result;
                }
            }
        }
        self.stats.laid_out += 1;
        if b.cache.layout.as_ref().is_some_and(|c| c.pass == pass) {
            self.stats.relaid += 1;
        }
        let kind = b.kind;
        // An independent formatting context's lines are not the clamped container's.
        let outer_clamp = if independent { self.clamp.take() } else { None };
        // (a box that leaves a multi-column flow takes its descendants out of it)
        let outer_flow = self.column_flow;
        if outer_flow && !crate::multicol::passes_spanners(b) {
            self.column_flow = false;
        }
        let result = if b.style.view() != self.frame {
            // A box in another writing mode (CSS Writing Modes 4 §3.2): laid out in its own frame.
            self.layout_across(id, kind, space)
        } else {
            self.dispatch(id, kind, space, bfc, at)
        };
        if independent {
            self.clamp = outer_clamp;
        }
        self.column_flow = outer_flow;
        if cacheable {
            self.tree.b_mut(id).cache.layout = Some(Box::new(CachedLayout {
                space: *space,
                pass,
                result: result.clone(),
            }));
        }
        result
    }

    /// Runs the layout of `id`'s formatting context in the current frame.
    pub(crate) fn dispatch(
        &mut self,
        id: BoxId,
        kind: BoxKind,
        space: &Space,
        bfc: Option<&mut Exclusions>,
        at: Point,
    ) -> LayoutResult<S> {
        match kind {
            BoxKind::Block | BoxKind::TableCell | BoxKind::TableCaption => {
                crate::block::layout_block_container(self, id, space, bfc, at)
            }
            BoxKind::Flex | BoxKind::Grid => crate::flex_grid::layout_container(self, id, space),
            BoxKind::TableWrapper => crate::table::layout_table_wrapper(self, id, space),
            BoxKind::Replaced => crate::replaced::layout_replaced(self, id, space),
            // Boxes only laid out through their parents; as a fallback, an empty block.
            _ => crate::block::layout_block_container(self, id, space, bfc, at),
        }
    }

    /// Whether `id`'s containing block is right-to-left (over-constrained margins, CSS 2 §10.3.3).
    /// The legacy alignment (HTML §15.3.3 "align descendants") `id`'s parent gives its block-level
    /// children.
    pub fn cb_legacy_align(&self, id: BoxId) -> Option<LegacyAlign> {
        self.tree
            .b(id)
            .parent
            .and_then(|p| self.tree.b(p).style.legacy_align())
    }

    pub fn cb_rtl(&self, id: BoxId) -> bool {
        self.tree
            .b(id)
            .parent
            .is_some_and(|p| self.tree.b(p).style.direction() == Direction::Rtl)
    }

    /// The content-box min-content and max-content widths of `id` (cached).
    pub fn content_sizes(&mut self, id: BoxId) -> ContentSizes {
        if self.tree.b(id).style.view() != self.frame {
            return self.content_sizes_across(id);
        }
        if self.pct_height_basis.is_some() {
            // (sizes that depend on a height basis are not the box's cached ones)
            return crate::intrinsic::compute_content_sizes(self, id);
        }
        let b = self.tree.b(id);
        if let Some((pass, sizes)) = b.cache.intrinsic {
            // (a flex or grid container's percentage height follows its parent's, which may change
            // with this box clean: `flex_grid::content_sizes`)
            let follows_parent = || {
                matches!(b.kind, BoxKind::Flex | BoxKind::Grid)
                    && matches!(b.style.height(), SizeValue::LengthPercentage(l) if l.has_percent())
            };
            if pass == self.pass || (!b.is_dirty() && !follows_parent()) {
                return sizes;
            }
        }
        let sizes = crate::intrinsic::compute_content_sizes(self, id);
        self.tree.b_mut(id).cache.intrinsic = Some((self.pass, sizes));
        sizes
    }

    /// The containing block for an out-of-flow box (CSS 2 §10.1, css-position-3 §2.1): for
    /// `absolute`, the nearest positioned ancestor; for both `absolute` and `fixed`, the nearest
    /// ancestor whose style makes it the containing block of all positioned descendants (a
    /// transform, filter, layout or paint containment: [`Style::establishes_fixed_cb`]); `None` for
    /// the initial containing block. Inline boxes a block was split out of are its ancestors here.
    pub fn containing_block_of(&self, id: BoxId) -> Option<BoxId> {
        let fixed = self.tree.b(id).style.position() == Position::Fixed;
        let mut child = id;
        let mut cur = self.tree.b(id).parent;
        while let Some(c) = cur {
            for &i in self.tree.split_inline_ancestors(child) {
                // (a non-atomic inline box is not transformable: positioning counts, and a filter,
                // which Filter Effects 1 §5 applies to inline boxes too)
                if self.tree.get(i).is_some_and(|b| {
                    !fixed && b.style.position().is_positioned() || b.style.has_filter()
                }) {
                    return Some(i);
                }
            }
            let b = self.tree.b(c);
            let s = &b.style;
            if b.has(flags::ROOT) {
                // The root is the containing block of absolutely positioned boxes only when
                // positioned, and of fixed ones when transformed (a filter on the root does not
                // make it one: Filter Effects 1 §5).
                return if !fixed && s.position().is_positioned() || s.has_transform() {
                    Some(c)
                } else {
                    None
                };
            }
            // Anonymous boxes and table wrappers inherit no positioning; the table box shares the
            // element's style with its wrapper, so only the wrapper counts.
            let counts = !b.is_anonymous() && b.kind != BoxKind::Table;
            if counts {
                let positioned = !fixed && s.position().is_positioned();
                let establishes = if b.kind == BoxKind::Inline {
                    s.has_filter()
                } else {
                    s.establishes_fixed_cb()
                };
                if positioned || establishes {
                    return Some(c);
                }
            }
            child = c;
            cur = b.parent;
        }
        None
    }

    /// A new out-of-flow candidate for `id` at a static position (relative to the fragment of the box
    /// whose layout is running).
    pub fn oof_candidate(&self, id: BoxId, x: StaticPos, y: StaticPos) -> Oof {
        Oof {
            id,
            cb: self.containing_block_of(id),
            x,
            y,
        }
    }

    /// Lays out the candidates whose containing block is `cb_box` (given its padding box in its
    /// fragment's coordinates), appending their fragments to `children`; returns the rest.
    pub fn place_oofs(
        &mut self,
        cb_box: BoxId,
        padding_box: Rect,
        candidates: Vec<Oof>,
        children: &mut Vec<FragmentChild<S>>,
    ) -> Vec<Oof> {
        let mut rest = Vec::new();
        for c in candidates {
            if c.cb == Some(cb_box) {
                let (child, more) = crate::positioned::layout_absolute(self, c, padding_box);
                children.push(child);
                rest.extend(more);
            } else {
                rest.push(c);
            }
        }
        rest
    }

    /// The relative offsets of the inline boxes block-level box `id` was split out of (CSS 2
    /// §9.2.1.1: their translation also affects the block), in their containing block.
    pub fn split_inline_offset(&self, id: BoxId, cb_width: f32, cb_height: Option<f32>) -> Point {
        let mut d = Point::ZERO;
        for &i in self.tree.split_inline_ancestors(id) {
            if let Some(b) = self.tree.get(i) {
                let r = self.relative_offset(&b.style, cb_width, cb_height);
                d = Point::new(d.x + r.x, d.y + r.y);
            }
        }
        d
    }

    /// The relative-position offset of a box (css-position-3 §3.1): `left` wins over `right` in ltr,
    /// `top` over `bottom`.
    pub fn relative_offset(&self, style: &S, cb_width: f32, cb_height: Option<f32>) -> Point {
        if style.position() != Position::Relative {
            return Point::ZERO;
        }
        let rtl = style.direction() == Direction::Rtl;
        let l = style.inset(Side::Left).resolve(cb_width);
        let r = style.inset(Side::Right).resolve(cb_width);
        let x = match (l, r) {
            (Some(l), Some(r)) => {
                if rtl {
                    -r
                } else {
                    l
                }
            }
            (Some(l), None) => l,
            (None, Some(r)) => -r,
            (None, None) => 0.0,
        };
        let res = |side| match style.inset(side) {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::LengthPercentage(lp) => match cb_height {
                Some(h) => Some(lp.resolve(h)),
                // Percentages of an indefinite height compute to auto.
                None if lp.has_percent() => None,
                None => Some(lp.length),
            },
        };
        let y = match (res(Side::Top), res(Side::Bottom)) {
            (Some(t), _) => t,
            (None, Some(b)) => -b,
            (None, None) => 0.0,
        };
        Point::new(x, y)
    }
}

/// Fragment flags common to box fragments of `b`.
pub(crate) fn box_flags<S: Style>(b: &LayoutBox<S>) -> u32 {
    let s = &b.style;
    let mut f = 0;
    if b.is_float() {
        f |= fragment_flags::FLOAT;
    }
    match s.position() {
        Position::Absolute if b.is_out_of_flow() => f |= fragment_flags::ABSOLUTE,
        Position::Fixed if b.is_out_of_flow() => f |= fragment_flags::FIXED,
        Position::Sticky => f |= fragment_flags::STICKY,
        _ => {}
    }
    if s.position().is_positioned() && b.kind != BoxKind::Table {
        f |= fragment_flags::POSITIONED;
    }
    if b.has(flags::ROOT) {
        f |= fragment_flags::ROOT;
    }
    if b.has(flags::FIELDSET) {
        f |= fragment_flags::FIELDSET;
    }
    if b.has(flags::RENDERED_LEGEND) {
        f |= fragment_flags::RENDERED_LEGEND;
    }
    if b.has(flags::VIEWPORT_OVERFLOW) {
        f |= fragment_flags::VIEWPORT_OVERFLOW;
    }
    if b.has(flags::TOP_LAYER) {
        f |= fragment_flags::TOP_LAYER;
    }
    // Overflow applies to block containers, flex/grid containers and replaced elements; not to
    // inline boxes or table-internal boxes other than cells.
    let applies = matches!(
        b.kind,
        BoxKind::Block | BoxKind::Flex | BoxKind::Grid | BoxKind::TableCell | BoxKind::TableCaption
    ) || b.kind == BoxKind::Replaced;
    if applies {
        // (paint containment clips like `overflow: clip`, CSS Contain 2 §3.4)
        let paint = s.paint_containment();
        if s.overflow_x() != Overflow::Visible || paint {
            f |= fragment_flags::CLIP_X;
        }
        if s.overflow_y() != Overflow::Visible || paint {
            f |= fragment_flags::CLIP_Y;
        }
        if s.overflow_x().is_scroll_container() || s.overflow_y().is_scroll_container() {
            f |= fragment_flags::SCROLL_CONTAINER;
        }
    }
    if b.level == Level::Inline && b.kind != BoxKind::Inline {
        f |= fragment_flags::ATOMIC_INLINE;
    }
    if b.pseudo == Some(PseudoElement::Marker) {
        f |= fragment_flags::LIST_MARKER;
    }
    if b.has(flags::BACKGROUND_TO_CANVAS) {
        f |= fragment_flags::BACKGROUND_TO_CANVAS;
    }
    if b.has(flags::FLEX_GRID_ITEM) {
        f |= fragment_flags::FLEX_GRID_ITEM;
    }
    f
}

/// `f` (just made, not shared yet) with its in-flow content bounds ([`Fragment::inflow`]).
pub(crate) fn with_inflow<S: Style>(
    mut f: Arc<Fragment<S>>,
    inflow: Option<Rect>,
) -> Arc<Fragment<S>> {
    // (a fragment just made has no other owner)
    if let Some(m) = Arc::get_mut(&mut f) {
        m.inflow = inflow;
    } else {
        debug_assert!(false, "with_inflow on a shared fragment");
    }
    f
}

/// A box fragment for `id` with the given geometry and children; computes overflow.
#[allow(clippy::too_many_arguments)]
pub(crate) fn box_fragment<S: Style>(
    cx: &Cx<'_, S>,
    id: BoxId,
    size: Size,
    border: Sides,
    padding: Sides,
    children: Vec<FragmentChild<S>>,
    baseline: Option<f32>,
    last_baseline: Option<f32>,
    content: BoxContent,
    extra_flags: u32,
) -> Arc<Fragment<S>> {
    let b = cx.tree.b(id);
    let mut overflow = Rect::new(0.0, 0.0, size.width, size.height);
    let mut bounds = overflow;
    for c in &children {
        // (`offset-path` works in the containing block's boxes, seen from where the child is)
        if let Some(child) = c.fragment.as_box() {
            if child.style.transform_needs_containing() {
                let outer = Rect::new(-c.offset.x, -c.offset.y, size.width, size.height);
                let inner = outer.deflate(&border);
                let r = crate::paint::radii(&b.style, size).corners();
                child.containing.set(ContainingBoxes {
                    border: outer,
                    padding: inner,
                    content: inner.deflate(&padding),
                    radii: r.map(|c| (c.width, c.height)),
                });
            }
        }
        add_overflow(&mut overflow, &mut bounds, c);
    }
    // (a blur or a drop shadow shows past what it filters: Filter Effects 1 §10)
    if b.style.has_filter() {
        let border_box = Rect::new(0.0, 0.0, size.width, size.height);
        bounds = snowlark_paint::filter::map_rect(&b.style.filter_ops(border_box), bounds);
    }
    Arc::new(Fragment {
        size,
        kind: FragmentKind::Box(BoxFragment {
            style: b.style.clone(),
            box_id: Some(id),
            node: b.node,
            pseudo: b.pseudo,
            border,
            padding,
            flags: box_flags(b) | extra_flags,
            order: b.order,
            subtree_end: cx.tree.subtree_end(id),
            content,
            containing: Default::default(),
            clip_reference: Default::default(),
            strip: Default::default(),
            stitched: None,
        }),
        children,
        overflow,
        paint_bounds: bounds,
        inflow: None,
        baseline,
        last_baseline,
    })
}

/// LayoutNG's size-based cache hit (`CalculateSizeBasedLayoutCacheStatus` in Blink's
/// `layout_utils.cc`): a block container's cached layout holds in `space` too when the spaces differ
/// only in what its layout does not read. Those are the percentage basis of widths, margins and
/// padding when the box has no percentage there (Blink compares the resolved inline size and
/// padding); the percentage basis of heights when neither the box nor what resolves against its
/// content height has a percentage block size; and, for a box measured for its content height (a
/// flex or grid item's measure), the imposed height it is then laid out at, when that is the height
/// it measured. (Blink leaves flex, grid and fieldset containers out, whose layout does read their
/// height, and so does this; tables have rules of their own.)
pub(crate) fn same_layout_in<S: Style>(
    tree: &BoxTree<S>,
    frame: crate::logical::Flip,
    id: BoxId,
    cached: &CachedLayout<S>,
    space: &Space,
) -> bool {
    let b = tree.b(id);
    if b.kind != BoxKind::Block || b.has(flags::FIELDSET) || b.style.view() != frame {
        return false;
    }
    let old = &cached.space;
    let mut same = *old;
    if old.cb_width != space.cb_width {
        if old.edge_basis.is_some()
            || space.edge_basis.is_some()
            || own_percentage_inline_size(&b.style)
        {
            return false;
        }
        same.cb_width = space.cb_width;
    }
    let mut heights = old.cb_height != space.cb_height;
    same.cb_height = space.cb_height;
    if same != *space {
        // Measured with no imposed height, laid out at the height it measured.
        let measured = old.fixed_height.is_none()
            && space.fixed_height == Some(cached.result.fragment.size.height)
            && b.is_independent();
        if !measured {
            return false;
        }
        same.fixed_height = space.fixed_height;
        same.fixed_height_definite = space.fixed_height_definite;
        same.content_height = space.content_height;
        if same != *space {
            return false;
        }
        heights = true;
    }
    !heights || !depends_on_percentage_block_size(tree, id)
}

/// A percentage in the box's widths, margins, padding or horizontal relative offsets: what it
/// resolves against its containing block's width.
fn own_percentage_inline_size<S: Style>(s: &S) -> bool {
    let size = |v: SizeValue| match v {
        SizeValue::LengthPercentage(l) | SizeValue::FitContent(Some(l)) => l.has_percent(),
        _ => false,
    };
    let max = match s.max_width() {
        MaxSizeValue::LengthPercentage(l) | MaxSizeValue::FitContent(Some(l)) => l.has_percent(),
        _ => false,
    };
    let auto = |v: LengthPercentageAuto| match v {
        LengthPercentageAuto::LengthPercentage(l) => l.has_percent(),
        LengthPercentageAuto::Auto => false,
    };
    const SIDES: [Side; 4] = [Side::Top, Side::Right, Side::Bottom, Side::Left];
    size(s.width())
        || size(s.min_width())
        || max
        || SIDES
            .iter()
            .any(|&side| auto(s.margin(side)) || s.padding(side).has_percent())
        || (s.position() != Position::Static
            && (auto(s.inset(Side::Left)) || auto(s.inset(Side::Right))))
}

/// Whether box `id`'s layout may read its containing block's height or its own content height: it
/// has a percentage (or `stretch`) height, min-height, max-height or relative inset, or one of the
/// boxes whose percentages resolve against its content height does (its in-flow and floating
/// children, through inline boxes and anonymous blocks), or is a box whose layout takes the height
/// it is given as its available space (another writing mode, grid and table layout, a wrapping
/// column flex container). A superset of Blink's `HasDescendantThatDependsOnPercentageBlockSize`,
/// taken from the styles rather than recorded by the layout.
pub(crate) fn depends_on_percentage_block_size<S: Style>(tree: &BoxTree<S>, id: BoxId) -> bool {
    let b = tree.b(id);
    own_percentage_block_size(&b.style)
        || b.children
            .iter()
            .any(|&c| child_depends_on_percentage_block_size(tree, c, b.style.view()))
}

fn child_depends_on_percentage_block_size<S: Style>(
    tree: &BoxTree<S>,
    id: BoxId,
    parent_view: crate::logical::Flip,
) -> bool {
    let b = tree.b(id);
    // (an absolutely positioned box resolves against its containing block's padding box, laid out
    // at the same height either way)
    if b.is_out_of_flow() || b.kind == BoxKind::Text || b.kind == BoxKind::LineBreak {
        return false;
    }
    let s = &b.style;
    if own_percentage_block_size(s) || s.view() != parent_view {
        return true;
    }
    match b.kind {
        BoxKind::Grid | BoxKind::TableWrapper | BoxKind::Table => true,
        BoxKind::Flex => {
            matches!(
                s.flex_direction(),
                FlexDirection::Column | FlexDirection::ColumnReverse
            ) && s.flex_wrap() != FlexWrap::Nowrap
        }
        BoxKind::Inline => b
            .children
            .iter()
            .any(|&c| child_depends_on_percentage_block_size(tree, c, parent_view)),
        _ if b.is_anonymous() => b
            .children
            .iter()
            .any(|&c| child_depends_on_percentage_block_size(tree, c, parent_view)),
        _ => false,
    }
}

/// A percentage or `stretch` in the box's block-axis sizes, or a percentage in its block-axis
/// relative offsets.
fn own_percentage_block_size<S: Style>(s: &S) -> bool {
    let lp = |v: SizeValue| match v {
        SizeValue::LengthPercentage(l) | SizeValue::FitContent(Some(l)) => l.has_percent(),
        SizeValue::Stretch => true,
        _ => false,
    };
    let max = match s.max_height() {
        MaxSizeValue::LengthPercentage(l) | MaxSizeValue::FitContent(Some(l)) => l.has_percent(),
        MaxSizeValue::Stretch => true,
        _ => false,
    };
    let inset = |side| match s.inset(side) {
        LengthPercentageAuto::LengthPercentage(l) => l.has_percent(),
        LengthPercentageAuto::Auto => false,
    };
    lp(s.height())
        || lp(s.min_height())
        || max
        || (matches!(s.position(), Position::Relative | Position::Sticky)
            && (inset(Side::Top) || inset(Side::Bottom)))
}
