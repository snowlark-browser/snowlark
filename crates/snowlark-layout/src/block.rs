// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Block layout (CSS 2 §9.4.1, §10.3.3, §10.6.3, §10.6.7, §8.3.1; css-sizing-3; css-box-4): widths
//! and auto margins, heights with min/max, margin collapsing through struts, floats and clearance in
//! the shared block formatting context, BFC roots avoiding floats, and the outside list marker.
//!
//! A box that doesn't establish a BFC is laid out at an *estimated* BFC position (its parent assumes its
//! top margin collapses only with the margins before it); when its real position differs and floats are
//! involved, it is laid out again at the right place (as WebKit and Gecko do), with the floats it added
//! rolled back first.

use std::sync::Arc;

use snowlark_paint::{Point, Rect, Sides, Size};

use crate::boxes::*;
use crate::floats::{ClearSide, Exclusions, FloatSide};
use crate::fragment::*;
use crate::layout::*;
use crate::sizing::*;
use crate::style::*;

/// The used border-box width, margins and content height constraints of a block container.
pub(crate) struct BlockSizes {
    pub edges: Edges,
    pub width: f32,
    pub margin_left: f32,
    pub margin_right: f32,
    /// Border-box height when definite (specified or imposed).
    pub height: Option<f32>,
    pub min_height: Option<f32>,
    pub max_height: Option<f32>,
    /// The space the box's custom scroll bars take ([`crate::gutter`]): between its border and
    /// padding, out of its content box.
    pub gutter: Sides,
}

/// The value a box's `width` is used as: a button's `auto` inline size is the fit-content inline
/// size (HTML §15.5.3 button layout), so a block-level button shrinks to fit as an inline-block
/// does; any other value is the computed one.
pub(crate) fn used_inline_size<S: Style>(cx: &Cx<'_, S>, id: BoxId, width: SizeValue) -> SizeValue {
    match width {
        SizeValue::Auto if cx.tree.b(id).has(flags::BUTTON) => SizeValue::FitContent(None),
        w => w,
    }
}

/// Resolves a block-level (or shrink-to-fit) box's width and margins (CSS 2 §10.3.3, §10.3.5, §10.4).
pub(crate) fn block_sizes<S: Style>(cx: &mut Cx<'_, S>, id: BoxId, space: &Space) -> BlockSizes {
    let style = cx.tree.b(id).style.clone();
    let mut edges = Edges::of(&style, space.edges_basis());
    if let Some(b) = space.collapsed_border {
        edges.border = b;
    }
    let pb = edges.pb_h();
    let mut rtl = cx.cb_rtl(id);
    if let Some(p) = cx.tree.b(id).parent {
        // (the containing block's start is at this frame's other end when the two run opposite ways)
        let (own, outer) = (style.view(), cx.tree.b(p).style.view());
        if own != outer {
            let m = crate::logical::Flip::between(own, outer);
            rtl ^= !m.swaps() && m.reverses_x();
        }
    }
    let available = space.available;
    let stretch = (available - edges.margin_h()).max(0.0);
    let justify = if space.mode == WidthMode::Fill && space.fixed_width.is_none() {
        justify_self_of(cx, id, &style)
    } else {
        None
    };
    // A shrink-to-fit width through `aspect-ratio` from a definite height (css-sizing-4 §2.2).
    // (in an orthogonal flow; elsewhere the content's size still counts, css-sizing-4 §5.1)
    let shrinks = space.edge_basis.is_some();
    let ratio_width = shrinks
        .then(|| style_ratio(&style))
        .flatten()
        .and_then(|r| {
            let pbv = edges.pb_v();
            let sizing = style.box_sizing();
            let h = space
                .fixed_height
                .or_else(|| resolve_height(style.height(), space.cb_height, pbv, sizing))?;
            Some(match sizing {
                BoxSizing::BorderBox => (h * r).max(pb),
                BoxSizing::ContentBox => (h - pbv).max(0.0) * r + pb,
            })
        });
    let width_value = used_inline_size(cx, id, style.width());
    let mut sizes_cache: Option<ContentSizes> = None;
    let mut content = || {
        if let Some(c) = sizes_cache {
            return c;
        }
        // (an orthogonal flow's definite block size is there for percentages in its content)
        let c = match space.fixed_height.filter(|_| space.edge_basis.is_some()) {
            Some(h) => {
                let saved = cx.pct_height_basis.replace((h - edges.pb_v()).max(0.0));
                let c = cx.content_sizes(id);
                cx.pct_height_basis = saved;
                c
            }
            None => cx.content_sizes(id),
        };
        sizes_cache = Some(c);
        c
    };
    let basis = (!space.inline_pct_auto).then_some(space.cb_width);
    let (width, margin_left, margin_right) = if let Some(w) = space.fixed_width {
        (w, edges.ml(), edges.mr())
    } else {
        let sizing = style.box_sizing();
        let specified = resolve_width(width_value, basis, pb, sizing, stretch, &mut content);
        let (min, max) = width_bounds(&style, basis, pb, stretch, &mut content);
        let auto_width = specified.is_none();
        let w = match specified {
            Some(w) => w,
            None => match space.mode {
                WidthMode::Fill if justify.is_none() => stretch,
                _ => match ratio_width {
                    Some(w) => w,
                    None => content().shrink_to_fit((stretch - pb).max(0.0)) + pb,
                },
            },
        };
        let w = clamp(w, min, max).max(pb);
        if space.mode == WidthMode::Fill && !(auto_width && w == stretch) {
            let legacy = cx.cb_legacy_align(id);
            let (l, r) = resolve_auto_margins(&edges, w, available, rtl, legacy);
            match justify {
                // (auto margins take the free space before alignment does)
                Some(j) if edges.margin[1].is_some() && edges.margin[3].is_some() => {
                    let free = available - w - edges.margin_h();
                    let self_rtl = style.direction() == Direction::Rtl;
                    let safe = style.align_safety().justify_self == OverflowPosition::Safe;
                    let d = justify_shift(j, free, rtl, self_rtl, safe);
                    (w, edges.ml() + d, edges.mr() + free - d)
                }
                _ => (w, l, r),
            }
        } else {
            (w, edges.ml(), edges.mr())
        }
    };
    let pbv = edges.pb_v();
    let sizing = style.box_sizing();
    let (min_height, max_height) = if space.content_height {
        (None, None)
    } else {
        height_bounds(&style, space.cb_height, pbv)
    };
    let height = match space.fixed_height {
        Some(h) => Some(h),
        None => (!space.content_height)
            .then(|| {
                // (`stretch` fills the containing block less the margins, css-sizing-4 §6)
                let basis = match style.height() {
                    SizeValue::Stretch => space.cb_height.map(|h| {
                        let (over, under) = uncollapsed_margins(cx, id, &edges);
                        h - over - under
                    }),
                    _ => space.cb_height,
                };
                resolve_height(style.height(), basis, pbv, sizing)
            })
            .flatten()
            .or_else(|| {
                // aspect-ratio with an auto height (css-sizing-4 §2.2)
                style_ratio(&style).map(|r| width / r)
            })
            .map(|h| clamp(h, min_height, max_height)),
    };
    BlockSizes {
        edges,
        width,
        margin_left,
        margin_right,
        height,
        min_height,
        max_height,
        gutter: crate::gutter::gutter(cx.tree.b(id)),
    }
}

