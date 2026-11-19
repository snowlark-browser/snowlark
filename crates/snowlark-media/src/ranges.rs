// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Time ranges (HTML §4.8.11.7): what `buffered`, `seekable` and `played` hold, normalized.

/// A normalized set of time ranges (seconds): sorted, non-empty, neither overlapping nor touching.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ranges(Vec<(f64, f64)>);

impl Ranges {
    pub fn new() -> Ranges {
        Ranges(Vec::new())
    }

    /// The normalized union of `ranges` (pairs with `end <= start` or NaN are dropped).
    pub fn from_pairs(ranges: impl IntoIterator<Item = (f64, f64)>) -> Ranges {
        let mut r = Ranges::new();
        for (s, e) in ranges {
            r.add(s, e);
        }
        r
    }

    /// One range.
    pub fn single(start: f64, end: f64) -> Ranges {
        Ranges::from_pairs([(start, end)])
    }

    pub fn as_slice(&self) -> &[(f64, f64)] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn start(&self, i: usize) -> Option<f64> {
        self.0.get(i).map(|r| r.0)
    }

    pub fn end(&self, i: usize) -> Option<f64> {
        self.0.get(i).map(|r| r.1)
    }

    /// Adds `[start, end)`, merging it with the ranges it overlaps or touches.
    pub fn add(&mut self, start: f64, end: f64) {
        if start.is_nan() || end.is_nan() || end <= start {
            return;
        }
        let mut s = start;
        let mut e = end;
        let mut out = Vec::with_capacity(self.0.len() + 1);
        let mut placed = false;
        for &(a, b) in &self.0 {
            if b < s {
                out.push((a, b));
            } else if a > e {
                if !placed {
                    out.push((s, e));
                    placed = true;
                }
                out.push((a, b));
            } else {
                s = s.min(a);
                e = e.max(b);
            }
        }
        if !placed {
            out.push((s, e));
        }
        self.0 = out;
    }

    /// The intersection with another set (MSE's "active source buffers' buffered ranges").
    pub fn intersect(&self, other: &Ranges) -> Ranges {
        let mut out = Ranges::new();
        for &(a, b) in &self.0 {
            for &(c, d) in &other.0 {
                let s = a.max(c);
                let e = b.min(d);
                if e > s {
                    out.0.push((s, e));
                }
            }
        }
        out.0.sort_by(|x, y| x.0.total_cmp(&y.0));
        Ranges::from_pairs(out.0)
    }

    /// Whether `t` is in a range (ends included).
    pub fn contains(&self, t: f64) -> bool {
        self.0.iter().any(|&(a, b)| a <= t && t <= b)
    }

    /// The end of the range holding `t`, if any.
    pub fn end_of_range_containing(&self, t: f64) -> Option<f64> {
        self.0.iter().find(|&&(a, b)| a <= t && t <= b).map(|r| r.1)
    }

    /// The position in the ranges nearest to `t` (HTML's seek step 11: "the nearest position in
    /// the seekable ranges", the earlier one on a tie).
    pub fn nearest(&self, t: f64) -> Option<f64> {
        let mut best: Option<(f64, f64)> = None;
        for &(a, b) in &self.0 {
            let p = t.clamp(a, b);
            let d = (p - t).abs();
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, p));
            }
        }
        best.map(|b| b.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        let r = Ranges::from_pairs([(5.0, 6.0), (0.0, 1.0), (1.0, 2.0), (3.0, 4.5), (4.0, 5.0)]);
        assert_eq!(r.as_slice(), &[(0.0, 2.0), (3.0, 6.0)]);
        let r = Ranges::from_pairs([(2.0, 1.0), (f64::NAN, 3.0), (0.0, 0.0)]);
        assert!(r.is_empty());
    }

    #[test]
    fn intersects_and_finds() {
        let a = Ranges::from_pairs([(0.0, 5.0), (8.0, 10.0)]);
        let b = Ranges::from_pairs([(3.0, 9.0)]);
        assert_eq!(a.intersect(&b).as_slice(), &[(3.0, 5.0), (8.0, 9.0)]);
        assert_eq!(a.nearest(6.0), Some(5.0));
        assert_eq!(a.nearest(7.5), Some(8.0));
        assert_eq!(a.nearest(6.5), Some(5.0));
        assert_eq!(a.end_of_range_containing(9.0), Some(10.0));
        assert!(a.contains(5.0) && !a.contains(6.0));
    }
}
