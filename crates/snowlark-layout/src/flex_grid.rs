// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Flex (css-flexbox-1) and grid (css-grid-2) layout through taffy's low-level traits over our box tree:
//! taffy node ids are box ids, each container and item keeps a converted `taffy::Style`, taffy's cache
//! and its last `taffy::Layout` on the box, and items that aren't flex/grid containers are laid out by
//! our own block, inline, table and replaced layout as taffy's leaf measure (the pattern of taffy's
//! "custom tree" examples and Blitz).
//!
//! Absolutely positioned children get their static position from taffy (css-flexbox-1 §4.1,
//! css-grid-2 §9) and are laid out by our positioning code.

use std::sync::Arc;

use snowlark_paint::{Point, Rect, Sides, Size};
use taffy::{
    compute_cached_layout, compute_flexbox_layout, compute_grid_layout, compute_leaf_layout,
    AvailableSpace, CacheTree, LayoutInput, LayoutOutput, LayoutPartialTree, RunMode, SizingMode,
    TraversePartialTree,
};

use crate::boxes::*;
use crate::fragment::*;
use crate::layout::*;
use crate::sizing::*;
use crate::style::*;

type TStyle = taffy::Style<Arc<str>>;
type Template = (
    Vec<taffy::GridTemplateComponent<Arc<str>>>,
    Vec<Vec<Arc<str>>>,
);

/// Taffy's per-node state on a box.
pub(crate) struct TaffyNode<S: Style> {
    /// The box's style as an item (in its parent's flow frame).
    pub style: TStyle,
    /// The view `style` was read through.
    pub view: crate::logical::Flip,
    /// The box's style as a container, in its own flow frame, when that differs from its parent's
    /// (CSS Writing Modes 4: an item in another writing mode).
    pub own_style: Option<TStyle>,
    pub cache: taffy::Cache,
    pub layout: taffy::Layout,
    /// A leaf's last full layout (its fragment and out-of-flow candidates).
    pub leaf: Option<LayoutResult<S>>,
    /// A container's out-of-flow children with their static positions, from its last full layout.
    pub oof: Vec<taffy::OofCandidate>,
    /// A grid container's tracks and line names from its last full layout, for the grid areas of
    /// its absolutely positioned descendants (css-grid-2 §9.1).
    pub grid_info: Option<Box<taffy::DetailedGridInfo<Arc<str>>>>,
    /// A flex container's children in order-modified document order (css-flexbox-1 §5.4), when some
    /// child's `order` is not zero; empty when that is document order.
    pub order: Vec<BoxId>,
    /// A grid container's in-flow items' first row lines (1-based), in document order, from its last
    /// layout (for its baseline, css-grid-2 §11.8).
    pub grid_rows: Vec<u16>,
    /// A grid container's tracks (columns, rows) from its last full layout.
    pub grid: Option<Box<GridTracks>>,
    /// How many times taffy has run this container's algorithm (a run may move its items).
    pub runs: u32,
    /// A nested container's fragment, built from its items' layouts after its last run.
    pub built: Option<Box<Built<S>>>,
    /// A subgrid's tracks from its parent grid, and the style it is laid out with for them
    /// (css-grid-2 §9).
    pub subgrid: Option<Box<(taffy::SubgridTracks<Arc<str>>, TStyle)>>,
}

/// A container's fragment and out-of-flow candidates as built after an algorithm run, with what
/// they were built from: the run, and the container's own size and edges.
pub(crate) struct Built<S: Style> {
    runs: u32,
    size: taffy::Size<f32>,
    border: taffy::Rect<f32>,
    padding: taffy::Rect<f32>,
    pub(crate) fragment: Arc<Fragment<S>>,
    oof: Vec<Oof>,
}

/// A grid container's used tracks in each axis, from its last full layout.
pub(crate) struct GridTracks {
    pub columns: taffy::detailed_info::DetailedGridTracksInfo<Arc<str>>,
    pub rows: taffy::detailed_info::DetailedGridTracksInfo<Arc<str>>,
}

/// CSS Grid 1 §7.2.6 "resolved value" of a track list: every track (implicit ones included) as its
/// used size in px, with the line names between them; `none` without tracks.
pub(crate) fn resolved_track_list(
    t: &taffy::detailed_info::DetailedGridTracksInfo<Arc<str>>,
) -> String {
    if t.positions.is_empty() {
        return "none".into();
    }
    let mut out = String::new();
    let names = |out: &mut String, line: usize| {
        let n = t.names_for_line(line);
        if !n.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push('[');
            for (i, name) in n.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(name);
            }
            out.push(']');
        }
    };
    for (i, p) in t.positions.iter().enumerate() {
        names(&mut out, i);
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&css_px(p.end - p.start));
    }
    names(&mut out, t.positions.len());
    out
}

/// A length in px as CSSOM serializes it (at most six decimals).
pub(crate) fn css_px(v: f32) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}px")
}

fn tid(id: BoxId) -> taffy::NodeId {
    taffy::NodeId::from(id.to_bits())
}

fn bid(n: taffy::NodeId) -> BoxId {
    BoxId::from_bits(u64::from(n)).expect("taffy node id is a box id")
}

// ----- style conversion -----

fn lp(v: LengthPercentage, calcs: &mut Vec<LengthPercentage>) -> taffy::LengthPercentage {
    match v.percent {
        None if v.is_linear() => taffy::LengthPercentage::length(v.length),
        Some(p) if v.length == 0.0 && v.is_linear() => taffy::LengthPercentage::percent(p),
        _ => taffy::LengthPercentage::calc(calc_handle(v, calcs, true)),
    }
}

/// A signed `<length-percentage> | auto` (margins, insets).
fn lpa(v: LengthPercentageAuto, calcs: &mut Vec<LengthPercentage>) -> taffy::LengthPercentageAuto {
    lpa_in(v, calcs, false)
}

fn lpa_in(
    v: LengthPercentageAuto,
    calcs: &mut Vec<LengthPercentage>,
    non_negative: bool,
) -> taffy::LengthPercentageAuto {
    match v {
        LengthPercentageAuto::Auto => taffy::LengthPercentageAuto::auto(),
        LengthPercentageAuto::LengthPercentage(v) => match v.percent {
            None if v.is_linear() => taffy::LengthPercentageAuto::length(v.length),
            Some(p) if v.length == 0.0 && v.is_linear() => taffy::LengthPercentageAuto::percent(p),
            _ => taffy::LengthPercentageAuto::calc(calc_handle(v, calcs, non_negative)),
        },
    }
}

/// A calc() handle for taffy: an index into the tree's table and whether the property's range is
/// non-negative (a `calc()` result is clamped to it, CSS Values 4 §10.12), shifted past taffy's tag
/// bits.
fn calc_handle(
    v: LengthPercentage,
    calcs: &mut Vec<LengthPercentage>,
    non_negative: bool,
) -> *const () {
    calcs.push(v);
    std::ptr::without_provenance(((calcs.len() << 1) | usize::from(non_negative)) << 3)
}

fn dimension(v: SizeValue, calcs: &mut Vec<LengthPercentage>) -> taffy::Dimension {
    match v {
        SizeValue::Auto | SizeValue::Stretch => taffy::Dimension::auto(),
        SizeValue::LengthPercentage(l) => match l.percent {
            None if l.is_linear() => taffy::Dimension::length(l.length),
            Some(p) if l.length == 0.0 && l.is_linear() => taffy::Dimension::percent(p),
            _ => taffy::Dimension::calc(calc_handle(l, calcs, true)),
        },
        SizeValue::MinContent => taffy::Dimension::min_content(),
        SizeValue::MaxContent => taffy::Dimension::max_content(),
        SizeValue::FitContent(Some(l)) if !l.has_percent() => {
            taffy::Dimension::fit_content_px(l.length)
        }
        SizeValue::FitContent(_) => taffy::Dimension::fit_content(),
    }
}

/// `min-width`/`min-height` (taffy takes lengths only; intrinsic keywords count as `auto`).
fn min_lpa(v: SizeValue, calcs: &mut Vec<LengthPercentage>) -> taffy::LengthPercentageAuto {
    match v {
        SizeValue::LengthPercentage(l) => {
            lpa_in(LengthPercentageAuto::LengthPercentage(l), calcs, true)
        }
        _ => taffy::LengthPercentageAuto::auto(),
    }
}

/// `max-width`/`max-height` (`none` and intrinsic keywords as `auto`).
fn max_lpa(v: MaxSizeValue, calcs: &mut Vec<LengthPercentage>) -> taffy::LengthPercentageAuto {
    match v {
        MaxSizeValue::LengthPercentage(l) => {
            lpa_in(LengthPercentageAuto::LengthPercentage(l), calcs, true)
        }
        _ => taffy::LengthPercentageAuto::auto(),
    }
}

fn align_items(a: Align) -> taffy::AlignItems {
    use taffy::AlignItems as A;
    match a {
        Align::Start | Align::Left => A::START,
        Align::End | Align::Right => A::END,
        Align::FlexStart => A::FLEX_START,
        Align::FlexEnd => A::FLEX_END,
        Align::Center => A::CENTER,
        Align::Baseline | Align::LastBaseline => A::BASELINE,
        Align::Stretch => A::STRETCH,
        Align::SelfStart => A::SELF_START,
        Align::SelfEnd => A::SELF_END,
        _ => A::NORMAL,
    }
}

fn align_self(a: Align) -> Option<taffy::AlignSelf> {
    match a {
        Align::Auto => None,
        a => Some(align_items(a)),
    }
}

/// `justify-content: left | right` (css-align-3 §6.1): toward the physical side when the main axis is
/// the inline axis, as `start` in a column flex container, whose main axis is not.
fn justify_content(
    a: Align,
    dir: FlexDirection,
    direction: Direction,
    kind: BoxKind,
    view: crate::logical::Flip,
) -> Align {
    let column = kind == BoxKind::Flex
        && matches!(dir, FlexDirection::Column | FlexDirection::ColumnReverse);
    let rtl = direction == Direction::Rtl;
    if view.swaps() && matches!(a, Align::Left | Align::Right) {
        // A vertical writing mode: the main axis of a column runs left and right.
        if !column {
            return Align::Start;
        }
        let side = if a == Align::Left {
            Side::Left
        } else {
            Side::Right
        };
        return if view.inverse().side(side) == Side::Top {
            Align::Start
        } else {
            Align::End
        };
    }
    match a {
        Align::Left | Align::Right if column => Align::Start,
        Align::Left if rtl => Align::End,
        Align::Right if rtl => Align::Start,
        a => a,
    }
}

fn safety(p: OverflowPosition) -> taffy::AlignmentSafety {
    match p {
        OverflowPosition::Default => taffy::AlignmentSafety::Default,
        OverflowPosition::Safe => taffy::AlignmentSafety::Safe,
        OverflowPosition::Unsafe => taffy::AlignmentSafety::Unsafe,
    }
}

