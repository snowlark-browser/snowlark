// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Multi-column layout (CSS Multi-column Layout 1), with the block fragmentation it uses (CSS
//! Fragmentation 3).
//!
//! A multi-column container's content is laid out once as one *flow*: a block container of the
//! column width, unfragmented. Fragmentation does not move anything inside the flow: the columns
//! are consecutive stretches of it, and choosing them is choosing where each column starts.
//! [`pieces`] reads the flow's fragment tree as the sequence fragmentation sees (CSS
//! Fragmentation 3 §4): unbreakable pieces (line boxes, monolithic boxes, a box's block-start and
//! block-end border and padding), each saying whether a break before it is valid (§4.4: between
//! siblings, class A; between line boxes, class B; at a non-zero gap after a box's content edge,
//! class C; never inside border or padding), and breakable content height. [`columns`] puts the
//! column starts for a column height: a piece that would cross a column's end starts the next
//! column when a break before it is valid, and otherwise stays and overflows its column. A break
//! between siblings starts the next column at the second one's border edge, so the margins between
//! them are truncated (§5.2). The column height is the container's, or under `column-fill:
//! balance` the result of balancing (Multi-column §8.1): it starts at the content height over the
//! column count, or the tallest unbreakable stretch if that is more, and grows by the least
//! shortage until the content fits. Then the flow's fragments are cut at the column starts into
//! pieces (a box split there keeps its borders and padding only on its first and last piece:
//! `box-decoration-break: slice`) and each column's pieces are placed in its column box.
//!
//! Spanners (`column-span: all`, §6) are not laid out in the flow: the block layout leaves an
//! empty mark where one is ([`fragment_flags::SPANNER`]), ends the margins before it there and
//! collapses nothing after it with anything before. The marks cut the flow into segments, each
//! balanced as a row of columns of its own; each spanner is laid out across the container's
//! content box between the rows, its margins collapsing with adjacent spanners' only.
//!
//! Column rules (§4): the container fragment keeps the gap between each two adjacent columns of
//! each row ([`BoxContent::ColumnGaps`]), as tall as the row, the last row's reaching the content
//! box's block-end edge; painting draws the rule in the middle of each from the container's style.
//!
//! Floats and out-of-flow boxes whose containing block is in the flow are flows of their own
//! ([`Parallel`]): laid out in the flow like everything else (an out-of-flow box against its
//! containing block as if that were not fragmented), each starts in the column its top is in and
//! breaks at its own column starts, going on at the top of the next columns of the row, past the
//! columns the block sequence takes if it needs more. Balancing makes room for them: the columns
//! grow while the row needs more columns than it has or one of them overflows a column with no
//! valid break. A box with one inside gets an empty piece in each further column it reaches.
//! Clearance and the line boxes beside a float are not redone for the column the float breaks
//! into.
//!
//! A table in the separated borders model breaks too: its captions as blocks, between its rows,
//! and inside a row where every cell can break ([`row_pieces`]); its column boxes take a piece in
//! each column. Border spacing and the table's borders are sliced like a block's.
//!
//! A multi-column container in the flow of one whose columns have a known height (`column-fill:
//! auto` with a height) and that crosses a column's end is a nested fragmentation context
//! ([`Nested`]): the outer flow is laid out again with it laid out in rows of columns, the first
//! as tall as the outer column leaves, the others as tall as the outer columns, and each row
//! goes whole into an outer column. Content that would overflow a first row shorter than the
//! next goes on in the next row. Elsewhere (balanced outer columns, spanners in either) a nested
//! container is monolithic.
//!
//! Not done yet: breaks inside flex, grid, tables with collapsed borders or a cell spanning rows,
//! and scroll containers (monolithic), and out-of-flow boxes whose containing block is outside
//! the container (their static position stays in flow coordinates).

use std::sync::Arc;

use snowlark_paint::{Point, Rect, Sides, Size};

use crate::block::{block_sizes, layout_block_container_box};
use crate::boxes::{BoxId, BoxKind, LayoutBox, Level};
use crate::fragment::*;
use crate::layout::*;
use crate::sizing::ContentSizes;
use crate::style::*;

/// Layout units below which two edges are the same.
const EPSILON: f32 = 0.01;

/// The most columns a flow is cut into (more content overflows the last one).
const MAX_COLUMNS: usize = 2000;

/// The furthest flow position fragmentation sees: a piece further down ends there (CSS Values 4
/// lets a UA clamp values outside the range it supports; this is the range of 1/64 px units in
/// 32 bits, where other engines' layout lengths stop too).
const MAX_EXTENT: f32 = 33_554_431.0;

/// A layout unit: 1/64 px. Column positions are computed exactly and then snapped to it, so that
/// columns put side by side end where the container's content box does.
fn snap(v: f64) -> f32 {
    ((v * 64.0).round() / 64.0) as f32
}

/// How good a break before a piece is (CSS Fragmentation 3 §4.4, §3): no valid break at all, a
/// valid one that `break-*: avoid` asks to avoid or that leaves fewer lines than `orphans` or
/// `widows` (§3.3), a valid one, or a forced one. A column end takes the best before it.
mod appeal {
    pub const NONE: u8 = 0;
    pub const AVOIDED: u8 = 1;
    pub const ORPHANS_WIDOWS: u8 = 2;
    pub const PERFECT: u8 = 3;
    pub const FORCED: u8 = 4;
}

/// A stretch of the flow as fragmentation sees it, in flow coordinates (from the flow's content
/// top).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Piece {
    /// Content that cannot be broken: a line box, a monolithic box, a box's block-start or
    /// block-end border and padding. `before`: the [`appeal`] of a break before it.
    Unbreakable { top: f32, bottom: f32, before: u8 },
    /// A box's content height that its children do not take: it breaks anywhere.
    Breakable { top: f32, bottom: f32 },
    /// A spanner (§6): the content before it and after it are in different rows of columns.
    Spanner { at: f32, id: BoxId },
}

impl Piece {
    /// The piece with its edges at most [`MAX_EXTENT`].
    fn clamped(self) -> Piece {
        match self {
            Piece::Unbreakable {
                top,
                bottom,
                before,
            } => Piece::Unbreakable {
                top: top.min(MAX_EXTENT),
                bottom: bottom.min(MAX_EXTENT),
                before,
            },
            Piece::Breakable { top, bottom } => Piece::Breakable {
                top: top.min(MAX_EXTENT),
                bottom: bottom.min(MAX_EXTENT),
            },
            p => p,
        }
    }
}

/// The columns a stretch of the flow is cut into for one column height.
#[derive(Debug)]
struct Columns {
    /// Where each column starts in the flow (the first where the stretch starts).
    starts: Vec<f32>,
    /// The column height.
    height: f32,
    /// The least amount the column height would have to grow for a piece that crossed a column's
    /// end to stay in its column.
    shortage: f32,
    /// Whether a piece with no valid break before it overflows its column.
    overflows: bool,
}

/// A float or an out-of-flow box whose containing block is in the flow: a flow of its own beside
/// the block sequence (CSS Fragmentation 3 §4.1, parallel flows). It starts in the column its top
/// is in, at the same place as the content beside it, and goes on at the top of the columns after
/// that one in its row.
struct Parallel {
    /// Its border-box top in the flow.
    top: f32,
    /// Its own pieces, in flow coordinates.
    pieces: Vec<Piece>,
    /// Whether it is a float (not an out-of-flow box).
    float: bool,
}

/// A box of the block sequence cleared past a float (CSS 2 §9.5.2) that ends above it in the flow:
/// in the columns too it starts below the float's end, so when the float breaks inside it (a forced
/// break, or content its column cannot hold) the box goes on in the float's last column, below it.
#[derive(Clone, Copy)]
struct Cleared<'a> {
    /// The box's border-box top in the flow.
    top: f32,
    float: &'a Parallel,
}

impl Parallel {
    /// Where its content ends in the flow.
    fn bottom(&self) -> f32 {
        self.pieces.iter().fold(self.top, |e, p| match *p {
            Piece::Unbreakable { bottom, .. } | Piece::Breakable { bottom, .. } => e.max(bottom),
            Piece::Spanner { .. } => e,
        })
    }
}

/// The index of the column a flow position that starts something is in, in `starts`.
fn column_of(starts: &[f32], y: f32) -> usize {
    starts.partition_point(|&s| s <= y + EPSILON).max(1) - 1
}

/// How a fragment's pieces map to the container's columns: where each of its columns starts in
/// the flow, which column of the container it is, and how far a box broken inside it reaches.
struct Mapping {
    starts: Vec<f32>,
    ids: Vec<usize>,
    height: Vec<f32>,
}

impl Mapping {
    fn of(&self, y: f32) -> usize {
        column_of(&self.starts, y)
    }

    fn of_end(&self, y: f32) -> usize {
        self.starts.partition_point(|&s| s < y - EPSILON).max(1) - 1
    }

    /// The index of column `id` of the container here, if it is one of these.
    fn local(&self, id: usize) -> Option<usize> {
        // (the columns of one row's block sequence are numbered in order)
        let guess = id.wrapping_sub(self.ids[0]);
        if self.ids.get(guess) == Some(&id) {
            return Some(guess);
        }
        self.ids.iter().position(|&x| x == id)
    }
}

/// A stretch of the flow between spanners, laid out as one row of columns.
struct Segment {
    /// Where it starts in the flow.
    start: f32,
    /// Its pieces, in the flow's list.
    pieces: std::ops::Range<usize>,
    /// The spanner after it.
    spanner: Option<BoxId>,
}

/// One row of columns.
#[derive(Debug)]
struct RowColumns {
    /// Its first column (the flow's columns are numbered in order).
    first: usize,
    /// How many columns the flow's block sequence takes in it.
    count: usize,
    /// The columns after those that only floats or out-of-flow boxes reach.
    extra: Vec<usize>,
    /// Its top, from the container's content-box top.
    top: f32,
    /// Its column height.
    height: f32,
}

/// Every column of the container, across its rows: first the columns of the flow's block
/// sequence, in order, then those that only floats or out-of-flow boxes reach.
#[derive(Debug, Default)]
struct Placed {
    /// Where each column of the block sequence starts in the flow (never decreasing).
    starts: Vec<f32>,
    /// Each column's index in its row (which sets its inline position).
    index: Vec<usize>,
    /// Its top, from the container's content-box top.
    top: Vec<f32>,
    /// How far a box broken inside it reaches, from its start: the column height, or for the last
    /// column of a row before a spanner, the content before the spanner.
    height: Vec<f32>,
    /// Its row.
    row: Vec<usize>,
    rows: Vec<RowColumns>,
    /// In a nested multi-column container ([`Nested`]), the columns of a row: a float or an
    /// out-of-flow box goes on in the next row's columns (the next outer column), not in further
    /// columns of its own row.
    wrap: Option<usize>,
}

impl Placed {
    /// Adds a row of columns at `top`; `barrier`: where the spanner after it is in the flow.
    /// Comes before any column that only floats or out-of-flow boxes reach.
    fn push(&mut self, cols: &Columns, top: f32, barrier: Option<f32>) {
        let n = cols.starts.len();
        self.rows.push(RowColumns {
            first: self.starts.len(),
            count: n,
            extra: Vec::new(),
            top,
            height: cols.height,
        });
        for (i, &s) in cols.starts.iter().enumerate() {
            self.starts.push(s);
            self.index.push(i);
            self.top.push(top);
            let h = match barrier {
                Some(b) if i + 1 == n => (b - s).max(0.0),
                _ => cols.height,
            };
            self.height.push(h);
            self.row.push(self.rows.len() - 1);
        }
    }

    /// Column `j` of row `row` (added when only floats or out-of-flow boxes reach it).
    fn at(&mut self, row: usize, j: usize) -> usize {
        let r = &self.rows[row];
        if j < r.count {
            return r.first + j;
        }
        let (top, height) = (r.top, r.height);
        while self.rows[row].extra.len() <= j - self.rows[row].count {
            let id = self.index.len();
            let i = self.rows[row].count + self.rows[row].extra.len();
            self.index.push(i);
            self.top.push(top);
            self.height.push(height);
            self.row.push(row);
            self.rows[row].extra.push(id);
        }
        self.rows[row].extra[j - self.rows[row].count]
    }

    /// The column of the block sequence a flow position that starts something (a top edge) is in.
    fn of(&self, y: f32) -> usize {
        column_of(&self.starts, y)
    }

    /// The columns of the block sequence, as a [`Mapping`].
    fn mapping(&self) -> Mapping {
        let n = self.starts.len();
        Mapping {
            starts: self.starts.clone(),
            ids: (0..n).collect(),
            height: self.height[..n].to_vec(),
        }
    }
}

/// The columns of a row for column height `h`: those of the block sequence `list` (from `start`),
/// and how many columns the row needs with its parallel flows `par` in them; with the least
/// shortage of either, and whether a parallel flow overflows a column with no valid break.
fn fit(
    list: &[Piece],
    par: &[&Parallel],
    after: &[Cleared],
    start: f32,
    h: f32,
) -> (Columns, usize) {
    let mut cols = columns_with(list, start, &|_| h, after);
    let mut count = cols.starts.len();
    // (a float or an out-of-flow box that overflows its column asks for taller columns; content of
    // the block sequence with no valid break stays and overflows)
    cols.overflows = false;
    for p in par {
        let k = column_of(&cols.starts, p.top);
        let own = columns(&p.pieces, cols.starts[k], h);
        count = count.max(k + own.starts.len());
        cols.shortage = cols.shortage.min(own.shortage);
        cols.overflows |= own.overflows;
    }
    (cols, count)
}

