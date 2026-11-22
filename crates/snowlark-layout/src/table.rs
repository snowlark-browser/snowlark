// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tables (CSS 2 §17, css-tables-3): the table grid (HTML's table forming algorithm for `colspan` and
//! `rowspan`, header and footer groups moved to the ends), the automatic (§17.5.2.2) and fixed
//! (§17.5.2.1) column width algorithms, row heights with baseline alignment and row-spanning cells,
//! `vertical-align` in cells, the separated (§17.6.1) and collapsing (§17.6.2) border models, and
//! captions (§17.4.1) in the table wrapper box.

use std::sync::Arc;

use snowlark_paint::{BorderStyle, Color, Point, Rect, Sides, Size};

use crate::boxes::*;
use crate::fragment::*;
use crate::layout::*;
use crate::sizing::*;
use crate::style::*;
use crate::table_heights::{distribute_extra, grow_rows, section_height, Section};
use crate::table_widths::{distribute, encompass_span, grid_max, Column as ColumnWidth};
use crate::table_widths::{fixed_widths, FixedSpec};

struct Cell {
    id: BoxId,
    row: usize,
    col: usize,
    rowspan: usize,
    colspan: usize,
}

struct Row {
    id: BoxId,
    group: Option<usize>,
}

struct Group {
    id: BoxId,
    first_row: usize,
    rows: usize,
}

#[derive(Clone, Copy, Default)]
struct Column {
    /// The column box and its group, if the column came from one.
    id: Option<BoxId>,
    group: Option<BoxId>,
}

/// The table grid.
struct Grid {
    rows: Vec<Row>,
    groups: Vec<Group>,
    cells: Vec<Cell>,
    columns: Vec<Column>,
    /// Column groups and columns in order, for painting.
    column_boxes: Vec<(BoxId, usize, usize)>,
    /// Columns with `visibility: collapse` (their own or their group's): zero wide, without
    /// spacing (css-tables-3 §visibility-collapse-cell-rendering).
    col_collapsed: Vec<bool>,
    /// Rows with `visibility: collapse` (their own or their row group's).
    row_collapsed: Vec<bool>,
}

fn build_grid<S: Style>(cx: &Cx<'_, S>, table: BoxId) -> Grid {
    let kids = &cx.tree.b(table).children;
    // Display order: the first header group first, the first footer group last (CSS 2 §17.2).
    let mut header = None;
    let mut footer = None;
    for &k in kids {
        match cx.tree.b(k).kind {
            BoxKind::TableRowGroup(RowGroup::Header) if header.is_none() => header = Some(k),
            BoxKind::TableRowGroup(RowGroup::Footer) if footer.is_none() => footer = Some(k),
            _ => {}
        }
    }
    let mut ordered: Vec<BoxId> = Vec::with_capacity(kids.len());
    ordered.extend(header);
    for &k in kids {
        if Some(k) != header && Some(k) != footer {
            ordered.push(k);
        }
    }
    ordered.extend(footer);

    let mut grid = Grid {
        rows: Vec::new(),
        groups: Vec::new(),
        cells: Vec::new(),
        columns: Vec::new(),
        column_boxes: Vec::new(),
        col_collapsed: Vec::new(),
        row_collapsed: Vec::new(),
    };
    // Columns.
    let mut ncol_elements = 0usize;
    for &k in kids {
        let b = cx.tree.b(k);
        let span = |id: BoxId| match cx.tree.b(id).data {
            BoxData::Column { span } => span.max(1) as usize,
            _ => 1,
        };
        match b.kind {
            BoxKind::TableColumnGroup => {
                let start = ncol_elements;
                if b.children.is_empty() {
                    for _ in 0..span(k) {
                        grid.columns.push(Column {
                            id: None,
                            group: Some(k),
                        });
                    }
                    ncol_elements += span(k);
                } else {
                    for &c in &b.children {
                        let s = span(c);
                        let cs = ncol_elements;
                        for _ in 0..s {
                            grid.columns.push(Column {
                                id: Some(c),
                                group: Some(k),
                            });
                        }
                        ncol_elements += s;
                        grid.column_boxes.push((c, cs, ncol_elements));
                    }
                }
                grid.column_boxes.push((k, start, ncol_elements));
            }
            BoxKind::TableColumn => {
                let s = span(k);
                let cs = ncol_elements;
                for _ in 0..s {
                    grid.columns.push(Column {
                        id: Some(k),
                        group: None,
                    });
                }
                ncol_elements += s;
                grid.column_boxes.push((k, cs, ncol_elements));
            }
            _ => {}
        }
    }
    // Rows and cells (HTML §4.9.12.1, simplified).
    let mut occupied: Vec<Vec<bool>> = Vec::new();
    let mut pending_zero_spans: Vec<usize> = Vec::new();
    for g in ordered {
        let gb = cx.tree.b(g);
        let (rows, group_index): (Vec<BoxId>, Option<usize>) = match gb.kind {
            BoxKind::TableRowGroup(_) => {
                grid.groups.push(Group {
                    id: g,
                    first_row: grid.rows.len(),
                    rows: gb.children.len(),
                });
                (gb.children.clone(), Some(grid.groups.len() - 1))
            }
            BoxKind::TableRow => (vec![g], None),
            _ => continue,
        };
        let group_first = grid.rows.len();
        pending_zero_spans.clear();
        for r in rows {
            let ri = grid.rows.len();
            grid.rows.push(Row {
                id: r,
                group: group_index,
            });
            if occupied.len() <= ri {
                occupied.resize(ri + 1, Vec::new());
            }
            let mut c = 0usize;
            for &cell in &cx.tree.b(r).children {
                let cb = cx.tree.b(cell);
                if cb.kind != BoxKind::TableCell {
                    continue;
                }
                let (colspan, rowspan) = match cb.data {
                    BoxData::Cell { colspan, rowspan } => {
                        (colspan.max(1) as usize, rowspan as usize)
                    }
                    _ => (1, 1),
                };
                while occupied[ri].get(c).copied().unwrap_or(false) {
                    c += 1;
                }
                let rs = if rowspan == 0 { 1 } else { rowspan };
                if rowspan == 0 {
                    pending_zero_spans.push(grid.cells.len());
                }
                for rr in ri..ri + rs {
                    if occupied.len() <= rr {
                        occupied.resize(rr + 1, Vec::new());
                    }
                    let row = &mut occupied[rr];
                    if row.len() < c + colspan {
                        row.resize(c + colspan, false);
                    }
                    for slot in row.iter_mut().skip(c).take(colspan) {
                        *slot = true;
                    }
                }
                grid.cells.push(Cell {
                    id: cell,
                    row: ri,
                    col: c,
                    rowspan: rs,
                    colspan,
                });
                c += colspan;
            }
        }
        // rowspan=0 extends to the end of the group; spans never cross groups.
        let group_end = grid.rows.len();
        for &ci in &pending_zero_spans {
            let cell = &mut grid.cells[ci];
            cell.rowspan = group_end - cell.row;
        }
        for cell in grid.cells.iter_mut() {
            if cell.row >= group_first && cell.row < group_end {
                cell.rowspan = cell.rowspan.min(group_end - cell.row).max(1);
            }
        }
    }
    let ncols = grid
        .cells
        .iter()
        .map(|c| c.col + c.colspan)
        .max()
        .unwrap_or(0)
        .max(grid.columns.len());
    grid.columns.resize(ncols, Column::default());
    // (the fixed layout algorithm keeps a cell's every column: css-tables-3 §3.1)
    let tstyle = &cx.tree.b(table).style;
    let fixed = tstyle.table_layout() == TableLayoutMode::Fixed
        && !matches!(tstyle.width(), SizeValue::Auto | SizeValue::MaxContent);
    if !fixed {
        merge_implicit_columns(&mut grid);
    }
    let collapsed = |id: Option<BoxId>| {
        id.is_some_and(|b| cx.tree.b(b).style.visibility() == Visibility::Collapse)
    };
    grid.col_collapsed = grid
        .columns
        .iter()
        .map(|c| collapsed(c.id) || collapsed(c.group))
        .collect();
    grid.row_collapsed = grid
        .rows
        .iter()
        .map(|r| {
            collapsed(Some(r.id)) || r.group.is_some_and(|g| collapsed(Some(grid.groups[g].id)))
        })
        .collect();
    grid
}