fn align_content(a: Align) -> taffy::AlignContent {
    use taffy::AlignContent as A;
    match a {
        Align::Start | Align::Left => A::START,
        Align::End | Align::Right => A::END,
        Align::FlexStart => A::FLEX_START,
        Align::FlexEnd => A::FLEX_END,
        Align::Center => A::CENTER,
        Align::Stretch => A::STRETCH,
        Align::SpaceBetween => A::SPACE_BETWEEN,
        Align::SpaceAround => A::SPACE_AROUND,
        Align::SpaceEvenly => A::SPACE_EVENLY,
        _ => A::NORMAL,
    }
}

fn min_track(b: TrackBreadth, calcs: &mut Vec<LengthPercentage>) -> taffy::MinTrackSizingFunction {
    use taffy::MinTrackSizingFunction as M;
    match b {
        TrackBreadth::LengthPercentage(l) => match l.percent {
            None if l.is_linear() => M::length(l.length),
            Some(p) if l.length == 0.0 && l.is_linear() => M::percent(p),
            _ => M::calc(calc_handle(l, calcs, true)),
        },
        TrackBreadth::MinContent => M::min_content(),
        TrackBreadth::MaxContent => M::max_content(),
        TrackBreadth::Auto | TrackBreadth::Fr(_) => M::auto(),
    }
}

fn max_track(b: TrackBreadth, calcs: &mut Vec<LengthPercentage>) -> taffy::MaxTrackSizingFunction {
    use taffy::MaxTrackSizingFunction as M;
    match b {
        TrackBreadth::LengthPercentage(l) => match l.percent {
            None if l.is_linear() => M::length(l.length),
            Some(p) if l.length == 0.0 && l.is_linear() => M::percent(p),
            _ => M::calc(calc_handle(l, calcs, true)),
        },
        TrackBreadth::MinContent => M::min_content(),
        TrackBreadth::MaxContent => M::max_content(),
        TrackBreadth::Auto => M::auto(),
        TrackBreadth::Fr(f) => M::fr(f),
    }
}

fn track(t: TrackSize, calcs: &mut Vec<LengthPercentage>) -> taffy::TrackSizingFunction {
    match t {
        TrackSize::Breadth(b) => taffy::MinMax {
            min: min_track(b, calcs),
            max: max_track(b, calcs),
        },
        TrackSize::MinMax(a, b) => taffy::MinMax {
            min: min_track(a, calcs),
            max: max_track(b, calcs),
        },
        TrackSize::FitContent(l) => taffy::MinMax {
            min: taffy::MinTrackSizingFunction::auto(),
            max: if let (Some(p), 0.0, true) = (l.percent, l.length, l.is_linear()) {
                taffy::MaxTrackSizingFunction::fit_content_percent(p)
            } else {
                taffy::MaxTrackSizingFunction::fit_content_px(l.length)
            },
        },
    }
}

fn template(list: &TrackList, calcs: &mut Vec<LengthPercentage>) -> Template {
    let items = list
        .items
        .iter()
        .map(|item| match item {
            TrackListItem::Track(t) => taffy::GridTemplateComponent::Single(track(*t, calcs)),
            TrackListItem::Repeat {
                count,
                tracks,
                line_names,
            } => taffy::GridTemplateComponent::Repeat(taffy::GridTemplateRepetition {
                count: match count {
                    RepeatCount::Count(n) => taffy::RepetitionCount::Count(*n),
                    RepeatCount::AutoFill => taffy::RepetitionCount::AutoFill,
                    RepeatCount::AutoFit => taffy::RepetitionCount::AutoFit,
                },
                tracks: tracks.iter().map(|t| track(*t, calcs)).collect(),
                line_names: line_names.clone(),
            }),
        })
        .collect();
    (items, list.line_names.clone())
}

fn placement(l: GridLine) -> taffy::GridPlacement<Arc<str>> {
    use taffy::style_helpers::TaffyGridLine;
    match l {
        GridLine::Auto => taffy::GridPlacement::Auto,
        GridLine::Line(i) => taffy::GridPlacement::from_line_index(i),
        GridLine::Span(n) => taffy::GridPlacement::Span(n.max(1)),
        GridLine::Named(name, i) => taffy::GridPlacement::NamedLine(name, i),
        GridLine::NamedSpan(name, n) => taffy::GridPlacement::NamedSpan(name, n.max(1)),
    }
}

/// The space a flex or grid container's custom scroll bars take ([`crate::gutter`]), for taffy,
/// which subtracts a scroll bar's thickness from the content box on an axis that is `scroll` (at
/// the inline end, the left in `rtl`, and the bottom). (A leaf item's own layout takes its bars'
/// space, so it is not given any here.)
fn scrollbar_gutter<S: Style>(t: &mut TStyle, b: &LayoutBox<S>) {
    if !matches!(b.kind, BoxKind::Flex | BoxKind::Grid) {
        return;
    }
    let g = crate::gutter::gutter(b);
    let (vertical, horizontal) = (g.left + g.right, g.bottom);
    let scroll = |on: bool, o: taffy::Overflow| match (on, o) {
        (true, _) => taffy::Overflow::Scroll,
        (false, taffy::Overflow::Scroll) => taffy::Overflow::Hidden,
        (false, o) => o,
    };
    if crate::gutter::custom_bars(b).is_some() {
        t.overflow = taffy::Point {
            x: scroll(horizontal > 0.0, t.overflow.x),
            y: scroll(vertical > 0.0, t.overflow.y),
        };
        // (taffy has one thickness for both bars; they differ only when the parts say so)
        t.scrollbar_width = vertical.max(horizontal);
    }
}

/// A flex or grid container's custom scroll bars changed ([`crate::gutter::set_bars`]): its
/// taffy style takes their new space.
pub(crate) fn scrollbars_changed<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) {
    let b = cx.tree.b(id);
    if !matches!(b.kind, BoxKind::Flex | BoxKind::Grid) || b.cache.taffy.is_none() {
        return;
    }
    let mut style = cx.tree.b(id).cache.taffy.as_ref().unwrap().style.clone();
    scrollbar_gutter(&mut style, cx.tree.b(id));
    let node = cx.tree.b_mut(id).cache.taffy.as_mut().unwrap();
    node.style = style;
    node.cache.clear();
    node.built = None;
}

/// Converts a box's style for taffy.
fn taffy_style<S: Style>(b: &LayoutBox<S>, s: &S, calcs: &mut Vec<LengthPercentage>) -> TStyle {
    let overflow = |o: Overflow| match o {
        Overflow::Visible => taffy::Overflow::Visible,
        Overflow::Clip => taffy::Overflow::Clip,
        Overflow::Hidden | Overflow::Auto => taffy::Overflow::Hidden,
        Overflow::Scroll => taffy::Overflow::Scroll,
    };
    let sides_lpa = |f: &dyn Fn(Side) -> LengthPercentageAuto,
                     calcs: &mut Vec<LengthPercentage>| taffy::Rect {
        left: lpa(f(Side::Left), calcs),
        right: lpa(f(Side::Right), calcs),
        top: lpa(f(Side::Top), calcs),
        bottom: lpa(f(Side::Bottom), calcs),
    };
    let safe = s.align_safety();
    let mut t = TStyle {
        display: match b.kind {
            BoxKind::Grid => taffy::Display::Grid,
            _ => taffy::Display::Flex,
        },
        item_is_table: b.kind == BoxKind::TableWrapper,
        item_is_replaced: b.kind == BoxKind::Replaced,
        item_is_collapsed: s.visibility() == Visibility::Collapse,
        box_sizing: match s.box_sizing() {
            BoxSizing::BorderBox => taffy::BoxSizing::BorderBox,
            BoxSizing::ContentBox => taffy::BoxSizing::ContentBox,
        },
        direction: match s.direction() {
            Direction::Ltr => taffy::Direction::Ltr,
            Direction::Rtl => taffy::Direction::Rtl,
        },
        overflow: taffy::Point {
            x: overflow(s.overflow_x()),
            y: overflow(s.overflow_y()),
        },
        position: match s.position() {
            Position::Static => taffy::Position::Static,
            Position::Relative => taffy::Position::Relative,
            Position::Absolute => taffy::Position::Absolute,
            Position::Fixed => taffy::Position::Fixed,
            Position::Sticky => taffy::Position::Sticky,
        },
        size: taffy::Size {
            width: dimension(s.width(), calcs),
            height: dimension(s.height(), calcs),
        },
        min_size: taffy::Size {
            width: min_lpa(s.min_width(), calcs),
            height: min_lpa(s.min_height(), calcs),
        },
        max_size: taffy::Size {
            width: max_lpa(s.max_width(), calcs),
            height: max_lpa(s.max_height(), calcs),
        },
        aspect_ratio: match s.aspect_ratio() {
            AspectRatio {
                auto: false,
                ratio: Some(r),
            } => Some(r),
            AspectRatio {
                auto: true,
                ratio: Some(r),
            } if b.kind != BoxKind::Replaced => Some(r),
            _ => None,
        },
        padding: taffy::Rect {
            left: lp(s.padding(Side::Left), calcs),
            right: lp(s.padding(Side::Right), calcs),
            top: lp(s.padding(Side::Top), calcs),
            bottom: lp(s.padding(Side::Bottom), calcs),
        },
        border: taffy::Rect {
            left: taffy::LengthPercentage::length(s.border_width(Side::Left)),
            right: taffy::LengthPercentage::length(s.border_width(Side::Right)),
            top: taffy::LengthPercentage::length(s.border_width(Side::Top)),
            bottom: taffy::LengthPercentage::length(s.border_width(Side::Bottom)),
        },
        align_items: align_items(s.align_items()).with_safety(safety(safe.align_items)),
        align_self: align_self(s.align_self()).map(|a| a.with_safety(safety(safe.align_self))),
        justify_items: align_items(s.justify_items()).with_safety(safety(safe.justify_items)),
        justify_self: align_self(s.justify_self())
            .map(|a| a.with_safety(safety(safe.justify_self))),
        align_content: align_content(s.align_content()).with_safety(safety(safe.align_content)),
        justify_content: align_content(justify_content(
            s.justify_content(),
            s.flex_direction(),
            s.direction(),
            b.kind,
            s.view(),
        ))
        .with_safety(safety(safe.justify_content)),
        gap: taffy::Size {
            width: lp(s.column_gap().unwrap_or(LengthPercentage::ZERO), calcs),
            height: lp(s.row_gap().unwrap_or(LengthPercentage::ZERO), calcs),
        },
        flex_direction: match s.flex_direction() {
            FlexDirection::Row => taffy::FlexDirection::Row,
            FlexDirection::RowReverse => taffy::FlexDirection::RowReverse,
            FlexDirection::Column => taffy::FlexDirection::Column,
            FlexDirection::ColumnReverse => taffy::FlexDirection::ColumnReverse,
        },
        flex_wrap: match s.flex_wrap() {
            FlexWrap::Nowrap => taffy::FlexWrap::NoWrap,
            FlexWrap::Wrap => taffy::FlexWrap::Wrap,
            FlexWrap::WrapReverse => taffy::FlexWrap::WrapReverse,
        },
        flex_grow: s.flex_grow(),
        flex_shrink: s.flex_shrink(),
        flex_basis: match s.flex_basis() {
            FlexBasis::Auto => taffy::Dimension::auto(),
            FlexBasis::Content => taffy::Dimension::content(),
            FlexBasis::LengthPercentage(l) => dimension(SizeValue::LengthPercentage(l), calcs),
        },
        grid_auto_flow: match s.grid_auto_flow() {
            GridAutoFlow {
                column: false,
                dense: false,
            } => taffy::GridAutoFlow::Row,
            GridAutoFlow {
                column: true,
                dense: false,
            } => taffy::GridAutoFlow::Column,
            GridAutoFlow {
                column: false,
                dense: true,
            } => taffy::GridAutoFlow::RowDense,
            GridAutoFlow {
                column: true,
                dense: true,
            } => taffy::GridAutoFlow::ColumnDense,
        },
        grid_row: taffy::Line {
            start: placement(s.grid_row_start()),
            end: placement(s.grid_row_end()),
        },
        grid_column: taffy::Line {
            start: placement(s.grid_column_start()),
            end: placement(s.grid_column_end()),
        },
        ..TStyle::DEFAULT
    };
    t.margin = sides_lpa(&|side| s.margin(side), calcs);
    t.inset = sides_lpa(&|side| s.inset(side), calcs);
    scrollbar_gutter(&mut t, b);
    if b.kind == BoxKind::Grid {
        let (cols, col_names) = template(&s.grid_template_columns(), calcs);
        let (rows, row_names) = template(&s.grid_template_rows(), calcs);
        t.grid_template_columns = cols;
        t.grid_template_rows = rows;
        t.grid_template_column_names = col_names;
        t.grid_template_row_names = row_names;
        // css-grid-2 §9: a subgrid takes its parent's gap where its own is `normal`.
        let (subgrid_columns, subgrid_rows) = s.grid_template_subgrid();
        t.subgrid = taffy::Size {
            width: subgrid_columns,
            height: subgrid_rows,
        };
        t.gap_is_normal = taffy::Size {
            width: s.column_gap().is_none(),
            height: s.row_gap().is_none(),
        };
        t.grid_auto_columns = s
            .grid_auto_columns()
            .iter()
            .map(|x| track(*x, calcs))
            .collect();
        t.grid_auto_rows = s
            .grid_auto_rows()
            .iter()
            .map(|x| track(*x, calcs))
            .collect();
        if let Some(areas) = s.grid_template_areas() {
            t.grid_template_areas = Some(taffy::GridTemplateAreas {
                areas: areas
                    .areas
                    .iter()
                    .map(|a| taffy::GridTemplateArea {
                        name: a.name.clone(),
                        row_start: a.row_start,
                        row_end: a.row_end,
                        column_start: a.column_start,
                        column_end: a.column_end,
                    })
                    .collect(),
                row_count: areas.rows,
                column_count: areas.columns,
            });
        }
    }
    t
}

