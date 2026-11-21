//! Snowlark patch: subgrids (css-grid-2 §9 "Subgrids").
//!
//! A grid item whose `grid-template-columns`/`-rows` is `subgrid` takes its tracks in that axis from the
//! grid it is placed in. Its own items take part in that grid's track sizing in the subgridded axes, as if
//! they were the grid's items, with the subgrid's margin, border and padding (and half the difference of
//! the gutters) as an extra layer of margin at its edges; the subgrid itself does not. A subgrid nested in a
//! subgrid in the same axis passes its items on in turn. Each subgrid is then laid out with its parent's
//! used track sizes as its explicit tracks ([`crate::Style::subgrid_view`]).
//!
//! Left: a subgrid's items are aligned to the baselines of its own rows only; a subgrid in another writing
//! mode or direction than its parent is taken as an ordinary grid item (the tree decides); in a subgrid
//! subgridded in both axes, a nested subgrid's items are measured without its own columns' geometry.
use super::types::{CellOccupancyMatrix, GridItem, GridLineNames, GridTrack, NamedLineResolver, TrackCounts};
use super::{
    align_tracks, compute_explicit_grid_size_in_axis, compute_grid_size_estimate, initialize_grid_tracks,
    place_grid_items, AutoRepeatStrategy, ItemPlacement, OriginZeroLine, MAX_GRID_TRACKS,
};
use crate::compute::grid::track_sizing::{
    determine_if_item_crosses_flexible_or_intrinsic_tracks, resolve_item_track_indexes, track_sizing_algorithm,
};
use crate::geometry::{AbsoluteAxis, AbstractAxis, InBothAbsAxis, Line, Rect, Size};
use crate::style::{AlignContent, AlignItems, AvailableSpace, SubgridAxisTracks, SubgridTracks};
use crate::tree::{LayoutGridContainer, LayoutPartialTreeExt, NodeId};
use crate::util::sys::{f32_max, GridTrackVec, Vec};
use crate::util::ResolveOrZero;
use crate::{BoxGenerationMode, CoreStyle, Dimension, GridContainerStyle, GridItemStyle, LengthPercentageAuto};

/// A subgrid among the items of a grid (or of a subgrid)
pub(super) struct Subgrid<S: crate::CheapCloneStr> {
    /// Its index among its parent's items (a grid's own items stay in front of its subgrids' items)
    item: usize,
    /// Its node
    node: NodeId,
    /// Its subgridded axes
    axes: Size<bool>,
    /// Its margin, border and padding at each edge
    edges: Rect<f32>,
    /// Its gutters in its subgridded axes
    gap: Size<f32>,
    /// Its own gutters (for the subgrids nested in it)
    own_gap: Size<f32>,
    /// Its parent's names of the lines it spans, in each subgridded axis
    line_names: InBothAbsAxis<Vec<Vec<S>>>,
    /// Its number of tracks in each subgridded axis
    track_count: Size<u16>,
    /// Its own items, placed in its grid
    items: Vec<GridItem>,
    /// The subgrids among its own items
    nested: Vec<Subgrid<S>>,
    /// Its own columns and rows
    columns: GridTrackVec<GridTrack>,
    rows: GridTrackVec<GridTrack>,
    /// Its `justify-content` and `align-content`
    justify_content: AlignContent,
    align_content: AlignContent,
    /// Its `justify-items` and `align-items`
    justify_items: AlignItems,
    align_items: AlignItems,
}

/// Mark a new grid item that is a subgrid: it takes no part in the track sizing of its subgridded axes,
/// and is stretched in them, whatever its self-alignment and size properties (css-grid-2 §9).
pub(super) fn mark_subgrid(item: &mut GridItem, axes: Size<bool>) {
    item.subgrid = axes;
    if axes.width {
        item.sizing.width = false;
        item.justify_self = AlignItems::STRETCH;
        item.size.width = Dimension::auto();
        item.min_size.width = LengthPercentageAuto::auto();
        item.max_size.width = LengthPercentageAuto::auto();
    }
    if axes.height {
        item.sizing.height = false;
        item.align_self = AlignItems::STRETCH;
        item.size.height = Dimension::auto();
        item.min_size.height = LengthPercentageAuto::auto();
        item.max_size.height = LengthPercentageAuto::auto();
    }
    item.aspect_ratio = None;
}

