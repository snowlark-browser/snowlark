// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Column width computation for the automatic table layout algorithm (css-tables-3 §6 "Computing
//! column measures", "Computing the table width", "Distributing width to columns"), on plain numbers
//! so that it is testable without a box tree.
//!
//! A column has a min-content and a max-content width, an optional percentage and whether any of its
//! cells has a specified (non-percentage) width. Cells spanning columns raise those the same way the
//! table's own width is distributed over the columns.

/// One column's constraints.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Column {
    pub min: f32,
    pub max: f32,
    /// A specified percentage as a fraction (0.5 is `50%`).
    pub percent: Option<f32>,
    /// Whether a cell or the column box gives it a specified width that is not a percentage.
    pub constrained: bool,
}

impl Column {
    /// Merges one cell's (or column box's) contribution into the column. A constrained
    /// contribution sets the column's max-content width and unconstrained ones no longer raise it
    /// (an unconstrained cell contributes only its min-content width to a constrained column).
    pub fn encompass(&mut self, min: f32, max: f32, constrained: bool, percent: Option<f32>) {
        self.min = self.min.max(min);
        if let Some(p) = percent {
            self.percent = Some(self.percent.map_or(p, |q| q.max(p)));
        }
        if constrained {
            self.max = if self.constrained {
                self.max.max(max)
            } else {
                max
            };
            self.constrained = true;
        } else if !self.constrained {
            self.max = self.max.max(max);
        }
        self.max = self.max.max(self.min);
    }

    /// Whether this column takes the excess width first: no percentage, no specified width.
    fn is_auto(&self) -> bool {
        self.percent.is_none() && !self.constrained
    }
}

/// The percentages of the columns clamped so that they sum to at most 100%, earlier columns first.
fn effective_percents(cols: &[Column]) -> Vec<Option<f32>> {
    let mut left = 1.0f32;
    cols.iter()
        .map(|c| {
            c.percent.map(|p| {
                let p = p.clamp(0.0, left);
                left -= p;
                p
            })
        })
        .collect()
}

/// The table grid's max-content width: the columns' max-content widths, raised so that percentage
/// columns get their percentage of it (css-tables-3 §6.3 "table max-content width": a percentage
/// column's max-content divided by its percentage, and the other columns' over what the percentages
/// leave).
pub(crate) fn grid_max(cols: &[Column]) -> f32 {
    let sum: f32 = cols.iter().map(|c| c.max).sum();
    let pct = effective_percents(cols);
    if pct.iter().all(Option::is_none) {
        return sum;
    }
    let mut max = sum;
    let mut total = 0.0f32;
    let mut others = 0.0f32;
    for (c, p) in cols.iter().zip(&pct) {
        match p {
            Some(p) if *p > 0.0 => {
                total += p;
                max = max.max(c.max / p);
            }
            Some(_) => {}
            None => others += c.max,
        }
    }
    if others > 0.0 && total < 1.0 {
        max = max.max(others / (1.0 - total));
    }
    max
}