/// CSS Box Alignment 3 §4.1: `left` and `right` are physical, but taffy's `start`/`end` follow the
/// grid's direction: in the inline axis of a right-to-left grid they swap. `parent_grid_rtl` is the
/// direction of the grid container `b` is an item of, if any.
fn physical_inline_alignment<S: Style>(
    t: &mut TStyle,
    b: &LayoutBox<S>,
    parent_grid_rtl: Option<bool>,
) {
    fn flip<T: Copy>(v: T, a: Align, start: T, end: T) -> T {
        match a {
            Align::Left => end,
            Align::Right => start,
            _ => v,
        }
    }
    let s = &b.style;
    if b.kind == BoxKind::Grid && s.direction() == Direction::Rtl {
        use taffy::AlignContent as C;
        use taffy::AlignItems as I;
        t.justify_content = flip(t.justify_content, s.justify_content(), C::START, C::END);
        t.justify_items = flip(t.justify_items, s.justify_items(), I::START, I::END);
    }
    if parent_grid_rtl == Some(true) {
        use taffy::AlignItems as I;
        t.justify_self = t
            .justify_self
            .map(|j| flip(j, s.justify_self(), I::START, I::END));
    }
    // css-align-3 §9.3: an absolutely positioned box takes no part in baseline alignment; `last
    // baseline` falls back to `end` (`first baseline` to `start`, taffy's own). The overflow
    // position stays the default of absolutely positioned boxes (§4.4: the area may be overflowed;
    // Chrome and Firefox agree in css-grid/abspos/grid-abspos-staticpos-*-last-baseline-*).
    if b.is_out_of_flow() {
        if s.align_self() == Align::LastBaseline {
            t.align_self = Some(taffy::AlignItems::END);
        }
        if s.justify_self() == Align::LastBaseline {
            t.justify_self = Some(taffy::AlignItems::END);
        }
    } else if s.align_self() == Align::LastBaseline {
        t.align_self = Some(taffy::AlignItems::LAST_BASELINE);
    }
    if s.align_items() == Align::LastBaseline {
        t.align_items = taffy::AlignItems::LAST_BASELINE;
    }
}

/// `self-start` and `self-end` of an item in another writing mode or direction than its grid
/// container (css-align-3 §4.2): the item's own start sides, whichever of its axes lies along each
/// of the container's, in its frame `frame`.
fn self_alignment_across<S: Style>(
    t: &mut TStyle,
    b: &LayoutBox<S>,
    frame: crate::logical::Flip,
    parent_grid_rtl: Option<bool>,
    parent_flex: Option<(bool, bool)>,
) {
    use taffy::AlignItems as I;
    let Some(grid_rtl) = parent_grid_rtl.or(parent_flex.map(|(_, rtl)| rtl)) else {
        return;
    };
    if b.style.view() == frame && (b.style.direction() == Direction::Rtl) == grid_rtl {
        return;
    }
    let view = b.style.view();
    let inline_start = if b.style.direction() == Direction::Rtl {
        Side::Right
    } else {
        Side::Left
    };
    // The item's two start sides as sides of the container's frame.
    let to_frame = |s| frame.inverse().side(view.side(s));
    let starts = [to_frame(inline_start), to_frame(Side::Top)];
    let horizontal_start = starts.contains(&Side::Left);
    let vertical_start = starts.contains(&Side::Top);
    let map = |a: Option<I>, start_is_start: bool| match a {
        Some(I::SELF_START) if start_is_start => Some(I::START),
        Some(I::SELF_START) => Some(I::END),
        Some(I::SELF_END) if start_is_start => Some(I::END),
        Some(I::SELF_END) => Some(I::START),
        a => a,
    };
    // (taffy's `start` in the inline axis follows the container's direction)
    if let Some((true, _)) = parent_flex {
        t.align_self = map(t.align_self, horizontal_start != grid_rtl);
        return;
    }
    t.justify_self = map(t.justify_self, horizontal_start != grid_rtl);
    t.align_self = map(t.align_self, vertical_start);
}

/// Whether the flex container `p` is a column one and right-to-left, if it is a flex container.
fn flex_parent<S: Style>(p: &LayoutBox<S>) -> Option<(bool, bool)> {
    (p.kind == BoxKind::Flex).then(|| {
        let column = matches!(
            p.style.flex_direction(),
            FlexDirection::Column | FlexDirection::ColumnReverse
        );
        (column, p.style.direction() == Direction::Rtl)
    })
}

/// Makes sure `id` and its children have taffy state.
fn ensure_nodes<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) {
    let kids = cx.tree.b(id).children.clone();
    // The container in its own frame; its items as seen from it (an item in another writing mode
    // through the container's view).
    let frame = cx.frame;
    for n in std::iter::once(id).chain(kids) {
        let own = cx.tree.b(n).style.view();
        if n == id && own != frame {
            // (not expected: a container lays out in its own frame)
            continue;
        }
        if let Some(node) = &cx.tree.b(n).cache.taffy {
            if node.view == frame || n == id && node.own_style.is_some() {
                continue;
            }
            // A container in another writing mode met first as an item, or the reverse.
            let mut calcs = std::mem::take(&mut cx.tree.calcs);
            let b = cx.tree.b(n);
            let s = b.style.with_view(frame);
            let mut style = taffy_style(b, &s, &mut calcs);
            let parent_grid_rtl = b.parent.map(|p| cx.tree.b(p)).and_then(|p| {
                (p.kind == BoxKind::Grid).then(|| p.style.direction() == Direction::Rtl)
            });
            physical_inline_alignment(&mut style, b, parent_grid_rtl);
            let parent_flex = b.parent.and_then(|p| flex_parent(cx.tree.b(p)));
            self_alignment_across(&mut style, b, frame, parent_grid_rtl, parent_flex);
            cx.tree.calcs = calcs;
            let node = cx.tree.b_mut(n).cache.taffy.as_mut().unwrap();
            if n == id {
                node.own_style = Some(style);
            } else {
                let old = std::mem::replace(&mut node.style, style);
                if node.view == own {
                    node.own_style = Some(old);
                }
                node.view = frame;
                node.subgrid = None;
                node.cache.clear();
            }
            continue;
        }
        {
            let mut calcs = std::mem::take(&mut cx.tree.calcs);
            let b = cx.tree.b(n);
            let parent = b.parent.map(|p| cx.tree.b(p));
            let parent_grid_rtl = parent.and_then(|p| {
                (p.kind == BoxKind::Grid).then(|| p.style.direction() == Direction::Rtl)
            });
            let s = if own == frame {
                b.style.clone()
            } else {
                b.style.with_view(frame)
            };
            let mut style = taffy_style(b, &s, &mut calcs);
            physical_inline_alignment(&mut style, b, parent_grid_rtl);
            let parent_flex = parent.and_then(flex_parent);
            self_alignment_across(&mut style, b, frame, parent_grid_rtl, parent_flex);
            let own_style = (own != frame).then(|| {
                let mut o = taffy_style(b, &b.style, &mut calcs);
                physical_inline_alignment(&mut o, b, None);
                o
            });
            cx.tree.calcs = calcs;
            cx.tree.b_mut(n).cache.taffy = Some(Box::new(TaffyNode {
                style,
                view: frame,
                own_style,
                cache: taffy::Cache::new(),
                layout: taffy::Layout::new(),
                leaf: None,
                oof: Vec::new(),
                grid_info: None,
                order: Vec::new(),
                grid_rows: Vec::new(),
                grid: None,
                runs: 0,
                built: None,
                subgrid: None,
            }));
        }
    }
}

/// Lays a flex or grid container's children out in order-modified document order (css-flexbox-1
/// §5.4.1: "flex items are laid out in order-modified document order", ordinal groups ascending,
/// document order within a group; css-grid-2 §4.2 and §8.5 for grid items' auto-placement).
/// Out-of-flow children keep their place: they are not flex items.
fn order_children<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) {
    let tree = &mut cx.tree;
    let reordered = tree
        .b(id)
        .children
        .iter()
        .any(|&c| tree.b(c).style.order() != 0);
    let mut order = std::mem::take(&mut tree.b_mut(id).cache.taffy.as_mut().unwrap().order);
    order.clear();
    if reordered {
        order.extend_from_slice(&tree.b(id).children);
        order.sort_by_key(|&c| tree.b(c).style.order());
    }
    tree.b_mut(id).cache.taffy.as_mut().unwrap().order = order;
    if cx.tree.b(id).kind == BoxKind::Flex {
        intrinsic_width_bounds(cx, id);
        table_item_heights(cx, id);
    }
}