/// HTML's table model merges consecutive columns that no `col` defines and that every cell either
/// covers both of or neither of (css-tables-3 §3.1 "track merging"): a `colspan` of 2 over nothing
/// else is one column.
fn merge_implicit_columns(grid: &mut Grid) {
    let n = grid.columns.len();
    if n < 2 {
        return;
    }
    // A cell starts or ends at boundary b: the line between columns b - 1 and b.
    let mut boundary = vec![false; n + 1];
    for c in &grid.cells {
        boundary[c.col] = true;
        boundary[(c.col + c.colspan).min(n)] = true;
    }
    let implicit = |c: &Column| c.id.is_none() && c.group.is_none();
    let mut map = vec![0usize; n];
    let mut merged: Vec<Column> = Vec::new();
    for i in 0..n {
        if i > 0 && implicit(&grid.columns[i]) && implicit(&grid.columns[i - 1]) && !boundary[i] {
            map[i] = merged.len() - 1;
        } else {
            map[i] = merged.len();
            merged.push(grid.columns[i]);
        }
    }
    if merged.len() == n {
        return;
    }
    for c in grid.cells.iter_mut() {
        let end = (c.col + c.colspan).min(n);
        let (first, last) = (map[c.col], map[end - 1]);
        c.col = first;
        c.colspan = last - first + 1;
    }
    for (_, c0, c1) in grid.column_boxes.iter_mut() {
        if *c1 > *c0 && *c1 <= n {
            let (a, b) = (map[*c0], map[*c1 - 1] + 1);
            *c0 = a;
            *c1 = b;
        }
    }
    grid.columns = merged;
}

struct TableGeom {
    border: Sides,
    padding: Sides,
    spacing: (f32, f32),
    collapse: bool,
}

fn table_geom<S: Style>(style: &S, cb_width: f32) -> TableGeom {
    let collapse = style.border_collapse() == BorderCollapse::Collapse;
    let e = Edges::of(style, cb_width);
    TableGeom {
        border: e.border,
        padding: if collapse {
            Sides::default()
        } else {
            e.padding
        },
        spacing: if collapse {
            (0.0, 0.0)
        } else {
            style.border_spacing()
        },
        collapse,
    }
}

/// A cell's contribution to its column (or a column box's): border-box min/max-content widths,
/// whether it has a specified non-percentage width, and its percentage.
struct CellWidth {
    min: f32,
    max: f32,
    constrained: bool,
    percent: Option<f32>,
}

/// A cell's border-box min/max-content widths, with `width`, `min-width` and `max-width` applied
/// (css-tables-3 §6.2 "computing cell measures").
fn cell_widths<S: Style>(cx: &mut Cx<'_, S>, cell: BoxId, collapsed: Option<Sides>) -> CellWidth {
    let style = cx.style_here(cell);
    let e = Edges::of(&style, 0.0);
    let border_h = match collapsed {
        Some(b) => b.left + b.right,
        None => e.border.left + e.border.right,
    };
    let pb = border_h + e.padding.left + e.padding.right;
    let c = cx.content_sizes(cell);
    let mut w = CellWidth {
        min: c.min + pb,
        max: c.max + pb,
        constrained: false,
        percent: None,
    };
    match style.width() {
        SizeValue::LengthPercentage(lp) if !lp.has_percent() => {
            let f = to_border_box(lp.length, pb, style.box_sizing());
            w.constrained = true;
            w.max = w.min.max(f);
        }
        SizeValue::LengthPercentage(lp) if lp.length == 0.0 && lp.is_linear() => {
            w.percent = lp.percent
        }
        _ => {}
    }
    // `max-width` bounds both contributions, `min-width` wins over it.
    if let MaxSizeValue::LengthPercentage(lp) = style.max_width() {
        if !lp.has_percent() {
            let hi = to_border_box(lp.length, pb, style.box_sizing());
            w.min = w.min.min(hi);
            w.max = w.max.min(hi);
        }
    }
    if let SizeValue::LengthPercentage(lp) = style.min_width() {
        if !lp.has_percent() {
            let lo = to_border_box(lp.length, pb, style.box_sizing());
            w.min = w.min.max(lo);
            w.max = w.max.max(lo);
        }
    }
    w
}

/// Column constraints from the column boxes and the cells: single-column cells first, then spanning
/// ones (narrowest first) distributed over the columns they cover. Collapsed columns have none.
fn column_widths<S: Style>(
    cx: &mut Cx<'_, S>,
    grid: &Grid,
    spacing: f32,
    cell_borders: &[Option<Sides>],
) -> Vec<ColumnWidth> {
    let n = grid.columns.len();
    let mut cols = vec![ColumnWidth::default(); n];
    for (i, col) in grid.columns.iter().enumerate() {
        if grid.col_collapsed[i] {
            continue;
        }
        for b in [col.id, col.group].into_iter().flatten() {
            match cx.tree.b(b).style.width() {
                SizeValue::LengthPercentage(lp) if !lp.has_percent() => {
                    cols[i].encompass(0.0, lp.length.max(0.0), true, None);
                    break;
                }
                SizeValue::LengthPercentage(lp) if lp.length == 0.0 && lp.is_linear() => {
                    cols[i].encompass(0.0, 0.0, false, lp.percent);
                    break;
                }
                _ => {}
            }
        }
    }
    let mut spanning = Vec::new();
    for (ci, cell) in grid.cells.iter().enumerate() {
        let covered: Vec<usize> = (cell.col..(cell.col + cell.colspan).min(n))
            .filter(|&c| !grid.col_collapsed[c])
            .collect();
        if covered.is_empty() {
            continue;
        }
        let w = cell_widths(cx, cell.id, cell_borders[ci]);
        if covered.len() == 1 {
            cols[covered[0]].encompass(w.min, w.max, w.constrained, w.percent);
        } else {
            spanning.push((covered, w));
        }
    }
    spanning.sort_by_key(|s| s.0.len());
    for (covered, w) in spanning {
        let gaps = spacing * (covered.len() as f32 - 1.0);
        let mut sub: Vec<ColumnWidth> = covered.iter().map(|&c| cols[c]).collect();
        let len = sub.len();
        encompass_span(
            &mut sub,
            0..len,
            (w.min - gaps).max(0.0),
            (w.max - gaps).max(0.0),
            w.percent,
        );
        for (k, &c) in covered.iter().enumerate() {
            cols[c] = sub[k];
        }
    }
    for c in cols.iter_mut() {
        c.max = c.max.max(c.min);
    }
    cols
}

/// The distribution of `target` over the columns that are not collapsed; collapsed ones get 0.
fn distribute_visible(grid: &Grid, cols: &[ColumnWidth], target: f32) -> Vec<f32> {
    let visible: Vec<usize> = (0..cols.len())
        .filter(|&i| !grid.col_collapsed[i])
        .collect();
    let sub: Vec<ColumnWidth> = visible.iter().map(|&i| cols[i]).collect();
    let w = distribute(&sub, target);
    let mut out = vec![0.0; cols.len()];
    for (k, &i) in visible.iter().enumerate() {
        out[i] = w[k];
    }
    out
}

/// Column widths for the fixed table layout algorithm: from the column boxes, then from the cells of
/// the first row (a spanning cell's width divided evenly over the columns it covers).
fn fixed_specs<S: Style>(cx: &mut Cx<'_, S>, grid: &Grid, spacing: f32) -> Vec<FixedSpec> {
    let n = grid.columns.len();
    let mut out = vec![FixedSpec::Auto; n];
    let of = |lp: LengthPercentage, pb: f32, sizing: BoxSizing| {
        if lp.has_percent() && lp.length == 0.0 && lp.is_linear() {
            Some(FixedSpec::Percent(lp.percent.unwrap_or(0.0)))
        } else if lp.has_percent() {
            None
        } else {
            Some(FixedSpec::Length(to_border_box(
                lp.length.max(0.0),
                pb,
                sizing,
            )))
        }
    };
    for (i, col) in grid.columns.iter().enumerate() {
        for b in [col.id, col.group].into_iter().flatten() {
            if let SizeValue::LengthPercentage(lp) = cx.tree.b(b).style.width() {
                if let Some(spec) = of(lp, 0.0, BoxSizing::ContentBox) {
                    out[i] = spec;
                }
                break;
            }
        }
    }
    for cell in grid.cells.iter().filter(|c| c.row == 0) {
        let style = cx.style_here(cell.id);
        if let SizeValue::LengthPercentage(lp) = style.width() {
            let e = Edges::of(&style, 0.0);
            let Some(spec) = of(lp, e.pb_h(), style.box_sizing()) else {
                continue;
            };
            let covered: Vec<usize> = (cell.col..(cell.col + cell.colspan).min(n))
                .filter(|&c| !grid.col_collapsed[c])
                .collect();
            if covered.is_empty() {
                continue;
            }
            let k = covered.len() as f32;
            let per = match spec {
                FixedSpec::Length(w) => FixedSpec::Length((w - spacing * (k - 1.0)).max(0.0) / k),
                FixedSpec::Percent(p) => FixedSpec::Percent(p / k),
                FixedSpec::Auto => FixedSpec::Auto,
            };
            for c in covered {
                if out[c] == FixedSpec::Auto {
                    out[c] = per;
                }
            }
        }
    }
    for (i, slot) in out.iter_mut().enumerate() {
        if grid.col_collapsed[i] {
            *slot = FixedSpec::Length(0.0);
        }
    }
    out
}