/// CSS Multi-column Layout 1 §8.1: one row of columns for the pieces of a segment starting at
/// `start`, with the floats and out-of-flow boxes that start in it (`par`), balanced, or filled to
/// `room` when `fill`. `room`: the height left in a container with a height. `used`: the flow's
/// content height, which the last segment ends at. `after`: the boxes in it cleared past floats.
/// The columns and the height the row takes.
#[allow(clippy::too_many_arguments)]
fn row(
    list: &[Piece],
    par: &[&Parallel],
    after: &[Cleared],
    start: f32,
    n: usize,
    room: Option<f32>,
    fill: bool,
    last: bool,
    used: f32,
) -> (Columns, f32) {
    // (content that overflows its box takes room in the columns too)
    let end = if last { used } else { start };
    let extent = list.iter().fold(end, |e, p| match *p {
        Piece::Unbreakable { bottom, .. } | Piece::Breakable { bottom, .. } => e.max(bottom),
        Piece::Spanner { .. } => e,
    });
    let extent = par.iter().fold(extent, |e, p| e.max(p.bottom())) - start;
    // (the first column holds at least what comes before the first valid break, from the start)
    let mut lead = 0.0f32;
    for (i, p) in list.iter().enumerate() {
        match *p {
            Piece::Unbreakable { bottom, before, .. } if i == 0 || before == appeal::NONE => {
                lead = lead.max(bottom - start);
            }
            _ => break,
        }
    }
    let mut h = if fill {
        room.unwrap_or(extent)
    } else {
        let tallest = par.iter().fold(tallest_unbreakable(list), |t, p| {
            t.max(tallest_unbreakable(&p.pieces))
        });
        let balanced = runs_height(list, after, start, start + extent, n)
            .max(tallest)
            .max(lead);
        room.map_or(balanced, |l| balanced.min(l))
    };
    // (a column is at least 1px tall, so that content always ends in a finite number of them)
    h = h.max(1.0);
    // (each forced break starts a column whatever the height)
    let forced = list
        .iter()
        .filter(|p| matches!(p, Piece::Unbreakable { before, .. } if *before == appeal::FORCED))
        .count();
    let (mut cols, mut count) = fit(list, par, after, start, h);
    if !fill {
        for _ in 0..32 {
            let capped = room.is_some_and(|l| h >= l - EPSILON);
            let fits = count <= n.max(forced + 1) && !cols.overflows;
            if fits || capped || !cols.shortage.is_finite() {
                break;
            }
            let grown = h + cols.shortage.max(EPSILON);
            h = room.map_or(grown, |l| grown.min(l));
            (cols, count) = fit(list, par, after, start, h);
        }
    }
    let taken = if count > 1 {
        h
    } else if last {
        (used - start).max(0.0)
    } else {
        extent
    };
    (cols, taken)
}

/// The first estimate of a balanced column height (§8.1: as short as the content allows in `n`
/// columns) for the pieces `list` from `start` to `end`: each forced break starts a column, so the
/// content is cut into runs at them, and the columns left after one per run go one by one to the
/// run that is then tallest per column. The tallest run per column is the estimate. A box cleared
/// past a float (`after`) carries the float's forced breaks into the block sequence.
fn runs_height(list: &[Piece], after: &[Cleared], start: f32, end: f32, n: usize) -> f32 {
    let forced = |p: &Piece| match *p {
        Piece::Unbreakable { top, before, .. } if before == appeal::FORCED => Some(top),
        _ => None,
    };
    let mut cuts: Vec<f32> = list.iter().filter_map(forced).collect();
    for c in after {
        let inside = c.float.pieces.iter().filter_map(forced);
        cuts.extend(inside.filter(|&t| t > c.float.top + EPSILON && t <= c.top + EPSILON));
    }
    cuts.sort_by(f32::total_cmp);
    let end = end.max(start);
    // (a forced break at the start of the row starts no column)
    let mut edges = vec![start];
    for t in cuts {
        if t > edges[edges.len() - 1] + EPSILON && t < end {
            edges.push(t);
        }
    }
    edges.push(end);
    let cuts = edges;
    let mut runs: Vec<(f32, usize)> = cuts.windows(2).map(|w| (w[1] - w[0], 1)).collect();
    let per_column = |&(h, k): &(f32, usize)| h / k as f32;
    for _ in runs.len()..n {
        let tallest = runs
            .iter_mut()
            .max_by(|a, b| per_column(a).total_cmp(&per_column(b)))
            .expect("a run");
        tallest.1 += 1;
    }
    runs.iter().map(per_column).fold(0.0, f32::max)
}

/// CSS Fragmentation 3 §4: the column starts for column height `h`, the first at `start`. A piece
/// that crosses a column's end makes the column end at the best break before it in the column (the
/// latest of the best [`appeal`]); with none, it stays and overflows the column. A forced break
/// ends a column wherever it is.
fn columns(pieces: &[Piece], start: f32, h: f32) -> Columns {
    columns_with(pieces, start, &|_| h, &[])
}

/// [`columns`] where column `i` (from the first) is `h(i)` tall, with the boxes `after` (in flow
/// order) cleared past floats.
fn columns_with(
    pieces: &[Piece],
    start: f32,
    h: &dyn Fn(usize) -> f32,
    after: &[Cleared],
) -> Columns {
    let mut starts = vec![start];
    let mut shortage = f32::INFINITY;
    let mut overflows = false;
    // (the first piece in a column: a break needs content before it in its column, so none ends a
    // column holding only margins)
    let mut first = 0;
    let mut i = 0;
    let mut cleared = 0;
    while i < pieces.len() {
        // A box cleared past a float that ends in a later column starts in that column: the
        // columns up to it start where the float's do (none of the block sequence is in them),
        // the last at most at the box's top.
        let at = match pieces[i] {
            Piece::Unbreakable { top, .. } | Piece::Breakable { top, .. } => top,
            Piece::Spanner { at, .. } => at,
        };
        while let Some(c) = after.get(cleared).filter(|c| c.top <= at + EPSILON) {
            cleared += 1;
            let k = column_of(&starts, c.float.top);
            let own = columns_with(&c.float.pieces, starts[k], &|j| h(k + j), &[]);
            for j in starts.len() - k..own.starts.len() {
                if starts.len() >= MAX_COLUMNS {
                    break;
                }
                let last = *starts.last().expect("a first column");
                starts.push(own.starts[j].max(last).min(c.top));
                first = i;
            }
        }
        let room = starts.len() < MAX_COLUMNS;
        let column = *starts.last().expect("a first column");
        match pieces[i] {
            Piece::Unbreakable {
                top,
                bottom,
                before,
            } => {
                let movable = |top: f32| i > first && top > column + EPSILON && room;
                if before == appeal::FORCED && movable(top) {
                    starts.push(top);
                    first = i;
                    continue;
                }
                let end = column + h(starts.len() - 1);
                if bottom <= end + EPSILON {
                    i += 1;
                    continue;
                }
                shortage = shortage.min(bottom - end);
                let best = (first + 1..=i)
                    .filter_map(|j| match pieces[j] {
                        Piece::Unbreakable { top, before, .. }
                            if before > appeal::NONE && top > column + EPSILON && room =>
                        {
                            Some((before, j, top))
                        }
                        _ => None,
                    })
                    .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
                match best {
                    Some((_, j, top)) => {
                        starts.push(top);
                        first = j;
                        i = j;
                    }
                    None => {
                        overflows = true;
                        i += 1;
                    }
                }
            }
            Piece::Breakable { top, bottom } => {
                while starts.len() < MAX_COLUMNS {
                    let end = *starts.last().expect("a first column") + h(starts.len() - 1);
                    if bottom <= end + EPSILON {
                        break;
                    }
                    // (what is left of it would have needed this much more room)
                    shortage = shortage.min(bottom - end);
                    starts.push(end.max(top));
                    first = i;
                }
                i += 1;
            }
            // (segments hold none)
            Piece::Spanner { .. } => i += 1,
        }
    }
    Columns {
        starts,
        height: h(0),
        shortage,
        overflows,
    }
}

/// A multi-column container laid out inside an outer one's columns (CSS Fragmentation 3 §1: a
/// fragmentation context nested in another): its first row of columns is as tall as the outer
/// column leaves below its content-box top, the next ones as tall as the outer columns, so that
/// each row is in one outer column.
#[derive(Clone, Debug)]
pub(crate) struct Nested {
    pub id: BoxId,
    pub first: f32,
    pub rest: f32,
    /// What its layout made: each row's top and height, from its content-box top.
    pub rows: Option<Vec<(f32, f32)>>,
    /// And the tallest stretch of its flow with no valid break: no outer column can be shorter.
    pub tallest: f32,
    /// And how much taller its rows but the last would have to be for more of its content to
    /// fit in them.
    pub shortage: f32,
}

/// Whether two lists of nested containers ask for the same rows.
fn same_nested(a: &[Nested], b: &[Nested]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.id == y.id
                && (x.first - y.first).abs() <= EPSILON
                && (x.rest - y.rest).abs() <= EPSILON
        })
}

/// The rows of a nested multi-column container ([`Nested`]) whose flow has the pieces `list` and
/// the parallel flows `par`, with `n` columns to a row, the first row `first` tall and the others
/// `rest`: the columns are filled in turn, a row ending where it has `n` (a break in the outer
/// fragmentation context, §1), and the last row is balanced unless `fill` (Multi-column §7.1: in a
/// fragmented context only the last fragment is balanced). A first row shorter than the next
/// ones ends before a column whose content would overflow it, which goes on in the next row,
/// where there is more room (CSS Fragmentation 3 §4.4: breaks that overflow are the last resort).
/// Each row's columns and the height it takes.
fn nested_rows(
    list: &[Piece],
    par: &[&Parallel],
    n: usize,
    (first, rest): (f32, f32),
    fill: bool,
    used: f32,
) -> Vec<(Columns, f32)> {
    let (first, rest) = (first.max(1.0), rest.max(1.0));
    let whole = |starts: Vec<f32>, h: f32| Columns {
        starts,
        height: h,
        shortage: f32::INFINITY,
        overflows: false,
    };
    // (what is left of the flow from `s`)
    let from = |s: f32| -> Vec<Piece> {
        list.iter()
            .filter(|p| match **p {
                Piece::Unbreakable { top, .. } => top >= s - EPSILON,
                Piece::Breakable { bottom, .. } => bottom > s + EPSILON,
                Piece::Spanner { .. } => false,
            })
            .copied()
            .collect()
    };
    let mut out: Vec<(Columns, f32)> = Vec::new();
    let mut s = 0.0f32;
    loop {
        let h = if out.is_empty() { first } else { rest };
        let sub = from(s);
        let cols = columns(&sub, s, h);
        let count = cols.starts.len();
        let mut take = None;
        if out.is_empty() && first < rest - EPSILON {
            let overflows = |i: usize| {
                let a = cols.starts[i];
                let b = cols.starts.get(i + 1).copied().unwrap_or(f32::INFINITY);
                sub.iter().any(|p| {
                    matches!(*p, Piece::Unbreakable { top, bottom, .. }
                        if top >= a - EPSILON && top < b - EPSILON && bottom > a + h + EPSILON)
                })
            };
            take = (0..count.min(n)).find(|&i| overflows(i));
        }
        if take.is_none() && (count <= n || out.len() * n >= MAX_COLUMNS) {
            let sub_par: Vec<&Parallel> = par
                .iter()
                .copied()
                .filter(|p| p.top >= s - EPSILON)
                .collect();
            out.push(row(&sub, &sub_par, &[], s, n, Some(h), fill, true, used));
            break;
        }
        let take = take.unwrap_or(n);
        let mut full = whole(cols.starts[..take].to_vec(), h);
        full.shortage = cols.shortage;
        out.push((full, h));
        s = cols.starts[take];
    }
    // (rows that only floats or out-of-flow boxes reach, past the last)
    let starts: Vec<f32> = out
        .iter()
        .flat_map(|r| r.0.starts.iter().copied())
        .collect();
    let slot = |g: usize| if g < n { first } else { rest };
    let mut total = out.len() * n;
    for p in par {
        // (column `g` of the rows: the row's index times `n` and the index in the row)
        let k = column_of(&starts, p.top);
        let (mut row, mut j, mut seen) = (0, 0, 0);
        for (r, (c, _)) in out.iter().enumerate() {
            if k < seen + c.starts.len() {
                (row, j) = (r, k - seen);
                break;
            }
            seen += c.starts.len();
        }
        let g = row * n + j;
        let own = columns_with(&p.pieces, starts[k], &|i| slot(g + i), &[]);
        total = total.max(g + own.starts.len());
    }
    while out.len() * n < total {
        out.push((whole(Vec::new(), rest), rest));
    }
    out
}

/// Appends to `out` the multi-column containers in box fragment `f` (border-box top at flow
/// position `top`) that cross the end of the column they start in, for the flow's pieces `list`
/// in columns `h` tall: in-flow ones in the block sequence (a float or an out-of-flow one stays
/// whole). Where one starts comes from the pieces before it alone (laid out whole, it may have
/// been moved to the next column, which a container that breaks is not). One already in `prev`
/// stays (its rows may now fit).
fn find_nested<S: Style>(
    cx: &Cx<'_, S>,
    f: &Fragment<S>,
    top: f32,
    (list, h): (&[Piece], f32),
    prev: &[Nested],
    out: &mut Vec<Nested>,
) {
    for c in &f.children {
        if !in_sequence(cx, &c.fragment) {
            continue;
        }
        let t = top + c.offset.y;
        let Some(b) = c.fragment.as_box() else {
            continue;
        };
        let multicol = b.box_id.filter(|&id| {
            let lb = cx.tree.b(id);
            lb.kind == BoxKind::Block && lb.style.is_multicol()
        });
        let Some(id) = multicol else {
            if splittable(cx, &c.fragment) {
                find_nested(cx, &c.fragment, t, (list, h), prev, out);
            }
            continue;
        };
        let before: Vec<Piece> = list
            .iter()
            .filter(|p| match **p {
                Piece::Unbreakable { top, .. } | Piece::Breakable { top, .. } => top < t - EPSILON,
                Piece::Spanner { .. } => false,
            })
            .copied()
            .collect();
        let starts = columns(&before, 0.0, h).starts;
        let end = starts[column_of(&starts, t)] + h;
        let known = prev.iter().any(|p| p.id == id);
        if !known && t + c.fragment.size.height <= end + EPSILON {
            continue;
        }
        let start_edge = b.border.top + b.padding.top;
        // (one whose content starts past the column's end starts in the next column)
        let first = if t + start_edge < end - EPSILON {
            end - (t + start_edge)
        } else {
            h - start_edge
        };
        if first > EPSILON {
            out.push(Nested {
                id,
                first,
                rest: h,
                rows: None,
                tallest: 0.0,
                shortage: f32::INFINITY,
            });
        }
    }
}

/// The multi-column containers with an auto height in the block sequence of box fragment `f`
/// (border-box top at flow position `top`) that are not laid out in rows: where each starts and
/// ends in the flow.
fn nested_spans<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>, top: f32, out: &mut Vec<(f32, f32)>) {
    for c in &f.children {
        if !in_sequence(cx, &c.fragment) {
            continue;
        }
        let t = top + c.offset.y;
        let Some(b) = c.fragment.as_box() else {
            continue;
        };
        let multicol = b.box_id.is_some_and(|id| {
            let lb = cx.tree.b(id);
            lb.kind == BoxKind::Block && lb.style.is_multicol() && lb.style.height().is_auto()
        });
        if multicol {
            if nested_rows_of(cx, &c.fragment).is_none() {
                out.push((t, t + c.fragment.size.height));
            }
        } else if splittable(cx, &c.fragment) {
            nested_spans(cx, &c.fragment, t, out);
        }
    }
}