/// A subgrid has no implicit tracks in a subgridded axis: each item's area is clamped to its explicit grid
/// (css-grid-2 §9, as for an overly large grid, §7.1 of css-grid-1).
pub(super) fn clamp_to_subgrid(items: &mut [GridItem], counts: &mut TrackCounts, axis: AbsoluteAxis) {
    let n = counts.explicit.max(1) as i16;
    for item in items.iter_mut() {
        let line = match axis {
            AbsoluteAxis::Horizontal => &mut item.column,
            AbsoluteAxis::Vertical => &mut item.row,
        };
        let start = line.start.0.clamp(0, n - 1);
        let end = line.end.0.clamp(start + 1, n);
        *line = Line { start: OriginZeroLine(start), end: OriginZeroLine(end) };
    }
    *counts = TrackCounts::from_raw(0, n as u16, 0);
}

/// The subgrids among a grid's `items`, each told its tracks (sizes unknown yet) and with its own items
/// placed. An item the tree does not lay out as a subgrid becomes an ordinary item again.
pub(super) fn collect_subgrids<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    items: &mut [GridItem],
    column_names: &GridLineNames<Tree::CustomIdent>,
    row_names: &GridLineNames<Tree::CustomIdent>,
    parent_gap: Size<f32>,
    inner_width: Option<f32>,
) -> Vec<Subgrid<Tree::CustomIdent>> {
    let mut subgrids = Vec::new();
    for (index, item) in items.iter_mut().enumerate() {
        if item.subgrid.width || item.subgrid.height {
            if let Some(subgrid) = make_subgrid(tree, item, index, column_names, row_names, parent_gap, inner_width) {
                subgrids.push(subgrid);
            }
        }
    }
    subgrids
}

/// A subgrid for `item`, told its tracks (sizes unknown yet), with its own items placed; `None` (and the
/// item made an ordinary one) when the tree does not lay it out as a subgrid
fn make_subgrid<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    item: &mut GridItem,
    index: usize,
    column_names: &GridLineNames<Tree::CustomIdent>,
    row_names: &GridLineNames<Tree::CustomIdent>,
    parent_gap: Size<f32>,
    inner_width: Option<f32>,
) -> Option<Subgrid<Tree::CustomIdent>> {
    let axes = item.subgrid;
    let names = |span: Line<OriginZeroLine>, names: &GridLineNames<Tree::CustomIdent>| -> Vec<Vec<_>> {
        (span.start.0..=span.end.0)
            .map(|line| if line < 0 { Vec::new() } else { names.line(line as usize).to_vec() })
            .collect()
    };
    let (edges, gap, own_gap) = {
        let style = tree.get_grid_child_style(item.node);
        let basis = inner_width.or(Some(0.0));
        let margin = style.margin().resolve_or_zero(basis, |val, basis| tree.calc(val, basis));
        let padding = style.padding().resolve_or_zero(basis, |val, basis| tree.calc(val, basis));
        let border = style.border().resolve_or_zero(basis, |val, basis| tree.calc(val, basis));
        let specified = style.own_gap();
        let resolve = |gap: Option<crate::LengthPercentage>, parent: f32| {
            gap.map_or(parent, |gap| gap.resolve_or_zero(inner_width, |val, basis| tree.calc(val, basis)))
        };
        let gap = Size {
            width: resolve(specified.width, parent_gap.width),
            height: resolve(specified.height, parent_gap.height),
        };
        // Its own gutters, where it is not subgridded (`normal` is 0 in a grid)
        let own_gap = Size {
            width: if axes.width { gap.width } else { resolve(specified.width, 0.0) },
            height: if axes.height { gap.height } else { resolve(specified.height, 0.0) },
        };
        (margin + padding + border, gap, own_gap)
    };
    let mut subgrid = Subgrid {
        item: index,
        node: item.node,
        axes,
        edges,
        gap,
        own_gap,
        line_names: InBothAbsAxis {
            horizontal: if axes.width { names(item.column, column_names) } else { Vec::new() },
            vertical: if axes.height { names(item.row, row_names) } else { Vec::new() },
        },
        track_count: Size {
            width: (item.column.end.0 - item.column.start.0).max(1) as u16,
            height: (item.row.end.0 - item.row.start.0).max(1) as u16,
        },
        items: Vec::new(),
        nested: Vec::new(),
        columns: GridTrackVec::new(),
        rows: GridTrackVec::new(),
        justify_content: AlignContent::START,
        align_content: AlignContent::START,
        justify_items: AlignItems::STRETCH,
        align_items: AlignItems::STRETCH,
    };
    if !tree.set_subgrid_tracks(item.node, &subgrid.tracks(None, None)) {
        item.subgrid = Size { width: false, height: false };
        item.sizing = Size { width: true, height: true };
        return None;
    }
    place_subgrid_items(tree, &mut subgrid);
    Some(subgrid)
}