/// Min/max-content widths of a table wrapper (the table's, with captions' minimums).
pub(crate) fn content_sizes<S: Style>(cx: &mut Cx<'_, S>, wrapper: BoxId) -> ContentSizes {
    table_content_sizes(cx, wrapper, true)
}

/// Min/max-content widths of a table wrapper as a flex item's content: the table box is the flex
/// item, so its `width` is the item's preferred size, applied by the flex algorithm, and does not
/// count as a minimum (css-flexbox-1 §4: "like width and height, the flex longhands apply to the table
/// box").
pub(crate) fn flex_item_content_sizes<S: Style>(
    cx: &mut Cx<'_, S>,
    wrapper: BoxId,
) -> ContentSizes {
    table_content_sizes(cx, wrapper, false)
}

fn table_content_sizes<S: Style>(
    cx: &mut Cx<'_, S>,
    wrapper: BoxId,
    with_width: bool,
) -> ContentSizes {
    let Some(table) = table_box(cx, wrapper) else {
        return ContentSizes::ZERO;
    };
    let style = cx.tree.b(table).style.clone();
    let geom = table_geom(&style, 0.0);
    let grid = build_grid(cx, table);
    let borders = collapsed_cell_borders(cx, &grid, &geom, table);
    let empty = grid.rows.is_empty();
    let cols = if empty {
        vec![ColumnWidth::default(); grid.columns.len()]
    } else {
        column_widths(cx, &grid, geom.spacing.0, &borders)
    };
    let visible = grid.col_collapsed.iter().filter(|c| !**c).count() as f32;
    let spacing = if visible == 0.0 || empty {
        0.0
    } else {
        geom.spacing.0 * (visible + 1.0)
    };
    let pb = geom.border.left + geom.border.right + geom.padding.left + geom.padding.right;
    let mut min: f32 = cols.iter().map(|c| c.min).sum::<f32>() + spacing + pb;
    let max: f32 = grid_max(&cols) + spacing + pb;
    let kids = cx.tree.b(wrapper).children.clone();
    for k in kids {
        if cx.tree.b(k).kind == BoxKind::TableCaption {
            min = min.max(crate::intrinsic::contribution(cx, k).min);
        }
    }
    // The table box's own width/min-width count (as specified lengths).
    let e = Edges::of(&style, 0.0);
    let spec = match style.width() {
        SizeValue::LengthPercentage(lp) if with_width && !lp.has_percent() => {
            Some(to_border_box(lp.length, e.pb_h(), style.box_sizing()))
        }
        _ => None,
    };
    let (min, max) = match spec {
        Some(w) => (min.max(w), min.max(w)),
        None => (min, max.max(min)),
    };
    // Content sizes are of the wrapper's content box, which is the table's border box.
    ContentSizes { min, max }
}

fn table_box<S: Style>(cx: &Cx<'_, S>, wrapper: BoxId) -> Option<BoxId> {
    cx.tree
        .b(wrapper)
        .children
        .iter()
        .copied()
        .find(|&c| cx.tree.b(c).kind == BoxKind::Table)
}