/// The pieces `list` with the nested multi-column containers at `spans` breakable anywhere, as
/// they will be once laid out in rows (CSS Fragmentation 3 §1: a nested fragmentation context
/// breaks in the outer one): the first estimate of the outer columns' balanced height.
fn breakable_spans(list: &[Piece], spans: &[(f32, f32)]) -> Vec<Piece> {
    let mut out = Vec::with_capacity(list.len() + spans.len());
    for &p in list {
        match p {
            Piece::Unbreakable {
                top,
                bottom,
                before,
            } if spans.iter().any(|&(t, b)| {
                (t - top).abs() <= EPSILON && (b - bottom).abs() <= EPSILON && b > t + EPSILON
            }) =>
            {
                out.push(Piece::Unbreakable {
                    top,
                    bottom: top,
                    before,
                });
                out.push(Piece::Breakable { top, bottom });
            }
            p => out.push(p),
        }
    }
    out
}

/// The rows of nested multi-column container fragment `f` ([`Nested`]), when it was laid out in
/// rows.
fn nested_rows_of<'c, S: Style>(cx: &'c Cx<'_, S>, f: &Fragment<S>) -> Option<&'c [(f32, f32)]> {
    let id = f.as_box()?.box_id?;
    cx.nested
        .iter()
        .find(|e| e.id == id)
        .and_then(|e| e.rows.as_deref())
}

/// The tallest stretch of the flow with no valid break inside: no column can be shorter
/// (balancing starts there).
fn tallest_unbreakable(pieces: &[Piece]) -> f32 {
    let mut tallest = 0.0f32;
    let mut run: Option<(f32, f32)> = None;
    for &p in pieces {
        match p {
            Piece::Unbreakable {
                top,
                bottom,
                before,
            } => match &mut run {
                Some((_, end)) if before == appeal::NONE => *end = end.max(bottom),
                _ => {
                    if let Some((a, b)) = run {
                        tallest = tallest.max(b - a);
                    }
                    run = Some((top, bottom));
                }
            },
            Piece::Breakable { .. } | Piece::Spanner { .. } => {
                if let Some((a, b)) = run.take() {
                    tallest = tallest.max(b - a);
                }
            }
        }
    }
    if let Some((a, b)) = run {
        tallest = tallest.max(b - a);
    }
    tallest
}

/// The min-content and max-content content widths of a multi-column container whose content in
/// the columns has `columns` and whose spanners have `spanners`. Multi-column Layout leaves them
/// to another module; as its 2016 draft defined and as implementations do: with a `column-width`,
/// min-content is the smaller of it and the content's, else the content's for each column plus
/// the gaps; max-content is the larger of the content's and `column-width`, for each column
/// (`column-count`, `auto` counting one) plus the gaps. A percentage gap counts as zero (CSS Box
/// Alignment 3 §8.1: it resolves against zero for intrinsic sizes).
pub(crate) fn content_sizes<S: Style>(
    style: &S,
    columns: ContentSizes,
    spanners: ContentSizes,
) -> ContentSizes {
    let n = style.column_count().unwrap_or(1).max(1) as f32;
    let gap = style
        .column_gap()
        .map_or(style.font_size(), |g| g.resolve(0.0))
        .max(0.0);
    let gaps = gap * (n - 1.0);
    let (min, max) = match style.column_width() {
        Some(w) => {
            let min = columns.min.min(w);
            (min, columns.max.max(w).max(min))
        }
        None => (columns.min * n + gaps, columns.max),
    };
    let max = (max * n + gaps).max(min);
    ContentSizes { min, max }.union(spanners)
}

/// CSS Multi-column Layout 1 §3.4: the used column count and width for `available` content width
/// (the width snapped down to a layout unit).
pub(crate) fn column_geometry(
    available: f32,
    gap: f32,
    width: Option<f32>,
    count: Option<u32>,
) -> (usize, f32) {
    let available = available.max(0.0);
    let n = match (width, count) {
        (None, Some(c)) => c.max(1) as usize,
        (Some(w), count) => {
            let fit = ((available + gap) / (w.max(0.0) + gap)).floor().max(1.0);
            let fit = if fit.is_finite() { fit as usize } else { 1 };
            count.map_or(fit, |c| fit.min(c.max(1) as usize))
        }
        (None, None) => 1,
    };
    let w = (available as f64 + gap as f64) / n as f64 - gap as f64;
    (n, ((w * 64.0).floor() / 64.0).max(0.0) as f32)
}

/// Lays out multi-column container `id` (a block container that establishes an independent
/// formatting context). `None` when it has a single column and no height to fragment in: then it
/// lays out as a block container.
pub(crate) fn layout_multicol<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    space: &Space,
    at: Point,
) -> Option<LayoutResult<S>> {
    let sizes = block_sizes(cx, id, space);
    let edges = sizes.edges;
    let style = cx.tree.b(id).style.clone();
    let content_width = (sizes.width - edges.pb_h()).max(0.0);
    // (`normal` is 1em in a multi-column container, CSS Box Alignment 3 §8.1)
    let gap = style
        .column_gap()
        .map_or(style.font_size(), |g| g.resolve(content_width))
        .max(0.0);
    let (n, w) = column_geometry(
        content_width,
        gap,
        style.column_width(),
        style.column_count(),
    );
    let pb_v = edges.pb_v();
    let limit = sizes
        .height
        .or(sizes.max_height)
        .map(|h| (h - pb_v).max(0.0));
    // (inside an outer container's columns, one with an auto height takes rows of columns as
    // tall as the outer columns leave, [`Nested`])
    let hint = cx
        .nested
        .iter()
        .find(|e| e.id == id)
        .map(|e| (e.first, e.rest))
        .filter(|_| sizes.height.is_none() && sizes.max_height.is_none());
    if n == 1 && limit.is_none() && hint.is_none() && !has_spanner(cx, id) {
        return None;
    }
    let fill_auto = style.column_fill_auto() && sizes.height.is_some();
    let flow_space = Space {
        fixed_width: Some(w + edges.pb_h()),
        fixed_height: None,
        fixed_height_definite: false,
        content_height: true,
        ..*space
    };
    let content_top = edges.border.top + edges.padding.top;
    let content_left = edges.border.left + edges.padding.left;
    // The flow, and the flow as fragmentation sees it. With columns of a known height, a
    // multi-column container inside that crosses a column's end is laid out again in rows that
    // fit the columns ([`Nested`]), and the flow with it, until where each starts settles.
    let fixed = limit.filter(|_| fill_auto && hint.is_none());
    let saved = (cx.nested.len(), cx.nested_path.len());
    let mut nested: Vec<Nested> = Vec::new();
    let mut passes = 0;
    let mut balanced_h: Option<f32> = None;
    let (r, list) = loop {
        cx.nested.truncate(saved.0);
        cx.nested.extend(nested.iter().cloned());
        cx.nested_path.truncate(saved.1);
        for e in &nested {
            let mut b = Some(e.id);
            while let Some(x) = b.filter(|&x| x != id) {
                cx.nested_path.push(x);
                b = cx.tree.b(x).parent;
            }
        }
        let outer_flow = std::mem::replace(&mut cx.column_flow, true);
        let r = layout_block_container_box(cx, id, &flow_space, None, at);
        cx.column_flow = outer_flow;
        let mut list = Vec::new();
        // (the flow's first piece starts it, where a break does nothing)
        children_pieces(
            cx,
            &r.fragment,
            -content_top,
            appeal::NONE,
            false,
            &mut list,
        );
        for p in &mut list {
            *p = p.clamped();
        }
        let most = if fixed.is_some() { 3 } else { 8 };
        if passes == most || list.iter().any(|p| matches!(p, Piece::Spanner { .. })) {
            break (r, list);
        }
        let h = match fixed {
            Some(h) => h.max(1.0),
            // Balanced columns (§8.1) with nested containers: the first height is balanced with
            // them breakable anywhere (their rows then follow it); while the content needs more
            // columns than there are, the height grows by the least shortage, the nested
            // containers' own columns' included, and they are laid out again. At least as tall as
            // each one's tallest stretch with no valid break.
            None if !fill_auto && hint.is_none() => {
                let mut spans = Vec::new();
                nested_spans(cx, &r.fragment, -content_top, &mut spans);
                if spans.is_empty() && nested.is_empty() {
                    break (r, list);
                }
                let mut par = Vec::new();
                parallels(cx, &r.fragment, -content_top, &mut par);
                for p in &mut par {
                    p.top = p.top.min(MAX_EXTENT);
                    for q in &mut p.pieces {
                        *q = q.clamped();
                    }
                }
                let par: Vec<&Parallel> = par.iter().collect();
                let laid: Vec<&Nested> = cx.nested[saved.0..]
                    .iter()
                    .filter(|e| e.rows.is_some())
                    .collect();
                let tallest = laid.iter().fold(0.0f32, |t, e| t.max(e.tallest));
                let h = match balanced_h {
                    None => {
                        let used = (r.fragment.size.height - pb_v).clamp(0.0, MAX_EXTENT);
                        let probe = breakable_spans(&list, &spans);
                        let (cols, _) = row(&probe, &par, &[], 0.0, n, limit, false, true, used);
                        cols.height
                    }
                    Some(h) => {
                        let forced = list
                            .iter()
                            .filter(|p| matches!(p, Piece::Unbreakable { before, .. } if *before == appeal::FORCED))
                            .count();
                        let (cols, count) = fit(&list, &par, &[], 0.0, h);
                        // (content with no valid break that overflows a column, such as the end
                        // of a nested container, asks for taller columns here too)
                        let stuck = columns(&list, 0.0, h).overflows;
                        let capped = limit.is_some_and(|l| h >= l - EPSILON);
                        let shortage = laid.iter().fold(cols.shortage, |s, e| s.min(e.shortage));
                        if (count <= n.max(forced + 1) && !cols.overflows && !stuck)
                            || capped
                            || !shortage.is_finite()
                        {
                            break (r, list);
                        }
                        h + shortage.max(EPSILON)
                    }
                };
                let h = h.max(tallest);
                let h = limit.map_or(h, |l| h.min(l)).max(1.0);
                balanced_h = Some(h);
                h
            }
            None => break (r, list),
        };
        let mut next = Vec::new();
        find_nested(
            cx,
            &r.fragment,
            -content_top,
            (&list, h),
            &nested,
            &mut next,
        );
        if same_nested(&next, &nested) {
            break (r, list);
        }
        nested = next;
        passes += 1;
    };
    let used = (r.fragment.size.height - pb_v).clamp(0.0, MAX_EXTENT);

    // The flow cut by its spanners into segments, each its own row of columns (§6.1: the content
    // before a spanner is balanced across the columns before the spanner).
    let mut segments: Vec<Segment> = Vec::new();
    let mut from = 0;
    let mut start = 0.0;
    for (i, p) in list.iter().enumerate() {
        if let Piece::Spanner { at, id } = *p {
            segments.push(Segment {
                start,
                pieces: from..i,
                spanner: Some(id),
            });
            from = i + 1;
            start = at;
        }
    }
    segments.push(Segment {
        start,
        pieces: from..list.len(),
        spanner: None,
    });
    // The floats and out-of-flow boxes in the flow, each in the segment its top is in (one at a
    // spanner's place comes after it).
    let mut par = Vec::new();
    parallels(cx, &r.fragment, -content_top, &mut par);
    for p in &mut par {
        p.top = p.top.min(MAX_EXTENT);
        for q in &mut p.pieces {
            *q = q.clamped();
        }
    }
    let segment_at = |y: f32| segments.partition_point(|s| s.start <= y + EPSILON).max(1) - 1;
    let segment_of = |p: &Parallel| segment_at(p.top);
    // The boxes cleared past floats, with each float of their row that ends above them in the flow.
    let mut tops = Vec::new();
    cleared_tops(cx, &r.fragment, -content_top, &mut tops);
    let mut cleared: Vec<Cleared> = Vec::new();
    for &top in &tops {
        for float in par.iter().filter(|p| {
            p.float
                && p.top < top - EPSILON
                && p.bottom() <= top + EPSILON
                && segment_of(p) == segment_at(top)
        }) {
            cleared.push(Cleared { top, float });
        }
    }
    cleared.sort_by(|a, b| a.top.total_cmp(&b.top));

    // The rows and spanners, stacked: margins between spanners collapse (and through rows with
    // no content), not with the content of a row.
    let trim = {
        let own = style.text_box_trim();
        TrimRequest {
            start: own.start || space.trim.start,
            end: own.end || space.trim.end,
        }
    };
    let cb_height = sizes.height.map(|h| (h - pb_v).max(0.0));
    // Where each column of a row is, from the content box's inline start.
    let rtl = style.direction() == Direction::Rtl;
    // (a column is at least 0 wide: with no room for the columns, only the gaps step them along)
    let step = ((content_width as f64 + gap as f64) / n as f64).max(gap as f64);
    let shift = |c: usize| {
        let i = if rtl {
            (n as f64 - 1.0) - c as f64
        } else {
            c as f64
        };
        snap(i * step)
    };
    // §4: a column rule is drawn in the middle of each gap between two columns of a row (the
    // columns that exist hold content), as tall as the row's columns; the rule's side is the
    // columns' inline-start side.
    let side = if rtl {
        Sides::new(0.0, 1.0, 0.0, 0.0)
    } else {
        Sides::new(0.0, 0.0, 0.0, 1.0)
    };
    let mut gaps: Vec<ColumnGap> = Vec::new();
    // (the gaps of the last row reach the content box's block-end edge)
    let mut last_row_gaps = 0;
    let nest = hint.filter(|_| segments.len() == 1);
    let mut placed = Placed {
        wrap: nest.map(|_| n),
        ..Placed::default()
    };
    let mut row_tops: Vec<(f32, f32)> = Vec::new();
    let mut nest_shortage = f32::INFINITY;
    let mut spanners: Vec<FragmentChild<S>> = Vec::new();
    let mut first_baseline: Option<f32> = None;
    let mut last_baseline: Option<f32> = None;
    let mut y = 0.0f32;
    let mut strut = Strut::default();
    let last = segments.len() - 1;
    for (k, seg) in segments.iter().enumerate() {
        let seg_list = &list[seg.pieces.clone()];
        let seg_par: Vec<&Parallel> = par.iter().filter(|p| segment_of(p) == k).collect();
        let seg_after: Vec<Cleared> = cleared
            .iter()
            .copied()
            .filter(|c| segment_of(c.float) == k)
            .collect();
        let empty = seg_list.is_empty() && seg_par.is_empty();
        if !empty {
            y += strut.resolve();
            strut = Strut::default();
        }
        let room = limit.map(|l| (l - y).max(0.0));
        let fill = fill_auto && k == last;
        let rows = match nest {
            Some(h) => nested_rows(seg_list, &seg_par, n, h, style.column_fill_auto(), used),
            None => vec![row(
                seg_list,
                &seg_par,
                &seg_after,
                seg.start,
                n,
                room,
                fill,
                k == last,
                used,
            )],
        };
        // (a nested container's rows but the last are full: what they lack is what the outer
        // columns lack)
        if nest.is_some() {
            let full = &rows[..rows.len().saturating_sub(1)];
            nest_shortage = full.iter().fold(nest_shortage, |s, r| s.min(r.0.shortage));
        }
        let count = rows.len();
        for (i, (cols, row_height)) in rows.into_iter().enumerate() {
            let row_height = if empty { 0.0 } else { row_height };
            let barrier = segments
                .get(k + 1)
                .filter(|_| i + 1 == count)
                .map(|s| s.start);
            placed.push(&cols, y, barrier);
            last_row_gaps = gaps.len();
            if !empty {
                for i in 1..cols.starts.len() {
                    let (a, b) = (shift(i - 1), shift(i));
                    let (left, right) = (a.min(b) + w, a.max(b));
                    gaps.push(ColumnGap {
                        rect: Rect::new(
                            content_left + left,
                            content_top + y,
                            (right - left).max(0.0),
                            cols.height,
                        ),
                        side,
                    });
                }
            }
            row_tops.push((y, row_height));
            y += row_height;
        }

        // The spanner after the row, across the whole content box.
        let Some(sid) = seg.spanner else {
            continue;
        };
        let content_before = segments[..=k].iter().any(|s| !s.pieces.is_empty());
        let content_after = segments[k + 1..].iter().any(|s| !s.pieces.is_empty());
        let mut sp = Space::fill(content_width, cb_height);
        sp.trim = TrimRequest {
            start: trim.start && !content_before && k == 0,
            end: trim.end && !content_after && k + 1 == last,
        };
        let sr = cx.layout(sid, &sp, None, Point::ZERO);
        let sstyle = cx.tree.b(sid).style.clone();
        strut.add(sr.margin.top);
        let top = y + strut.resolve();
        let rel = cx.relative_offset(&sstyle, content_width, cb_height);
        let offset = Point::new(
            content_left + sr.margin.left + rel.x,
            content_top + top + rel.y,
        );
        if let Some(b) = sr.fragment.baseline {
            first_baseline = Some(first_baseline.unwrap_or(offset.y + b));
        }
        if let Some(b) = sr.fragment.last_baseline.or(sr.fragment.baseline) {
            last_baseline = Some(offset.y + b);
        }
        y = top + sr.fragment.size.height;
        strut = Strut::of(sr.margin.bottom);
        spanners.push(FragmentChild {
            offset,
            fragment: sr.fragment,
            stacking_owner: 0,
        });
    }
    y += strut.resolve();
    // (size containment in the block axis sizes the container as if it were empty, CSS Contain 2
    // §3.1: the flow's own height already is)
    let contained = style.size_containment().height;
    let content_height = if contained { used } else { y.max(0.0) };
    let mut height = sizes.height.unwrap_or(content_height + pb_v);
    if sizes.height.is_none() {
        if let Some(max) = sizes.max_height {
            height = height.min(max);
        }
        if let Some(min) = sizes.min_height {
            height = height.max(min);
        }
    }
    // §4: rules end at the content box's block-end edge (a row before a spanner ends at it).
    let content_bottom = height - edges.border.bottom - edges.padding.bottom;
    for g in &mut gaps[last_row_gaps..] {
        g.rect.height = g.rect.height.max(content_bottom - g.rect.y);
    }

    // The flow's fragments, cut into columns and placed in the column boxes.
    let mut children = Vec::new();
    let flow = placed.mapping();
    for child in &r.fragment.children {
        let top = child.offset.y - content_top;
        for (c, t, piece) in split(cx, &child.fragment, top, &flow, &mut placed) {
            children.push(FragmentChild {
                offset: Point::new(
                    child.offset.x + shift(placed.index[c]),
                    content_top + placed.top[c] + t,
                ),
                fragment: piece,
                stacking_owner: child.stacking_owner,
            });
        }
    }
    children.extend(spanners);
    cx.nested.truncate(saved.0);
    cx.nested_path.truncate(saved.1);
    if nest.is_some() {
        // (no valid break separates the block-start border and padding from the first content, nor
        // the last from the block-end ones)
        let end_edge = edges.border.bottom + edges.padding.bottom;
        let mut whole = Vec::with_capacity(list.len() + 2);
        whole.push(Piece::Unbreakable {
            top: -content_top,
            bottom: 0.0,
            before: appeal::NONE,
        });
        whole.extend_from_slice(&list);
        // (a gap before them is a class C break)
        let gap = match list.last() {
            Some(Piece::Unbreakable { bottom, .. }) => *bottom < used - EPSILON,
            _ => true,
        };
        whole.push(Piece::Unbreakable {
            top: used,
            bottom: used + end_edge,
            before: class_c(gap, false),
        });
        let tallest = par.iter().fold(tallest_unbreakable(&whole), |t, p| {
            t.max(tallest_unbreakable(&p.pieces))
        });
        if let Some(e) = cx.nested.iter_mut().find(|e| e.id == id) {
            e.rows = Some(row_tops);
            e.tallest = tallest;
            e.shortage = nest_shortage;
        }
    }
    // CSS Box Alignment 3 §9.1: the first baseline is the first column's or the first spanner's,
    // the last one the last baseline of the column it falls in or the last spanner's (whichever
    // comes first, and last).
    let in_column = |y: f32| {
        let c = placed.of(y - content_top);
        y - placed.starts[c] + placed.top[c]
    };
    let first_baseline = match (r.fragment.baseline.map(in_column), first_baseline) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let flow_last = r
        .fragment
        .last_baseline
        .or(r.fragment.baseline)
        .map(in_column);
    let last_baseline = match (flow_last, last_baseline) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let size = Size::new(sizes.width, height);
    let content = if gaps.is_empty() {
        r.fragment
            .as_box()
            .map_or(BoxContent::None, |b| b.content.clone())
    } else {
        BoxContent::ColumnGaps(gaps.into())
    };
    let fragment = box_fragment(
        cx,
        id,
        size,
        edges.border,
        edges.padding,
        children,
        first_baseline,
        last_baseline,
        content,
        0,
    );
    let inflow = Rect::new(
        edges.border.left + edges.padding.left,
        content_top,
        content_width,
        content_height,
    );
    let fragment = with_inflow(fragment, Some(inflow));
    Some(LayoutResult {
        fragment,
        margin: Sides {
            top: r.margin.top,
            right: sizes.margin_right,
            bottom: r.margin.bottom,
            left: sizes.margin_left,
        },
        start: r.start,
        end: r.end,
        through: r.through && height <= 0.0,
        floats: Vec::new(),
        oof: r.oof,
    })
}