/// A table item's preferred height is its table box's height with its captions counting as the
/// table box's border and padding (css-flexbox-1 §4: "like width and height, the flex longhands
/// apply to the table box"), so a table item's taffy height is its `height` plus its captions.
fn table_item_heights<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) {
    for i in 0..cx.tree.b(id).children.len() {
        let c = cx.tree.b(id).children[i];
        let b = cx.tree.b(c);
        if b.kind != BoxKind::TableWrapper {
            continue;
        }
        let SizeValue::LengthPercentage(h) = b.style.height() else {
            continue;
        };
        if h.has_percent() || !h.is_linear() {
            continue;
        }
        let has_caption = b
            .children
            .iter()
            .any(|&k| cx.tree.b(k).kind == BoxKind::TableCaption);
        if !has_caption {
            continue;
        }
        // (in the box's `box-sizing`, as taffy reads it)
        let height = h.length.max(0.0);
        let last_width = b.cache.taffy.as_ref().map_or(0.0, |t| t.layout.size.width);
        let width = if last_width > 0.0 {
            last_width
        } else {
            crate::table::flex_item_content_sizes(cx, c).max
        };
        let captions = crate::table::captions_height(cx, c, width);
        if let Some(node) = cx.tree.b_mut(c).cache.taffy.as_mut() {
            node.style.size.height = taffy::Dimension::length(height + captions);
        }
    }
}

/// `min-width`/`max-width: min-content | max-content` of flex items as lengths (taffy's min and max
/// sizes are lengths): the item's intrinsic widths now (css-sizing-3 §3.2), refreshed at every layout
/// of the container since they follow the item's content.
fn intrinsic_width_bounds<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) {
    for i in 0..cx.tree.b(id).children.len() {
        let c = cx.tree.b(id).children[i];
        let style = &cx.style_here(c);
        let intrinsic = |v: SizeValue| matches!(v, SizeValue::MinContent | SizeValue::MaxContent);
        let (min, max) = (style.min_width(), style.max_width());
        let max = match max {
            MaxSizeValue::MinContent => SizeValue::MinContent,
            MaxSizeValue::MaxContent => SizeValue::MaxContent,
            _ => SizeValue::Auto,
        };
        if !intrinsic(min) && !intrinsic(max) {
            continue;
        }
        let pb = Edges::of(style, 0.0).pb_h();
        // Taffy adds a content-box item's padding and border to its min and max sizes.
        let extra = match style.box_sizing() {
            BoxSizing::BorderBox => pb,
            BoxSizing::ContentBox => 0.0,
        };
        let sizes = cx.content_sizes(c);
        let length = |v: SizeValue| match v {
            SizeValue::MinContent => Some(taffy::LengthPercentageAuto::length(sizes.min + extra)),
            SizeValue::MaxContent => Some(taffy::LengthPercentageAuto::length(sizes.max + extra)),
            _ => None,
        };
        let (min, max) = (length(min), length(max));
        let Some(node) = cx.tree.b_mut(c).cache.taffy.as_mut() else {
            continue;
        };
        if let Some(v) = min {
            node.style.min_size.width = v;
        }
        if let Some(v) = max {
            node.style.max_size.width = v;
        }
    }
}

// ----- the tree adapter -----

struct Tree<'c, 'a, S: Style> {
    cx: &'c mut Cx<'a, S>,
    /// A container measured as if it had no children (size containment).
    empty: Option<taffy::NodeId>,
}

impl<S: Style> Tree<'_, '_, S> {
    /// A container's children in the order its layout algorithm takes them.
    fn children(&self, parent: taffy::NodeId) -> &[BoxId] {
        // A size-contained container measured as if empty (`contained_inputs`).
        if self.empty == Some(parent) {
            return &[];
        }
        let b = self.cx.tree.b(bid(parent));
        match b.cache.taffy.as_ref() {
            Some(t) if !t.order.is_empty() => &t.order,
            _ => &b.children,
        }
    }

    fn node(&self, n: taffy::NodeId) -> &TaffyNode<S> {
        self.cx
            .tree
            .b(bid(n))
            .cache
            .taffy
            .as_ref()
            .expect("taffy node")
    }

    /// The style of `n` as the container being laid out (in its own frame).
    fn container_style(&self, n: taffy::NodeId) -> &TStyle {
        let node = self.node(n);
        if let Some(subgrid) = &node.subgrid {
            return &subgrid.1;
        }
        match &node.own_style {
            Some(s) if node.view != self.cx.frame => s,
            _ => &node.style,
        }
    }

    fn node_mut(&mut self, n: taffy::NodeId) -> &mut TaffyNode<S> {
        self.cx
            .tree
            .b_mut(bid(n))
            .cache
            .taffy
            .as_mut()
            .expect("taffy node")
    }

    /// `inputs` with an available height that the layout of `id` does not read made a single value,
    /// so that taffy's cache keys for the same layout match (taffy keys its measurements on the
    /// available space, so each available height was a separate cache miss).
    ///
    /// - A leaf in the container's flow reads no available height: its height is its layout's at its
    ///   width (CSS 2 §10.6), resolved against the containing block's height (`parent_size`); only an
    ///   orthogonal flow, laid out in its own frame, takes the available height as its inline size.
    /// - A flex container reads a min-content available height as a max-content one (CSS Sizing 3 §5.1:
    ///   the min-content block size of a box is its max-content block size; taffy's
    ///   `compute_flexbox_layout` maps one to the other before its algorithm).
    ///
    /// A grid container's track sizing reads both, and so does a box in another writing mode.
    ///
    /// - A flex item that reads no percentage block size (its own heights and relative insets, and
    ///   what [`crate::layout::depends_on_percentage_block_size`] finds below it) lays out the same
    ///   whatever its containing block's height, so that height is left out (`parent_size`). Taffy
    ///   keys an item's final layout on it, and a column container passes its own height, which any
    ///   item growing changes: every other item missed the cache and ran its algorithm again.
    ///
    /// - A flex container measured without a known height reads its available height only to pass it
    ///   on: to its stretched items as their (not definite) height while their flex base sizes are
    ///   found, and to its items as their available height. When no item can turn a height it is
    ///   given into a width or a content height ([`given_height_reaches_width`]), the container's
    ///   measurements are the same for every available height. A row container laid out with a known
    ///   height measures its items again with that height available, after measuring them with none.
    fn unread_height_folded(&mut self, id: BoxId, mut inputs: LayoutInput) -> LayoutInput {
        let b = self.cx.tree.b(id);
        if b.style.view() != self.cx.frame {
            return inputs;
        }
        if b.kind == BoxKind::Flex
            && inputs.run_mode == RunMode::ComputeSize
            && inputs.known_dimensions.height.is_none()
            && matches!(inputs.available_space.height, AvailableSpace::Definite(_))
            && !given_height_reaches_width(self.cx.tree, id)
        {
            inputs.available_space.height = AvailableSpace::MaxContent;
        }
        let b = self.cx.tree.b(id);
        if inputs.parent_size.height.is_some()
            && b.parent
                .is_some_and(|p| self.cx.tree.b(p).kind == BoxKind::Flex)
            && !crate::layout::depends_on_percentage_block_size(self.cx.tree, id)
        {
            inputs.parent_size.height = None;
        }
        let height = &mut inputs.available_space.height;
        match b.kind {
            BoxKind::Grid => {}
            BoxKind::Flex => {
                if *height == AvailableSpace::MinContent {
                    *height = AvailableSpace::MaxContent;
                }
            }
            _ => *height = AvailableSpace::MaxContent,
        }
        inputs
    }