/// Lays out a table wrapper box: the table grid and its captions.
#[allow(clippy::needless_range_loop)]
pub(crate) fn layout_table_wrapper<S: Style>(
    cx: &mut Cx<'_, S>,
    wrapper: BoxId,
    space: &Space,
) -> LayoutResult<S> {
    let wstyle = cx.tree.b(wrapper).style.clone();
    let rtl = wstyle.direction() == Direction::Rtl;
    let cb_rtl = cx.cb_rtl(wrapper);
    let Some(table) = table_box(cx, wrapper) else {
        // An empty wrapper.
        let f = box_fragment(
            cx,
            wrapper,
            Size::ZERO,
            Sides::default(),
            Sides::default(),
            Vec::new(),
            None,
            None,
            BoxContent::None,
            fragment_flags::TABLE_WRAPPER,
        );
        return LayoutResult {
            fragment: f,
            margin: Sides::default(),
            start: Strut::default(),
            end: Strut::default(),
            through: false,
            floats: Vec::new(),
            oof: Vec::new(),
        };
    };
    let style = cx.tree.b(table).style.clone();
    // The wrapper takes the table's margins (css-tables-3 §3.1).
    let wedges = Edges::of(&wstyle, space.cb_width);
    let geom = table_geom(&style, space.cb_width);
    let grid = build_grid(cx, table);
    let borders = collapsed_cell_borders(cx, &grid, &geom, table);
    let mut geom = geom;
    if geom.collapse {
        // The table's border is half of its outer collapsed borders (CSS 2 §17.6.2).
        geom.border = outer_collapsed_border(cx, &grid, &geom, table);
    }
    let ncols = grid.columns.len();
    // A table with no rows is empty: no tracks, no spacing (css-tables-3 §3.1 step 3B). One with
    // rows but no columns has rows as wide as the table's inner width.
    let empty = grid.rows.is_empty();
    let cols = if empty {
        vec![ColumnWidth::default(); ncols]
    } else {
        column_widths(cx, &grid, geom.spacing.0, &borders)
    };
    let pb_h = geom.border.left + geom.border.right + geom.padding.left + geom.padding.right;
    let pb_v = geom.border.top + geom.border.bottom + geom.padding.top + geom.padding.bottom;
    let visible_cols = grid.col_collapsed.iter().filter(|c| !**c).count();
    let spacing_h = if visible_cols > 0 && !empty {
        geom.spacing.0 * (visible_cols as f32 + 1.0)
    } else {
        0.0
    };
    // Fixed layout (css-tables-3 §in-fixed-mode) applies unless `width` is `auto` or `max-content`.
    let fixed_layout = style.table_layout() == TableLayoutMode::Fixed
        && !matches!(style.width(), SizeValue::Auto | SizeValue::MaxContent);
    let mut min_w: f32 = if empty {
        pb_h
    } else if fixed_layout {
        // Only the specified lengths of the columns count: the cells' content does not.
        let specs = fixed_specs(cx, &grid, geom.spacing.0);
        specs
            .iter()
            .map(|s| match s {
                FixedSpec::Length(w) => *w,
                _ => 0.0,
            })
            .sum::<f32>()
            + spacing_h
            + pb_h
    } else {
        cols.iter().map(|c| c.min).sum::<f32>() + spacing_h + pb_h
    };
    // The table is at least as wide as its captions' min-content (css-tables-3 §3.9.4).
    let caption_kids = cx.tree.b(wrapper).children.clone();
    for k in caption_kids {
        if cx.tree.b(k).kind == BoxKind::TableCaption {
            min_w = min_w.max(crate::intrinsic::contribution(cx, k).min);
        }
    }
    let max_w: f32 = (grid_max(&cols) + spacing_h + pb_h).max(min_w);
    let available = (space.available - wedges.margin_h()).max(0.0);
    let style_pb_h = Edges::of(&style, space.cb_width).pb_h();
    // A `width`, `min-width` or `max-width` value as a border-box size of the table.
    let resolve_size = |lp: Option<LengthPercentage>, keyword: Option<SizeValue>| -> Option<f32> {
        if let Some(lp) = lp {
            return lp
                .resolve_opt(Some(space.cb_width))
                .map(|w| to_border_box(w, style_pb_h, style.box_sizing()));
        }
        match keyword? {
            SizeValue::MinContent => Some(min_w),
            SizeValue::MaxContent => Some(max_w),
            SizeValue::FitContent(None) => Some(max_w.min(available.max(min_w))),
            SizeValue::FitContent(Some(arg)) => {
                Some(max_w.min(arg.resolve(space.cb_width).max(min_w)))
            }
            SizeValue::Stretch => Some(available),
            _ => None,
        }
    };
    let size_of = |v: SizeValue| match v {
        SizeValue::LengthPercentage(lp) => resolve_size(Some(lp), None),
        other => resolve_size(None, Some(other)),
    };
    let specified = match space.fixed_width {
        Some(w) => Some(w),
        None => size_of(style.width()),
    };
    // (`min-width` and `max-width` of the table box are read in the horizontal writing mode only:
    // in another one they are not its inline sizes)
    let swapped = cx.frame.swaps();
    let max_size = match style.max_width() {
        _ if swapped => None,
        MaxSizeValue::None => None,
        MaxSizeValue::LengthPercentage(lp) => resolve_size(Some(lp), None),
        MaxSizeValue::MinContent => Some(min_w),
        MaxSizeValue::MaxContent => Some(max_w),
        MaxSizeValue::FitContent(a) => size_of(SizeValue::FitContent(a)),
        MaxSizeValue::Stretch => Some(available),
    };
    let min_size = if space.fixed_width.is_some() || swapped {
        None
    } else {
        size_of(style.min_width())
    };
    let mut table_width = match specified {
        Some(w) => w.max(min_w),
        None => max_w.min(available).max(min_w),
    };
    if space.fixed_width.is_none() {
        if let Some(hi) = max_size {
            table_width = table_width.min(hi).max(min_w);
        }
        if let Some(lo) = min_size {
            table_width = table_width.max(lo);
        }
    }
    let content_w = (table_width - pb_h - spacing_h).max(0.0);
    let widths = if fixed_layout {
        let specs = fixed_specs(cx, &grid, geom.spacing.0);
        fixed_widths(&specs, content_w)
    } else {
        distribute_visible(&grid, &cols, content_w)
    };
    // Column positions (physical; columns run right to left in rtl tables). A collapsed column
    // takes no spacing.
    let mut col_x = Vec::with_capacity(ncols + 1);
    let mut x = geom.border.left + geom.padding.left + geom.spacing.0;
    for (i, &w) in widths.iter().enumerate() {
        col_x.push(x);
        if !grid.col_collapsed[i] {
            x += w + geom.spacing.0;
        }
    }
    col_x.push(x);
    let col_left = |c: usize, span: usize| -> (f32, f32) {
        let end = (c + span).min(ncols);
        let shown = (c..end).filter(|&i| !grid.col_collapsed[i]).count();
        let w: f32 =
            widths[c..end].iter().sum::<f32>() + geom.spacing.0 * (shown.max(1) as f32 - 1.0);
        let left = if rtl {
            table_width - col_x[c] - w
        } else {
            col_x[c]
        };
        (left, w)
    };

    // Row heights: lay out cells at their widths.
    let nrows = grid.rows.len();
    let mut row_h = vec![0.0f32; nrows];
    let mut row_above = vec![0.0f32; nrows];
    // Whether a cell of the row is baseline-aligned (and so gives the row its baseline), and
    // whether the row has cells at all.
    let mut row_aligned = vec![false; nrows];
    let mut row_has_cells = vec![false; nrows];
    let mut row_below = vec![0.0f32; nrows];
    // A flex item's table wrapper gets its border-box height from the flex algorithm; the captions
    // count as the table box's border and padding there (css-flexbox-1 §4), so the table box is
    // what remains of it. Measured for its content height, the table's own height does not count.
    let table_height_spec = match space.fixed_height {
        Some(h) => Some((h - captions_height(cx, wrapper, table_width)).max(0.0)),
        None if space.content_height => None,
        None => resolve_height(style.height(), space.cb_height, pb_v, style.box_sizing()),
    };
    // Cells all of whose rows or columns are collapsed are neither laid out nor painted; one that
    // spans collapsed tracks and others is as big as the rest of them (css-tables-3
    // §visibility-collapse-cell-rendering) and is clipped to that.
    let rows_shown = |cell: &Cell| -> usize {
        (cell.row..(cell.row + cell.rowspan).min(grid.rows.len()))
            .filter(|&r| !grid.row_collapsed[r])
            .count()
    };
    let cols_shown = |cell: &Cell| -> usize {
        (cell.col..(cell.col + cell.colspan).min(ncols))
            .filter(|&c| !grid.col_collapsed[c])
            .count()
    };
    let cell_hidden: Vec<bool> = grid
        .cells
        .iter()
        .map(|c| rows_shown(c) == 0 || cols_shown(c) == 0)
        .collect();
    let cell_clipped: Vec<bool> = grid
        .cells
        .iter()
        .map(|c| {
            rows_shown(c) < c.rowspan.min(grid.rows.len() - c.row) || cols_shown(c) < c.colspan
        })
        .collect();
    for (ri, row) in grid.rows.iter().enumerate() {
        if grid.row_collapsed[ri] || empty {
            continue;
        }
        if let Some(h) = resolve_height(
            cx.tree.b(row.id).style.height(),
            None,
            0.0,
            BoxSizing::ContentBox,
        ) {
            row_h[ri] = h;
        }
    }
    let mut cell_results: Vec<(f32, Option<f32>)> = Vec::with_capacity(grid.cells.len());
    for (ci, cell) in grid.cells.iter().enumerate() {
        if rows_shown(cell) == 0 {
            cell_results.push((0.0, None));
            continue;
        }
        let (_, w) = col_left(cell.col, cell.colspan);
        // A cell in collapsed columns is not shown but still gives its rows their height, as
        // tall as it is at its own width.
        let w = if cols_shown(cell) == 0 {
            cell_widths(cx, cell.id, borders[ci]).max
        } else {
            w
        };
        let mut space = cell_space(w, None, content_w);
        space.collapsed_border = borders[ci];
        // (a cell's block size is a minimum: what the row needs is its content; in the horizontal
        // writing mode the row height below takes the larger of the two)
        space.content_height = cx.frame.swaps();
        let r = cx.layout(cell.id, &space, None, Point::ZERO);
        let h = r.fragment.size.height;
        let b = r.fragment.baseline;
        cell_results.push((h, b));
        let style = cx.style_here(cell.id);
        let spec_h = if cx.frame.swaps() {
            // (measured for its content above, so its own padding and border count here)
            let e = Edges::of(&style, 0.0);
            let border = borders[ci].unwrap_or(e.border);
            let pb = border.top + border.bottom + e.padding.top + e.padding.bottom;
            resolve_height(style.height(), None, pb, style.box_sizing())
        } else {
            resolve_height(style.height(), None, 0.0, BoxSizing::BorderBox)
        }
        .unwrap_or(0.0);
        let h = h.max(spec_h);
        row_has_cells[cell.row] = true;
        if cell.rowspan == 1 {
            if style.vertical_align() == VerticalAlign::Baseline {
                let bl = b.unwrap_or_else(|| content_bottom(&r.fragment));
                row_above[cell.row] = row_above[cell.row].max(bl);
                row_aligned[cell.row] = true;
                row_below[cell.row] = row_below[cell.row].max(h - bl);
            }
            row_h[cell.row] = row_h[cell.row].max(h);
        }
    }
    for ri in 0..nrows {
        row_h[ri] = row_h[ri].max(row_above[ri] + row_below[ri]);
    }
    // Row-spanning cells distribute the height they need beyond their rows' (inner cells before
    // the cells enclosing them, then upper ones first): to the rows whose height is not specified
    // and is not zero in proportion to it; with none, to the rows with a specified height in
    // proportion to it; else to the last row.
    // A row's height is specified when the row's or one of its cells' is a length.
    let is_length = |cx: &Cx<'_, S>, id: BoxId| {
        matches!(
            cx.tree.b(id).style.height(),
            SizeValue::LengthPercentage(lp) if !lp.has_percent()
        )
    };
    let mut row_fixed: Vec<bool> = grid.rows.iter().map(|r| is_length(cx, r.id)).collect();
    for cell in &grid.cells {
        if cell.rowspan == 1 && is_length(cx, cell.id) {
            row_fixed[cell.row] = true;
        }
    }
    let mut spanners: Vec<usize> = (0..grid.cells.len())
        .filter(|&ci| grid.cells[ci].rowspan > 1 && !cell_hidden[ci] && !cell_clipped[ci])
        .collect();
    spanners.sort_by_key(|&ci| (grid.cells[ci].rowspan, grid.cells[ci].row));
    for ci in spanners {
        // (a cell that spans collapsed rows is shrunk to the others and clipped: it asks nothing)
        let cell = &grid.cells[ci];
        let end = (cell.row + cell.rowspan).min(nrows);
        let shown = rows_shown(cell);
        let have: f32 = (cell.row..end)
            .filter(|&r| !grid.row_collapsed[r])
            .map(|r| row_h[r])
            .sum::<f32>()
            + geom.spacing.1 * (shown.max(1) - 1) as f32;
        let need = cell_results[ci].0;
        if need <= have {
            continue;
        }
        let extra = need - have;
        let rows: Vec<usize> = (cell.row..end)
            .filter(|&r| !grid.row_collapsed[r])
            .collect();
        let unconstrained: Vec<usize> = rows.iter().copied().filter(|&r| !row_fixed[r]).collect();
        let nonempty: Vec<usize> = unconstrained
            .iter()
            .copied()
            .filter(|&r| row_h[r] > 0.0)
            .collect();
        let constrained: Vec<usize> = rows.iter().copied().filter(|&r| row_fixed[r]).collect();
        let (pool, last_only) = if !nonempty.is_empty() {
            (nonempty, false)
        } else if !constrained.is_empty() {
            (constrained, false)
        } else {
            (unconstrained, true)
        };
        if pool.is_empty() {
            continue;
        }
        if last_only {
            row_h[*pool.last().unwrap()] += extra;
            continue;
        }
        let total: f32 = pool.iter().map(|&r| row_h[r]).sum();
        for &r in &pool {
            let share = if total > 0.0 {
                row_h[r] / total
            } else {
                1.0 / pool.len() as f32
            };
            row_h[r] += extra * share;
        }
    }
    let visible_rows = grid.row_collapsed.iter().filter(|c| !**c).count();
    let spacing_v = if visible_rows > 0 && !empty {
        geom.spacing.1 * (visible_rows as f32 + 1.0)
    } else {
        0.0
    };
    // Row groups (and rows outside any) as sections that take extra height (css-tables-3 §row-layout).
    let length_of = |cx: &Cx<'_, S>, id: BoxId| match cx.tree.b(id).style.height() {
        SizeValue::LengthPercentage(lp) if !lp.has_percent() => Some(lp.length.max(0.0)),
        _ => None,
    };
    let percent_of = |cx: &Cx<'_, S>, id: BoxId| match cx.tree.b(id).style.height() {
        SizeValue::LengthPercentage(lp) if lp.length == 0.0 && lp.is_linear() => lp.percent,
        _ => None,
    };
    let mut sections: Vec<Section> = Vec::new();
    let mut ri = 0;
    while ri < nrows {
        match grid.rows[ri].group {
            Some(g) => {
                let group = &grid.groups[g];
                let end = (group.first_row + group.rows).min(nrows).max(ri + 1);
                sections.push(Section {
                    first: ri,
                    end,
                    header_footer: matches!(
                        cx.tree.b(group.id).kind,
                        BoxKind::TableRowGroup(RowGroup::Header | RowGroup::Footer)
                    ),
                    length: length_of(cx, group.id),
                    percent: percent_of(cx, group.id),
                    constrained: length_of(cx, group.id).is_some()
                        || (ri..end).any(|r| row_fixed[r]),
                });
                ri = end;
            }
            None => {
                sections.push(Section {
                    first: ri,
                    end: ri + 1,
                    header_footer: false,
                    length: length_of(cx, grid.rows[ri].id),
                    percent: percent_of(cx, grid.rows[ri].id),
                    constrained: row_fixed[ri],
                });
                ri += 1;
            }
        }
    }
    // A row group with a length height is at least that tall.
    for sec in &sections {
        if let Some(l) = sec.length {
            let have = section_height(&row_h, &grid.row_collapsed, sec, geom.spacing.1);
            grow_rows(&mut row_h, &grid.row_collapsed, &row_fixed, sec, l - have);
        }
    }
    let mut rows_total: f32 = row_h.iter().sum();
    if let Some(h) = table_height_spec {
        let want = h - pb_v - spacing_v;
        if want > rows_total && empty {
            // A table without rows is as tall as its height says (CSS 2 §17.5.3: the height is a
            // minimum height for the table).
            rows_total = want;
        } else if want > rows_total {
            let extra = want - rows_total;
            let heights: Vec<f32> = sections
                .iter()
                .map(|sec| section_height(&row_h, &grid.row_collapsed, sec, geom.spacing.1))
                .collect();
            let basis = (want).max(0.0);
            let add = distribute_extra(&sections, &heights, extra, basis);
            for (sec, a) in sections.iter().zip(add) {
                grow_rows(&mut row_h, &grid.row_collapsed, &row_fixed, sec, a);
            }
            rows_total = row_h.iter().sum();
            if rows_total < want {
                // (nothing could take it: every row collapsed)
                rows_total = want;
            }
        }
    }
    let table_height = rows_total + spacing_v + pb_v;
    let mut row_y = Vec::with_capacity(nrows + 1);
    let mut y = geom.border.top + geom.padding.top + geom.spacing.1;
    for (ri, &h) in row_h.iter().enumerate() {
        row_y.push(y);
        if !grid.row_collapsed[ri] {
            y += h + geom.spacing.1;
        }
    }
    row_y.push(y);

    // Final cell layout at their heights, with vertical alignment.
    let mut row_children: Vec<Vec<FragmentChild<S>>> = (0..nrows).map(|_| Vec::new()).collect();
    // Out-of-flow candidates from each row's cells, in the row's coordinates.
    let mut row_oof: Vec<Vec<Oof>> = (0..nrows).map(|_| Vec::new()).collect();
    let row_left =
        geom.border.left + geom.padding.left + if ncols == 0 { 0.0 } else { geom.spacing.0 };
    let row_width = if ncols == 0 {
        (table_width - pb_h).max(0.0)
    } else {
        (table_width - pb_h - 2.0 * geom.spacing.0).max(0.0)
    };
    // css-position-3 §3.1: relative offsets of table parts. Percentages resolve against the
    // parent part (the table, a row group, a row) and, vertically, only against its specified
    // height (the used one depends on the content being offset).
    let specified_height = |cx: &Cx<'_, S>, id: BoxId| {
        resolve_height(
            cx.tree.b(id).style.height(),
            None,
            0.0,
            BoxSizing::ContentBox,
        )
    };
    let table_spec_h = specified_height(cx, table);
    for (ci, cell) in grid.cells.iter().enumerate() {
        if cell_hidden[ci] {
            continue;
        }
        let (left, w) = col_left(cell.col, cell.colspan);
        let end = (cell.row + cell.rowspan).min(nrows);
        let h = (cell.row..end)
            .filter(|&r| !grid.row_collapsed[r])
            .map(|r| row_h[r])
            .sum::<f32>()
            + geom.spacing.1 * (rows_shown(cell).max(1) - 1) as f32;
        let mut space = cell_space(w, Some(h), content_w);
        space.collapsed_border = borders[ci];
        let r = cx.layout(cell.id, &space, None, Point::ZERO);
        let style = cx.style_here(cell.id);
        let mut content_h = cell_results[ci].0;
        if !cx.frame.swaps()
            && !matches!(style.vertical_align(), VerticalAlign::Baseline)
            && !style.height().is_auto()
        {
            // (a specified height is not the content's: what is aligned is the content)
            let mut measure = cell_space(w, None, content_w);
            measure.collapsed_border = borders[ci];
            measure.content_height = true;
            content_h = cx
                .layout(cell.id, &measure, None, Point::ZERO)
                .fragment
                .size
                .height;
        }
        let foreign = cx.tree.b(cell.id).style.view() != cx.frame;
        // (`align-content` other than `normal` takes over from `vertical-align`, css-align-3 §5.4)
        let valign = match style.align_content() {
            Align::Start | Align::FlexStart => VerticalAlign::Top,
            Align::Center => VerticalAlign::Middle,
            Align::End | Align::FlexEnd => VerticalAlign::Bottom,
            _ => style.vertical_align(),
        };
        let shift = match valign {
            // (a cell in another writing mode aligns its content in its own block axis)
            _ if foreign => 0.0,
            VerticalAlign::Middle => (h - content_h) / 2.0,
            VerticalAlign::Bottom => h - content_h,
            VerticalAlign::Baseline if cell.rowspan == 1 => {
                let bl = cell_results[ci]
                    .1
                    .unwrap_or_else(|| content_bottom(&r.fragment));
                row_above[cell.row] - bl
            }
            _ => 0.0,
        };
        let mut frag = r.fragment;
        let empty = frag.children.is_empty();
        let mut extra_flags = 0;
        if empty && style.empty_cells() == EmptyCells::Hide && !geom.collapse {
            extra_flags |= fragment_flags::HIDE_EMPTY;
        }
        if cell_clipped[ci] {
            extra_flags |= fragment_flags::CLIP_X | fragment_flags::CLIP_Y;
        }
        if shift.abs() > 0.001 || extra_flags != 0 || borders[ci].is_some() {
            frag = adjust_cell(&frag, shift.max(0.0), extra_flags, borders[ci]);
        }
        let row_spec_h = specified_height(cx, grid.rows[cell.row].id);
        let rel = cx.relative_offset(&style, row_width, row_spec_h);
        let offset_in_row = Point::new(left - row_left + rel.x, rel.y);
        for o in &r.oof {
            row_oof[cell.row].push(o.shifted(offset_in_row.x, offset_in_row.y));
        }
        row_children[cell.row].push(FragmentChild {
            offset: offset_in_row,
            fragment: frag,
            stacking_owner: 0,
        });
    }

    // Fragments: column groups and columns, then row groups holding rows holding cells.
    let mut children: Vec<FragmentChild<S>> = Vec::new();
    let rows_top = row_y.first().copied().unwrap_or(0.0);
    let rows_bottom = row_y.last().map_or(0.0, |y| {
        if visible_rows > 0 {
            y - geom.spacing.1
        } else {
            *y
        }
    });
    for &(b, c0, c1) in &grid.column_boxes {
        if c1 <= c0 || c1 > ncols {
            continue;
        }
        let (left, w) = col_left(c0, c1 - c0);
        let f = box_fragment(
            cx,
            b,
            Size::new(w, (rows_bottom - rows_top).max(0.0)),
            Sides::default(),
            Sides::default(),
            Vec::new(),
            None,
            None,
            BoxContent::None,
            fragment_flags::TABLE_PART,
        );
        children.push(FragmentChild {
            offset: Point::new(left, rows_top),
            fragment: f,
            stacking_owner: 0,
        });
    }
    // A row's baseline is that of its baseline-aligned cells; a row with cells but none of them
    // baseline-aligned has none, and an empty row's is its center (css-tables-3 §3.10.2). The
    // table's is the first row's that has one.
    let row_baseline = |ri: usize| {
        if row_aligned[ri] {
            Some(row_above[ri])
        } else if row_has_cells[ri] {
            None
        } else {
            Some(row_h[ri] / 2.0)
        }
    };
    let baseline = (0..nrows).find_map(|r| row_baseline(r).map(|b| row_y[r] + b));
    let mut ri = 0;
    let mut oof = Vec::new();
    // A row (or row group) lays out the out-of-flow boxes it is the containing block of; returns
    // its fragment and the rest of the candidates, in its own coordinates.
    let place_in_part = |cx: &mut Cx<'_, S>,
                         id: BoxId,
                         size: Size,
                         kids: &mut Vec<FragmentChild<S>>,
                         cands: Vec<Oof>|
     -> Vec<Oof> {
        if cands.iter().any(|o| o.cb == Some(id)) {
            cx.place_oofs(
                id,
                Rect::new(0.0, 0.0, size.width, size.height),
                cands,
                kids,
            )
        } else {
            cands
        }
    };
    let row_fragment = |cx: &mut Cx<'_, S>,
                        ri: usize,
                        mut kids: Vec<FragmentChild<S>>,
                        cands: Vec<Oof>,
                        parent_h: Option<f32>| {
        let row = &grid.rows[ri];
        let size = Size::new(row_width, row_h[ri]);
        let rest = place_in_part(cx, row.id, size, &mut kids, cands);
        let rel = cx.relative_offset(&cx.tree.b(row.id).style, row_width, parent_h);
        let f = box_fragment(
            cx,
            row.id,
            Size::new(row_width, row_h[ri]),
            Sides::default(),
            Sides::default(),
            kids,
            row_baseline(ri),
            row_baseline(ri),
            BoxContent::None,
            fragment_flags::TABLE_PART,
        );
        let off = Point::new(row_left + rel.x, row_y[ri] + rel.y);
        let rest: Vec<Oof> = rest.into_iter().map(|o| o.shifted(off.x, off.y)).collect();
        (off, f, rest)
    };
    let mut row_children = row_children.into_iter().map(Some).collect::<Vec<_>>();
    while ri < nrows {
        match grid.rows[ri].group {
            Some(g) => {
                let group = &grid.groups[g];
                let first = group.first_row;
                let last = (first + group.rows).min(nrows);
                let gy = row_y[first];
                let shown = (first..last).filter(|&r| !grid.row_collapsed[r]).count();
                let gh = (first..last)
                    .filter(|&r| !grid.row_collapsed[r])
                    .map(|r| row_h[r])
                    .sum::<f32>()
                    + geom.spacing.1 * (shown.max(1) - 1) as f32;
                let group_spec_h = specified_height(cx, group.id);
                let mut kids = Vec::new();
                let mut cands = Vec::new();
                for r in first..last {
                    let k = row_children[r].take().unwrap_or_default();
                    let c = std::mem::take(&mut row_oof[r]);
                    let (off, f, rest) = row_fragment(cx, r, k, c, group_spec_h);
                    cands.extend(rest.into_iter().map(|o| o.shifted(-row_left, -gy)));
                    kids.push(FragmentChild {
                        offset: Point::new(off.x - row_left, off.y - gy),
                        fragment: f,
                        stacking_owner: 0,
                    });
                }
                let gsize = Size::new(row_width, gh.max(0.0));
                let rest = place_in_part(cx, group.id, gsize, &mut kids, cands);
                let grel = cx.relative_offset(&cx.tree.b(group.id).style, row_width, table_spec_h);
                let goff = Point::new(row_left + grel.x, gy + grel.y);
                oof.extend(rest.into_iter().map(|o| o.shifted(goff.x, goff.y)));
                let f = box_fragment(
                    cx,
                    group.id,
                    Size::new(row_width, gh.max(0.0)),
                    Sides::default(),
                    Sides::default(),
                    kids,
                    None,
                    None,
                    BoxContent::None,
                    fragment_flags::TABLE_PART,
                );
                children.push(FragmentChild {
                    offset: goff,
                    fragment: f,
                    stacking_owner: 0,
                });
                ri = last.max(ri + 1);
            }
            None => {
                let k = row_children[ri].take().unwrap_or_default();
                let c = std::mem::take(&mut row_oof[ri]);
                let (off, f, rest) = row_fragment(cx, ri, k, c, table_spec_h);
                oof.extend(rest);
                children.push(FragmentChild {
                    offset: off,
                    fragment: f,
                    stacking_owner: 0,
                });
                ri += 1;
            }
        }
    }
    let content = if geom.collapse {
        BoxContent::CollapsedBorders(Arc::new(collapsed_segments(
            cx,
            &grid,
            &geom,
            table,
            &col_x,
            &widths,
            &row_y,
            &row_h,
            table_width,
            rtl,
        )))
    } else {
        BoxContent::None
    };
    let table_border = if geom.collapse {
        Sides::default()
    } else {
        geom.border
    };
    let table_frag = box_fragment(
        cx,
        table,
        Size::new(table_width, table_height),
        table_border,
        geom.padding,
        children,
        baseline,
        baseline,
        content,
        fragment_flags::TABLE,
    );

    // Captions above and below, as wide as the table.
    let mut wchildren = Vec::new();
    let mut caption_oof: Vec<Oof> = Vec::new();
    let mut y = 0.0f32;
    let kids = cx.tree.b(wrapper).children.clone();
    let mut bottom_caps = Vec::new();
    for &k in &kids {
        if cx.tree.b(k).kind != BoxKind::TableCaption {
            continue;
        }
        if cx.tree.b(k).style.caption_side() == CaptionSide::Bottom {
            bottom_caps.push(k);
            continue;
        }
        let r = cx.layout(k, &Space::fill(table_width, None), None, Point::ZERO);
        y += r.margin.top;
        let rel = cx.relative_offset(&cx.tree.b(k).style, table_width, None);
        let at = Point::new(r.margin.left + rel.x, y + rel.y);
        caption_oof.extend(r.oof.iter().map(|o| o.shifted(at.x, at.y)));
        wchildren.push(FragmentChild {
            offset: at,
            fragment: r.fragment.clone(),
            stacking_owner: 0,
        });
        y += r.fragment.size.height + r.margin.bottom;
    }
    let table_y = y;
    let wbaseline = table_frag.baseline.map(|b| b + table_y);
    wchildren.push(FragmentChild {
        offset: Point::new(0.0, table_y),
        fragment: table_frag,
        stacking_owner: 0,
    });
    for o in &mut oof {
        *o = o.shifted(0.0, table_y);
    }
    y += table_height;
    for k in bottom_caps {
        let r = cx.layout(k, &Space::fill(table_width, None), None, Point::ZERO);
        y += r.margin.top;
        let rel = cx.relative_offset(&cx.tree.b(k).style, table_width, None);
        let at = Point::new(r.margin.left + rel.x, y + rel.y);
        caption_oof.extend(r.oof.iter().map(|o| o.shifted(at.x, at.y)));
        wchildren.push(FragmentChild {
            offset: at,
            fragment: r.fragment.clone(),
            stacking_owner: 0,
        });
        y += r.fragment.size.height + r.margin.bottom;
    }
    let (ml, mr) = if space.mode == WidthMode::Fill && space.fixed_width.is_none() {
        let legacy = cx.cb_legacy_align(wrapper);
        resolve_auto_margins(&wedges, table_width, space.available, cb_rtl, legacy)
    } else {
        (wedges.ml(), wedges.mr())
    };
    oof.extend(caption_oof);
    if oof.iter().any(|o| o.cb == Some(wrapper)) {
        let pad = Rect::new(0.0, 0.0, table_width, y);
        oof = cx.place_oofs(wrapper, pad, oof, &mut wchildren);
    }
    let fragment = box_fragment(
        cx,
        wrapper,
        Size::new(table_width, y.max(0.0)),
        Sides::default(),
        Sides::default(),
        wchildren,
        wbaseline,
        wbaseline,
        BoxContent::None,
        fragment_flags::TABLE_WRAPPER,
    );
    LayoutResult {
        fragment,
        margin: Sides::new(wedges.mt(), mr, wedges.mb(), ml),
        start: Strut::of(wedges.mt()),
        end: Strut::of(wedges.mb()),
        through: false,
        floats: Vec::new(),
        oof,
    }
}