/// The block-axis margins of `id` that do not adjoin its parent's edges (css-sizing-4 §6.1: margins
/// that collapse through the containing block's edges take no room from a stretched box).
pub(crate) fn uncollapsed_margins<S: Style>(
    cx: &Cx<'_, S>,
    id: BoxId,
    edges: &Edges,
) -> (f32, f32) {
    let b = cx.tree.b(id);
    let Some(p) = b.parent else {
        return (edges.mt(), edges.mb());
    };
    let pb = cx.tree.b(p);
    if pb.is_independent() {
        return (edges.mt(), edges.mb());
    }
    let pe = Edges::of(&pb.style, 0.0);
    let top = pe.border.top + pe.padding.top != 0.0;
    let bottom = pe.border.bottom + pe.padding.bottom != 0.0;
    (
        if top { edges.mt() } else { 0.0 },
        if bottom { edges.mb() } else { 0.0 },
    )
}

/// The `justify-self` of a block-level box that aligns it in its containing block (css-align-3
/// §6.1.2): `auto` is its parent's `justify-items`; `None` for the default stretching behavior.
pub(crate) fn justify_self_of<S: Style>(cx: &Cx<'_, S>, id: BoxId, style: &S) -> Option<Align> {
    let a = match style.justify_self() {
        // (`justify-items` is its parent's, and an inline box a block was split out of has none;
        // an anonymous block has no `justify-self` of its own to resolve)
        Align::Auto
            if cx.tree.b(id).is_anonymous() || !cx.tree.split_inline_ancestors(id).is_empty() =>
        {
            Align::Normal
        }
        Align::Auto => cx
            .tree
            .b(id)
            .parent
            .map_or(Align::Normal, |p| cx.tree.b(p).style.justify_items()),
        a => a,
    };
    matches!(
        a,
        Align::Start
            | Align::End
            | Align::FlexStart
            | Align::FlexEnd
            | Align::Center
            | Align::SelfStart
            | Align::SelfEnd
            | Align::Left
            | Align::Right
    )
    .then_some(a)
}

/// How far from the left of `free` space a box aligned by `a` goes, its containing block being
/// right-to-left if `rtl`, itself if `self_rtl`. `safe`: overflowing, it stays at the start edge.
pub(crate) fn justify_shift(a: Align, free: f32, rtl: bool, self_rtl: bool, safe: bool) -> f32 {
    if safe && free < 0.0 {
        return if rtl { free } else { 0.0 };
    }
    let end = |r: bool| if r { 0.0 } else { free };
    let start = |r: bool| if r { free } else { 0.0 };
    match a {
        Align::Center => free / 2.0,
        Align::Left => 0.0,
        Align::Right => free,
        Align::End | Align::FlexEnd => end(rtl),
        Align::SelfStart => start(self_rtl),
        Align::SelfEnd => end(self_rtl),
        _ => start(rtl),
    }
}

pub(crate) fn clear_side<S: Style>(s: &S) -> Option<ClearSide> {
    let rtl = s.direction() == Direction::Rtl;
    match s.clear() {
        Clear::None => None,
        Clear::Left => Some(ClearSide::Left),
        Clear::Right => Some(ClearSide::Right),
        Clear::Both => Some(ClearSide::Both),
        Clear::InlineStart => Some(if rtl {
            ClearSide::Right
        } else {
            ClearSide::Left
        }),
        Clear::InlineEnd => Some(if rtl {
            ClearSide::Left
        } else {
            ClearSide::Right
        }),
    }
}

pub(crate) fn float_side<S: Style>(s: &S, cb_rtl: bool) -> FloatSide {
    match s.float() {
        Float::Right => FloatSide::Right,
        Float::InlineEnd if !cb_rtl => FloatSide::Right,
        Float::InlineStart if cb_rtl => FloatSide::Right,
        _ => FloatSide::Left,
    }
}