    /// Our layout as taffy's leaf: sizes from the box's content, full layout stored on the box.
    fn leaf(&mut self, id: BoxId, inputs: LayoutInput) -> LayoutOutput {
        let style = self.node(tid(id)).style.clone();
        let parent_size = inputs.parent_size;
        let cb_width = parent_size.width.unwrap_or(0.0);
        let cb_height = parent_size.height;
        let edges = Edges::of(&self.cx.style_here(id), cb_width);
        let inset_h = edges.pb_h();
        let inset_v = edges.pb_v();
        let definite_h = inputs.known_dimensions_are_definite.height;
        // Measured for its content size, the item's own sizes are taffy's to apply.
        let content_size = inputs.sizing_mode == SizingMode::ContentSize;
        let is_table = self.cx.tree.b(id).kind == BoxKind::TableWrapper;
        let is_replaced = self.cx.tree.b(id).kind == BoxKind::Replaced;
        let parent_kind = self.cx.tree.b(id).parent.map(|p| self.cx.tree.b(p).kind);
        let in_grid = parent_kind == Some(BoxKind::Grid);
        let flex_item = parent_kind == Some(BoxKind::Flex);
        let size = if inputs.run_mode == RunMode::PerformLayout {
            match (
                inputs.known_dimensions.width,
                inputs.known_dimensions.height,
            ) {
                (Some(w), Some(h)) => Some(taffy::Size {
                    width: w,
                    height: h,
                }),
                _ => None,
            }
        } else {
            None
        };
        let size = match size {
            Some(s) => s,
            None => {
                let calcs = self.cx.tree.calcs.clone();
                // Whether the box's width is settled before its content is looked at: imposed by the
                // container (a stretched or placed item), or its own definite `width`, resolved as
                // taffy's leaf resolves it. The available width the measure gets is then that
                // width's content box, and the box's height is its content's at that width (CSS 2
                // §10.6.3), not at the content's shrink-to-fit width. (A table's width is also
                // bounded below by its content, CSS 2 §17.5.2.)
                let own_width = !is_table
                    && (inputs.known_dimensions.width.is_some()
                        || (!content_size && {
                            use taffy::MaybeResolve;
                            style
                                .size
                                .maybe_resolve(parent_size, |v, basis| {
                                    resolve_calc(&calcs, v, basis)
                                })
                                .maybe_apply_aspect_ratio(style.aspect_ratio)
                                .width
                                .is_some()
                        }));
                let cx = &mut *self.cx;
                let out = compute_leaf_layout(
                    inputs,
                    &style,
                    |v, basis| resolve_calc(&calcs, v, basis),
                    |known, avail| {
                        if !is_replaced
                            && !is_table
                            && crate::logical::Flip::between(cx.tree.b(id).style.view(), cx.frame)
                                .swaps()
                        {
                            // An orthogonal flow (CSS Writing Modes 4 §7.3): its inline axis is
                            // the height here, its width is the block size it then takes.
                            let own = cx.own_content_sizes(id);
                            let h = match (known.height, avail.height) {
                                (Some(h), _) => (h - inset_v).max(0.0),
                                (None, AvailableSpace::Definite(a)) => {
                                    own.shrink_to_fit((a - inset_v).max(0.0))
                                }
                                (None, AvailableSpace::MinContent) => own.min,
                                (None, AvailableSpace::MaxContent) => own.max,
                            };
                            let mut space = Space::fill(cb_width, cb_height);
                            space.fixed_height = Some(h + inset_v);
                            space.fixed_width = known.width;
                            space.content_width = content_size;
                            cx.measuring_across =
                                cb_height.is_none() || parent_size.width.is_none();
                            let r = cx.layout(id, &space, None, Point::ZERO);
                            cx.measuring_across = false;
                            return taffy::Size {
                                width: (r.fragment.size.width - inset_h).max(0.0),
                                height: h,
                            };
                        }
                        if is_replaced {
                            // Sized for its inherent size (a grid item's contributions, css-grid-2
                            // §6.6 and §11.5, and its layout), its own definite `width`/`height`,
                            // and a size imposed by stretching (css-align-3 §6.1), transfer through
                            // the ratio as well (CSS 2 §10.3.2, §10.6.2).
                            // (taffy's leaf passes no known sizes to a full layout's measure)
                            let (mut kw, mut kh) = (
                                known.width.or(inputs.known_dimensions.width),
                                known.height.or(inputs.known_dimensions.height),
                            );
                            if !content_size {
                                let s = &cx.style_here(id);
                                let sizing = s.box_sizing();
                                kw = kw.or_else(|| {
                                    resolve_height(s.width(), parent_size.width, inset_h, sizing)
                                });
                                kh = kh.or_else(|| {
                                    resolve_height(s.height(), parent_size.height, inset_v, sizing)
                                });
                            }
                            let content = crate::replaced::content_size(
                                cx,
                                id,
                                (parent_size.width, cb_height),
                                kw,
                                kh,
                            );
                            if let Some((width, height)) = content {
                                return taffy::Size {
                                    width: width - inset_h,
                                    height: height - inset_v,
                                };
                            }
                        }
                        let settled = match avail.width {
                            AvailableSpace::Definite(a) if own_width => Some(a.max(0.0)),
                            _ => None,
                        };
                        let w = match (known.width, settled) {
                            (Some(w), _) => (w - inset_h).max(0.0),
                            // (taffy's leaf passes no known sizes to a full layout's measure)
                            (None, Some(w)) => w,
                            (None, None) => {
                                let c = if is_table {
                                    crate::table::flex_item_content_sizes(cx, id)
                                } else if in_grid {
                                    // css-grid-2 §11.5: a grid item's height that resolves
                                    // against its grid area is definite for the percentage
                                    // heights in its content (a replaced child's width follows
                                    // through its ratio).
                                    let s = &cx.style_here(id);
                                    let h = known.height.or_else(|| {
                                        resolve_height(
                                            s.height(),
                                            parent_size.height,
                                            inset_v,
                                            s.box_sizing(),
                                        )
                                    });
                                    let saved = std::mem::replace(
                                        &mut cx.pct_height_basis,
                                        h.map(|h| (h - inset_v).max(0.0)),
                                    );
                                    let c = cx.content_sizes(id);
                                    cx.pct_height_basis = saved;
                                    c
                                } else if let Some(h) = known.height.filter(|_| {
                                    // css-flexbox-1 §9.8: a flex item's definite height, for the
                                    // percentage heights of the replaced boxes it holds
                                    flex_item && definite_h && has_percent_height_replaced(cx, id)
                                }) {
                                    let saved = cx.pct_height_basis.replace((h - inset_v).max(0.0));
                                    let c = cx.content_sizes(id);
                                    cx.pct_height_basis = saved;
                                    c
                                } else {
                                    cx.content_sizes(id)
                                };
                                match avail.width {
                                    AvailableSpace::Definite(a) => c.shrink_to_fit(a.max(0.0)),
                                    AvailableSpace::MinContent => c.min,
                                    AvailableSpace::MaxContent => c.max,
                                }
                            }
                        };
                        let h = match known.height {
                            Some(h) => (h - inset_v).max(0.0),
                            // Only the width is asked for (an intrinsic inline size contribution,
                            // css-sizing-3 §5): taffy reads no height from it, so the box is not
                            // laid out, as LayoutNG takes min/max sizes without a layout.
                            None if inputs.axis == taffy::RequestedAxis::Horizontal => 0.0,
                            None => {
                                let mut space = Space::fill(cb_width, cb_height);
                                space.fixed_width = Some(w + inset_h);
                                space.available = w + inset_h;
                                space.content_height = content_size;
                                let r = cx.layout(id, &space, None, Point::ZERO);
                                (r.fragment.size.height - inset_v).max(0.0)
                            }
                        };
                        taffy::Size {
                            width: w,
                            height: h,
                        }
                    },
                );
                if inputs.run_mode != RunMode::PerformLayout {
                    return out;
                }
                out.size
            }
        };
        let mut space = Space::fill(cb_width, cb_height);
        space.fixed_width = Some(size.width);
        space.available = size.width;
        space.fixed_height = Some(size.height);
        space.fixed_height_definite = definite_h;
        let r = self.cx.layout(id, &space, None, Point::ZERO);
        let f = &r.fragment;
        let baselines = taffy::Baselines {
            first: f.baseline,
            last: f.last_baseline,
        };
        let out = LayoutOutput {
            size: taffy::Size {
                width: f.size.width,
                height: f.size.height,
            },
            scrollable_overflow_rect: taffy::Rect {
                left: f.overflow.x.min(0.0),
                right: f.overflow.right(),
                top: f.overflow.y.min(0.0),
                bottom: f.overflow.bottom(),
            },
            baselines,
            top_margin: taffy::CollapsibleMarginSet::ZERO,
            bottom_margin: taffy::CollapsibleMarginSet::ZERO,
            margins_can_collapse_through: false,
            oof_candidates: taffy::OofCandidates::NONE,
            oof_positioning_area: None,
        };
        self.node_mut(tid(id)).leaf = Some(r);
        out
    }
}

/// Whether flex container `id`'s measurements can change with a block size it is given that is not
/// definite: its available height, which it passes to its items as their available height, and to
/// its stretched items as their height while their flex base sizes are found (css-flexbox-1 §9.2,
/// §9.4). That is so when the container wraps its column into lines by that height, or an item can
/// turn a height into a width: a replaced box or one with an `aspect-ratio` (through its ratio,
/// css-sizing-4 §5), a box in another writing mode (whose inline size it is, CSS Writing Modes 4
/// §7.3), grid, table and multi-column layout (which size their tracks, rows and columns in it), and
/// a nested flex container with such an item. A block container's width does not depend on its
/// height (CSS 2 §10.3); a height that is not definite does not reach its children's percentages
/// (CSS 2 §10.5). Kept on the box until its subtree or styles change (the dirty path clears it).
fn given_height_reaches_width<S: Style>(tree: &mut BoxTree<S>, id: BoxId) -> bool {
    if let Some(v) = tree.b(id).cache.height_reaches_width {
        return v;
    }
    let b = tree.b(id);
    let s = &b.style;
    let mut reaches = style_ratio(s).is_some()
        || matches!(
            s.flex_direction(),
            FlexDirection::Column | FlexDirection::ColumnReverse
        ) && s.flex_wrap() != FlexWrap::Nowrap;
    let view = s.view();
    let mut i = 0;
    while !reaches && i < tree.b(id).children.len() {
        let c = tree.b(id).children[i];
        i += 1;
        let cb = tree.b(c);
        if cb.is_out_of_flow() || matches!(cb.kind, BoxKind::Text | BoxKind::LineBreak) {
            continue;
        }
        let cs = &cb.style;
        reaches = match cb.kind {
            _ if cs.view() != view || style_ratio(cs).is_some() || cs.is_multicol() => true,
            BoxKind::Replaced | BoxKind::Grid | BoxKind::TableWrapper | BoxKind::Table => true,
            BoxKind::Flex => given_height_reaches_width(tree, c),
            _ => false,
        };
    }
    tree.b_mut(id).cache.height_reaches_width = Some(reaches);
    reaches
}

/// Whether a replaced box with a percentage height is a child of `id` (or of its anonymous blocks):
/// then `id`'s intrinsic widths depend on its height.
fn has_percent_height_replaced<S: Style>(cx: &Cx<'_, S>, id: BoxId) -> bool {
    fn go<S: Style>(cx: &Cx<'_, S>, id: BoxId, depth: u32) -> bool {
        cx.tree.b(id).children.iter().any(|&c| {
            let b = cx.tree.b(c);
            match b.kind {
                BoxKind::Replaced => {
                    matches!(b.style.height(), SizeValue::LengthPercentage(l) if l.has_percent())
                }
                _ if b.is_anonymous() && depth < 4 => go(cx, c, depth + 1),
                _ => false,
            }
        })
    }
    go(cx, id, 0)
}

/// CSS Contain 2 §3.1/§3.2: a container under size containment whose size in a contained axis is
/// `auto` is sized as if it had no content: a flex container to its padding and border, a grid
/// container to its tracks without items (within its min/max sizes). The size found that way is then
/// the known size its content is laid out in.
fn contained_inputs<S: Style>(
    tree: &mut Tree<'_, '_, S>,
    node: taffy::NodeId,
    mut inputs: LayoutInput,
) -> LayoutInput {
    let id = bid(node);
    let b = tree.cx.tree.b(id);
    let s = &b.style;
    let contained = s.size_containment();
    if !contained.width && !contained.height {
        return inputs;
    }
    let parent = inputs.parent_size;
    let auto = |v: SizeValue, basis: Option<f32>| match v {
        SizeValue::Auto => true,
        SizeValue::LengthPercentage(lp) => lp.has_percent() && basis.is_none(),
        _ => false,
    };
    let width =
        contained.width && inputs.known_dimensions.width.is_none() && auto(s.width(), parent.width);
    let height = contained.height
        && inputs.known_dimensions.height.is_none()
        && auto(s.height(), parent.height);
    if !width && !height {
        return inputs;
    }
    let edges = Edges::of(s, parent.width.unwrap_or(0.0));
    let (pbh, pbv) = (edges.pb_h(), edges.pb_v());
    let (min_w, max_w) = width_bounds(s, parent.width, pbh, pbh, &mut || ContentSizes::ZERO);
    let (min_h, max_h) = height_bounds(s, parent.height, pbv);
    let grid = b.kind == BoxKind::Grid;
    let explicit = (contained.intrinsic_width, contained.intrinsic_height);
    let empty = if grid && (explicit.0.is_none() || explicit.1.is_none()) {
        let measure = LayoutInput {
            run_mode: RunMode::ComputeSize,
            sizing_mode: SizingMode::InherentSize,
            axis: taffy::RequestedAxis::Both,
            ..inputs
        };
        let saved = tree.empty.replace(node);
        let size = compute_grid_layout(tree, node, measure).size;
        tree.empty = saved;
        (size.width, size.height)
    } else {
        (pbh, pbv)
    };
    // CSS Sizing 4 §5.1: `contain-intrinsic-*` gives the content size instead.
    let empty = (
        explicit.0.map_or(empty.0, |w| w + pbh),
        explicit.1.map_or(empty.1, |h| h + pbv),
    );
    if width {
        inputs.known_dimensions.width = Some(clamp(empty.0, min_w, max_w).max(pbh));
    }
    if height {
        inputs.known_dimensions.height = Some(clamp(empty.1, min_h, max_h).max(pbv));
    }
    inputs
}

fn resolve_calc(calcs: &[LengthPercentage], v: *const (), basis: f32) -> f32 {
    let handle = v.addr() >> 3;
    let i = (handle >> 1).wrapping_sub(1);
    let value = calcs.get(i).map_or(0.0, |lp| lp.resolve(basis));
    if handle & 1 == 1 {
        value.max(0.0)
    } else {
        value
    }
}

