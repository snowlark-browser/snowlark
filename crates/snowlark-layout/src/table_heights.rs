// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Distribution of a table's extra block size over its row groups and their rows (css-tables-3
//! §row-layout "distribute extra height"), on plain numbers.
//!
//! The extra height of a table taller than its rows goes first to row groups with a percentage
//! height (up to it); then to the groups whose height is not specified and that have height, in
//! proportion to it; then to empty ones (bodies before headers and footers); then to the groups with
//! a specified height (bodies first). A group's share goes to its rows the same way.

/// A run of rows: a row group, or a row outside any group.
#[derive(Clone, Debug)]
pub(crate) struct Section {
    pub first: usize,
    pub end: usize,
    /// A header or footer group (the last to take extra height).
    pub header_footer: bool,
    /// A `height` that is a length.
    pub length: Option<f32>,
    /// A `height` that is a percentage (a fraction).
    pub percent: Option<f32>,
    /// Its height is specified: its own `height`, a row's or a cell's is a length.
    pub constrained: bool,
}

/// The height of rows `first..end` that are shown: their heights and the spacing between them.
pub(crate) fn section_height(row_h: &[f32], collapsed: &[bool], s: &Section, spacing: f32) -> f32 {
    let shown: Vec<usize> = (s.first..s.end).filter(|&r| !collapsed[r]).collect();
    shown.iter().map(|&r| row_h[r]).sum::<f32>() + spacing * shown.len().saturating_sub(1) as f32
}

/// Grows the shown rows of `s` by `extra` in all: the rows with no specified height (`constrained`
/// says which have one) that have height, in proportion to it; else the other unconstrained rows
/// equally; else all of them in proportion.
pub(crate) fn grow_rows(
    row_h: &mut [f32],
    collapsed: &[bool],
    constrained: &[bool],
    s: &Section,
    extra: f32,
) {
    if extra <= 0.0 {
        return;
    }
    let shown: Vec<usize> = (s.first..s.end).filter(|&r| !collapsed[r]).collect();
    let free: Vec<usize> = shown.iter().copied().filter(|&r| !constrained[r]).collect();
    let tall: Vec<usize> = free.iter().copied().filter(|&r| row_h[r] > 0.0).collect();
    let pool = if !tall.is_empty() {
        tall
    } else if !free.is_empty() {
        free
    } else {
        shown
    };
    if pool.is_empty() {
        return;
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

/// How much of `extra` each section takes, given their current heights `heights`; percentages are
/// of `basis` (the height of the rows area).
pub(crate) fn distribute_extra(
    sections: &[Section],
    heights: &[f32],
    extra: f32,
    basis: f32,
) -> Vec<f32> {
    let mut add = vec![0.0f32; sections.len()];
    let mut left = extra;
    for (i, s) in sections.iter().enumerate() {
        if let Some(p) = s.percent {
            let a = (p * basis - heights[i]).clamp(0.0, left);
            add[i] = a;
            left -= a;
        }
    }
    if left <= 0.0 {
        return add;
    }
    type Pool = Box<dyn Fn(&Section, f32) -> bool>;
    let pools: [Pool; 5] = [
        Box::new(|s, h| s.percent.is_none() && !s.constrained && h > 0.0),
        Box::new(|s, h| s.percent.is_none() && !s.constrained && h <= 0.0 && !s.header_footer),
        Box::new(|s, h| s.percent.is_none() && !s.constrained && h <= 0.0),
        Box::new(|s, _| s.percent.is_none() && s.constrained && !s.header_footer),
        Box::new(|s, _| s.percent.is_none() && s.constrained),
    ];
    for pool in &pools {
        let members: Vec<usize> = (0..sections.len())
            .filter(|&i| pool(&sections[i], heights[i]))
            .collect();
        if members.is_empty() {
            continue;
        }
        let total: f32 = members.iter().map(|&i| heights[i]).sum();
        for &i in &members {
            let share = if total > 0.0 {
                heights[i] / total
            } else {
                1.0 / members.len() as f32
            };
            add[i] += left * share;
        }
        return add;
    }
    // Only percentage sections: the first takes the rest.
    if let Some(i) = (0..sections.len()).find(|&i| sections[i].percent.is_some()) {
        add[i] += left;
    }
    add
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(first: usize, hf: bool, constrained: bool, percent: Option<f32>) -> Section {
        Section {
            first,
            end: first + 1,
            header_footer: hf,
            length: constrained.then_some(10.0),
            percent,
            constrained,
        }
    }

    #[test]
    fn empty_bodies_take_extra_before_empty_headers() {
        let s = [sec(0, true, false, None), sec(1, false, false, None)];
        assert_eq!(
            distribute_extra(&s, &[0.0, 0.0], 100.0, 100.0),
            vec![0.0, 100.0]
        );
    }

    #[test]
    fn sections_with_height_share_in_proportion_whatever_their_kind() {
        let s = [sec(0, true, false, None), sec(1, false, false, None)];
        assert_eq!(
            distribute_extra(&s, &[10.0, 10.0], 80.0, 100.0),
            vec![40.0, 40.0]
        );
    }

    #[test]
    fn a_percentage_section_takes_its_share_then_auto_gets_the_rest() {
        let s = [sec(0, false, false, Some(0.4)), sec(1, false, false, None)];
        let a = distribute_extra(&s, &[0.0, 0.0], 100.0, 100.0);
        assert_eq!(a, vec![40.0, 60.0]);
    }

    #[test]
    fn specified_sections_take_extra_only_without_free_ones() {
        let s = [sec(0, false, true, None), sec(1, false, false, None)];
        assert_eq!(
            distribute_extra(&s, &[40.0, 0.0], 60.0, 100.0),
            vec![0.0, 60.0]
        );
        let s = [sec(0, true, true, None), sec(1, false, true, None)];
        assert_eq!(
            distribute_extra(&s, &[20.0, 30.0], 50.0, 100.0),
            vec![0.0, 50.0]
        );
    }

    #[test]
    fn rows_grow_in_proportion_free_ones_first() {
        let mut h = [10.0, 30.0, 5.0];
        let s = Section {
            first: 0,
            end: 3,
            header_footer: false,
            length: None,
            percent: None,
            constrained: false,
        };
        grow_rows(&mut h, &[false; 3], &[false, false, true], &s, 40.0);
        assert_eq!(h, [20.0, 60.0, 5.0]);
    }
}