/// The margin-box heights of a table wrapper's captions at the table's width, summed.
pub(crate) fn captions_height<S: Style>(
    cx: &mut Cx<'_, S>,
    wrapper: BoxId,
    table_width: f32,
) -> f32 {
    let kids = cx.tree.b(wrapper).children.clone();
    let mut total = 0.0;
    for k in kids {
        if cx.tree.b(k).kind == BoxKind::TableCaption {
            let r = cx.layout(k, &Space::fill(table_width, None), None, Point::ZERO);
            total += r.margin.top + r.fragment.size.height + r.margin.bottom;
        }
    }
    total
}

fn cell_space(width: f32, height: Option<f32>, cb_width: f32) -> Space {
    let mut s = Space::fill(cb_width, None);
    s.available = width;
    s.fixed_width = Some(width);
    s.fixed_height = height;
    s.fixed_height_definite = false;
    s
}

/// The bottom of a fragment's content box (a cell without a baseline aligns there).
fn content_bottom<S: Style>(f: &Fragment<S>) -> f32 {
    let r = f.content_rect();
    r.y + r.height
}

/// A cell fragment with its content shifted down (vertical alignment), extra flags, and the collapsed
/// border halves as its border.
fn adjust_cell<S: Style>(
    f: &Arc<Fragment<S>>,
    shift: f32,
    extra_flags: u32,
    collapsed: Option<Sides>,
) -> Arc<Fragment<S>> {
    let FragmentKind::Box(b) = &f.kind else {
        return f.clone();
    };
    let children: Vec<FragmentChild<S>> = f
        .children
        .iter()
        .map(|c| {
            // (an absolutely positioned box placed by a vertical inset keeps its place; one at its
            // static position moves with the content)
            let inset_placed = c.fragment.as_box().is_some_and(|b| {
                b.flags & (fragment_flags::ABSOLUTE | fragment_flags::FIXED) != 0
                    && !(b.style.inset(Side::Top).is_auto()
                        && b.style.inset(Side::Bottom).is_auto())
            });
            let dy = if inset_placed { 0.0 } else { shift };
            FragmentChild {
                offset: Point::new(c.offset.x, c.offset.y + dy),
                fragment: c.fragment.clone(),
                stacking_owner: c.stacking_owner,
            }
        })
        .collect();
    let mut overflow = f.border_rect();
    let mut bounds = overflow;
    for c in &children {
        add_overflow(&mut overflow, &mut bounds, c);
    }
    Arc::new(Fragment {
        size: f.size,
        kind: FragmentKind::Box(BoxFragment {
            style: b.style.clone(),
            box_id: b.box_id,
            node: b.node,
            pseudo: b.pseudo,
            pseudo_of: b.pseudo_of,
            border: collapsed.unwrap_or(b.border),
            padding: b.padding,
            flags: b.flags
                | extra_flags
                | if collapsed.is_some() {
                    fragment_flags::NO_BORDER_PAINT
                } else {
                    0
                },
            order: b.order,
            subtree_end: b.subtree_end,
            content: b.content.clone(),
            containing: Default::default(),
            clip_reference: Default::default(),
            strip: Default::default(),
            stitched: None,
        }),
        children,
        overflow,
        paint_bounds: bounds,
        inflow: None,
        baseline: f.baseline.map(|v| v + shift),
        last_baseline: f.last_baseline.map(|v| v + shift),
    })
}