/// Whether multi-column container `id` has a spanner (CSS Multi-column Layout 1 §6.1: an in-flow
/// block-level descendant with `column-span: all` in its formatting context).
fn has_spanner<S: Style>(cx: &Cx<'_, S>, id: BoxId) -> bool {
    cx.tree.b(id).children.iter().any(|&c| {
        let b = cx.tree.b(c);
        if b.is_out_of_flow_or_float() || b.level != Level::Block {
            return false;
        }
        b.style.column_span_all() || (passes_spanners(b) && has_spanner(cx, c))
    })
}

/// Whether the in-flow block-level children of box `b`, a box of a multi-column container's flow,
/// are in that flow too, where `column-span: all` makes a spanner (§6.1): `b` is a block container
/// in the container's formatting context that fragmentation goes into ([`splittable`]), and not
/// the containing block of fixed-position descendants (which spanners do not escape).
pub(crate) fn passes_spanners<S: Style>(b: &LayoutBox<S>) -> bool {
    let s = &b.style;
    let contained = s.size_containment();
    b.kind == BoxKind::Block
        && b.level == Level::Block
        && !b.is_out_of_flow_or_float()
        && !b.is_independent()
        && !s.establishes_fixed_cb()
        && s.overflow_x() == Overflow::Visible
        && s.overflow_y() == Overflow::Visible
        && !(contained.width && contained.height)
        && matches!(
            s.display(),
            Display::Box {
                inside: DisplayInside::Flow,
                ..
            }
        )
}

/// Whether fragment `f` is where a spanner is in the flow.
fn is_spanner_mark<S: Style>(f: &Fragment<S>) -> bool {
    f.as_box()
        .is_some_and(|b| b.flags & fragment_flags::SPANNER != 0)
}

/// Whether fragment `f` has a spanner inside.
fn has_spanner_mark<S: Style>(f: &Fragment<S>) -> bool {
    f.as_box()
        .is_some_and(|b| b.flags & fragment_flags::HAS_SPANNER != 0)
}

/// Where the last spanner inside box fragment `f` is, from its border-box top.
pub(crate) fn last_spanner_mark<S: Style>(f: &Fragment<S>) -> Option<f32> {
    if !has_spanner_mark(f) {
        return None;
    }
    f.children.iter().rev().find_map(|c| {
        if is_spanner_mark(&c.fragment) {
            Some(c.offset.y)
        } else {
            last_spanner_mark(&c.fragment).map(|y| c.offset.y + y)
        }
    })
}

/// Fragments that are not in the flow's block direction sequence: floats, out-of-flow boxes
/// (each a [`Parallel`] flow), outside list markers (which go, whole, to the column their top is
/// in).
fn out_of_sequence<S: Style>(f: &Fragment<S>) -> bool {
    f.as_box().is_some_and(|b| {
        b.flags
            & (fragment_flags::FLOAT
                | fragment_flags::ABSOLUTE
                | fragment_flags::FIXED
                | fragment_flags::LIST_MARKER)
            != 0
    })
}

/// Whether fragment `f` is in the flow's block direction sequence: not [`out_of_sequence`], and
/// not a table column or column group ([`table_track`]).
fn in_sequence<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>) -> bool {
    !out_of_sequence(f) && !table_track(cx, f)
}

/// Whether fragment `f` is a float or an out-of-flow box of the flow (one whose containing block
/// is in the flow: the others are not laid out in it): a [`Parallel`] flow.
fn parallel<S: Style>(f: &Fragment<S>) -> bool {
    f.as_box().is_some_and(|b| {
        b.flags & (fragment_flags::FLOAT | fragment_flags::ABSOLUTE | fragment_flags::FIXED) != 0
    })
}

/// Appends the floats and out-of-flow boxes inside box fragment `f` (border-box top at flow
/// position `top`) that fragmentation reaches to `out`, those inside them too.
fn parallels<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>, top: f32, out: &mut Vec<Parallel>) {
    for c in &f.children {
        let t = top + c.offset.y;
        let p = parallel(&c.fragment);
        if p {
            out.push(Parallel {
                top: t,
                pieces: own_pieces(cx, &c.fragment, t),
                float: c
                    .fragment
                    .as_box()
                    .is_some_and(|b| b.flags & fragment_flags::FLOAT != 0),
            });
        }
        if (p || !out_of_sequence(&c.fragment)) && splittable(cx, &c.fragment) {
            parallels(cx, &c.fragment, t, out);
        }
    }
}

/// Appends the flow tops of the boxes of the block sequence inside box fragment `f` (border-box
/// top at flow position `top`) that clear floats (CSS 2 §9.5.2) to `out`.
fn cleared_tops<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>, top: f32, out: &mut Vec<f32>) {
    for c in &f.children {
        if !in_sequence(cx, &c.fragment) {
            continue;
        }
        let t = top + c.offset.y;
        if let Some(b) = c.fragment.as_box() {
            if !matches!(b.style.clear(), Clear::None) {
                out.push(t);
            }
        }
        if splittable(cx, &c.fragment) {
            cleared_tops(cx, &c.fragment, t, out);
        }
    }
}

/// The pieces of fragment `f` itself (border-box top at flow position `top`), as the start of a
/// flow.
fn own_pieces<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>, top: f32) -> Vec<Piece> {
    let mut out = Vec::new();
    box_pieces(cx, f, top, appeal::NONE, false, &mut out);
    out
}

/// Whether a fragment can be cut at a column boundary: a block container box of the flow, flow
/// roots (floats, out-of-flow boxes, `display: flow-root`) included (CSS Fragmentation 3 §4.1),
/// and a table in the separated borders model with its parts: captions, row groups, rows and
/// cells (a row breaks where all its cells can, [`row_pieces`]), and its column and column group
/// boxes, which take each piece of the grid ([`table_track`]). Flex, grid, a table with collapsed
/// borders or with a cell spanning past its row, a scroll container, a nested multi-column
/// container, a box in an orthogonal writing mode, a size-contained box, a replaced element and a
/// line box are monolithic here.
fn splittable<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>) -> bool {
    let Some(b) = f.as_box() else {
        return false;
    };
    let monolithic = fragment_flags::ATOMIC_INLINE
        | fragment_flags::CLIP_X
        | fragment_flags::CLIP_Y
        | fragment_flags::SCROLL_CONTAINER
        | fragment_flags::INLINE_BOX;
    if b.flags & monolithic != 0 || !matches!(b.content, BoxContent::None) {
        return false;
    }
    // (and so is a size-contained box, CSS Contain 2 §3.1)
    let contained = b.style.size_containment();
    if contained.width && contained.height {
        return false;
    }
    let flow = matches!(
        b.style.display(),
        Display::Box {
            inside: DisplayInside::Flow,
            ..
        }
    );
    let Some(id) = b.box_id else {
        return flow;
    };
    let lb = cx.tree.b(id);
    match lb.kind {
        BoxKind::Block | BoxKind::TableCaption | BoxKind::TableCell => {
            if lb.is_independent() {
                let special = crate::boxes::flags::FIELDSET
                    | crate::boxes::flags::RENDERED_LEGEND
                    | crate::boxes::flags::BUTTON;
                let horizontal = |s: &S| s.writing_mode() == WritingMode::HorizontalTb;
                let orthogonal = lb
                    .parent
                    .is_some_and(|p| horizontal(&cx.tree.b(p).style) != horizontal(&lb.style));
                if lb.flags & special != 0 || lb.style.is_multicol() || orthogonal {
                    return false;
                }
            }
            // (captions and cells are block containers whatever their display says)
            lb.kind != BoxKind::Block || flow
        }
        BoxKind::Table => cells_stay_in_their_rows(cx, f),
        BoxKind::TableWrapper
        | BoxKind::TableRowGroup(_)
        | BoxKind::TableRow
        | BoxKind::TableColumnGroup
        | BoxKind::TableColumn => true,
        _ => false,
    }
}