impl<S: Style> TraversePartialTree for Tree<'_, '_, S> {
    type ChildIter<'b>
        = std::iter::Map<std::slice::Iter<'b, BoxId>, fn(&BoxId) -> taffy::NodeId>
    where
        Self: 'b;

    fn child_ids(&self, parent: taffy::NodeId) -> Self::ChildIter<'_> {
        fn conv(b: &BoxId) -> taffy::NodeId {
            tid(*b)
        }
        self.children(parent)
            .iter()
            .map(conv as fn(&BoxId) -> taffy::NodeId)
    }

    fn child_count(&self, parent: taffy::NodeId) -> usize {
        if self.empty == Some(parent) {
            return 0;
        }
        self.cx.tree.b(bid(parent)).children.len()
    }

    fn get_child_id(&self, parent: taffy::NodeId, index: usize) -> taffy::NodeId {
        tid(self.children(parent)[index])
    }
}

impl<S: Style> LayoutPartialTree for Tree<'_, '_, S> {
    type CoreContainerStyle<'b>
        = &'b TStyle
    where
        Self: 'b;
    type CustomIdent = Arc<str>;

    fn get_core_container_style(&self, node: taffy::NodeId) -> Self::CoreContainerStyle<'_> {
        self.container_style(node)
    }

    fn resolve_calc_value(&self, val: *const (), basis: f32) -> f32 {
        resolve_calc(&self.cx.tree.calcs, val, basis)
    }

    fn set_unrounded_layout(&mut self, node: taffy::NodeId, layout: &taffy::Layout) {
        self.node_mut(node).layout = *layout;
    }

    fn compute_child_layout(&mut self, node: taffy::NodeId, inputs: LayoutInput) -> LayoutOutput {
        if inputs.run_mode == RunMode::PerformHiddenLayout {
            return LayoutOutput::HIDDEN;
        }
        let inputs = self.unread_height_folded(bid(node), inputs);
        compute_cached_layout(self, node, inputs, |tree, node, inputs| {
            let id = bid(node);
            let foreign = tree.cx.tree.b(id).style.view() != tree.cx.frame;
            match tree.cx.tree.b(id).kind {
                // (a container in another writing mode is laid out through `Cx::layout`)
                BoxKind::Flex | BoxKind::Grid if !foreign => {
                    ensure_nodes(tree.cx, id);
                    let n = tree.node_mut(node);
                    n.runs = n.runs.wrapping_add(1);
                    tree.cx.stats.algorithm_runs += 1;
                    // css-grid-2 §8.5: grid items are auto-placed in order-modified document order.
                    order_children(tree.cx, id);
                    let inputs = contained_inputs(tree, node, inputs);
                    let mut out = if tree.cx.tree.b(id).kind == BoxKind::Flex {
                        compute_flexbox_layout(tree, node, inputs)
                    } else {
                        compute_grid_layout(tree, node, inputs)
                    };
                    if inputs.run_mode == RunMode::PerformLayout {
                        // Out-of-flow children are positioned by our code, from this container.
                        let oof = out.oof_candidates.take();
                        let n = tree.node_mut(node);
                        n.oof = oof.iter().copied().collect();
                    }
                    out
                }
                _ => tree.leaf(id, inputs),
            }
        })
    }
}

impl<S: Style> CacheTree for Tree<'_, '_, S> {
    fn cache_get(&mut self, node: taffy::NodeId, input: &LayoutInput) -> Option<LayoutOutput> {
        self.node_mut(node).cache.get(input)
    }

    fn cache_store(&mut self, node: taffy::NodeId, input: &LayoutInput, output: LayoutOutput) {
        self.node_mut(node).cache.store(input, output)
    }

    fn cache_clear(&mut self, node: taffy::NodeId) {
        self.node_mut(node).cache.clear();
    }
}

impl<S: Style> taffy::LayoutFlexboxContainer for Tree<'_, '_, S> {
    type FlexboxContainerStyle<'b>
        = &'b TStyle
    where
        Self: 'b;
    type FlexboxItemStyle<'b>
        = &'b TStyle
    where
        Self: 'b;

    fn get_flexbox_container_style(&self, node: taffy::NodeId) -> Self::FlexboxContainerStyle<'_> {
        self.container_style(node)
    }

    fn get_flexbox_child_style(&self, child: taffy::NodeId) -> Self::FlexboxItemStyle<'_> {
        &self.node(child).style
    }
}

impl<S: Style> taffy::LayoutGridContainer for Tree<'_, '_, S> {
    type GridContainerStyle<'b>
        = &'b TStyle
    where
        Self: 'b;
    type GridItemStyle<'b>
        = &'b TStyle
    where
        Self: 'b;

    fn get_grid_container_style(&self, node: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.container_style(node)
    }

    fn get_grid_child_style(&self, child: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.node(child).style
    }

    fn set_detailed_grid_info(
        &mut self,
        node: taffy::NodeId,
        info: taffy::DetailedGridInfo<Self::CustomIdent>,
    ) {
        let n = self.node_mut(node);
        n.grid_rows.clear();
        n.grid_rows.extend(info.items.iter().map(|i| i.row_start));
        n.grid = Some(Box::new(GridTracks {
            columns: info.columns.clone(),
            rows: info.rows.clone(),
        }));
        n.grid_info = Some(Box::new(info));
    }

    fn set_subgrid_tracks(
        &mut self,
        node: taffy::NodeId,
        tracks: &taffy::SubgridTracks<Self::CustomIdent>,
    ) -> bool {
        let id = bid(node);
        let b = self.cx.tree.b(id);
        // A subgrid in another writing mode or direction than its parent grid is laid out as an
        // ordinary grid here.
        let parent_direction = b.parent.map(|p| self.cx.tree.b(p).style.direction());
        if b.style.view() != self.cx.frame || parent_direction != Some(b.style.direction()) {
            return false;
        }
        ensure_nodes(self.cx, id);
        order_children(self.cx, id);
        let n = self.node_mut(node);
        if n.own_style.is_some() {
            return false;
        }
        if n.subgrid.as_ref().is_some_and(|s| s.0 == *tracks) {
            return true;
        }
        let view = n.style.subgrid_view(tracks);
        n.subgrid = Some(Box::new((tracks.clone(), view)));
        n.cache.clear();
        true
    }
}

// ----- entry points -----

/// Min-content and max-content content widths of a flex or grid container.
pub(crate) fn content_sizes<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) -> ContentSizes {
    ensure_nodes(cx, id);
    let style = &cx.tree.b(id).style;
    let edges = Edges::of(style, 0.0);
    // A height that is definite without a containing block (a length) is known while the widths are
    // measured: the items' cross sizes and percentages depend on it (css-flexbox-1 §9.8), and the
    // container's inline size is computed after it only in writing modes we don't have.
    let (min_h, max_h) = height_bounds(style, None, edges.pb_v());
    // (a percentage of a parent's length is as definite)
    let basis = cx.tree.b(id).parent.and_then(|p| {
        let parent = cx.tree.b(p);
        let ps = &parent.style;
        let e = Edges::of(ps, 0.0);
        resolve_height(ps.height(), None, e.pb_v(), ps.box_sizing())
            .filter(|_| !parent.is_anonymous() && ps.view() == style.view())
            .map(|h| (h - e.pb_v()).max(0.0))
    });
    let height = resolve_height(style.height(), basis, edges.pb_v(), style.box_sizing())
        .map(|h| clamp(h, min_h, max_h))
        .filter(|_| {
            // (single-line flex containers: a multi-line column container measured with its height
            // known would wrap into lines its intrinsic widths do not have, css-flexbox-1 §9.9.2)
            cx.tree.b(id).kind == BoxKind::Flex && style.flex_wrap() == FlexWrap::Nowrap
        });
    let mut measure = |space: AvailableSpace| {
        let inputs = LayoutInput {
            run_mode: RunMode::ComputeSize,
            sizing_mode: SizingMode::ContentSize,
            axis: taffy::RequestedAxis::Horizontal,
            known_dimensions: taffy::Size {
                width: None,
                height,
            },
            known_dimensions_are_definite: taffy::Size {
                width: true,
                height: true,
            },
            parent_size: taffy::Size::NONE,
            available_space: taffy::Size {
                width: space,
                height: AvailableSpace::MaxContent,
            },
            vertical_margins_are_collapsible: taffy::Line::FALSE,
        };
        let mut tree = Tree {
            cx: &mut *cx,
            empty: None,
        };
        tree.compute_child_layout(tid(id), inputs).size.width
    };
    let min = measure(AvailableSpace::MinContent);
    let max = measure(AvailableSpace::MaxContent);
    ContentSizes {
        min: (min - edges.pb_h()).max(0.0),
        max: (max - edges.pb_h()).max(min - edges.pb_h()).max(0.0),
    }
}