/// Lays out a float in its containing block (whose content box spans `left..right` in BFC
/// coordinates), no higher than `min_top`. Returns its fragment offset in BFC coordinates.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_float<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    cb_width: f32,
    cb_height: Option<f32>,
    cb_rtl: bool,
    bfc: &mut Exclusions,
    left: f32,
    right: f32,
    min_top: f32,
) -> (Point, Arc<Fragment<S>>, Vec<Oof>) {
    let space = Space::shrink(cb_width, cb_height, cb_width);
    let result = cx.layout(id, &space, None, Point::ZERO);
    let style = cx.tree.b(id).style.clone();
    let side = float_side(&style, cb_rtl);
    let m = result.margin;
    let size = result.fragment.size;
    let mut min_top = min_top;
    if let Some(clear) = clear_side(&style) {
        if let Some(y) = bfc.clearance_y(clear) {
            min_top = min_top.max(y);
        }
    }
    let rect = bfc.place(
        side,
        (size.width + m.left + m.right).max(0.0),
        (size.height + m.top + m.bottom).max(0.0),
        min_top,
        left,
        right,
    );
    // CSS Shapes 1 §3: lines beside the float wrap around its float area, in its containing
    // block's writing mode (the float's own may differ).
    if style.has_shape_outside() {
        use crate::logical::Flip;
        let frame = cx.tree.b(id).parent().map_or(Flip::IDENTITY, |p| {
            Flip::of(cx.tree.b(p).style.writing_mode())
        });
        if let Some(shape) = crate::shapes::float_shape(&style.with_view(frame), size, m, cb_width)
        {
            bfc.set_last_shape(shape);
        }
    }
    // A relatively positioned float is shifted after it is placed (CSS Position 3 §3.1: the
    // offset does not affect the flow, the floats' exclusions included).
    let rel = cx.relative_offset(&style, cb_width, cb_height);
    (
        Point::new(rect.x + m.left + rel.x, rect.y + m.top + rel.y),
        result.fragment,
        result.oof,
    )
}

/// Lays out a block container: CSS 2 §10.3.3 widths, its block children or inline content, §10.6.3
/// heights. A line-clamp container (CSS Overflow 4 §4) is laid out twice when its lines need
/// clamping: once to count them, once with the clamp point.
pub(crate) fn layout_block_container<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    space: &Space,
    parent_bfc: Option<&mut Exclusions>,
    at: Point,
) -> LayoutResult<S> {
    let b = cx.tree.b(id);
    let clamp = if b.kind == BoxKind::Block && b.is_independent() {
        b.style.line_clamp()
    } else {
        None
    };
    let Some(clamp) = clamp else {
        // CSS Multi-column Layout 1: a multi-column container lays its content out in columns.
        if b.kind == BoxKind::Block && b.is_independent() && b.style.is_multicol() {
            if let Some(r) = crate::multicol::layout_multicol(cx, id, space, at) {
                return r;
            }
        }
        return layout_block_container_box(cx, id, space, parent_bfc, at);
    };
    let sizes = block_sizes(cx, id, space);
    let edges = sizes.edges;
    let content_top = edges.border.top + edges.padding.top;
    let outer = cx.clamp.replace(Clamp {
        content_top,
        ..Clamp::default()
    });
    let first = layout_block_container_box(cx, id, space, None, at);
    let counted = cx.clamp.take().unwrap_or_default();
    // Where the clamp point goes: after `max-lines` lines, and for `auto` after the last point
    // that fits in the box's block size (or maximum block size).
    let limit = if clamp.auto {
        sizes
            .height
            .or(sizes.max_height)
            .map(|h| (h - edges.pb_v()).max(0.0))
    } else {
        None
    };
    let result = match counted.clamp_point(clamp.lines, limit) {
        None => first,
        Some(after) => {
            cx.clamp = Some(Clamp {
                content_top,
                after: Some(after),
                // (no point fits: nothing is shown)
                reached: after == 0,
                planned: counted.points,
                ellipsis: clamp.ellipsis,
                ..Clamp::default()
            });
            let mut r = layout_block_container_box(cx, id, space, None, at);
            if let Some(f) = Arc::get_mut(&mut r.fragment) {
                if let FragmentKind::Box(b) = &mut f.kind {
                    b.flags |= fragment_flags::LINE_CLAMPED;
                }
            }
            r
        }
    };
    cx.clamp = outer;
    result
}