impl<S: crate::CheapCloneStr> Subgrid<S> {
    /// Its tracks, with these sizes in each subgridded axis (`None`: not sized yet)
    fn tracks(&self, columns: Option<Vec<Option<f32>>>, rows: Option<Vec<Option<f32>>>) -> SubgridTracks<S> {
        let axis = |subgridded: bool, sizes: Option<Vec<Option<f32>>>, count: u16, names: &Vec<Vec<S>>, gap: f32| {
            subgridded.then(|| SubgridAxisTracks {
                sizes: sizes.unwrap_or_else(|| (0..count).map(|_| None).collect()),
                line_names: names.clone(),
                gap,
            })
        };
        SubgridTracks {
            columns: axis(
                self.axes.width,
                columns,
                self.track_count.width,
                &self.line_names.horizontal,
                self.gap.width,
            ),
            rows: axis(self.axes.height, rows, self.track_count.height, &self.line_names.vertical, self.gap.height),
        }
    }
}

/// Place a subgrid's own items in its grid, as its own layout does (the grid's steps 2 to 5), and make
/// the subgrids among them
fn place_subgrid_items<Tree: LayoutGridContainer>(tree: &mut Tree, subgrid: &mut Subgrid<Tree::CustomIdent>) {
    let node = subgrid.node;
    let style = tree.get_grid_container_style(node);
    let (col_repetitions, template_cols) = compute_explicit_grid_size_in_axis(
        &style,
        None,
        AutoRepeatStrategy::MinRepetitionsThatDoOverflow,
        |val, basis| tree.calc(val, basis),
        AbsoluteAxis::Horizontal,
    );
    let (row_repetitions, template_rows) = compute_explicit_grid_size_in_axis(
        &style,
        None,
        AutoRepeatStrategy::MinRepetitionsThatDoOverflow,
        |val, basis| tree.calc(val, basis),
        AbsoluteAxis::Vertical,
    );
    let mut name_resolver = NamedLineResolver::new(&style, col_repetitions, row_repetitions);
    let explicit_cols = template_cols.max(name_resolver.area_column_count()).min(MAX_GRID_TRACKS);
    let explicit_rows = template_rows.max(name_resolver.area_row_count()).min(MAX_GRID_TRACKS);
    name_resolver.set_explicit_column_count(explicit_cols);
    name_resolver.set_explicit_row_count(explicit_rows);
    let (align_items, justify_items) = (style.align_items(), style.justify_items());
    subgrid.align_items = align_items;
    subgrid.justify_items = justify_items;
    subgrid.align_content = style.grid_align_content(AbstractAxis::Block);
    subgrid.justify_content = style.grid_align_content(AbstractAxis::Inline);

    let mut items: Vec<GridItem> = Vec::new();
    let mut placements: Vec<ItemPlacement> = Vec::new();
    for (index, child) in tree.child_ids(node).enumerate() {
        let child_style = tree.get_grid_child_style(child);
        if child_style.box_generation_mode() == BoxGenerationMode::None || child_style.position().is_out_of_flow() {
            continue;
        }
        placements.push(InBothAbsAxis {
            horizontal: name_resolver.resolve_column_names(&child_style.grid_column()).into_origin_zero(explicit_cols),
            vertical: name_resolver.resolve_row_names(&child_style.grid_row()).into_origin_zero(explicit_rows),
        });
        let axes = child_style.subgrid();
        let mut item = GridItem::new_with_style_and_order(child, child_style, align_items, justify_items, index as u16);
        if axes.width || axes.height {
            mark_subgrid(&mut item, axes);
        }
        items.push(item);
    }
    let (est_cols, est_rows) = compute_grid_size_estimate(explicit_cols, explicit_rows, &placements);
    let mut occupancy = CellOccupancyMatrix::with_track_counts(est_cols, est_rows);
    place_grid_items(&mut occupancy, &mut items, &placements, style.grid_auto_flow());
    let mut col_counts = *occupancy.track_counts(AbsoluteAxis::Horizontal);
    let mut row_counts = *occupancy.track_counts(AbsoluteAxis::Vertical);
    if subgrid.axes.width {
        clamp_to_subgrid(&mut items, &mut col_counts, AbsoluteAxis::Horizontal);
    }
    if subgrid.axes.height {
        clamp_to_subgrid(&mut items, &mut row_counts, AbsoluteAxis::Vertical);
    }
    let mut columns = GridTrackVec::new();
    let mut rows = GridTrackVec::new();
    initialize_grid_tracks(&mut columns, col_counts, &style, AbsoluteAxis::Horizontal, col_repetitions, |i| {
        occupancy.column_is_occupied(i)
    });
    initialize_grid_tracks(&mut rows, row_counts, &style, AbsoluteAxis::Vertical, row_repetitions, |i| {
        occupancy.row_is_occupied(i)
    });
    drop(style);
    resolve_item_track_indexes(&mut items, col_counts, row_counts);
    determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut items, &columns, &rows);

    if items.iter().any(|item| item.subgrid.width || item.subgrid.height) {
        let column_names = name_resolver.detailed_line_names(AbsoluteAxis::Horizontal);
        let row_names = name_resolver.detailed_line_names(AbsoluteAxis::Vertical);
        let own_gap = subgrid.own_gap;
        for (index, item) in items.iter_mut().enumerate() {
            if item.subgrid.width || item.subgrid.height {
                if let Some(nested) = make_subgrid(tree, item, index, &column_names, &row_names, own_gap, None) {
                    subgrid.nested.push(nested);
                }
            }
        }
    }
    subgrid.items = items;
    subgrid.columns = columns;
    subgrid.rows = rows;
}