/// Lays out a flex or grid container in our flow: the width from our block rules, the rest by taffy.
pub(crate) fn layout_container<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    space: &Space,
) -> LayoutResult<S> {
    ensure_nodes(cx, id);
    let mut sizes = crate::block::block_sizes(cx, id, space);
    // The ratio taken off taffy's style while the content height overrides it (taffy would also
    // transfer it through `max-width` into a maximum height), put back after the layout.
    let mut ratio_off: Option<Option<f32>> = None;
    // css-sizing-4 §5.1: a height from `aspect-ratio` (with `height: auto`) has an automatic minimum
    // of the content height when `min-height` is `auto` and the box is not a scroll container.
    if let (None, Some(ratio_height)) = (space.fixed_height, sizes.height) {
        let s = &cx.tree.b(id).style;
        let edges = &sizes.edges;
        let from_ratio = resolve_height(s.height(), space.cb_height, edges.pb_v(), s.box_sizing())
            .is_none()
            && style_ratio(s).is_some()
            && matches!(s.min_height(), SizeValue::Auto)
            && !s.overflow_x().is_scroll_container()
            && !s.overflow_y().is_scroll_container();
        if from_ratio {
            let measure = LayoutInput {
                run_mode: RunMode::ComputeSize,
                sizing_mode: SizingMode::ContentSize,
                axis: taffy::RequestedAxis::Vertical,
                known_dimensions: taffy::Size {
                    width: Some(sizes.width),
                    height: None,
                },
                known_dimensions_are_definite: taffy::Size {
                    width: true,
                    height: false,
                },
                parent_size: taffy::Size {
                    width: Some(space.edges_basis()),
                    height: space.cb_height,
                },
                available_space: taffy::Size {
                    width: AvailableSpace::Definite(sizes.width),
                    height: AvailableSpace::MaxContent,
                },
                vertical_margins_are_collapsible: taffy::Line::FALSE,
            };
            // (measured without the ratio, which taffy also transfers through the max-width)
            let node = cx.tree.b_mut(id).cache.taffy.as_mut().unwrap();
            let ratio = node.style.aspect_ratio.take();
            node.cache.clear();
            let content = Tree {
                cx: &mut *cx,
                empty: None,
            }
            .compute_child_layout(tid(id), measure)
            .size
            .height;
            let node = cx.tree.b_mut(id).cache.taffy.as_mut().unwrap();
            if content > ratio_height {
                sizes.height = Some(content);
                sizes.min_height = Some(sizes.min_height.unwrap_or(0.0).max(content));
                ratio_off = Some(ratio);
            } else {
                node.style.aspect_ratio = ratio;
                node.cache.clear();
            }
        }
    }
    let inputs = LayoutInput {
        run_mode: RunMode::PerformLayout,
        sizing_mode: SizingMode::InherentSize,
        axis: taffy::RequestedAxis::Both,
        known_dimensions: taffy::Size {
            width: Some(sizes.width),
            height: sizes.height,
        },
        known_dimensions_are_definite: taffy::Size {
            width: true,
            height: space.fixed_height.is_none() || space.fixed_height_definite,
        },
        parent_size: taffy::Size {
            width: Some(space.edges_basis()),
            height: space.cb_height,
        },
        available_space: taffy::Size {
            width: AvailableSpace::Definite(sizes.width),
            // (the parent's inline size bounds nothing in an orthogonal flow's block axis)
            height: if space.edge_basis.is_some() {
                sizes.height
            } else {
                space.cb_height
            }
            .map_or(AvailableSpace::MaxContent, AvailableSpace::Definite),
        },
        vertical_margins_are_collapsible: taffy::Line::FALSE,
    };
    // Taffy's caches persist across our layouts; a dirty container starts over.
    let out = {
        let mut tree = Tree {
            cx: &mut *cx,
            empty: None,
        };
        tree.compute_child_layout(tid(id), inputs)
    };
    if let Some(ratio) = ratio_off {
        let node = cx.tree.b_mut(id).cache.taffy.as_mut().unwrap();
        node.style.aspect_ratio = ratio;
        node.cache.clear();
    }
    let height = clamp(out.size.height, sizes.min_height, sizes.max_height);
    let edges = sizes.edges;
    // Taffy stores a node's layout from its parent's algorithm; a container in our flow has no taffy
    // parent, so its own border and padding (CSS 2 §8) come from the sizes resolved above.
    if let Some(node) = cx.tree.b_mut(id).cache.taffy.as_mut() {
        let rect = |s: Sides| taffy::Rect {
            left: s.left,
            right: s.right,
            top: s.top,
            bottom: s.bottom,
        };
        node.layout.border = rect(edges.border);
        node.layout.padding = rect(edges.padding);
        node.layout.size = taffy::Size {
            width: sizes.width,
            height,
        };
    }
    let (fragment, oof) = build_fragment(cx, id, Some(Size::new(sizes.width, height)));
    LayoutResult {
        fragment,
        margin: Sides::new(
            edges.mt(),
            sizes.margin_right,
            edges.mb(),
            sizes.margin_left,
        ),
        start: Strut::of(edges.mt()),
        end: Strut::of(edges.mb()),
        through: false,
        floats: Vec::new(),
        oof,
    }
}

/// css-grid-2 §9.1: an absolutely positioned box whose containing block is a grid container has the
/// grid area its grid-placement properties determine as its containing block (lines that are `auto`,
/// or that do not exist in the grid, stand for the grid container's padding edges). Lays out the
/// candidates whose containing block is the grid `id` (padding box `pad` in its fragment's
/// coordinates), appending their fragments to `children`; returns the rest.
fn place_grid_oofs<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    pad: Rect,
    candidates: Vec<Oof>,
    children: &mut Vec<FragmentChild<S>>,
) -> Vec<Oof> {
    let info = cx
        .tree
        .b_mut(id)
        .cache
        .taffy
        .as_mut()
        .and_then(|t| t.grid_info.take());
    let Some(info) = info else {
        return cx.place_oofs(id, pad, candidates, children);
    };
    let direction = match cx.tree.b(id).style.direction() {
        Direction::Ltr => taffy::Direction::Ltr,
        Direction::Rtl => taffy::Direction::Rtl,
    };
    let padding_box = taffy::Rect {
        left: pad.x,
        right: pad.x + pad.width,
        top: pad.y,
        bottom: pad.y + pad.height,
    };
    let mut rest = Vec::new();
    for c in candidates {
        if c.cb != Some(id) {
            rest.push(c);
            continue;
        }
        let s = &cx.tree.b(c.id).style;
        let area = info.resolve_absolute_grid_area(
            taffy::Line {
                start: placement(s.grid_row_start()),
                end: placement(s.grid_row_end()),
            },
            taffy::Line {
                start: placement(s.grid_column_start()),
                end: placement(s.grid_column_end()),
            },
            direction,
            padding_box,
        );
        let rect = Rect::new(
            area.left,
            area.top,
            (area.right - area.left).max(0.0),
            (area.bottom - area.top).max(0.0),
        );
        let (child, more) = crate::positioned::layout_absolute(cx, c, rect);
        children.push(child);
        rest.extend(more);
    }
    if let Some(t) = cx.tree.b_mut(id).cache.taffy.as_mut() {
        t.grid_info = Some(info);
    }
    rest
}

/// A grid container's grid area (its tracks from its last layout), in its fragment's coordinates:
/// part of its in-flow bounds ([`Fragment::inflow`]), so that tracks overflowing a scroll container
/// can be scrolled to, with its end padding, even with no item in them (CSS Overflow 3 §2.2).
fn grid_tracks_rect<S: Style>(cx: &Cx<'_, S>, id: BoxId) -> Option<Rect> {
    let info = cx.tree.b(id).cache.taffy.as_ref()?.grid_info.as_ref()?;
    let extent = |p: &[taffy::Line<f32>]| {
        p.iter().fold(None, |acc: Option<(f32, f32)>, l| {
            let (a, b) = (l.start.min(l.end), l.start.max(l.end));
            Some(acc.map_or((a, b), |(x, y)| (x.min(a), y.max(b))))
        })
    };
    let (Some((x0, x1)), Some((y0, y1))) = (
        extent(&info.columns.positions),
        extent(&info.rows.positions),
    ) else {
        return None;
    };
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// An item of a flex or grid container, for the container's baseline.
struct BaselineItem<'a, S: Style> {
    /// It participates in first-baseline alignment (`align-self: baseline`).
    participates: bool,
    /// It participates in last-baseline alignment (`align-self: last baseline`).
    participates_last: bool,
    /// A grid item's first row line (1-based).
    row: u16,
    child: &'a FragmentChild<S>,
}

// (not derived: that would require `S: Copy`)
impl<S: Style> Clone for BaselineItem<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S: Style> Copy for BaselineItem<'_, S> {}

/// The first baseline of a flex or grid container from its items (in order-modified document order),
/// from the container's border-box top. css-flexbox-1 §8.5 and css-grid-2 §11.8: the shared
/// alignment baseline of the items in the first line (row) that participate in baseline alignment;
/// otherwise the first item's baseline, synthesized from its border box's bottom edge when it has
/// none (CSS Align 3 §9.1); no items, no baseline. Unlike a block container's, it is kept when the
/// container is a scroll container. "First" is the topmost line (for `wrap-reverse` too) and its
/// start-most item (the leftmost for `row-reverse`), as Chrome takes them.
fn first_baseline<S: Style>(
    grid: bool,
    direction: FlexDirection,
    wraps: bool,
    central: bool,
    items: &[BaselineItem<'_, S>],
) -> Option<f32> {
    // (synthesized from the bottom edge, or the middle for the central baseline: CSS Inline 3 §4.1)
    let item_baseline = |c: &FragmentChild<S>| {
        let h = c.fragment.size.height;
        c.offset.y
            + c.fragment
                .baseline
                .unwrap_or(if central { h - h / 2.0 } else { h })
    };
    if items.is_empty() {
        return None;
    }
    // The items of the first line (flex) or row (grid), in the order to take the first from.
    let first: Vec<BaselineItem<'_, S>> = if grid {
        let top = items.iter().map(|i| i.row).min().unwrap_or(0);
        let mut row: Vec<BaselineItem<'_, S>> =
            items.iter().copied().filter(|i| i.row == top).collect();
        // grid order within the row: by column
        row.sort_by(|a, b| a.child.offset.x.total_cmp(&b.child.offset.x));
        row
    } else {
        match direction {
            FlexDirection::Row | FlexDirection::RowReverse => {
                let reverse = direction == FlexDirection::RowReverse;
                let lines = row_lines(items, reverse, wraps);
                let line_top = |l: &Vec<BaselineItem<'_, S>>| {
                    l.iter()
                        .map(|i| i.child.offset.y)
                        .fold(f32::INFINITY, f32::min)
                };
                let mut line = lines
                    .into_iter()
                    .min_by(|a, b| line_top(a).total_cmp(&line_top(b)))
                    .unwrap_or_default();
                if reverse {
                    line.reverse();
                }
                line
            }
            // Baseline alignment of a column container's items is not along its main axis.
            FlexDirection::Column => items
                .iter()
                .map(|&i| BaselineItem {
                    participates: false,
                    participates_last: false,
                    ..i
                })
                .collect(),
            FlexDirection::ColumnReverse => items
                .iter()
                .rev()
                .map(|&i| BaselineItem {
                    participates: false,
                    participates_last: false,
                    ..i
                })
                .collect(),
        }
    };
    // (a grid's first baseline is its first-baseline group's; a flex line has one shared baseline)
    if let Some(i) = first
        .iter()
        .find(|i| i.participates || (i.participates_last && !grid))
    {
        return Some(item_baseline(i.child));
    }
    first.first().map(|i| item_baseline(i.child))
}

/// The lines of a row flex container (only a wrapping one has more than one): a line ends where the
/// next item starts before the previous one ends on the main axis (full-width items all start at
/// the same position, so the position alone does not tell).
fn row_lines<'a, S: Style>(
    items: &[BaselineItem<'a, S>],
    reverse: bool,
    wraps: bool,
) -> Vec<Vec<BaselineItem<'a, S>>> {
    let mut lines: Vec<Vec<BaselineItem<'a, S>>> = vec![vec![items[0]]];
    for w in items.windows(2) {
        let (a, b) = (w[0].child, w[1].child);
        let (a_end, b_end) = (
            a.offset.x + a.fragment.size.width,
            b.offset.x + b.fragment.size.width,
        );
        let new_line = if reverse {
            b_end > a.offset.x + 0.01
        } else {
            b.offset.x < a_end - 0.01
        };
        if wraps && new_line {
            lines.push(Vec::new());
        }
        lines.last_mut().unwrap().push(w[1]);
    }
    lines
}