/// [`layout_block_container`] of one box.
pub(crate) fn layout_block_container_box<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    space: &Space,
    parent_bfc: Option<&mut Exclusions>,
    at: Point,
) -> LayoutResult<S> {
    let sizes = block_sizes(cx, id, space);
    let b = cx.tree.b(id);
    let style = b.style.clone();
    let independent = b.is_independent() || parent_bfc.is_none();
    let inline_content = b.has_inline_content();
    // (the space custom scroll bars take is padding to the content's layout: CSS Overflow 3 §3)
    let gutter = sizes.gutter;
    let mut edges = sizes.edges;
    edges.padding = Sides::new(
        edges.padding.top + gutter.top,
        edges.padding.right + gutter.right,
        edges.padding.bottom + gutter.bottom,
        edges.padding.left + gutter.left,
    );
    let border = edges.border;
    let padding = edges.padding;
    // (inside a line-clamp container, a clamp point in this box must leave room for its block-end
    // border and padding)
    let clamp_bp = if independent {
        0.0
    } else {
        border.bottom + padding.bottom
    };
    if let Some(c) = cx.clamp.as_mut() {
        c.end_bp += clamp_bp;
    }
    // CSS Inline 3 §6.1: the box's own `text-box-trim` (its border and padding do not intervene),
    // else what an ancestor asks of its first/last formatted line.
    let own_trim = style.text_box_trim();
    let trim = TrimRequest {
        start: own_trim.start || space.trim.start,
        end: own_trim.end || space.trim.end,
    };
    let content_width = (sizes.width - edges.pb_h()).max(0.0);
    let content_height = sizes.height.map(|h| (h - edges.pb_v()).max(0.0));
    // Percentages of children's heights resolve against a definite height only.
    let child_cb_height = if space.fixed_height.is_some() && !space.fixed_height_definite {
        None
    } else {
        content_height
    };

    let mut own_bfc;
    let (bfc, origin): (&mut Exclusions, Point) = if independent {
        own_bfc = Exclusions::new();
        (&mut own_bfc, Point::ZERO)
    } else {
        // The border box's left edge in BFC coordinates.
        (
            parent_bfc.unwrap(),
            Point::new(at.x + sizes.margin_left, at.y),
        )
    };
    let floats_before = bfc.len();
    // HTML §15.3.13: a fieldset's rendered legend sits in its block-start border, which grows to
    // the legend's height; the content starts below it.
    let mut legend_child: Option<FragmentChild<S>> = None;
    let mut legend_shift = 0.0;
    if cx.tree.b(id).has(flags::FIELDSET) {
        let legend = cx
            .tree
            .b(id)
            .children
            .iter()
            .copied()
            .find(|&c| cx.tree.b(c).has(flags::RENDERED_LEGEND));
        if let Some(l) = legend {
            let ls = Space {
                cb_width: content_width,
                cb_height: None,
                available: content_width,
                mode: WidthMode::ShrinkToFit,
                fixed_width: None,
                fixed_height: None,
                fixed_height_definite: false,
                collapsed_border: None,
                content_height: false,
                trim: TrimRequest::default(),
                edge_basis: None,
                inline_pct_auto: false,
                content_width: false,
            };
            let r = cx.layout(l, &ls, None, Point::ZERO);
            let margin_h = r.margin.top + r.fragment.size.height + r.margin.bottom;
            let area = border.top.max(margin_h);
            legend_shift = area - border.top;
            let margin_w = r.margin.left + r.fragment.size.width + r.margin.right;
            let free = (content_width - margin_w).max(0.0);
            // `justify-self` places it in the inline axis (the UA sheet maps `align`).
            let rtl = style.direction() == Direction::Rtl;
            let x = match (cx.tree.b(l).style.justify_self(), rtl) {
                (Align::Center, _) => free / 2.0,
                (Align::Right, _) => free,
                (Align::Left, _) => 0.0,
                (Align::End, false) => free,
                (Align::End, true) => 0.0,
                (_, true) => free,
                _ => 0.0,
            };
            legend_child = Some(FragmentChild {
                offset: Point::new(
                    border.left + padding.left + x + r.margin.left,
                    (area - margin_h) / 2.0 + r.margin.top,
                ),
                fragment: r.fragment,
                stacking_owner: 0,
            });
        }
    }
    let content_origin = Point::new(
        origin.x + border.left + padding.left,
        origin.y + border.top + legend_shift + padding.top,
    );

    let mut children: Vec<FragmentChild<S>> = Vec::new();
    let mut oof: Vec<Oof> = Vec::new();
    let baseline;
    let last_baseline;
    let collapse_top = !independent && border.top == 0.0 && padding.top == 0.0;
    let height_auto = sizes.height.is_none();
    let collapse_bottom = !independent
        && border.bottom == 0.0
        && padding.bottom == 0.0
        && height_auto
        && sizes.min_height.is_none_or(|m| m <= edges.pb_v());

    let mut start = Strut::of(edges.mt());
    let mut end = Strut::of(edges.mb());
    let mut through = false;

    // The in-flow content's bounds (CSS Overflow 3 §2.2), in the border box's coordinates.
    let mut inflow: Option<Rect> = None;
    let mut has_spanner = false;
    let content_bottom = if inline_content {
        let marker = cx.tree.b(id).marker;
        let out = crate::inline::layout_inline_content(
            cx,
            id,
            content_width,
            child_cb_height,
            bfc,
            content_origin,
            marker,
            trim,
        );
        let inset = Point::new(
            border.left + padding.left,
            border.top + legend_shift + padding.top,
        );
        for mut c in out.children {
            c.offset.x += inset.x;
            c.offset.y += inset.y;
            if let Some(r) = c.fragment.inflow {
                let r = r.translate(c.offset.x, c.offset.y);
                inflow = Some(inflow.map_or(r, |a| unite(a, r)));
            }
            children.push(c);
        }
        for o in out.oof {
            oof.push(o.shifted(inset.x, inset.y));
        }
        baseline = out.first_baseline.map(|b| b + inset.y);
        last_baseline = out.last_baseline.map(|b| b + inset.y);
        let mut h = out.height;
        if !out.has_content {
            // Only phantom lines: the box may be empty for margin collapsing.
            h = 0.0;
            if collapse_top && collapse_bottom && sizes.min_height.is_none_or(|m| m <= 0.0) {
                through = true;
            }
        }
        h
    } else {
        let r = layout_block_children(
            cx,
            id,
            &style,
            content_width,
            child_cb_height,
            bfc,
            content_origin,
            collapse_top,
            collapse_bottom,
            trim,
            &mut children,
            &mut oof,
        );
        let inset = Point::new(
            border.left + padding.left,
            border.top + legend_shift + padding.top,
        );
        for c in &mut children {
            c.offset.x += inset.x;
            c.offset.y += inset.y;
        }
        for o in &mut oof {
            *o = o.shifted(inset.x, inset.y);
        }
        // (block-end margins that collapse inside the box are part of its content height)
        inflow = r.inflow_x.map(|(lo, hi)| {
            Rect::from_ltrb(
                lo + inset.x,
                inset.y,
                hi + inset.x,
                inset.y + r.height.max(0.0),
            )
        });
        baseline = r.first_baseline.map(|b| b + inset.y);
        last_baseline = r.last_baseline.map(|b| b + inset.y);
        has_spanner = r.spanner;
        if collapse_top {
            start = start.join(r.top_strut);
        }
        if r.empty {
            if collapse_top && collapse_bottom && sizes.min_height.is_none_or(|m| m <= 0.0) {
                through = true;
                end = end.join(r.bottom_strut);
            }
        } else if collapse_bottom {
            end = end.join(r.bottom_strut);
        }
        r.height
    };

    // Out-of-flow boxes whose containing block is an inline box laid out here (CSS 2 §10.1 item 4,
    // css-position-3 §2.1): positioned against the rectangle its fragments span.
    if oof.iter().any(|o| {
        o.cb.is_some_and(|c| {
            cx.tree.b(c).kind == BoxKind::Inline && inline_cb_owner(cx, c) == Some(id)
        })
    }) {
        let (inline_cb, rest): (Vec<Oof>, Vec<Oof>) =
            std::mem::take(&mut oof).into_iter().partition(|o| {
                o.cb.is_some_and(|c| {
                    cx.tree.b(c).kind == BoxKind::Inline && inline_cb_owner(cx, c) == Some(id)
                })
            });
        oof = rest;
        for o in inline_cb {
            let cb = o.cb.unwrap();
            let pad = inline_padding_box(cx, cb, &children);
            let (child, more) = crate::positioned::layout_absolute(cx, o, pad, &children);
            children.push(child);
            oof.extend(more);
        }
    }

    // The outside marker of a list item whose content is blocks: beside the first line inside.
    if !inline_content {
        if let Some(m) = cx.tree.b(id).marker {
            let inset = Point::new(
                border.left + padding.left,
                border.top + legend_shift + padding.top,
            );
            let child = layout_outside_marker(cx, m, &style, content_width, baseline, inset);
            children.push(child);
        }
    }

    // A BFC root's auto height includes its floats (CSS 2 §10.6.7).
    let mut auto_content = content_bottom;
    if independent {
        if let Some(fb) = bfc.bottom() {
            auto_content = auto_content.max(fb - content_origin.y);
        }
    }
    // CSS Contain 2 §3.1: under block-size containment the auto height is that of the box with no
    // content (not for internal table boxes).
    let contained = style.size_containment();
    if contained.height && cx.tree.b(id).kind == BoxKind::Block {
        auto_content = contained.intrinsic_height.unwrap_or(0.0);
    }
    let used_height = match sizes.height {
        Some(h) => h,
        None => clamp(
            auto_content.max(0.0) + edges.pb_v() + legend_shift,
            sizes.min_height,
            sizes.max_height,
        ),
    };
    // CSS Box Alignment 3 §5.4: `align-content` on a block container aligns its content as a
    // whole in its content box (here for boxes that establish an independent formatting context,
    // as buttons do); `normal` is `start` (a button's: centered, below),
    // distribution values take their fallback, and content that does not fit stays at the start
    // (the default overflow alignment keeps it out of the unscrollable start side).
    let mut baseline = baseline;
    let mut last_baseline = last_baseline;
    let cell = cx.tree.b(id).kind == BoxKind::TableCell;
    // (`vertical-align` of a cell in another writing mode than its table: its table cannot move the
    // content in the cell's block axis, `table::layout_table_grid` does it for the others)
    let foreign_cell = cell
        && cx
            .tree
            .b(id)
            .parent
            .is_some_and(|p| cx.tree.b(p).style.view() != style.view());
    if independent && (!cell || foreign_cell) {
        let box_height = used_height - edges.pb_v() - legend_shift;
        let free = box_height - auto_content.max(0.0);
        let shift = if cell {
            match style.vertical_align() {
                VerticalAlign::Middle => free / 2.0,
                VerticalAlign::Bottom => free,
                _ => 0.0,
            }
        } else {
            // (a button's content box is centered in the block axis when it fits, HTML §15.5.3
            // button layout; `align-content` keeps its computed `normal`)
            let button = cx.tree.b(id).has(flags::BUTTON);
            match style.align_content() {
                Align::Normal if button => free / 2.0,
                Align::Center | Align::SpaceAround | Align::SpaceEvenly => free / 2.0,
                Align::End | Align::FlexEnd => free,
                _ => 0.0,
            }
        };
        if shift > 0.0 && shift.is_finite() {
            for c in &mut children {
                c.offset.y += shift;
            }
            for o in &mut oof {
                *o = o.shifted(0.0, shift);
            }
            inflow = inflow.map(|r| r.translate(0.0, shift));
            baseline = baseline.map(|b| b + shift);
            last_baseline = last_baseline.map(|b| b + shift);
        }
    }
    if let Some(l) = legend_child {
        children.insert(0, l);
    }
    if through && used_height > 0.0 {
        through = false;
    }

    // Floats placed in our parent's BFC by our subtree, relative to our border box.
    let mut escaped = Vec::new();
    if !independent {
        for f in &bfc.floats[floats_before..] {
            let mut f = f.clone();
            f.rect = f.rect.translate(-at.x, -at.y);
            escaped.push(f);
        }
    }

    let size = Size::new(sizes.width, used_height);
    let mut frag_border = border;
    // Table cells and captions paint their own borders; anything else as computed.
    frag_border.top = border.top;
    if oof.iter().any(|o| o.cb == Some(id)) {
        // (the padding box, inside the scroll bars)
        let pad = Rect::new(
            border.left + gutter.left,
            border.top,
            (size.width - border.left - border.right - gutter.left - gutter.right).max(0.0),
            (size.height - border.top - border.bottom - gutter.bottom).max(0.0),
        );
        oof = cx.place_oofs(id, pad, oof, &mut children);
    }
    let mut extra = if cx.tree.b(id).kind == BoxKind::TableCell {
        fragment_flags::TABLE_CELL
    } else {
        0
    };
    if has_spanner {
        extra |= fragment_flags::HAS_SPANNER;
    }
    // An inline-block's baseline is its last line box's, unless it clips (CSS 2 §10.8.1).
    if let Some(c) = cx.clamp.as_mut() {
        c.end_bp -= clamp_bp;
    }
    let fragment = with_scrollbar(
        with_inflow(
            box_fragment(
                cx,
                id,
                size,
                frag_border,
                sizes.edges.padding,
                children,
                baseline,
                last_baseline,
                BoxContent::None,
                extra,
            ),
            inflow,
        ),
        gutter,
    );
    LayoutResult {
        fragment,
        margin: Sides::new(
            edges.mt(),
            sizes.margin_right,
            edges.mb(),
            sizes.margin_left,
        ),
        start,
        end,
        through,
        floats: escaped,
        oof,
    }
}