// ----- collapsing borders (CSS 2 §17.6.2) -----

/// A border candidate for one edge.
#[derive(Clone, Copy, Debug, PartialEq)]
struct EdgeBorder {
    width: f32,
    style: BorderStyle,
    color: Color,
    /// cell 6, row 5, row group 4, column 3, column group 2, table 1
    origin: u8,
}

impl EdgeBorder {
    const NONE: EdgeBorder = EdgeBorder {
        width: 0.0,
        style: BorderStyle::None,
        color: Color::TRANSPARENT,
        origin: 0,
    };

    fn style_rank(s: BorderStyle) -> u8 {
        match s {
            BorderStyle::Double => 8,
            BorderStyle::Solid => 7,
            BorderStyle::Dashed => 6,
            BorderStyle::Dotted => 5,
            BorderStyle::Ridge => 4,
            BorderStyle::Outset => 3,
            BorderStyle::Groove => 2,
            BorderStyle::Inset => 1,
            _ => 0,
        }
    }

    /// The §17.6.2.1 conflict resolution: `hidden` wins, `none` loses, wider wins, then the style
    /// order, then the origin (cell over row over row group over column over column group over table);
    /// on a full tie the existing (left/top) one stays.
    fn resolve(self, other: EdgeBorder) -> EdgeBorder {
        if self.style == BorderStyle::Hidden {
            return self;
        }
        if other.style == BorderStyle::Hidden {
            return other;
        }
        if other.style == BorderStyle::None || other.width <= 0.0 {
            return self;
        }
        if self.style == BorderStyle::None || self.width <= 0.0 {
            return other;
        }
        if other.width != self.width {
            return if other.width > self.width {
                other
            } else {
                self
            };
        }
        let (a, b) = (Self::style_rank(self.style), Self::style_rank(other.style));
        if a != b {
            return if b > a { other } else { self };
        }
        if other.origin > self.origin {
            other
        } else {
            self
        }
    }