/// Distributes `target` (the width of the columns without the spacing between them) over the
/// columns: the largest "sizing guess" not above it, and between two guesses the interpolation
/// between them; past the last guess, the excess to the auto columns (css-tables-3 §6.4).
pub(crate) fn distribute(cols: &[Column], target: f32) -> Vec<f32> {
    let n = cols.len();
    let min: Vec<f32> = cols.iter().map(|c| c.min).collect();
    if n == 0 || target <= min.iter().sum::<f32>() {
        return min;
    }
    let pct = effective_percents(cols);
    let pct_width = |i: usize| pct[i].map(|p| (p * target).max(cols[i].min));
    // min-content-percentage guess.
    let g1: Vec<f32> = (0..n)
        .map(|i| pct_width(i).unwrap_or(cols[i].min))
        .collect();
    // min-content-specified guess.
    let g2: Vec<f32> = (0..n)
        .map(|i| {
            pct_width(i).unwrap_or(if cols[i].constrained {
                cols[i].max
            } else {
                cols[i].min
            })
        })
        .collect();
    // max-content guess.
    let g3: Vec<f32> = (0..n)
        .map(|i| pct_width(i).map_or(cols[i].max, |w| w.max(cols[i].max)))
        .collect();
    // Guards the guesses' monotonicity.
    let mut guesses = vec![min];
    for g in [g1, g2, g3] {
        let prev = guesses.last().unwrap();
        let g: Vec<f32> = g.iter().zip(prev).map(|(a, b)| a.max(*b)).collect();
        guesses.push(g);
    }
    let sums: Vec<f32> = guesses.iter().map(|g| g.iter().sum()).collect();
    // The last guess not above the target.
    let k = (0..guesses.len())
        .rev()
        .find(|&k| sums[k] <= target)
        .unwrap_or(0);
    if k + 1 < guesses.len() {
        let span = sums[k + 1] - sums[k];
        let t = if span > 0.0 {
            (target - sums[k]) / span
        } else {
            0.0
        };
        return guesses[k]
            .iter()
            .zip(&guesses[k + 1])
            .map(|(a, b)| a + (b - a) * t)
            .collect();
    }
    let mut out = guesses[3].clone();
    let excess = target - sums[3];
    if excess <= 0.0 {
        return out;
    }
    // To the auto columns in proportion to their max-content widths, else to the columns with
    // specified widths, else to those with percentages, else to all.
    let pools: [Vec<usize>; 3] = [
        (0..n).filter(|&i| cols[i].is_auto()).collect(),
        (0..n)
            .filter(|&i| cols[i].percent.is_none() && cols[i].constrained)
            .collect(),
        (0..n).filter(|&i| cols[i].percent.is_some()).collect(),
    ];
    let pool = pools.iter().find(|p| !p.is_empty());
    let (pool, weights): (Vec<usize>, Vec<f32>) = match pool {
        Some(p) if std::ptr::eq(p, &pools[2]) => (
            p.clone(),
            p.iter().map(|&i| pct[i].unwrap_or(0.0)).collect(),
        ),
        Some(p) => (p.clone(), p.iter().map(|&i| cols[i].max).collect()),
        None => ((0..n).collect(), vec![0.0; n]),
    };
    let total: f32 = weights.iter().sum();
    for (j, &i) in pool.iter().enumerate() {
        let share = if total > 0.0 {
            weights[j] / total
        } else {
            1.0 / pool.len() as f32
        };
        out[i] += excess * share;
    }
    out
}

/// A cell spanning columns `range` with min-content width `min`, max-content width `max` (both
/// without the spacing between the columns, which the caller removes) and optional percentage:
/// raises the columns by distributing those over them (css-tables-3 §6.2 "spanning cells").
pub(crate) fn encompass_span(
    cols: &mut [Column],
    range: std::ops::Range<usize>,
    min: f32,
    max: f32,
    percent: Option<f32>,
) {
    if range.is_empty() {
        return;
    }
    let span = &mut cols[range];
    if let Some(p) = percent {
        let have: f32 = span.iter().filter_map(|c| c.percent).sum();
        if p > have {
            let free: Vec<usize> = (0..span.len())
                .filter(|&i| span[i].percent.is_none())
                .collect();
            let extra = p - have;
            if free.is_empty() {
                // Every column has one: the largest takes it (nothing to add).
            } else {
                let total: f32 = free.iter().map(|&i| span[i].max).sum();
                for &i in &free {
                    let share = if total > 0.0 {
                        span[i].max / total
                    } else {
                        1.0 / free.len() as f32
                    };
                    span[i].percent = Some(extra * share);
                }
            }
        }
    }
    let have_min: f32 = span.iter().map(|c| c.min).sum();
    if min > have_min {
        let widths = distribute(span, min);
        for (c, w) in span.iter_mut().zip(widths) {
            c.min = c.min.max(w);
            c.max = c.max.max(c.min);
        }
    }
    let have_max: f32 = span.iter().map(|c| c.max).sum();
    if max > have_max {
        // The extra max-content goes to the auto columns, else those with specified widths, else
        // those with percentages, in proportion to their max-content widths (equally when they
        // have none).
        let extra = max - have_max;
        let pools: [Vec<usize>; 3] = [
            (0..span.len()).filter(|&i| span[i].is_auto()).collect(),
            (0..span.len())
                .filter(|&i| span[i].percent.is_none() && span[i].constrained)
                .collect(),
            (0..span.len())
                .filter(|&i| span[i].percent.is_some())
                .collect(),
        ];
        let pool: Vec<usize> = pools
            .into_iter()
            .find(|p| !p.is_empty())
            .unwrap_or_else(|| (0..span.len()).collect());
        let total: f32 = pool.iter().map(|&i| span[i].max).sum();
        for &i in &pool {
            let share = if total > 0.0 {
                span[i].max / total
            } else {
                1.0 / pool.len() as f32
            };
            span[i].max += extra * share;
        }
    }
}