/// The size of the tracks (and gutters between them) from `start` to `end` (indexes into `tracks`)
fn span_size(tracks: &[GridTrack], indexes: Line<u16>) -> f32 {
    tracks[indexes.start as usize + 1..indexes.end as usize].iter().map(|track| track.base_size).sum()
}

/// Add the items of the subgrids to the grid's items as items that take part in its track sizing in their
/// subgrid's subgridded axes. `columns_sized` says whether the grid's columns have been sized: before, the
/// items of the subgrids subgridded in columns are added; after, those of the subgrids subgridded in rows
/// only, whose areas in the subgrid's own columns are known from then on.
#[allow(clippy::too_many_arguments)]
pub(super) fn add_subgrid_items<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    subgrids: &mut [Subgrid<Tree::CustomIdent>],
    items: &mut Vec<GridItem>,
    columns: &[GridTrack],
    rows: &[GridTrack],
    col_counts: TrackCounts,
    row_counts: TrackCounts,
    parent_gap: Size<f32>,
    columns_sized: bool,
) {
    let first_new = items.len();
    for subgrid in subgrids.iter_mut() {
        let add = if columns_sized { subgrid.axes.height && !subgrid.axes.width } else { subgrid.axes.width };
        if !add {
            continue;
        }
        let parent = &items[subgrid.item];
        let lines = InBothAbsAxis { horizontal: parent.column, vertical: parent.row };
        let content_width = columns_sized
            .then(|| f32_max(span_size(columns, parent.column_indexes) - subgrid.edges.horizontal_axis_sum(), 0.0));
        let mut new_items = Vec::new();
        flatten(tree, subgrid, lines, subgrid.axes, Rect::ZERO, content_width, parent_gap, &mut new_items);
        items.append(&mut new_items);
    }
    resolve_item_track_indexes(&mut items[first_new..], col_counts, row_counts);
    for item in items[first_new..].iter_mut() {
        item.crosses_flexible_column =
            item.track_range_excluding_lines(AbstractAxis::Inline).any(|i| columns[i].is_flexible());
        item.crosses_intrinsic_column =
            item.track_range_excluding_lines(AbstractAxis::Inline).any(|i| columns[i].has_intrinsic_sizing_function());
        item.crosses_flexible_row =
            item.track_range_excluding_lines(AbstractAxis::Block).any(|i| rows[i].is_flexible());
        item.crosses_intrinsic_row =
            item.track_range_excluding_lines(AbstractAxis::Block).any(|i| rows[i].has_intrinsic_sizing_function());
    }
}