    fn of<S: Style>(s: &S, side: Side, origin: u8) -> EdgeBorder {
        EdgeBorder {
            width: if s.border_style(side) == BorderStyle::Hidden {
                0.0
            } else {
                s.border_width(side)
            },
            style: s.border_style(side),
            color: s.border_color(side),
            origin,
        }
    }

    fn painted_width(&self) -> f32 {
        if self.style.is_invisible() {
            0.0
        } else {
            self.width
        }
    }
}

/// Resolved collapsed edges: horizontal `h[r][c]` (rows+1 × cols) and vertical `v[r][c]`
/// (rows × cols+1), in logical column order.
struct CollapsedEdges {
    h: Vec<Vec<EdgeBorder>>,
    v: Vec<Vec<EdgeBorder>>,
}

// (two-dimensional edge grids read clearest indexed)
#[allow(clippy::needless_range_loop)]
fn resolve_edges<S: Style>(cx: &Cx<'_, S>, grid: &Grid, table: BoxId) -> CollapsedEdges {
    let nr = grid.rows.len();
    let nc = grid.columns.len();
    let mut h = vec![vec![EdgeBorder::NONE; nc]; nr + 1];
    let mut v = vec![vec![EdgeBorder::NONE; nc + 1]; nr];
    let ts = &cx.tree.b(table).style;
    // Table outer edges.
    for c in 0..nc {
        h[0][c] = h[0][c].resolve(EdgeBorder::of(ts, Side::Top, 1));
        h[nr][c] = h[nr][c].resolve(EdgeBorder::of(ts, Side::Bottom, 1));
    }
    for r in 0..nr {
        v[r][0] = v[r][0].resolve(EdgeBorder::of(ts, Side::Left, 1));
        v[r][nc] = v[r][nc].resolve(EdgeBorder::of(ts, Side::Right, 1));
    }
    // Columns and column groups.
    for &(b, c0, c1) in &grid.column_boxes {
        let s = &cx.tree.b(b).style;
        let origin = if cx.tree.b(b).kind == BoxKind::TableColumnGroup {
            2
        } else {
            3
        };
        for r in 0..nr {
            if c0 < nc + 1 {
                v[r][c0] = v[r][c0].resolve(EdgeBorder::of(s, Side::Left, origin));
            }
            if c1 <= nc {
                v[r][c1] = v[r][c1].resolve(EdgeBorder::of(s, Side::Right, origin));
            }
        }
        for c in c0..c1.min(nc) {
            h[0][c] = h[0][c].resolve(EdgeBorder::of(s, Side::Top, origin));
            h[nr][c] = h[nr][c].resolve(EdgeBorder::of(s, Side::Bottom, origin));
        }
    }
    // Row groups and rows.
    for g in &grid.groups {
        let s = &cx.tree.b(g.id).style;
        let (r0, r1) = (g.first_row, (g.first_row + g.rows).min(nr));
        for c in 0..nc {
            h[r0][c] = h[r0][c].resolve(EdgeBorder::of(s, Side::Top, 4));
            h[r1][c] = h[r1][c].resolve(EdgeBorder::of(s, Side::Bottom, 4));
        }
        for r in r0..r1 {
            v[r][0] = v[r][0].resolve(EdgeBorder::of(s, Side::Left, 4));
            v[r][nc] = v[r][nc].resolve(EdgeBorder::of(s, Side::Right, 4));
        }
    }
    for (ri, row) in grid.rows.iter().enumerate() {
        let s = &cx.tree.b(row.id).style;
        for c in 0..nc {
            h[ri][c] = h[ri][c].resolve(EdgeBorder::of(s, Side::Top, 5));
            h[ri + 1][c] = h[ri + 1][c].resolve(EdgeBorder::of(s, Side::Bottom, 5));
        }
        v[ri][0] = v[ri][0].resolve(EdgeBorder::of(s, Side::Left, 5));
        v[ri][nc] = v[ri][nc].resolve(EdgeBorder::of(s, Side::Right, 5));
    }
    for cell in &grid.cells {
        let s = &cx.style_here(cell.id);
        let (r0, r1) = (cell.row, (cell.row + cell.rowspan).min(nr));
        let (c0, c1) = (cell.col, (cell.col + cell.colspan).min(nc));
        for c in c0..c1 {
            h[r0][c] = h[r0][c].resolve(EdgeBorder::of(s, Side::Top, 6));
            h[r1][c] = h[r1][c].resolve(EdgeBorder::of(s, Side::Bottom, 6));
        }
        for r in r0..r1 {
            v[r][c0] = v[r][c0].resolve(EdgeBorder::of(s, Side::Left, 6));
            v[r][c1] = v[r][c1].resolve(EdgeBorder::of(s, Side::Right, 6));
        }
    }
    CollapsedEdges { h, v }
}