/// The last baseline of a flex or grid container, as [`first_baseline`] from the other end: the
/// items of the bottommost line or row in last-baseline alignment, else its endmost item.
fn last_baseline<S: Style>(
    grid: bool,
    direction: FlexDirection,
    wraps: bool,
    central: bool,
    items: &[BaselineItem<'_, S>],
) -> Option<f32> {
    let item_baseline = |c: &FragmentChild<S>| {
        let h = c.fragment.size.height;
        let b = c.fragment.last_baseline.or(c.fragment.baseline);
        c.offset.y + b.unwrap_or(if central { h - h / 2.0 } else { h })
    };
    if items.is_empty() {
        return None;
    }
    let by = |l: &mut Vec<BaselineItem<'_, S>>, key: fn(&FragmentChild<S>) -> f32| {
        l.sort_by(|a, b| key(a.child).total_cmp(&key(b.child)));
    };
    let mut last: Vec<BaselineItem<'_, S>> = if grid {
        let bottom = items.iter().map(|i| i.row).max().unwrap_or(0);
        let mut row: Vec<BaselineItem<'_, S>> =
            items.iter().copied().filter(|i| i.row == bottom).collect();
        by(&mut row, |c| c.offset.x);
        row
    } else {
        match direction {
            FlexDirection::Row | FlexDirection::RowReverse => {
                let reverse = direction == FlexDirection::RowReverse;
                let line_bottom = |l: &Vec<BaselineItem<'_, S>>| {
                    l.iter()
                        .map(|i| i.child.offset.y + i.child.fragment.size.height)
                        .fold(f32::NEG_INFINITY, f32::max)
                };
                let mut line = row_lines(items, reverse, wraps)
                    .into_iter()
                    .max_by(|a, b| line_bottom(a).total_cmp(&line_bottom(b)))
                    .unwrap_or_default();
                by(&mut line, |c| c.offset.x);
                line
            }
            // (baseline alignment of a column container's items is not along its main axis)
            FlexDirection::Column | FlexDirection::ColumnReverse => {
                let mut column: Vec<BaselineItem<'_, S>> = items
                    .iter()
                    .map(|&i| BaselineItem {
                        participates: false,
                        participates_last: false,
                        ..i
                    })
                    .collect();
                by(&mut column, |c| c.offset.y);
                column
            }
        }
    };
    last.reverse();
    if let Some(i) = last
        .iter()
        .find(|i| i.participates_last || (i.participates && !grid))
    {
        return Some(item_baseline(i.child));
    }
    last.first().map(|i| item_baseline(i.child))
}

/// The fragment of a container nested in a taffy layout. Its fragment depends only on its items'
/// layouts, which only a run of its algorithm changes, and on its own size and edges: while taffy
/// has not run it again (its layout was a cache hit, or its parent's run did not reach it) and those
/// are the same, the fragment built after its last run stands, as a clean box's does in our flow.
/// A change inside it marks it dirty, which clears taffy's cache and so reruns it.
fn nested_fragment<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) -> (Arc<Fragment<S>>, Vec<Oof>) {
    let node = cx.tree.b(id).cache.taffy.as_ref().unwrap();
    let l = &node.layout;
    if let Some(b) = node.built.as_ref().filter(|b| {
        b.runs == node.runs && b.size == l.size && b.border == l.border && b.padding == l.padding
    }) {
        cx.stats.reused += 1;
        return (b.fragment.clone(), b.oof.clone());
    }
    let (fragment, oof) = build_fragment(cx, id, None);
    let node = cx.tree.b_mut(id).cache.taffy.as_mut().unwrap();
    let l = node.layout;
    node.built = Some(Box::new(Built {
        runs: node.runs,
        size: l.size,
        border: l.border,
        padding: l.padding,
        fragment: fragment.clone(),
        oof: oof.clone(),
    }));
    (fragment, oof)
}

/// The fragment of a container laid out by taffy, from its children's taffy layouts (a container
/// laid out in our flow passes its size; `layout_container` records its border and padding in its
/// taffy layout, which taffy itself only sets from a parent's algorithm).
fn build_fragment<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    size: Option<Size>,
) -> (Arc<Fragment<S>>, Vec<Oof>) {
    cx.stats.containers_built += 1;
    let node = cx.tree.b(id).cache.taffy.as_ref().unwrap();
    let layout = node.layout;
    let size = size.unwrap_or(Size::new(layout.size.width, layout.size.height));
    let border = Sides::new(
        layout.border.top,
        layout.border.right,
        layout.border.bottom,
        layout.border.left,
    );
    let padding = Sides::new(
        layout.padding.top,
        layout.padding.right,
        layout.padding.bottom,
        layout.padding.left,
    );
    let taffy_oof = node.oof.clone();
    let is_flex = cx.tree.b(id).kind == BoxKind::Flex;
    let kids = cx.tree.b(id).children.clone();
    let grid = cx.tree.b(id).kind == BoxKind::Grid;
    let mut items: Vec<(i32, usize, FragmentChild<S>)> = Vec::new();
    // Per item (by child index): whether it participates in first-baseline alignment.
    let mut participates: Vec<bool> = vec![false; kids.len()];
    let mut participates_last: Vec<bool> = vec![false; kids.len()];
    let align_items = cx.tree.b(id).style.align_items();
    let mut oof: Vec<Oof> = Vec::new();
    // The items' margin boxes (CSS Overflow 3 §2.2: they count toward a scroll container's
    // scrollable overflow).
    let mut inflow: Option<Rect> = None;
    for (i, c) in kids.iter().copied().enumerate() {
        let cb = cx.tree.b(c);
        // A collapsed flex item is not rendered (css-flexbox-1 §4.4); taffy left it out.
        if cb.is_out_of_flow() || (is_flex && cb.style.visibility() == Visibility::Collapse) {
            continue;
        }
        let order = cb.style.order();
        let l = cb
            .cache
            .taffy
            .as_ref()
            .map(|t| t.layout)
            .unwrap_or_default();
        let offset = Point::new(l.location.x, l.location.y);
        // (in-flow bounds are taken before relative positioning, which taffy applied)
        let rel = cx.relative_offset(
            &cb.style,
            (size.width - border.left - border.right - padding.left - padding.right).max(0.0),
            Some(
                (size.height - border.top - border.bottom - padding.top - padding.bottom).max(0.0),
            ),
        );
        let (x, y) = (l.location.x - rel.x, l.location.y - rel.y);
        let margin_box = Rect::from_ltrb(
            x - l.margin.left,
            y - l.margin.top,
            x + l.size.width + l.margin.right,
            y + l.size.height + l.margin.bottom,
        );
        inflow = Some(inflow.map_or(margin_box, |a| unite(a, margin_box)));
        let frag = match cb.kind {
            BoxKind::Flex | BoxKind::Grid => {
                let (f, o) = nested_fragment(cx, c);
                oof.extend(o.into_iter().map(|o| o.shifted(offset.x, offset.y)));
                f
            }
            _ => {
                let leaf = cx
                    .tree
                    .b(c)
                    .cache
                    .taffy
                    .as_ref()
                    .and_then(|t| t.leaf.as_ref());
                match leaf {
                    Some(r) => {
                        oof.extend(r.oof.iter().map(|o| o.shifted(offset.x, offset.y)));
                        r.fragment.clone()
                    }
                    None => continue,
                }
            }
        };
        let align = match cx.tree.b(c).style.align_self() {
            Align::Auto => align_items,
            a => a,
        };
        // (an item with an auto margin in the block axis is aligned by its margins instead,
        // css-flexbox-1 §8.1, css-grid-2 §11.2)
        let cs = &cx.style_here(c);
        let margins = !cs.margin(Side::Top).is_auto() && !cs.margin(Side::Bottom).is_auto();
        participates[i] = align == Align::Baseline && margins;
        participates_last[i] = align == Align::LastBaseline && margins;
        items.push((
            order,
            i,
            FragmentChild {
                offset,
                fragment: frag,
                stacking_owner: 0,
            },
        ));
    }
    // Painting follows order-modified document order (css-flexbox-1 §5.4, css-grid-2 §9).
    items.sort_by_key(|(o, i, _)| (*o, *i));
    let (baseline, last_line_baseline) = {
        let b = cx.tree.b(id);
        // Grid items' rows: taffy lists its in-flow items in document order.
        // (in the order taffy took them: order-modified document order)
        let mut row_of: Vec<u16> = vec![0; kids.len()];
        if b.kind == BoxKind::Grid {
            let t = b.cache.taffy.as_ref().unwrap();
            let rows = &t.grid_rows;
            let seq: &[BoxId] = if t.order.is_empty() { &kids } else { &t.order };
            let in_flow = seq.iter().filter(|&&c| !cx.tree.b(c).is_out_of_flow());
            for (k, c) in in_flow.enumerate() {
                if let Some(i) = kids.iter().position(|x| x == c) {
                    row_of[i] = rows.get(k).copied().unwrap_or(0);
                }
            }
        }
        let in_order: Vec<BaselineItem<'_, S>> = items
            .iter()
            .map(|(_, i, c)| BaselineItem {
                participates: participates[*i],
                participates_last: participates_last[*i],
                row: row_of[*i],
                child: c,
            })
            .collect();
        let (grid, direction) = (b.kind == BoxKind::Grid, b.style.flex_direction());
        let wraps = b.style.flex_wrap() != FlexWrap::Nowrap;
        let central = crate::inline::central_baseline(&b.style);
        (
            first_baseline(grid, direction, wraps, central, &in_order),
            last_baseline(grid, direction, wraps, central, &in_order),
        )
    };
    let mut children: Vec<FragmentChild<S>> = items.into_iter().map(|(_, _, c)| c).collect();
    for cand in taffy_oof {
        let c = bid(cand.node);
        let edge = |e: taffy::AxisStaticEdge| match e {
            taffy::AxisStaticEdge::Start => StaticAlign::Start,
            taffy::AxisStaticEdge::Center => StaticAlign::Center,
            taffy::AxisStaticEdge::End => StaticAlign::End,
        };
        let sp = |p: taffy::AxisStaticPosition| StaticPos {
            start: p.area.start,
            end: p.area.end,
            align: edge(p.align.keyword),
            safe_fallback: (p.align.safety == taffy::AlignmentSafety::Safe)
                .then(|| edge(p.align.fallback)),
        };
        let o = cx.oof_candidate(c, sp(cand.static_position.x), sp(cand.static_position.y));
        oof.push(o);
    }
    // (taffy took the space of the container's custom scroll bars out of its content box)
    let gutter = crate::gutter::gutter(cx.tree.b(id));
    if oof.iter().any(|o| o.cb == Some(id)) {
        // (the padding box, inside the scroll bars)
        let pad = Rect::new(
            border.left + gutter.left,
            border.top,
            (size.width - border.left - border.right - gutter.left - gutter.right).max(0.0),
            (size.height - border.top - border.bottom - gutter.bottom).max(0.0),
        );
        if grid {
            oof = place_grid_oofs(cx, id, pad, oof, &mut children);
        } else {
            oof = cx.place_oofs(id, pad, oof, &mut children);
        }
    }
    // (a grid container's in-flow bounds also cover its grid: CSS Overflow 3 §2.2 with css-grid-2
    // §5.3, the scrollable overflow includes the tracks, padded by the end padding)
    if grid {
        if let Some(tracks) = grid_tracks_rect(cx, id) {
            inflow = Some(inflow.map_or(tracks, |a| unite(a, tracks)));
        }
    }
    let fragment = with_scrollbar(
        with_inflow(
            box_fragment(
                cx,
                id,
                size,
                border,
                padding,
                children,
                baseline,
                last_line_baseline,
                BoxContent::None,
                0,
            ),
            inflow,
        ),
        gutter,
    );
    (fragment, oof)
}