/// What laying out a block container's block children gives.
pub(crate) struct ChildrenResult {
    /// Content height (from the content-box top).
    pub height: f32,
    /// No in-flow content separated the top and bottom (all children collapsed through).
    pub empty: bool,
    /// Margins that collapse through the container's top (when it can collapse with its children).
    pub top_strut: Strut,
    /// Margins pending at the bottom (collapse with the container's bottom margin when allowed).
    pub bottom_strut: Strut,
    pub first_baseline: Option<f32>,
    pub last_baseline: Option<f32>,
    /// The horizontal extent of the in-flow children's margin boxes (before relative
    /// positioning), from the content-box left; `None` without in-flow children.
    pub inflow_x: Option<(f32, f32)>,
    /// A spanner of a multi-column container is among the children or inside them.
    pub spanner: bool,
}

/// Lays out the block-level children of a block container in its content box (offsets relative to
/// the content box), collapsing margins (CSS 2 §8.3.1) and placing floats.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_block_children<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    style: &S,
    content_width: f32,
    cb_height: Option<f32>,
    bfc: &mut Exclusions,
    content_origin: Point,
    collapse_top: bool,
    collapse_bottom: bool,
    trim: TrimRequest,
    out: &mut Vec<FragmentChild<S>>,
    oof: &mut Vec<Oof>,
) -> ChildrenResult {
    let rtl = style.direction() == Direction::Rtl;
    let kids = cx.tree.b(id).children.clone();
    // The children `text-box-trim` passes to (the first and last in-flow block-level ones).
    let in_flow = |b: &LayoutBox<S>| {
        !b.is_out_of_flow()
            && !b.is_float()
            && !b.has(flags::RENDERED_LEGEND)
            && !matches!(b.kind, BoxKind::TableColumn | BoxKind::TableColumnGroup)
    };
    let (first_in_flow, last_in_flow) = if trim == TrimRequest::default() {
        (None, None)
    } else {
        (
            kids.iter().position(|&c| in_flow(cx.tree.b(c))),
            kids.iter().rposition(|&c| in_flow(cx.tree.b(c))),
        )
    };
    let left = content_origin.x;
    let right = content_origin.x + content_width;
    let mut at_top = collapse_top;
    let mut top_strut = Strut::default();
    let mut pending = Strut::default();
    let mut y = 0.0f32;
    let mut first_baseline = None;
    let mut last_baseline = None;
    let mut inflow_x: Option<(f32, f32)> = None;
    let mut spanner = false;
    let place_float = |cx: &mut Cx<'_, S>,
                       bfc: &mut Exclusions,
                       f: BoxId,
                       at_y: f32,
                       out: &mut Vec<FragmentChild<S>>,
                       oof: &mut Vec<Oof>| {
        let (pos, frag, f_oof) = layout_float(
            cx,
            f,
            content_width,
            cb_height,
            rtl,
            bfc,
            left,
            right,
            content_origin.y + at_y,
        );
        let off = Point::new(pos.x - content_origin.x, pos.y - content_origin.y);
        for o in f_oof {
            oof.push(o.shifted(off.x, off.y));
        }
        out.push(FragmentChild {
            offset: off,
            fragment: frag,
            stacking_owner: 0,
        });
    };

    for (ci, child) in kids.into_iter().enumerate() {
        let cb = cx.tree.b(child);
        if cb.has(flags::RENDERED_LEGEND) {
            // Laid out by the fieldset in its border.
            continue;
        }
        // CSS Overflow 4 §4: past a line-clamp container's clamp point, in-flow content and
        // floats are not laid out.
        if cx.clamp.as_ref().is_some_and(|c| c.reached) && !cb.is_out_of_flow() {
            continue;
        }
        if cb.is_out_of_flow() {
            let sy = if at_top { 0.0 } else { y + pending.resolve() };
            let sx = if cb.has(flags::ORIGINALLY_INLINE) {
                // An originally inline-level box among blocks: where it would be on a line of
                // its own (an anonymous block's), beside the floats there and aligned by
                // `text-align` (css-position-3 §4.2).
                let band = bfc.band(content_origin.y + sy, 0.0, left, right);
                let x = match (style.text_align(), rtl) {
                    (TextAlign::Center, _) => (band.left + band.right) / 2.0,
                    (TextAlign::Right, _) | (TextAlign::End, false) => band.right,
                    (TextAlign::Left, _) | (TextAlign::End, true) => band.left,
                    (_, true) => band.right,
                    _ => band.left,
                };
                StaticPos::at(x - content_origin.x)
            } else {
                StaticPos::block_level(content_width)
            };
            let c = cx.oof_candidate(child, sx, StaticPos::at(sy));
            oof.push(c);
            continue;
        }
        if cb.is_float() {
            // A float goes where the next box would start, after the margins before it (and no
            // higher than earlier floats, CSS 2 §9.5.1).
            let fy = if at_top { 0.0 } else { y + pending.resolve() };
            place_float(cx, bfc, child, fy, out, oof);
            continue;
        }
        if cb.kind == BoxKind::TableColumn || cb.kind == BoxKind::TableColumnGroup {
            continue;
        }
        if cx.column_flow && cb.level == Level::Block && cb.style.column_span_all() {
            // CSS Multi-column Layout 1 §6: a spanner, laid out by the multi-column container
            // across its columns. Here it only marks where the content before it ends: the margins
            // before it end at the break before it, and nothing after it collapses with anything
            // before.
            if at_top {
                at_top = false;
                top_strut = top_strut.join(pending);
            }
            pending = Strut::default();
            spanner = true;
            let mark = box_fragment(
                cx,
                child,
                Size::new(content_width, 0.0),
                Sides::default(),
                Sides::default(),
                Vec::new(),
                None,
                None,
                BoxContent::None,
                fragment_flags::SPANNER,
            );
            out.push(FragmentChild {
                offset: Point::new(0.0, y),
                fragment: mark,
                stacking_owner: 0,
            });
            continue;
        }
        let child_style = cx.style_here(child);
        let independent = cb.is_independent();
        let child_mt = child_style.margin(Side::Top).resolve_or_zero(content_width);
        // The estimated border-box top: its own top margin collapses with what's pending.
        let estimate = if at_top {
            0.0
        } else {
            let mut s = pending;
            s.add(child_mt);
            y + s.resolve()
        };
        let floats_mark = bfc.len();
        let floats_before = bfc.any_below(content_origin.y + estimate.min(y));

        // Clearance (CSS 2 §9.5.2): the border edge goes below the floats it clears.
        let mut clearance_top: Option<f32> = None;
        if let Some(side) = clear_side(&child_style) {
            if let Some(cy) = bfc.clearance_y(side) {
                let cy = cy - content_origin.y;
                if cy > estimate {
                    clearance_top = Some(cy);
                }
            }
        }
        let mut top = clearance_top.unwrap_or(estimate);
        // Trimming passes into a block container child (not a flex, grid or table one) when no
        // border or padding of the child intervenes (CSS Inline 3 §6.1).
        let child_trim = if first_in_flow == Some(ci) || last_in_flow == Some(ci) {
            let b = cx.tree.b(child);
            let passes = b.kind == BoxKind::Block;
            let e = Edges::of(&child_style, content_width);
            TrimRequest {
                start: trim.start
                    && passes
                    && first_in_flow == Some(ci)
                    && e.border.top + e.padding.top == 0.0,
                end: trim.end
                    && passes
                    && last_in_flow == Some(ci)
                    && e.border.bottom + e.padding.bottom == 0.0,
            }
        } else {
            TrimRequest::default()
        };
        let space_for = |band_left: f32, band_right: f32| {
            let mut s = Space::fill(content_width, cb_height);
            s.available = (band_right - band_left).max(0.0);
            s.trim = child_trim;
            s
        };
        let mut band = (left, right);
        // (laying the child out again must count its lines again)
        let clamp_before = if independent { None } else { cx.clamp.clone() };
        let points_before = cx.clamp.as_ref().map_or(0, |c| c.points.len());
        if independent && floats_before {
            // BFC roots, tables and replaced boxes must not overlap floats (CSS 2 §9.5).
            let b = bfc.band(content_origin.y + top, 0.0, left, right);
            band = (b.left, b.right);
        }
        let mut result = cx.layout(
            child,
            &space_for(band.0, band.1),
            if independent { None } else { Some(bfc) },
            Point::new(band.0, content_origin.y + top),
        );
        if independent && floats_before {
            // Move down until the box fits beside the floats.
            for _ in 0..8 {
                let m = result.margin;
                let w = result.fragment.size.width + m.left + m.right;
                let h = result.fragment.size.height;
                let (fy, _) =
                    bfc.find_space(content_origin.y + top, w.min(right - left), h, left, right);
                let new_top = (fy - content_origin.y).max(top);
                let b = bfc.band(content_origin.y + new_top, h, left, right);
                if (new_top - top).abs() < 0.01
                    && (b.left - band.0).abs() < 0.01
                    && (b.right - band.1).abs() < 0.01
                {
                    break;
                }
                top = new_top;
                band = (b.left, b.right);
                result = cx.layout(child, &space_for(band.0, band.1), None, Point::ZERO);
                clearance_top = Some(top);
            }
        }

        // Where the child really goes.
        let actual_top;
        if let Some(ct) = clearance_top {
            // Clearance separates the child from the margins before it.
            if at_top {
                at_top = false;
                top_strut = top_strut.join(pending);
            }
            pending = Strut::default();
            actual_top = ct;
        } else if at_top {
            actual_top = 0.0;
        } else {
            actual_top = y + pending.join(result.start).resolve();
        }
        // Lay out again at the right place when floats depend on the position.
        let floats_involved = floats_before || bfc.len() > floats_mark;
        if !independent
            && clearance_top.is_none()
            && floats_involved
            && (actual_top - top).abs() > 0.01
        {
            bfc.truncate(floats_mark);
            if clamp_before.is_some() {
                cx.clamp = clamp_before;
            }
            result = cx.layout(
                child,
                &space_for(left, right),
                Some(bfc),
                Point::new(left, content_origin.y + actual_top),
            );
        }

        let frag = &result.fragment;
        let h = frag.size.height;
        let x = (band.0 - left) + result.margin.left;
        let own_rel = cx.relative_offset(&child_style, content_width, cb_height);
        let split = cx.split_inline_offset(child, content_width, cb_height);
        let rel = Point::new(own_rel.x + split.x, own_rel.y + split.y);
        // (a block an inline was split around sits in an anonymous block box across the content
        // box, CSS 2 §9.2.1.1, which the inline's offset moves and the block's own does not: the
        // inline's client rect beside the block. Kept in a physical frame only.)
        if style.view().is_identity() && cx.tree.split_owners.contains_key(&child) {
            let wrapper = Rect::new(-(x + own_rel.x), -own_rel.y, content_width, h);
            cx.tree.split_wrappers.insert(child, wrapper);
        }
        let offset = Point::new(x + rel.x, actual_top + rel.y);
        // (the margins as computed: an over-constrained end margin is not shortened here)
        let computed = |side: Side, used: f32| match child_style.margin(side) {
            LengthPercentageAuto::Auto => used,
            m => m.resolve_or_zero(content_width),
        };
        let (lo, hi) = (
            x - computed(Side::Left, result.margin.left),
            x + frag.size.width + computed(Side::Right, result.margin.right),
        );
        inflow_x = Some(inflow_x.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
        for o in &result.oof {
            oof.push(o.shifted(offset.x, offset.y));
        }
        // A possible clamp point of a line-clamp container after an in-flow box with content
        // whose own lines gave none (one that ends with a line ends where that line's point is).
        if !result.through {
            if let Some(c) = cx.clamp.as_mut() {
                if c.points.len() == points_before {
                    c.point(false, content_origin.y + actual_top + h);
                }
            }
        }
        if result.through && clearance_top.is_none() {
            // An empty box: its margins collapse together and with what's around them.
            if at_top {
                top_strut = top_strut.join(result.start).join(result.end);
            } else {
                pending = pending.join(result.start).join(result.end);
            }
        } else {
            if at_top {
                top_strut = top_strut.join(result.start);
                at_top = false;
            }
            y = actual_top + h;
            // (CSS Multi-column Layout 1 §6.1: a spanner breaks the boxes it is in, so what
            // follows one starts after it even when a box around it ends before its place)
            if cx.column_flow {
                if let Some(at) = crate::multicol::last_spanner_mark(frag) {
                    y = y.max(actual_top + at);
                }
            }
            pending = result.end;
            if first_baseline.is_none() {
                first_baseline = frag.baseline.map(|b| b + actual_top);
            }
            if let Some(b) = frag.last_baseline.or(frag.baseline) {
                last_baseline = Some(b + actual_top);
            }
        }
        if result
            .fragment
            .as_box()
            .is_some_and(|b| b.flags & fragment_flags::HAS_SPANNER != 0)
        {
            spanner = true;
        }
        out.push(FragmentChild {
            offset,
            fragment: result.fragment,
            stacking_owner: 0,
        });
    }
    let empty = at_top;
    let (height, bottom_strut) = if empty {
        (0.0, top_strut)
    } else if collapse_bottom {
        (y, pending)
    } else {
        (y + pending.resolve(), Strut::default())
    };
    ChildrenResult {
        height,
        empty,
        top_strut,
        bottom_strut,
        first_baseline,
        last_baseline,
        inflow_x,
        spanner,
    }
}

/// Whether `b` is an inline box inside block container `container`'s inline content.
/// The block container whose layout places the out-of-flow boxes an inline box is the containing
/// block of: the one holding all of the inline's fragments. An inline box split around blocks
/// (CSS 2 §9.2.1.1) has fragments in the anonymous blocks around them, so that is their parent.
fn inline_cb_owner<S: Style>(cx: &Cx<'_, S>, inline: BoxId) -> Option<BoxId> {
    let mut cur = cx.tree.b(inline).parent;
    while let Some(c) = cur {
        let b = cx.tree.b(c);
        if b.kind == BoxKind::Inline {
            cur = b.parent;
            continue;
        }
        if b.is_anonymous() && b.kind == BoxKind::Block {
            if let Some(p) = b.parent {
                if cx.tree.b(p).is_block_container() && !cx.tree.b(p).has_inline_content() {
                    return Some(p);
                }
            }
        }
        return Some(c);
    }
    None
}

/// The containing block an inline box forms for out-of-flow descendants, from its fragments in
/// `children` (css-position-3 §2.1): the inline-start and block-start padding edges of its first
/// fragment and the inline-end and block-end padding edges of its last.
fn inline_padding_box<S: Style>(
    cx: &Cx<'_, S>,
    inline: BoxId,
    children: &[FragmentChild<S>],
) -> Rect {
    let b = cx.tree.b(inline);
    let node = b.node;
    let rtl = b.style.direction() == Direction::Rtl;
    let mut first: Option<Rect> = None;
    let mut last: Option<Rect> = None;
    if node.is_none() {
        return Rect::default();
    }
    for c in children {
        c.fragment.walk(&mut |f, at| {
            if let Some(b) = f.as_box() {
                if b.flags & fragment_flags::INLINE_BOX != 0 && b.node == node {
                    let r = Rect::new(
                        c.offset.x + at.x,
                        c.offset.y + at.y,
                        f.size.width,
                        f.size.height,
                    )
                    .deflate(&b.border);
                    if first.is_none() {
                        first = Some(r);
                    }
                    last = Some(r);
                }
            }
        });
    }
    let (Some(first), Some(last)) = (first, last) else {
        return Rect::default();
    };
    let (mut x0, mut x1) = if rtl {
        (last.x, first.x + first.width)
    } else {
        (first.x, last.x + last.width)
    };
    if x1 < x0 {
        std::mem::swap(&mut x0, &mut x1);
    }
    let y0 = first.y;
    let y1 = (last.y + last.height).max(y0);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

/// Lays out an outside `::marker` (CSS Lists 3 §3.1.1) beside the item's first line: its end edge
/// against the content box's start edge, its baseline on the first baseline (or the top).
pub(crate) fn layout_outside_marker<S: Style>(
    cx: &mut Cx<'_, S>,
    marker: BoxId,
    item_style: &S,
    content_width: f32,
    first_baseline: Option<f32>,
    content_inset: Point,
) -> FragmentChild<S> {
    let space = Space::shrink(content_width, None, f32::INFINITY);
    let r = cx.layout(marker, &space, None, Point::ZERO);
    let f = r.fragment;
    let w = f.size.width;
    let rtl = item_style.direction() == Direction::Rtl;
    let x = if rtl {
        content_inset.x + content_width
    } else {
        content_inset.x - w
    };
    let y = match (first_baseline, f.baseline) {
        (Some(b), Some(mb)) => b - mb,
        _ => content_inset.y,
    };
    FragmentChild {
        offset: Point::new(x, y),
        fragment: f,
        stacking_owner: 0,
    }
}