/// Each cell's border in the collapsing model: half of each resolved edge (the widest along a spanned
/// side). `None` per cell in the separated model.
fn collapsed_cell_borders<S: Style>(
    cx: &Cx<'_, S>,
    grid: &Grid,
    geom: &TableGeom,
    table: BoxId,
) -> Vec<Option<Sides>> {
    if !geom.collapse {
        return vec![None; grid.cells.len()];
    }
    let e = resolve_edges(cx, grid, table);
    let nr = grid.rows.len();
    let nc = grid.columns.len();
    grid.cells
        .iter()
        .map(|cell| {
            let (r0, r1) = (cell.row, (cell.row + cell.rowspan).min(nr));
            let (c0, c1) = (cell.col, (cell.col + cell.colspan).min(nc));
            let top = (c0..c1)
                .map(|c| e.h[r0][c].painted_width())
                .fold(0.0, f32::max);
            let bottom = (c0..c1)
                .map(|c| e.h[r1][c].painted_width())
                .fold(0.0, f32::max);
            let left = (r0..r1)
                .map(|r| e.v[r][c0].painted_width())
                .fold(0.0, f32::max);
            let right = (r0..r1)
                .map(|r| e.v[r][c1].painted_width())
                .fold(0.0, f32::max);
            Some(Sides::new(top / 2.0, right / 2.0, bottom / 2.0, left / 2.0))
        })
        .collect()
}

/// Half of the widest collapsed border on each outer side (the table's used border).
fn outer_collapsed_border<S: Style>(
    cx: &Cx<'_, S>,
    grid: &Grid,
    _geom: &TableGeom,
    table: BoxId,
) -> Sides {
    let e = resolve_edges(cx, grid, table);
    let nr = grid.rows.len();
    let nc = grid.columns.len();
    if nr == 0 || nc == 0 {
        let s = &cx.tree.b(table).style;
        return Sides::new(
            s.border_width(Side::Top) / 2.0,
            s.border_width(Side::Right) / 2.0,
            s.border_width(Side::Bottom) / 2.0,
            s.border_width(Side::Left) / 2.0,
        );
    }
    let top = e.h[0].iter().map(|b| b.painted_width()).fold(0.0, f32::max);
    let bottom = e.h[nr]
        .iter()
        .map(|b| b.painted_width())
        .fold(0.0, f32::max);
    let left = (0..nr)
        .map(|r| e.v[r][0].painted_width())
        .fold(0.0, f32::max);
    let right = (0..nr)
        .map(|r| e.v[r][nc].painted_width())
        .fold(0.0, f32::max);
    Sides::new(top / 2.0, right / 2.0, bottom / 2.0, left / 2.0)
}

#[allow(clippy::too_many_arguments)]
fn collapsed_segments<S: Style>(
    cx: &Cx<'_, S>,
    grid: &Grid,
    geom: &TableGeom,
    table: BoxId,
    col_x: &[f32],
    widths: &[f32],
    row_y: &[f32],
    row_h: &[f32],
    table_width: f32,
    rtl: bool,
) -> Vec<CollapsedBorderSegment> {
    let _ = geom;
    let e = resolve_edges(cx, grid, table);
    let nr = grid.rows.len();
    let nc = grid.columns.len();
    let mut out = Vec::new();
    if nr == 0 || nc == 0 {
        return out;
    }
    let phys_x = |x: f32, w: f32| if rtl { table_width - x - w } else { x };
    let line_x = |c: usize| if c < nc { col_x[c] } else { col_x[nc] };
    let line_y = |r: usize| {
        if r < nr {
            row_y[r]
        } else {
            row_y[nr - 1] + row_h[nr - 1]
        }
    };
    for (r, row) in e.h.iter().enumerate() {
        for (c, b) in row.iter().enumerate() {
            let w = b.painted_width();
            if w <= 0.0 || b.color.is_transparent() {
                continue;
            }
            // Extend over half of the vertical borders meeting its ends so corners join.
            let meeting = |line: usize| {
                let above = r
                    .checked_sub(1)
                    .map_or(0.0, |ra| e.v[ra][line].painted_width());
                let below = if r < nr {
                    e.v[r][line].painted_width()
                } else {
                    0.0
                };
                above.max(below) / 2.0
            };
            let left_ext = meeting(c);
            let right_ext = meeting(c + 1);
            let x = line_x(c) - left_ext;
            let width = widths[c] + left_ext + right_ext;
            out.push(CollapsedBorderSegment {
                rect: Rect::new(phys_x(x, width), line_y(r) - w / 2.0, width, w),
                color: b.color,
                style: b.style,
                horizontal: true,
            });
        }
    }
    for (r, row) in e.v.iter().enumerate() {
        for (c, b) in row.iter().enumerate() {
            let w = b.painted_width();
            if w <= 0.0 || b.color.is_transparent() {
                continue;
            }
            let x = line_x(c) - w / 2.0;
            out.push(CollapsedBorderSegment {
                rect: Rect::new(phys_x(x, w), row_y[r], w, row_h[r]),
                color: b.color,
                style: b.style,
                horizontal: false,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsed_border_conflicts() {
        let a = EdgeBorder {
            width: 2.0,
            style: BorderStyle::Solid,
            color: Color::BLACK,
            origin: 6,
        };
        let b = EdgeBorder {
            width: 3.0,
            style: BorderStyle::Dotted,
            color: Color::WHITE,
            origin: 1,
        };
        assert_eq!(a.resolve(b), b);
        let hidden = EdgeBorder {
            style: BorderStyle::Hidden,
            ..EdgeBorder::NONE
        };
        assert_eq!(b.resolve(hidden), hidden);
        let dbl = EdgeBorder {
            style: BorderStyle::Double,
            ..a
        };
        assert_eq!(a.resolve(dbl), dbl);
    }
}