/// The width the fixed table layout algorithm (css-tables-3 §fixed-layout) takes for a column from
/// its column box, or else from a cell of the first row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FixedSpec {
    Auto,
    Length(f32),
    /// A fraction (0.5 is `50%`).
    Percent(f32),
}

/// The fixed table layout algorithm over `target`, the columns' share of the table (without spacing,
/// border and padding): lengths as specified, percentages of `target` (scaled to sum to at most
/// 100%), auto columns equally what is left. With no auto columns the excess grows the percentage
/// columns in proportion, else the non-zero lengths in proportion, else all columns equally; when
/// lengths and percentages together exceed `target`, the lengths stay and the percentages share
/// what they leave.
pub(crate) fn fixed_widths(specs: &[FixedSpec], target: f32) -> Vec<f32> {
    let n = specs.len();
    if n == 0 {
        return Vec::new();
    }
    let pct_total: f32 = specs
        .iter()
        .map(|s| match s {
            FixedSpec::Percent(p) => *p,
            _ => 0.0,
        })
        .sum();
    let scale = if pct_total > 1.0 {
        1.0 / pct_total
    } else {
        1.0
    };
    let lengths: f32 = specs
        .iter()
        .map(|s| match s {
            FixedSpec::Length(w) => *w,
            _ => 0.0,
        })
        .sum();
    let mut out: Vec<f32> = specs
        .iter()
        .map(|s| match s {
            FixedSpec::Length(w) => *w,
            FixedSpec::Percent(p) => p * scale * target,
            FixedSpec::Auto => 0.0,
        })
        .collect();
    let pct_px: f32 = specs
        .iter()
        .zip(&out)
        .filter(|(s, _)| matches!(s, FixedSpec::Percent(_)))
        .map(|(_, w)| *w)
        .sum();
    let autos: Vec<usize> = (0..n).filter(|&i| specs[i] == FixedSpec::Auto).collect();
    let used = lengths + pct_px;
    if used > target {
        // Overconstrained: the percentage columns take what the lengths leave.
        let avail = (target - lengths).max(0.0);
        if pct_px > 0.0 {
            for (i, s) in specs.iter().enumerate() {
                if matches!(s, FixedSpec::Percent(_)) {
                    out[i] *= avail / pct_px;
                }
            }
        }
    } else if !autos.is_empty() {
        let each = (target - used) / autos.len() as f32;
        for &i in &autos {
            out[i] = each;
        }
    } else if used < target {
        let excess = target - used;
        let pcts: Vec<usize> = (0..n)
            .filter(|&i| matches!(specs[i], FixedSpec::Percent(_)))
            .collect();
        let nonzero: Vec<usize> = (0..n).filter(|&i| out[i] > 0.0).collect();
        let pool = if !pcts.is_empty() && pct_px > 0.0 {
            pcts
        } else if !nonzero.is_empty() {
            nonzero
        } else {
            (0..n).collect()
        };
        let total: f32 = pool.iter().map(|&i| out[i]).sum();
        for &i in &pool {
            let share = if total > 0.0 {
                out[i] / total
            } else {
                1.0 / pool.len() as f32
            };
            out[i] += excess * share;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(min: f32, max: f32) -> Column {
        Column {
            min,
            max,
            ..Column::default()
        }
    }

    #[test]
    fn unconstrained_cells_do_not_raise_a_constrained_columns_max() {
        let mut c = Column::default();
        c.encompass(20.0, 50.0, true, None);
        c.encompass(40.0, 80.0, false, None);
        assert_eq!((c.min, c.max), (40.0, 50.0));
    }

    #[test]
    fn a_percentage_column_widens_the_grid() {
        // 80% column with max 100 and an auto column of 30: 150 in total.
        let cols = [
            Column {
                percent: Some(0.8),
                ..col(100.0, 100.0)
            },
            col(30.0, 30.0),
        ];
        assert!((grid_max(&cols) - 150.0).abs() < 0.01);
        let w = distribute(&cols, 150.0);
        assert!(
            (w[0] - 120.0).abs() < 0.01 && (w[1] - 30.0).abs() < 0.01,
            "{w:?}"
        );
    }

    #[test]
    fn between_min_and_max_the_columns_interpolate() {
        let cols = [col(10.0, 30.0), col(10.0, 70.0)];
        let w = distribute(&cols, 60.0);
        assert!(
            (w[0] - 20.0).abs() < 0.01 && (w[1] - 40.0).abs() < 0.01,
            "{w:?}"
        );
    }

    #[test]
    fn excess_goes_to_auto_columns_by_max_content() {
        let cols = [
            Column {
                constrained: true,
                ..col(0.0, 50.0)
            },
            col(0.0, 10.0),
            col(0.0, 30.0),
        ];
        let w = distribute(&cols, 130.0);
        assert!((w[0] - 50.0).abs() < 0.01, "{w:?}");
        assert!(
            (w[1] - 20.0).abs() < 0.01 && (w[2] - 60.0).abs() < 0.01,
            "{w:?}"
        );
    }

    #[test]
    fn fixed_layout_follows_the_lengths_then_percentages_then_auto() {
        use FixedSpec::*;
        let w = fixed_widths(&[Percent(0.5), Length(30.0), Auto], 100.0);
        assert_eq!(w, vec![50.0, 30.0, 20.0]);
        // Overconstrained: the lengths stay, the percentages share the rest by their sizes.
        let w = fixed_widths(&[Percent(0.2), Percent(0.6), Length(60.0)], 100.0);
        assert!(
            (w[0] - 10.0).abs() < 0.01 && (w[1] - 30.0).abs() < 0.01 && w[2] == 60.0,
            "{w:?}"
        );
        // Percentages over 100% are scaled.
        let w = fixed_widths(&[Percent(2.0), Percent(3.0), Percent(5.0)], 100.0);
        assert!(
            (w[0] - 20.0).abs() < 0.01 && (w[2] - 50.0).abs() < 0.01,
            "{w:?}"
        );
        // Zero lengths alone share evenly; next to others they do not grow.
        assert_eq!(
            fixed_widths(&[Length(0.0), Length(0.0)], 100.0),
            vec![50.0, 50.0]
        );
        assert_eq!(
            fixed_widths(&[Length(0.0), Length(50.0)], 100.0),
            vec![0.0, 100.0]
        );
        assert_eq!(fixed_widths(&[Length(0.0), Auto], 100.0), vec![0.0, 100.0]);
    }

    #[test]
    fn a_spanning_cell_raises_the_columns_it_covers() {
        let mut cols = vec![col(10.0, 10.0), col(10.0, 30.0)];
        encompass_span(&mut cols, 0..2, 60.0, 60.0, None);
        assert!(cols[0].min + cols[1].min >= 59.99);
    }
}