/// The kind of the box that fragment `f` is of.
fn kind_of<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>) -> Option<BoxKind> {
    f.as_box()
        .and_then(|b| b.box_id)
        .map(|id| cx.tree.b(id).kind)
}

/// Whether fragment `f` is a table column or column group: not in the block sequence (it lies
/// across the rows), it is cut like any box, a piece in each column the grid is in.
fn table_track<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>) -> bool {
    matches!(
        kind_of(cx, f),
        Some(BoxKind::TableColumn | BoxKind::TableColumnGroup)
    )
}

/// Whether every cell of table fragment `f` ends in the row it is in, so that the rows are the
/// table's block sequence (a cell spanning rows reaches past its own).
fn cells_stay_in_their_rows<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>) -> bool {
    f.children.iter().all(|c| match kind_of(cx, &c.fragment) {
        Some(BoxKind::TableRowGroup(_)) => cells_stay_in_their_rows(cx, &c.fragment),
        Some(BoxKind::TableRow) => {
            let h = c.fragment.size.height;
            c.fragment
                .children
                .iter()
                .all(|k| k.offset.y + k.fragment.size.height <= h + EPSILON)
        }
        _ => true,
    })
}

/// Appends the pieces of table row fragment `row` (top at flow position `top`) to `out`: its cells
/// side by side, so a break inside it is valid only where it is valid in every cell (CSS
/// Fragmentation 3 §4.4: each cell breaks as a block container). The cells' unbreakable pieces
/// are merged where they overlap, a break before the merged piece taking the worst appeal of
/// those starting there; between them the row breaks anywhere (each cell there is in breakable
/// space), unless breaks are avoided.
fn row_pieces<S: Style>(
    cx: &Cx<'_, S>,
    row: &Fragment<S>,
    top: f32,
    before: u8,
    avoid: bool,
    out: &mut Vec<Piece>,
) {
    let mut cells = Vec::new();
    for c in &row.children {
        pieces(cx, c, top + c.offset.y, before, avoid, &mut cells);
    }
    let mut hard: Vec<(f32, f32, u8)> = cells
        .iter()
        .filter_map(|p| match *p {
            Piece::Unbreakable {
                top,
                bottom,
                before,
            } => Some((top, bottom, before)),
            _ => None,
        })
        .collect();
    hard.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f32, f32, u8)> = Vec::with_capacity(hard.len());
    for (t, b, a) in hard {
        match merged.last_mut() {
            Some(m) if t < m.1 - EPSILON || (t - m.0).abs() <= EPSILON => {
                if (t - m.0).abs() <= EPSILON {
                    m.2 = m.2.min(a);
                }
                m.1 = m.1.max(b);
            }
            _ => merged.push((t, b, a)),
        }
    }
    if merged.first().is_none_or(|m| m.0 > top + EPSILON) {
        // (the break before the row, where no cell has anything at its top to carry it)
        out.push(Piece::Unbreakable {
            top,
            bottom: top,
            before,
        });
    }
    let mut y = top;
    let gap = |from: f32, to: f32, out: &mut Vec<Piece>| {
        if to > from + EPSILON {
            out.push(if avoid {
                Piece::Unbreakable {
                    top: from,
                    bottom: to,
                    before: appeal::AVOIDED,
                }
            } else {
                Piece::Breakable {
                    top: from,
                    bottom: to,
                }
            });
        }
    };
    for (t, b, a) in merged {
        gap(y, t, out);
        out.push(Piece::Unbreakable {
            top: t,
            bottom: b,
            before: a,
        });
        y = y.max(b);
    }
    gap(y, top + row.size.height, out);
}

/// What `break-before` or `break-after` asks of fragmentation into columns: 0 nothing, 1 to avoid
/// the break, 2 to force it (CSS Fragmentation 3 §3.1; page breaks are not column breaks).
fn wants(v: BreakBetween) -> u8 {
    match v {
        BreakBetween::Column | BreakBetween::Always => 2,
        BreakBetween::Avoid | BreakBetween::AvoidColumn => 1,
        _ => 0,
    }
}

/// What a box asks of a break before it: its own `break-before` joined with what its first child's
/// asks (§3.1: a first child's value propagates to its parent), forced before avoided.
fn wants_before<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>) -> u8 {
    let Some(b) = f.as_box() else {
        return 0;
    };
    if is_spanner_mark(f) {
        return 0;
    }
    let own = wants(b.style.break_before());
    let first = splittable(cx, f)
        .then(|| f.children.iter().find(|c| in_sequence(cx, &c.fragment)))
        .flatten();
    first.map_or(own, |c| own.max(wants_before(cx, &c.fragment)))
}

/// What a box asks of a break after it: its own `break-after` joined with its last child's.
fn wants_after<S: Style>(cx: &Cx<'_, S>, f: &Fragment<S>) -> u8 {
    let Some(b) = f.as_box() else {
        return 0;
    };
    if is_spanner_mark(f) {
        return 0;
    }
    let own = wants(b.style.break_after());
    let last = splittable(cx, f)
        .then(|| {
            f.children
                .iter()
                .rev()
                .find(|c| in_sequence(cx, &c.fragment))
        })
        .flatten();
    last.map_or(own, |c| own.max(wants_after(cx, &c.fragment)))
}

/// Appends the pieces of in-flow fragment `child` (border-box top at flow position `top`) to
/// `out`. `before`: the [`appeal`] of a break before it; `avoid`: an ancestor in the flow has
/// `break-inside: avoid`.
fn pieces<S: Style>(
    cx: &Cx<'_, S>,
    child: &FragmentChild<S>,
    top: f32,
    before: u8,
    avoid: bool,
    out: &mut Vec<Piece>,
) {
    if !in_sequence(cx, &child.fragment) {
        return;
    }
    box_pieces(cx, &child.fragment, top, before, avoid, out);
}

/// Appends the pieces of fragment `f` (border-box top at flow position `top`) to `out`, as
/// [`pieces`] does.
fn box_pieces<S: Style>(
    cx: &Cx<'_, S>,
    f: &Fragment<S>,
    top: f32,
    before: u8,
    avoid: bool,
    out: &mut Vec<Piece>,
) {
    if is_spanner_mark(f) {
        if let Some(id) = f.as_box().and_then(|b| b.box_id) {
            out.push(Piece::Spanner { at: top, id });
        }
        return;
    }
    let bottom = top + f.size.height;
    if let Some(rows) = nested_rows_of(cx, f) {
        // A nested multi-column container breaks between its rows of columns only.
        let b = f.as_box().expect("a nested container is a box");
        let start_edge = b.border.top + b.padding.top;
        let content_top = top + start_edge;
        if start_edge > EPSILON {
            out.push(Piece::Unbreakable {
                top,
                bottom: content_top,
                before,
            });
        }
        for (j, &(t, h)) in rows.iter().enumerate() {
            let before = match j {
                0 if start_edge > EPSILON => appeal::NONE,
                0 => before,
                _ => appeal::PERFECT,
            };
            out.push(Piece::Unbreakable {
                top: content_top + t,
                bottom: content_top + t + h,
                before,
            });
        }
        let end = content_top + rows.last().map_or(0.0, |r| r.0 + r.1);
        if bottom > end + EPSILON {
            out.push(Piece::Unbreakable {
                top: end,
                bottom,
                before: appeal::NONE,
            });
        }
        return;
    }
    if !splittable(cx, f) {
        out.push(Piece::Unbreakable {
            top,
            bottom,
            before,
        });
        return;
    }
    let b = f.as_box().expect("a splittable fragment is a box");
    let start_edge = b.border.top + b.padding.top;
    let end_edge = b.border.bottom + b.padding.bottom;
    let content_end = bottom - end_edge;
    if start_edge > EPSILON {
        out.push(Piece::Unbreakable {
            top,
            bottom: top + start_edge,
            before,
        });
    } else if !f.children.iter().any(|c| in_sequence(cx, &c.fragment)) {
        // (a box with nothing at its top to carry the break before it, such as an empty box with
        // a height, carries it on an empty piece)
        out.push(Piece::Unbreakable {
            top,
            bottom: top,
            before,
        });
    }
    let avoid = avoid
        || matches!(
            b.style.break_inside(),
            BreakInside::Avoid | BreakInside::AvoidColumn
        );
    let last_bottom = if kind_of(cx, f) == Some(BoxKind::TableRow) {
        row_pieces(cx, f, top, before, avoid, out);
        bottom
    } else {
        children_pieces(cx, f, top, before, avoid, out)
    };
    if content_end > last_bottom + EPSILON {
        if avoid {
            // (where breaks are avoided, the empty space is one piece, after a class C break
            // when children end before it)
            let gap = last_bottom > top + start_edge + EPSILON;
            out.push(Piece::Unbreakable {
                top: last_bottom,
                bottom: content_end,
                before: class_c(gap, avoid),
            });
        } else {
            out.push(Piece::Breakable {
                top: last_bottom,
                bottom: content_end,
            });
        }
    }
    if end_edge > EPSILON {
        // (a break before the block-end border and padding only at a gap, class C)
        let gap = content_end > last_bottom + EPSILON;
        out.push(Piece::Unbreakable {
            top: content_end,
            bottom,
            before: class_c(gap, avoid),
        });
    }
}

/// The appeal of a class C break (§4.4: at a gap after a content edge), when there is one.
fn class_c(gap: bool, avoid: bool) -> u8 {
    match (gap, avoid) {
        (false, _) => appeal::NONE,
        (true, true) => appeal::AVOIDED,
        (true, false) => appeal::PERFECT,
    }
}

/// Appends the pieces of the in-flow children of box fragment `f` (border-box top at flow position
/// `top`; `before`: the appeal of a break before it) to `out`; the flow position where the last
/// one ends (at least `f`'s content top).
fn children_pieces<S: Style>(
    cx: &Cx<'_, S>,
    f: &Fragment<S>,
    top: f32,
    before: u8,
    avoid: bool,
    out: &mut Vec<Piece>,
) -> f32 {
    let (start_edge, style) = match f.as_box() {
        Some(b) => (b.border.top + b.padding.top, &b.style),
        None => return top,
    };
    let content_top = top + start_edge;
    let kids = || f.children.iter().filter(|c| in_sequence(cx, &c.fragment));
    // §3.3: between the line boxes of a block container, a break that leaves fewer than `orphans`
    // lines before it or `widows` after it in the container is avoided.
    let lines = kids().all(|c| matches!(c.fragment.kind, FragmentKind::Line { .. }));
    let count = kids().count();
    let (orphans, widows) = (style.orphans() as usize, style.widows() as usize);
    let mut last_bottom = content_top;
    let mut after = 0;
    for (i, c) in kids().enumerate() {
        let t = top + c.offset.y;
        // §4.4: before the first child only where a break before this box is (no border or
        // padding between), or at a gap after the content edge (class C); else between siblings
        // (class A) or line boxes (class B).
        let appeal = if i == 0 {
            if start_edge <= EPSILON {
                before
            } else {
                class_c(t > content_top + EPSILON, avoid)
            }
        } else {
            match after.max(wants_before(cx, &c.fragment)) {
                2 => appeal::FORCED,
                1 => appeal::AVOIDED,
                _ if avoid => appeal::AVOIDED,
                _ if lines && (i < orphans || count - i < widows) => appeal::ORPHANS_WIDOWS,
                _ => appeal::PERFECT,
            }
        };
        pieces(cx, c, t, appeal, avoid, out);
        after = wants_after(cx, &c.fragment);
        last_bottom = last_bottom.max(t + c.fragment.size.height);
    }
    last_bottom
}

/// The pieces of fragment `f`, whose border-box top is at flow position `top`, cut at the column
/// starts of `m`: each piece's column of the container, its top in that column, and the piece. A
/// float or an out-of-flow box is cut at its own column starts, from the column its top is in.
fn split<S: Style>(
    cx: &Cx<'_, S>,
    f: &Arc<Fragment<S>>,
    top: f32,
    m: &Mapping,
    placed: &mut Placed,
) -> Vec<(usize, f32, Arc<Fragment<S>>)> {
    // (the container places its spanners itself)
    if is_spanner_mark(f) {
        return Vec::new();
    }
    if parallel(f) {
        let k = m.of(top);
        let id = m.ids[k];
        let (row, j) = (placed.row[id], placed.index[id]);
        let h = placed.rows[row].height;
        let own = match placed.wrap {
            // (column `g` of the nested container's rows of `nn`; past its last row, more of the
            // last)
            Some(nn) => {
                let g0 = row * nn + j;
                let last = placed.rows.len() - 1;
                let slot = |g: usize| (g / nn).min(last);
                let (own, heights): (Columns, Vec<f32>) = {
                    let rows = &placed.rows;
                    let own = columns_with(
                        &own_pieces(cx, f, top),
                        m.starts[k],
                        &|i| rows[slot(g0 + i)].height,
                        &[],
                    );
                    let heights = (0..own.starts.len())
                        .map(|i| rows[slot(g0 + i)].height)
                        .collect();
                    (own, heights)
                };
                let ids = (0..own.starts.len())
                    .map(|i| {
                        let g = g0 + i;
                        let r = slot(g);
                        placed.at(r, g - r * nn)
                    })
                    .collect();
                Mapping {
                    height: heights,
                    starts: own.starts,
                    ids,
                }
            }
            None => {
                let own = columns(&own_pieces(cx, f, top), m.starts[k], h);
                let ids = (0..own.starts.len())
                    .map(|i| placed.at(row, j + i))
                    .collect();
                Mapping {
                    height: vec![h; own.starts.len()],
                    starts: own.starts,
                    ids,
                }
            }
        };
        return split_box(cx, f, top, &own, placed);
    }
    split_box(cx, f, top, m, placed)
}