/// The left and right space outside each of a subgrid's items' areas in its own columns, laid out in a
/// content box `width` wide
fn own_column_areas<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    subgrid: &mut Subgrid<Tree::CustomIdent>,
    width: f32,
) -> Vec<(f32, f32)> {
    let inner = Size { width: Some(width), height: None };
    let available = Size { width: AvailableSpace::Definite(width), height: AvailableSpace::MaxContent };
    for item in subgrid.items.iter_mut() {
        item.grid_area_size_cache = None;
        item.min_content_contribution_cache = Size::NONE;
        item.max_content_contribution_cache = Size::NONE;
        item.minimum_contribution_cache = Size::NONE;
        item.known_dimensions_cache = None;
    }
    track_sizing_algorithm(
        tree,
        AbstractAxis::Inline,
        None,
        None,
        subgrid.justify_content,
        subgrid.align_content,
        available,
        inner,
        &mut subgrid.columns,
        &mut subgrid.rows,
        &mut subgrid.items,
        |track: &GridTrack, parent_size: Option<f32>, tree: &Tree| {
            track.max_track_sizing_function.definite_value(parent_size, |val, basis| tree.calc(val, basis))
        },
        false,
    );
    align_tracks(
        width,
        Line { start: 0.0, end: 0.0 },
        Line { start: 0.0, end: 0.0 },
        &mut subgrid.columns,
        subgrid.justify_content,
        false,
    );
    subgrid
        .items
        .iter()
        .map(|item| {
            let left = subgrid.columns[item.column_indexes.start as usize + 1].offset;
            let right = subgrid.columns[item.column_indexes.end as usize].offset;
            (left, width - right)
        })
        .collect()
}

/// Push the items of `subgrid` as items of the outermost grid: `lines` is its area there, `mapped` the
/// axes in which its tracks are that grid's, `margin` its own extra margin as an item there, and
/// `content_width` its content box's width once the grid's columns are sized. A nested subgrid in a
/// mapped axis passes its items on in turn.
#[allow(clippy::too_many_arguments)]
fn flatten<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    subgrid: &mut Subgrid<Tree::CustomIdent>,
    lines: InBothAbsAxis<Line<OriginZeroLine>>,
    mapped: Size<bool>,
    margin: Rect<f32>,
    content_width: Option<f32>,
    grid_gap: Size<f32>,
    out: &mut Vec<GridItem>,
) {
    let areas = match content_width {
        Some(width) if !mapped.width => Some(own_column_areas(tree, subgrid, width)),
        _ => None,
    };
    let edges = subgrid.edges;
    let half = |gap: f32, grid: f32| (gap - grid) / 2.0;
    let mut nested = core::mem::take(&mut subgrid.nested);
    for (index, own) in subgrid.items.iter().enumerate() {
        let mut item_lines = lines;
        let mut item_margin = Rect::ZERO;
        if mapped.width {
            let n = subgrid.track_count.width as i16;
            item_lines.horizontal = Line {
                start: OriginZeroLine(lines.horizontal.start.0 + own.column.start.0),
                end: OriginZeroLine(lines.horizontal.start.0 + own.column.end.0),
            };
            let between = half(subgrid.gap.width, grid_gap.width);
            item_margin.left = if own.column.start.0 == 0 { margin.left + edges.left } else { between };
            item_margin.right = if own.column.end.0 == n { margin.right + edges.right } else { between };
        } else {
            let (left, right) = areas.as_ref().map_or((0.0, 0.0), |areas| areas[index]);
            item_margin.left = margin.left + edges.left + left;
            item_margin.right = margin.right + edges.right + right;
        }
        if mapped.height {
            let n = subgrid.track_count.height as i16;
            item_lines.vertical = Line {
                start: OriginZeroLine(lines.vertical.start.0 + own.row.start.0),
                end: OriginZeroLine(lines.vertical.start.0 + own.row.end.0),
            };
            let between = half(subgrid.gap.height, grid_gap.height);
            item_margin.top = if own.row.start.0 == 0 { margin.top + edges.top } else { between };
            item_margin.bottom = if own.row.end.0 == n { margin.bottom + edges.bottom } else { between };
        } else {
            item_margin.top = margin.top + edges.top;
            item_margin.bottom = margin.bottom + edges.bottom;
        }

        let nested_mapped =
            Size { width: own.subgrid.width && mapped.width, height: own.subgrid.height && mapped.height };
        // The item takes part in the grid's sizing as a box in the mapped axes its items do not
        let box_sizing =
            Size { width: mapped.width && !nested_mapped.width, height: mapped.height && !nested_mapped.height };
        if nested_mapped.width || nested_mapped.height {
            if let Some(inner) = nested.iter_mut().find(|inner| inner.item == index) {
                let width = areas.as_ref().map(|areas| {
                    let (left, right) = areas[index];
                    f32_max(content_width.unwrap_or(0.0) - left - right - inner.edges.horizontal_axis_sum(), 0.0)
                });
                flatten(tree, inner, item_lines, nested_mapped, item_margin, width, grid_gap, out);
            }
        }
        if !box_sizing.width && !box_sizing.height {
            continue;
        }
        let style = tree.get_grid_child_style(own.node);
        let mut item = GridItem::new_with_style_and_order(
            own.node,
            style,
            subgrid.align_items,
            subgrid.justify_items,
            own.source_order,
        );
        item.is_subgrid_item = true;
        item.sizing = box_sizing;
        item.column = item_lines.horizontal;
        item.row = item_lines.vertical;
        item.extra_margin = item_margin;
        out.push(item);
    }
    subgrid.nested = nested;
}

/// Remove the items of the subgrids subgridded in rows only (to add them again for new column sizes)
pub(super) fn remove_row_subgrid_items(items: &mut Vec<GridItem>) {
    items.retain(|item| !(item.is_subgrid_item && !item.sizing.width));
}

/// Tell each subgrid its used tracks, from the grid's sized tracks: in columns, and in rows once `offsets`
/// says the tracks have been aligned (their offsets set; before, the gutters are taken at their base size).
pub(super) fn set_subgrid_tracks<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    subgrids: &[Subgrid<Tree::CustomIdent>],
    items: &[GridItem],
    columns: &[GridTrack],
    rows: &[GridTrack],
    offsets: bool,
    rtl: bool,
) {
    for subgrid in subgrids {
        let item = &items[subgrid.item];
        let sizes = |tracks: &[GridTrack], indexes: Line<u16>, edges: Line<f32>, gap: f32, aligned: bool| {
            let first = indexes.start as usize + 1;
            let count = ((indexes.end - indexes.start) / 2).max(1) as usize;
            // The used gutter after the subgrid's track `k`
            let gutter = |k: usize| {
                let (before, after) = (&tracks[first + 2 * k], &tracks[first + 2 * k + 2]);
                if aligned {
                    after.offset - (before.offset + before.base_size)
                } else {
                    tracks[first + 2 * k + 1].base_size
                }
            };
            let at = |gap: f32| {
                (0..count).map(move |k| {
                    let start = if k == 0 { edges.start } else { (gap - gutter(k - 1)) / 2.0 };
                    let end = if k + 1 == count { edges.end } else { (gap - gutter(k)) / 2.0 };
                    tracks[first + 2 * k].base_size - start - end
                })
            };
            // The subgrid's lines stay on its parent's: where its gutters do not fit between them, they
            // narrow to the largest size that leaves no track negative.
            let mut used_gap = gap;
            if at(gap).any(|size| size < 0.0) {
                let (mut low, mut high) = (0.0, gap);
                for _ in 0..24 {
                    let mid = (low + high) / 2.0;
                    if at(mid).any(|size| size < 0.0) {
                        high = mid;
                    } else {
                        low = mid;
                    }
                }
                used_gap = low;
            }
            (at(used_gap).map(|size| Some(f32_max(size, 0.0))).collect::<Vec<_>>(), used_gap)
        };
        let column_sizes = subgrid.axes.width.then(|| {
            sizes(
                columns,
                item.column_indexes,
                Line { start: subgrid.edges.left, end: subgrid.edges.right },
                subgrid.gap.width,
                offsets && !rtl,
            )
        });
        let row_sizes = (subgrid.axes.height && offsets).then(|| {
            sizes(
                rows,
                item.row_indexes,
                Line { start: subgrid.edges.top, end: subgrid.edges.bottom },
                subgrid.gap.height,
                true,
            )
        });
        let mut tracks = subgrid.tracks(
            column_sizes.as_ref().map(|(sizes, _)| sizes.clone()),
            row_sizes.as_ref().map(|(sizes, _)| sizes.clone()),
        );
        if let (Some(axis), Some((_, gap))) = (tracks.columns.as_mut(), &column_sizes) {
            axis.gap = *gap;
        }
        if let (Some(axis), Some((_, gap))) = (tracks.rows.as_mut(), &row_sizes) {
            axis.gap = *gap;
        }
        tree.set_subgrid_tracks(subgrid.node, &tracks);
    }
}