/// [`split`] for fragment `f` in mapping `m`.
fn split_box<S: Style>(
    cx: &Cx<'_, S>,
    f: &Arc<Fragment<S>>,
    top: f32,
    m: &Mapping,
    placed: &mut Placed,
) -> Vec<(usize, f32, Arc<Fragment<S>>)> {
    let first = m.of(top);
    let column_top = |c: usize| m.starts[c];
    if let Some(rows) = nested_rows_of(cx, f) {
        return split_nested(f, top, m, rows);
    }
    if (out_of_sequence(f) && !parallel(f)) || !splittable(cx, f) {
        return vec![(m.ids[first], top - column_top(first), f.clone())];
    }
    let bottom = top + f.size.height;
    let last = m.of_end(bottom).max(first);
    // (a float or an out-of-flow box inside can reach the next column even from the last one)
    let next = m
        .starts
        .get(first + 1)
        .copied()
        .unwrap_or(column_top(first) + m.height[first]);
    if first == last && f.overflow.bottom() + top <= next + EPSILON && !has_spanner_mark(f) {
        return vec![(m.ids[first], top - column_top(first), f.clone())];
    }
    // Each child's pieces, gathered by column: the columns of `m` the box spans, then any other
    // column a float or an out-of-flow box inside it reaches (where the box has an empty piece).
    let mut by_column: Vec<(usize, Option<usize>, Vec<FragmentChild<S>>)> = (first..=last)
        .map(|c| (m.ids[c], Some(c), Vec::new()))
        .collect();
    for child in &f.children {
        for (id, t, piece) in split(cx, &child.fragment, top + child.offset.y, m, placed) {
            let local = m.local(id).map(|c| c.max(first));
            let i = match local {
                Some(c) if c - first < by_column.len() => c - first,
                Some(c) => {
                    while by_column.len() <= c - first {
                        let c = first + by_column.len();
                        by_column.push((m.ids[c], Some(c), Vec::new()));
                    }
                    c - first
                }
                None => match by_column.iter().position(|e| e.0 == id) {
                    Some(i) => i,
                    None => {
                        by_column.push((id, None, Vec::new()));
                        by_column.len() - 1
                    }
                },
            };
            let piece_top = if by_column[i].1 == Some(first) {
                top - column_top(first)
            } else {
                0.0
            };
            by_column[i].2.push(FragmentChild {
                offset: Point::new(child.offset.x, t - piece_top),
                fragment: piece,
                stacking_owner: child.stacking_owner,
            });
        }
    }
    let b = f.as_box().expect("a splittable fragment is a box");
    let mut out = Vec::with_capacity(by_column.len());
    let several = by_column.len() > 1;
    for (id, local, children) in by_column {
        let start = if local == Some(first) {
            top - column_top(first)
        } else {
            0.0
        };
        // (a box broken inside takes the rest of its column, and more for monolithic content that
        // overflows the column)
        let end = match local {
            Some(c) if c == last => bottom - column_top(c),
            Some(c) if c < last => {
                let content = children
                    .iter()
                    .filter(|k| !out_of_sequence(&k.fragment))
                    .fold(0.0f32, |e, k| e.max(k.offset.y + k.fragment.size.height));
                m.height[c].max(start).max(start + content)
            }
            _ => start,
        };
        let c = local.unwrap_or(usize::MAX);
        let mut border = b.border;
        let mut padding = b.padding;
        if c != first {
            border.top = 0.0;
            padding.top = 0.0;
        }
        if c != last {
            border.bottom = 0.0;
            padding.bottom = 0.0;
        }
        let size = Size::new(f.size.width, (end - start).max(0.0));
        let baseline = if c == first {
            f.baseline.filter(|&y| y <= size.height)
        } else {
            None
        };
        // (the first piece keeps the whole box's scrollable overflow, which its scroll size is)
        let stitched = (several && c == first).then(|| Box::new(f.scrollable_overflow()));
        let p = piece(
            f,
            b,
            size,
            (border, padding),
            (children, BoxContent::None),
            baseline,
            stitched,
        );
        out.push((id, start, p));
    }
    out
}

/// [`split`] for a nested multi-column container `f` laid out in `rows` ([`Nested`]): a piece for
/// each row, in the column of `m` the row starts in, with the row's columns and column gaps.
fn split_nested<S: Style>(
    f: &Fragment<S>,
    top: f32,
    m: &Mapping,
    rows: &[(f32, f32)],
) -> Vec<(usize, f32, Arc<Fragment<S>>)> {
    let b = f.as_box().expect("a nested container is a box");
    let start_edge = b.border.top + b.padding.top;
    let end_edge = b.border.bottom + b.padding.bottom;
    let content_top = top + start_edge;
    let row_of = |y: f32| rows.partition_point(|r| r.0 <= y + EPSILON).max(1) - 1;
    // (where row `j`'s content starts in the whole box: only the first piece has the block-start
    // border and padding)
    let origin = |j: usize| rows[j].0 + if j == 0 { 0.0 } else { start_edge };
    let mut children: Vec<Vec<FragmentChild<S>>> = rows.iter().map(|_| Vec::new()).collect();
    for c in &f.children {
        let j = row_of(c.offset.y - start_edge);
        children[j].push(FragmentChild {
            offset: Point::new(c.offset.x, c.offset.y - origin(j)),
            fragment: c.fragment.clone(),
            stacking_owner: c.stacking_owner,
        });
    }
    let mut gaps: Vec<Vec<ColumnGap>> = rows.iter().map(|_| Vec::new()).collect();
    if let BoxContent::ColumnGaps(all) = &b.content {
        for g in all.iter() {
            let j = row_of(g.rect.y - start_edge);
            let mut g = *g;
            g.rect.y -= origin(j);
            gaps[j].push(g);
        }
    }
    let last = rows.len() - 1;
    let several = rows.len() > 1;
    let mut out = Vec::with_capacity(rows.len());
    for (j, (kids, gaps)) in children.into_iter().zip(gaps).enumerate() {
        let (column, start) = if j == 0 {
            let c = m.of(top);
            (c, top - m.starts[c])
        } else {
            let y = content_top + rows[j].0;
            let c = m.of(y);
            (c, y - m.starts[c])
        };
        let mut border = b.border;
        let mut padding = b.padding;
        let mut height = rows[j].1;
        if j == 0 {
            height += start_edge;
        } else {
            border.top = 0.0;
            padding.top = 0.0;
        }
        if j == last {
            height += end_edge;
        } else {
            border.bottom = 0.0;
            padding.bottom = 0.0;
        }
        let size = Size::new(f.size.width, height.max(0.0));
        let baseline = if j == 0 {
            f.baseline.filter(|&y| y <= size.height)
        } else {
            None
        };
        let stitched = (several && j == 0).then(|| Box::new(f.scrollable_overflow()));
        let content = if gaps.is_empty() {
            BoxContent::None
        } else {
            BoxContent::ColumnGaps(gaps.into())
        };
        let p = piece(
            f,
            b,
            size,
            (border, padding),
            (kids, content),
            baseline,
            stitched,
        );
        out.push((m.ids[column], start, p));
    }
    out
}

/// A piece of box fragment `f` (of box `b`) with the given geometry, children and content.
fn piece<S: Style>(
    f: &Fragment<S>,
    b: &BoxFragment<S>,
    size: Size,
    (border, padding): (Sides, Sides),
    (children, content): (Vec<FragmentChild<S>>, BoxContent),
    baseline: Option<f32>,
    stitched: Option<Box<Rect>>,
) -> Arc<Fragment<S>> {
    let mut overflow = Rect::new(0.0, 0.0, size.width, size.height);
    let mut bounds = overflow;
    for c in &children {
        add_overflow(&mut overflow, &mut bounds, c);
    }
    let inflow = f
        .inflow
        .map(|_| Rect::new(0.0, 0.0, size.width, size.height));
    Arc::new(Fragment {
        size,
        kind: FragmentKind::Box(BoxFragment {
            style: b.style.clone(),
            box_id: b.box_id,
            node: b.node,
            pseudo: b.pseudo,
            border,
            padding,
            flags: b.flags,
            order: b.order,
            subtree_end: b.subtree_end,
            content,
            containing: b.containing.copied(),
            clip_reference: Default::default(),
            strip: Default::default(),
            stitched,
        }),
        children,
        overflow,
        paint_bounds: bounds,
        inflow,
        baseline,
        last_baseline: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestStyle;
    use crate::tests::{rect, T};

    /// A multi-column container 210px wide with two columns and a 10px gap (columns 100px wide).
    fn two_columns(s: &mut TestStyle) {
        s.width = SizeValue::px(210.0);
        s.column_count = Some(2);
        s.column_gap = Some(LengthPercentage::px(10.0));
    }

    /// §8.1: four one-line blocks balance two to a column; the container is as tall as a column.
    #[test]
    fn blocks_balance_into_columns() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let items: Vec<_> = (0..4)
            .map(|_| {
                let d = t.doc.block(mc, |_| {});
                t.doc.text(d, "aa");
                d
            })
            .collect();
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        assert_eq!(c.height, 20.0);
        let at = |i: usize| {
            let r = rect(&f, items[i]);
            (r.x - c.x, r.y - c.y, r.width)
        };
        assert_eq!(at(0), (0.0, 0.0, 100.0));
        assert_eq!(at(1), (0.0, 10.0, 100.0));
        assert_eq!(at(2), (110.0, 0.0, 100.0));
        assert_eq!(at(3), (110.0, 10.0, 100.0));
    }

    /// A paragraph of five lines splits into two pieces: balancing grows the column from 25 (the
    /// third line would cross its end) to 30, so the first column takes three lines.
    #[test]
    fn a_paragraph_breaks_between_lines() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let p = t.doc.block(mc, |_| {});
        t.doc.text(p, "aaaaaaa bbbbbbb ccccccc ddddddd eeeeeee");
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        assert_eq!(c.height, 30.0);
        let pieces: Vec<_> = f
            .rects_of(p)
            .into_iter()
            .map(|r| (r.x - c.x, r.y - c.y, r.height))
            .collect();
        assert_eq!(pieces, vec![(0.0, 0.0, 30.0), (110.0, 0.0, 20.0)]);
    }

    /// CSS Fragmentation 3 §5.2: a break between two blocks truncates the margins between them:
    /// the second column starts at the second block's border edge.
    #[test]
    fn a_break_between_blocks_truncates_their_margins() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let items: Vec<_> = (0..2)
            .map(|_| {
                t.doc.block(mc, |s| {
                    s.height = SizeValue::px(30.0);
                    s.margin[0] = LengthPercentageAuto::px(10.0);
                    s.margin[2] = LengthPercentageAuto::px(10.0);
                })
            })
            .collect();
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        let b = rect(&f, items[1]);
        assert_eq!((b.x - c.x, b.y - c.y), (110.0, 0.0));
    }

    /// CSS Fragmentation 3 §4.4: no break inside a box's padding nor before its first child: a
    /// monolithic block inside 25px of padding above and below keeps the whole box in one column
    /// (`multicol-fill-balance-008`).
    #[test]
    fn padding_and_a_monolithic_child_do_not_break() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(100.0);
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
        });
        let outer = t.doc.block(mc, |s| {
            s.padding[0] = LengthPercentage::px(25.0);
            s.padding[2] = LengthPercentage::px(25.0);
        });
        t.doc.block(outer, |s| {
            s.height = SizeValue::px(50.0);
            s.display = Display::FLOW_ROOT;
        });
        let f = t.run(400.0, 400.0);
        assert_eq!(rect(&f, mc).height, 100.0);
        assert_eq!(f.rects_of(outer).len(), 1);
    }

    /// CSS Fragmentation 3 §3.1: `break-before: column` starts a column; balancing then fits the
    /// rest in the columns left.
    #[test]
    fn a_forced_break_starts_a_column() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let items: Vec<_> = (0..3)
            .map(|i| {
                let d = t.doc.block(mc, |s| {
                    if i == 1 {
                        s.break_before = BreakBetween::Column;
                    }
                });
                t.doc.text(d, "aa");
                d
            })
            .collect();
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        assert_eq!(c.height, 20.0);
        let at = |i: usize| {
            let r = rect(&f, items[i]);
            (r.x - c.x, r.y - c.y)
        };
        assert_eq!(
            (at(0), at(1), at(2)),
            ((0.0, 0.0), (110.0, 0.0), (110.0, 10.0))
        );
    }

    /// Three columns 100px wide, no gap.
    fn three_columns(s: &mut TestStyle) {
        s.width = SizeValue::px(300.0);
        s.column_count = Some(3);
        s.column_gap = Some(LengthPercentage::px(0.0));
    }

    /// §8.1: balancing with a forced break gives the columns left to the content after it: 20px,
    /// then a forced break and 200px, in three columns, is 100px tall (one column for the 20px,
    /// two for the 200px), not 220px / 3 grown until it fits.
    #[test]
    fn balancing_shares_the_columns_among_forced_breaks() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, three_columns);
        t.doc.block(mc, |s| s.height = SizeValue::px(20.0));
        let tall = t.doc.block(mc, |s| {
            s.break_before = BreakBetween::Column;
            s.height = SizeValue::px(200.0);
        });
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        assert_eq!(c.height, 100.0);
        let parts: Vec<_> = f
            .rects_of(tall)
            .iter()
            .map(|r| (r.x - c.x, r.y - c.y, r.height))
            .collect();
        assert_eq!(parts, [(100.0, 0.0, 100.0), (200.0, 0.0, 100.0)]);
    }

    /// CSS 2 §9.5.2 in columns: a box cleared past a float with a forced break inside starts in the
    /// float's last column, below the float (`multicol-fill-balance-025`).
    #[test]
    fn a_cleared_box_follows_a_float_broken_inside() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, three_columns);
        let float = t.doc.block(mc, |s| {
            s.float = Float::Left;
            s.width = SizeValue::px(100.0);
        });
        t.doc.block(float, |s| s.height = SizeValue::px(20.0));
        t.doc.block(float, |s| {
            s.break_before = BreakBetween::Column;
            s.height = SizeValue::px(50.0);
        });
        let clear = t.doc.block(mc, |s| {
            s.clear = Clear::Both;
            s.height = SizeValue::px(150.0);
        });
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        assert_eq!(c.height, 100.0);
        let parts: Vec<_> = f
            .rects_of(clear)
            .iter()
            .map(|r| (r.x - c.x, r.y - c.y, r.height))
            .collect();
        assert_eq!(parts, [(100.0, 50.0, 50.0), (200.0, 0.0, 100.0)]);
    }

    /// A forced break after an empty box with a height still starts a column
    /// (`multicol-fill-balance-007`), and the empty space of a `break-inside: avoid` box does not
    /// break (`multicol-fill-balance-014`).
    #[test]
    fn empty_boxes_carry_their_breaks() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let first = t.doc.block(mc, |s| {
            s.height = SizeValue::px(10.0);
            s.break_after = BreakBetween::Column;
        });
        let second = t.doc.block(mc, |s| {
            s.height = SizeValue::px(60.0);
            s.break_inside = BreakInside::Avoid;
        });
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        let r = rect(&f, second);
        assert_eq!(rect(&f, first).y - c.y, 0.0);
        assert_eq!((r.x - c.x, r.y - c.y, r.height), (110.0, 0.0, 60.0));
        assert_eq!(f.rects_of(second).len(), 1);
        assert_eq!(c.height, 60.0);
    }

    /// §3.2: `break-inside: avoid` keeps a three-line block whole: the columns grow to hold it
    /// rather than break inside it.
    #[test]
    fn break_inside_avoid_keeps_a_block_whole() {
        let build = |avoid: bool| {
            let mut t = T::new();
            let r0 = t.root();
            let mc = t.doc.block(r0, two_columns);
            let a = t.doc.block(mc, |_| {});
            t.doc.text(a, "aa");
            let b = t.doc.block(mc, |s| {
                s.orphans = 1;
                s.widows = 1;
                if avoid {
                    s.break_inside = BreakInside::Avoid;
                }
            });
            t.doc.text(b, "aaaaaaa bbbbbbb ccccccc");
            let c = t.doc.block(mc, |_| {});
            t.doc.text(c, "aa");
            let f = t.run(400.0, 400.0);
            (rect(&f, mc).height, f.rects_of(b).len())
        };
        assert_eq!(build(false), (30.0, 2));
        assert_eq!(build(true), (40.0, 1));
    }

    /// §3.3: with `orphans` and `widows` at 2, a three-line paragraph cannot split 1+2 or 2+1,
    /// so it moves whole to the second column; at 1 it splits.
    #[test]
    fn orphans_and_widows_keep_lines_together() {
        let build = |n: u32| {
            let mut t = T::new();
            let r0 = t.root();
            let mc = t.doc.block(r0, two_columns);
            let p1 = t.doc.block(mc, |_| {});
            t.doc.text(p1, "aa");
            let p2 = t.doc.block(mc, |s| {
                s.orphans = n;
                s.widows = n;
            });
            t.doc.text(p2, "aaaaaaa bbbbbbb ccccccc");
            let f = t.run(400.0, 400.0);
            let c = rect(&f, mc);
            let pieces: Vec<_> = f
                .rects_of(p2)
                .into_iter()
                .map(|r| (r.x - c.x, r.y - c.y, r.height))
                .collect();
            (c.height, pieces)
        };
        assert_eq!(build(2), (30.0, vec![(110.0, 0.0, 30.0)]));
        assert_eq!(
            build(1),
            (20.0, vec![(0.0, 10.0, 10.0), (110.0, 0.0, 20.0)])
        );
    }

    /// A box broken inside reaches past its column's end where monolithic content in it overflows
    /// the column (`getclientrects-008`); the next box starts the next column.
    #[test]
    fn a_broken_box_holds_its_overflowing_monolithic_content() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(320.0);
            s.height = SizeValue::px(100.0);
            s.column_count = Some(3);
            s.column_gap = Some(LengthPercentage::px(10.0));
            s.column_fill_auto = true;
        });
        let container = t.doc.block(mc, |_| {});
        for h in [250.0, 50.0, 250.0] {
            t.doc.block(container, |s| {
                s.height = SizeValue::px(h);
                s.display = Display::FLOW_ROOT;
            });
        }
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        let pieces: Vec<_> = f
            .rects_of(container)
            .into_iter()
            .map(|r| (r.x - c.x, r.y - c.y, r.height))
            .collect();
        assert_eq!(
            pieces,
            vec![(0.0, 0.0, 250.0), (110.0, 0.0, 100.0), (220.0, 0.0, 250.0)]
        );
    }

    /// A box split into columns measures its scroll size stitched together: a 100px box with a
    /// 300px child overflowing it, in three columns 100px tall, scrolls 300px
    /// (`scroll-width-height.tentative`); and the overflowing child takes room in the columns.
    #[test]
    fn a_split_box_scrolls_as_if_stitched() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(400.0);
            s.height = SizeValue::px(400.0);
            s.column_count = Some(3);
        });
        let child = t.doc.block(mc, |s| {
            s.width = SizeValue::px(100.0);
            s.height = SizeValue::px(100.0);
        });
        let grandchild = t.doc.block(child, |s| {
            s.width = SizeValue::px(100.0);
            s.height = SizeValue::px(300.0);
        });
        let f = t.run(800.0, 600.0);
        assert_eq!(f.rects_of(grandchild).len(), 3);
        for n in [child, grandchild] {
            let g = t.layout.geometry(n).expect("geometry");
            let size = (g.scrollable_overflow.width, g.scrollable_overflow.height);
            assert_eq!(size, (100.0, 300.0));
        }
    }

    /// §6.1: a spanner spans every column, and the content before it is balanced in the columns
    /// above it (two one-line blocks to a column), the content after it in a row of its own; a
    /// `column-span: all` box inside a nested formatting context is no spanner.
    #[test]
    fn a_spanner_ends_a_row_of_columns() {
        let build = |nested: bool| {
            let mut t = T::new();
            let r0 = t.root();
            let mc = t.doc.block(r0, two_columns);
            let line = |t: &mut T, parent| {
                let d = t.doc.block(parent, |_| {});
                t.doc.text(d, "aa");
                d
            };
            let before: Vec<_> = (0..4).map(|_| line(&mut t, mc)).collect();
            let holder = if nested {
                t.doc.block(mc, |s| s.display = Display::FLOW_ROOT)
            } else {
                mc
            };
            let spanner = t.doc.block(holder, |s| {
                s.column_span_all = true;
                s.height = SizeValue::px(15.0);
            });
            let after: Vec<_> = (0..2).map(|_| line(&mut t, mc)).collect();
            let f = t.run(400.0, 400.0);
            let c = rect(&f, mc);
            let at = |n| {
                let r = rect(&f, n);
                (r.x - c.x, r.y - c.y, r.width)
            };
            (
                c.height,
                at(before[3]),
                at(spanner),
                at(after[0]),
                at(after[1]),
            )
        };
        assert_eq!(
            build(false),
            (
                45.0,
                (110.0, 10.0, 100.0),
                (0.0, 20.0, 210.0),
                (0.0, 35.0, 100.0),
                (110.0, 35.0, 100.0)
            )
        );
        // (nested: seven boxes in one flow, the 15px one unbreakable)
        let (h, b3, s, ..) = build(true);
        assert_eq!((h, b3.0, s.2), (40.0, 0.0, 100.0));
    }

    /// §4: a column rule is drawn in the middle of the gap between two columns of each row, as
    /// tall as the row's columns, as a border on the columns' inline-start side; not across a
    /// spanner, nor in a row of one column; `inset` draws as `ridge`.
    #[test]
    fn column_rules_are_drawn_between_the_columns_of_each_row() {
        use snowlark_paint::{DisplayListBuilder, Item};
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            two_columns(s);
            s.column_rule_style = BorderStyle::Inset;
            s.column_rule_width = 4.0;
        });
        let line = |t: &mut T| {
            let d = t.doc.block(mc, |_| {});
            t.doc.text(d, "aa");
        };
        (0..4).for_each(|_| line(&mut t));
        t.doc.block(mc, |s| {
            s.column_span_all = true;
            s.height = SizeValue::px(15.0);
        });
        (0..2).for_each(|_| line(&mut t));
        t.doc.block(mc, |s| {
            s.column_span_all = true;
            s.height = SizeValue::px(5.0);
        });
        line(&mut t);
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        let mut b = DisplayListBuilder::new();
        crate::paint(&f, &t.text, Size::new(400.0, 400.0), &mut b);
        let rules: Vec<_> = b
            .build()
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Border(b) => Some((
                    (b.rect.x - c.x, b.rect.y - c.y, b.rect.width, b.rect.height),
                    (
                        b.widths.left,
                        b.widths.top + b.widths.right + b.widths.bottom,
                    ),
                    b.styles[3],
                )),
                _ => None,
            })
            .collect();
        let rule = |y, h| ((103.0, y, 4.0, h), (4.0, 0.0), BorderStyle::Ridge);
        assert_eq!(rules, vec![rule(0.0, 20.0), rule(35.0, 10.0)]);
    }

    /// §4: the last row's rules end at the content box's block-end edge, below balanced columns
    /// that do not fill it; in a right-to-left container the rule is the columns' right side.
    #[test]
    fn the_last_rows_rules_reach_the_content_edge() {
        use snowlark_paint::{DisplayListBuilder, Item};
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            two_columns(s);
            s.height = SizeValue::px(50.0);
            s.direction = Direction::Rtl;
            s.column_rule_style = BorderStyle::Solid;
            s.column_rule_width = 2.0;
        });
        for _ in 0..4 {
            let d = t.doc.block(mc, |_| {});
            t.doc.text(d, "aa");
        }
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        let mut b = DisplayListBuilder::new();
        crate::paint(&f, &t.text, Size::new(400.0, 400.0), &mut b);
        let rules: Vec<_> = b
            .build()
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Border(b) => Some((
                    (b.rect.x - c.x, b.rect.y - c.y, b.rect.width, b.rect.height),
                    (
                        b.widths.right,
                        b.widths.top + b.widths.left + b.widths.bottom,
                    ),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(rules, vec![((104.0, 0.0, 2.0, 50.0), (2.0, 0.0))]);
    }

    /// The margins of a spanner collapse with nothing in the columns: the content's margins before
    /// it end at the break, the first box after it keeps its own in its column; a box with the
    /// spanner inside breaks around it. (The 20px boxes are monolithic.)
    #[test]
    fn a_spanner_breaks_its_parent_and_keeps_its_margins() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let wrapper = t.doc.block(mc, |_| {});
        let first = t.doc.block(wrapper, |s| {
            s.height = SizeValue::px(20.0);
            s.display = Display::FLOW_ROOT;
            s.margin[2] = LengthPercentageAuto::px(7.0);
        });
        let spanner = t.doc.block(wrapper, |s| {
            s.column_span_all = true;
            s.height = SizeValue::px(10.0);
            s.margin[0] = LengthPercentageAuto::px(10.0);
            s.margin[2] = LengthPercentageAuto::px(10.0);
        });
        let next = t.doc.block(wrapper, |s| {
            s.height = SizeValue::px(20.0);
            s.display = Display::FLOW_ROOT;
            s.margin[0] = LengthPercentageAuto::px(5.0);
        });
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        let y = |n| rect(&f, n).y - c.y;
        assert_eq!(y(first), 0.0);
        // (one column of 20px, then the spanner's 10px margin)
        assert_eq!(y(spanner), 30.0);
        assert_eq!(y(next), 55.0);
        let pieces: Vec<_> = f
            .rects_of(wrapper)
            .into_iter()
            .map(|r| (r.y - c.y, r.height))
            .collect();
        assert_eq!(pieces, vec![(0.0, 20.0), (50.0, 25.0)]);
        assert_eq!(c.height, 75.0);
    }

    /// §6.1: what follows a box broken by a spanner starts after the spanner even when the box
    /// ends before the spanner's place (a `height: 0` parent): its 40px take the next row's two
    /// columns, 20px each, not 10px.
    #[test]
    fn content_after_a_short_box_holding_a_spanner_comes_after_the_spanner() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let wrapper = t.doc.block(mc, |s| s.height = SizeValue::px(0.0));
        t.doc.block(wrapper, |s| s.height = SizeValue::px(20.0));
        let spanner = t.doc.block(wrapper, |s| {
            s.column_span_all = true;
            s.height = SizeValue::px(10.0);
        });
        let next = t.doc.block(mc, |s| s.height = SizeValue::px(40.0));
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        assert_eq!(rect(&f, spanner).y - c.y, 10.0);
        let parts: Vec<_> = f
            .rects_of(next)
            .iter()
            .map(|r| (r.y - c.y, r.height))
            .collect();
        assert_eq!(parts, [(20.0, 20.0), (20.0, 20.0)]);
        assert_eq!(c.height, 40.0);
    }

    #[test]
    fn column_count_and_width() {
        // §3.4: a count alone divides the space; a width alone fits as many as it can.
        assert_eq!(column_geometry(300.0, 20.0, None, Some(3)), (3, 86.65625));
        assert_eq!(column_geometry(300.0, 20.0, Some(100.0), None), (2, 140.0));
        assert_eq!(column_geometry(50.0, 20.0, Some(100.0), None), (1, 50.0));
        assert_eq!(
            column_geometry(1000.0, 0.0, Some(100.0), Some(4)),
            (4, 250.0)
        );
    }

    fn unbreakable(top: f32, bottom: f32, before: bool) -> Piece {
        Piece::Unbreakable {
            top,
            bottom,
            before: if before {
                appeal::PERFECT
            } else {
                appeal::NONE
            },
        }
    }

    #[test]
    fn a_piece_crossing_a_column_end_starts_the_next_column() {
        let list = [
            unbreakable(0.0, 20.0, false),
            unbreakable(20.0, 40.0, true),
            unbreakable(90.0, 110.0, true),
            unbreakable(110.0, 130.0, true),
        ];
        let c = columns(&list, 0.0, 100.0);
        assert_eq!(c.starts, vec![0.0, 90.0]);
        assert_eq!(c.shortage, 10.0);
        // Without a valid break before it, it stays and overflows its column.
        let list = [
            unbreakable(0.0, 20.0, false),
            unbreakable(90.0, 110.0, false),
        ];
        let c = columns(&list, 0.0, 100.0);
        assert_eq!(c.starts, vec![0.0]);
        assert_eq!(c.shortage, 10.0);
        assert!(c.overflows);
        // Breakable content breaks at each column end; what is left at the last break is the
        // shortage.
        let c = columns(
            &[Piece::Breakable {
                top: 0.0,
                bottom: 250.0,
            }],
            0.0,
            100.0,
        );
        assert_eq!(c.starts, vec![0.0, 100.0, 200.0]);
        assert_eq!(c.shortage, 50.0);
        assert!(!c.overflows);
    }

    #[test]
    fn the_tallest_unbreakable_stretch_runs_until_a_valid_break() {
        let list = [
            unbreakable(0.0, 25.0, false),
            unbreakable(25.0, 75.0, false),
            unbreakable(75.0, 100.0, false),
            unbreakable(100.0, 110.0, true),
        ];
        assert_eq!(tallest_unbreakable(&list), 100.0);
    }

    /// The pieces of `node` relative to the container's border box: (x, y, height).
    fn pieces_in(
        f: &crate::tests::F,
        mc: snowlark_dom::NodeId,
        node: snowlark_dom::NodeId,
    ) -> Vec<(f32, f32, f32)> {
        let c = rect(f, mc);
        f.rects_of(node)
            .into_iter()
            .map(|r| (r.x - c.x, r.y - c.y, r.height))
            .collect()
    }

    /// A float is a flow of its own (CSS Fragmentation 3 §4.1): a float taller than the columns
    /// breaks across them, and balancing gives it room in every column.
    #[test]
    fn a_float_breaks_across_columns() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let fl = t.doc.block(mc, |s| {
            s.float = Float::Left;
            s.width = SizeValue::px(10.0);
            s.height = SizeValue::px(200.0);
        });
        let f = t.run(400.0, 400.0);
        assert_eq!(rect(&f, mc).height, 100.0);
        assert_eq!(
            pieces_in(&f, mc, fl),
            vec![(0.0, 0.0, 100.0), (110.0, 0.0, 100.0)]
        );
    }

    /// An absolutely positioned box whose containing block is in the flow is laid out against it as
    /// if it were whole, then broken across the columns from where it starts; balancing makes room
    /// for it although it takes none in the flow.
    #[test]
    fn an_absolute_box_breaks_from_where_it_starts() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let cb = t.doc.block(mc, |s| s.position = Position::Relative);
        t.doc.block(cb, |s| s.height = SizeValue::px(60.0));
        let abs = t.doc.block(cb, |s| {
            s.position = Position::Absolute;
            s.width = SizeValue::px(10.0);
            s.height = SizeValue::px(100.0);
        });
        let f = t.run(400.0, 400.0);
        // (60px of flow and 100px from 60: 160 over two columns)
        assert_eq!(rect(&f, mc).height, 80.0);
        assert_eq!(
            pieces_in(&f, mc, abs),
            vec![(0.0, 60.0, 20.0), (110.0, 0.0, 80.0)]
        );
    }

    /// Multi-column §8.1: a float that cannot break and would cross the column's end makes the
    /// columns taller, so that it fits in the column its top is in.
    #[test]
    fn balancing_makes_room_for_a_float_that_cannot_break() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        t.doc.block(mc, |s| s.height = SizeValue::px(60.0));
        let fl = t.doc.block(mc, |s| {
            s.float = Float::Left;
            s.width = SizeValue::px(100.0);
            s.height = SizeValue::px(40.0);
            s.size_containment = SizeContainment {
                width: true,
                height: true,
                ..Default::default()
            };
        });
        t.doc.block(mc, |s| {
            s.clear = Clear::Left;
            s.height = SizeValue::px(90.0);
        });
        let f = t.run(400.0, 400.0);
        // (190 over two columns is 95, where the float would cross the end by 5)
        assert_eq!(rect(&f, mc).height, 100.0);
        assert_eq!(pieces_in(&f, mc, fl), vec![(0.0, 60.0, 40.0)]);
    }

    /// A shrink-to-fit multi-column container is as wide as its content's max-content width for
    /// each column, plus the gaps, a percentage gap counting zero there (it then resolves against
    /// that width for the layout); a `column-width` wider than the content counts instead.
    #[test]
    fn intrinsic_width_is_the_content_for_each_column() {
        let mut t = T::new();
        let r0 = t.root();
        let a = t.doc.block(r0, |s| {
            s.float = Float::Left;
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::percent(25.0));
        });
        let x = t.doc.block(a, |_| {});
        t.doc.text(x, "aa");
        let b = t.doc.block(r0, |s| {
            s.float = Float::Left;
            s.column_count = Some(3);
            s.column_width = Some(30.0);
            s.column_gap = Some(LengthPercentage::px(10.0));
        });
        let y = t.doc.block(b, |_| {});
        t.doc.text(y, "aa");
        let f = t.run(400.0, 400.0);
        assert_eq!(rect(&f, a).width, 40.0);
        // (10px of gap in 40: columns of 15)
        assert_eq!(rect(&f, x).width, 15.0);
        assert_eq!(rect(&f, b).width, 110.0);
    }

    /// A table breaks between its rows (CSS Fragmentation 3 §4.4, class A), and its column boxes
    /// take a piece in each column the rows are in.
    #[test]
    fn a_table_breaks_between_its_rows() {
        use crate::tests::{cell, row, table};
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let tb = table(&mut t, mc, |_| {});
        let col = t
            .doc
            .element(tb, "col", |s| s.display = Display::TABLE_COLUMN);
        let cells: Vec<_> = (0..3)
            .map(|_| {
                let tr = row(&mut t, tb);
                let td = cell(&mut t, tr, |_| {});
                t.doc.block(td, |s| {
                    s.width = SizeValue::px(20.0);
                    s.height = SizeValue::px(40.0);
                    s.size_containment = SizeContainment {
                        width: true,
                        height: true,
                        ..Default::default()
                    };
                });
                td
            })
            .collect();
        let f = t.run(400.0, 400.0);
        // (120 over two columns is 60, where the second row would cross the end: it starts the
        // second column, which then needs 80)
        assert_eq!(rect(&f, mc).height, 80.0);
        assert_eq!(pieces_in(&f, mc, cells[2]), vec![(110.0, 0.0, 40.0)]);
        // (the table wrapper's pieces and the table's)
        assert_eq!(
            pieces_in(&f, mc, tb),
            vec![
                (0.0, 0.0, 80.0),
                (0.0, 0.0, 80.0),
                (110.0, 0.0, 40.0),
                (110.0, 0.0, 40.0)
            ]
        );
        assert_eq!(
            pieces_in(&f, mc, col),
            vec![(0.0, 0.0, 80.0), (110.0, 0.0, 40.0)]
        );
    }

    /// A row breaks where every cell can: between the lines of one cell where the other cell's
    /// unbreakable content has ended.
    #[test]
    fn a_table_row_breaks_where_all_its_cells_can() {
        use crate::tests::{cell, row, table};
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, two_columns);
        let tb = table(&mut t, mc, |_| {});
        let tr = row(&mut t, tb);
        let lines = cell(&mut t, tr, |s| {
            s.width = SizeValue::px(30.0);
            s.vertical_align = VerticalAlign::Top;
        });
        t.doc.text(lines, "aa bb cc dd");
        let other = cell(&mut t, tr, |s| s.vertical_align = VerticalAlign::Top);
        t.doc.block(other, |s| {
            s.width = SizeValue::px(20.0);
            s.height = SizeValue::px(25.0);
            s.size_containment = SizeContainment {
                width: true,
                height: true,
                ..Default::default()
            };
        });
        let f = t.run(400.0, 400.0);
        // (the block reaches into the third line, so the first break is before the fourth)
        assert_eq!(rect(&f, mc).height, 30.0);
        assert_eq!(
            pieces_in(&f, mc, lines),
            vec![(0.0, 0.0, 30.0), (110.0, 0.0, 10.0)]
        );
    }

    /// A nested multi-column container (two columns of 50, eight 20px blocks: 80px tall whole)
    /// after 40px in balanced outer columns (two of 100): balanced with the nested container
    /// breakable, the outer columns are 60px tall, so its first row is the 20px left in the first
    /// outer column (a block in each column) and its second the next outer column (three in each).
    #[test]
    fn a_nested_container_breaks_in_balanced_outer_columns() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(200.0);
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
        });
        t.doc.block(mc, |s| s.height = SizeValue::px(40.0));
        let inner = t.doc.block(mc, |s| {
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
        });
        let blocks: Vec<_> = (0..8)
            .map(|_| t.doc.block(inner, |s| s.height = SizeValue::px(20.0)))
            .collect();
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        assert_eq!(c.height, 60.0);
        let at: Vec<_> = blocks
            .iter()
            .map(|&b| {
                let r = rect(&f, b);
                (r.x - c.x, r.y - c.y)
            })
            .collect();
        assert_eq!(
            at,
            [
                (0.0, 40.0),
                (50.0, 40.0),
                (100.0, 0.0),
                (100.0, 20.0),
                (100.0, 40.0),
                (150.0, 0.0),
                (150.0, 20.0),
                (150.0, 40.0)
            ]
        );
    }

    /// Balanced outer columns that do not fit grow by the least shortage, a nested container's own
    /// columns' included: 20px, then a nested container (5px padding above and below, eleven
    /// monolithic 20px blocks in two columns). The first estimate, 75px, leaves its first row 50
    /// (two blocks a column) and needs a third row; at 80px its padding does not fit; at 85px its
    /// rows are 60 and 60 (three blocks, then three and two).
    #[test]
    fn balanced_outer_columns_grow_by_a_nested_containers_shortage() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(200.0);
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
        });
        t.doc.block(mc, |s| s.height = SizeValue::px(20.0));
        let inner = t.doc.block(mc, |s| {
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
            s.padding[0] = LengthPercentage::px(5.0);
            s.padding[2] = LengthPercentage::px(5.0);
        });
        let blocks: Vec<_> = (0..11)
            .map(|_| {
                t.doc.block(inner, |s| {
                    s.height = SizeValue::px(20.0);
                    s.size_containment = SizeContainment {
                        width: true,
                        height: true,
                        ..Default::default()
                    };
                })
            })
            .collect();
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        assert_eq!(c.height, 85.0);
        let at = |i: usize| {
            let r = rect(&f, blocks[i]);
            (r.x - c.x, r.y - c.y)
        };
        assert_eq!(
            [at(2), at(3), at(6), at(9), at(10)],
            [
                (0.0, 65.0),
                (50.0, 25.0),
                (100.0, 0.0),
                (150.0, 0.0),
                (150.0, 20.0)
            ]
        );
    }

    /// A nested container's later rows start at the top of their outer column: the block-start
    /// padding is only in its first piece (10px here, then two rows of two monolithic 20px blocks
    /// a column).
    #[test]
    fn a_nested_containers_later_rows_have_no_block_start_padding() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(200.0);
            s.height = SizeValue::px(60.0);
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
            s.column_fill_auto = true;
        });
        let inner = t.doc.block(mc, |s| {
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
            s.padding[0] = LengthPercentage::px(10.0);
        });
        let blocks: Vec<_> = (0..8)
            .map(|_| {
                t.doc.block(inner, |s| {
                    s.height = SizeValue::px(20.0);
                    s.size_containment = SizeContainment {
                        width: true,
                        height: true,
                        ..Default::default()
                    };
                })
            })
            .collect();
        let f = t.run(400.0, 400.0);
        let c = rect(&f, mc);
        let at = |i: usize| {
            let r = rect(&f, blocks[i]);
            (r.x - c.x, r.y - c.y)
        };
        assert_eq!(
            [at(0), at(2), at(4), at(6)],
            [(0.0, 10.0), (50.0, 10.0), (100.0, 0.0), (150.0, 0.0)]
        );
    }

    /// A nested multi-column container (two columns of 50) 50px down an outer column 100px tall
    /// (three columns 100 wide, 50 apart): its first row of columns takes the 50px the outer
    /// column leaves, the next ones a whole outer column each; the last row is balanced unless
    /// `column-fill: auto`, and an absolutely positioned box inside goes on in the next row's
    /// columns (`getclientrects-003`).
    fn nested(fill: bool) -> (crate::tests::F, [snowlark_dom::NodeId; 3]) {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(400.0);
            s.height = SizeValue::px(100.0);
            s.column_count = Some(3);
            s.column_gap = Some(LengthPercentage::px(50.0));
            s.column_fill_auto = true;
        });
        t.doc.block(mc, |s| s.height = SizeValue::px(50.0));
        let inner = t.doc.block(mc, |s| {
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
            s.column_fill_auto = fill;
        });
        let cb = t.doc.block(inner, |s| {
            s.position = Position::Relative;
            s.height = SizeValue::px(350.0);
        });
        t.doc.block(cb, |s| s.height = SizeValue::px(160.0));
        let abs = t.doc.block(cb, |s| {
            s.position = Position::Absolute;
            s.width = SizeValue::px(10.0);
            s.height = SizeValue::px(170.0);
        });
        (t.run(800.0, 400.0), [mc, cb, abs])
    }

    #[test]
    fn a_nested_container_takes_a_row_of_columns_in_each_outer_column() {
        let (f, [mc, cb, abs]) = nested(true);
        assert_eq!(
            pieces_in(&f, mc, cb),
            vec![
                (0.0, 50.0, 50.0),
                (50.0, 50.0, 50.0),
                (150.0, 0.0, 100.0),
                (200.0, 0.0, 100.0),
                (300.0, 0.0, 50.0),
            ]
        );
        assert_eq!(
            pieces_in(&f, mc, abs),
            vec![(150.0, 60.0, 40.0), (200.0, 0.0, 100.0), (300.0, 0.0, 30.0)]
        );
    }

    /// A monolithic block that would overflow the nested container's first row (30px, below 70px
    /// of the outer column) goes on in the next row, in the next outer column, where it fits
    /// (`nested-non-auto-inline-size-offset-top`).
    #[test]
    fn a_nested_first_row_leaves_what_would_overflow_it_to_the_next() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(600.0);
            s.height = SizeValue::px(100.0);
            s.column_count = Some(2);
            s.column_gap = Some(LengthPercentage::px(0.0));
            s.column_fill_auto = true;
        });
        t.doc.block(mc, |s| s.height = SizeValue::px(70.0));
        let inner = t.doc.block(mc, |s| {
            s.width = SizeValue::px(40.0);
            s.column_width = Some(20.0);
            s.column_gap = Some(LengthPercentage::px(0.0));
            s.column_fill_auto = true;
        });
        t.doc.block(inner, |s| s.height = SizeValue::px(20.0));
        let tall = t.doc.block(inner, |s| {
            s.height = SizeValue::px(50.0);
            s.size_containment = SizeContainment {
                width: true,
                height: true,
                ..Default::default()
            };
        });
        let f = t.run(800.0, 400.0);
        assert_eq!(pieces_in(&f, mc, tall), vec![(300.0, 0.0, 50.0)]);
    }

    /// With no room for its columns, a container's columns are 0 wide and step along by the gap
    /// (`subpixel-column-progression`).
    #[test]
    fn columns_with_no_room_step_by_the_gap() {
        let mut t = T::new();
        let r0 = t.root();
        let mc = t.doc.block(r0, |s| {
            s.width = SizeValue::px(0.0);
            s.height = SizeValue::px(1.0);
            s.column_count = Some(10);
            s.column_gap = Some(LengthPercentage::px(2.0));
            s.column_fill_auto = true;
        });
        t.doc.block(mc, |s| s.height = SizeValue::px(10.0));
        let after = t.doc.block(mc, |s| s.height = SizeValue::px(1.0));
        let f = t.run(400.0, 400.0);
        assert_eq!(pieces_in(&f, mc, after), vec![(20.0, 0.0, 1.0)]);
    }

    #[test]
    fn a_nested_containers_last_row_is_balanced() {
        let (f, [mc, cb, _]) = nested(false);
        assert_eq!(
            pieces_in(&f, mc, cb)[4..],
            [(300.0, 0.0, 25.0), (350.0, 0.0, 25.0)]
        );
    }
}
