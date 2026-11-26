// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filter functions as display-list data (Filter Effects 1 §10): what a `filter` list's functions
//! are, as a rasterizer applies them to a group's content, and how far each moves pixels.
//!
//! A filter is a run of [`FilterOp`]s in a display list's side vector ([`crate::DisplayList::filters`]),
//! referred to by a [`FilterRange`] from the group item ([`crate::Item::PushFilter`]); each function
//! takes the previous one's result (the first the group's content) and the last one's result is what
//! the group shows. Lengths are in the list's coordinates where the group is pushed (CSS px); the
//! rasterizer scales them to device pixels. The functions operate on non-premultiplied sRGB colors
//! (§10: the shorthand functions are defined with `color-interpolation-filters: sRGB`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::geom::{Color, Point, Rect};
use crate::svg_filter::SvgFilter;

/// One filter function of Filter Effects 1 §10, or a reference to an SVG filter (§5).
#[derive(Clone, Debug, PartialEq)]
pub enum FilterOp {
    /// `blur(<length>)`: a Gaussian blur with this standard deviation (CSS px) in both axes.
    Blur(f32),
    /// `brightness(<number>)`: 0 black, 1 unchanged, above 1 brighter.
    Brightness(f32),
    /// `contrast(<number>)`: 0 gray, 1 unchanged.
    Contrast(f32),
    /// `grayscale(<number>)`: 0 unchanged, 1 fully gray.
    Grayscale(f32),
    /// `hue-rotate(<angle>)`: degrees.
    HueRotate(f32),
    /// `invert(<number>)`: 0 unchanged, 1 inverted.
    Invert(f32),
    /// `opacity(<number>)`: 0 transparent, 1 unchanged.
    Opacity(f32),
    /// `saturate(<number>)`: 0 unsaturated, 1 unchanged, above 1 more saturated.
    Saturate(f32),
    /// `sepia(<number>)`: 0 unchanged, 1 fully sepia.
    Sepia(f32),
    /// `drop-shadow()`: the content's alpha, blurred by `sigma` (a standard deviation, CSS px),
    /// in `color`, drawn `offset` away under the content.
    DropShadow {
        offset: Point,
        sigma: f32,
        color: Color,
    },
    /// `url()` naming a `<filter>` element: its primitives, resolved for the filtered box.
    Svg(Arc<SvgFilter>),
}

/// A run of [`FilterOp`]s in [`crate::DisplayList::filters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FilterRange {
    pub start: u32,
    pub len: u32,
}

/// The standard deviations a rasterizer applies at most, in device pixels (a blur wider than this
/// looks the same and costs the frame).
pub const MAX_SIGMA: f32 = 250.0;

impl FilterOp {
    /// Whether the values are finite and in the function's range (the amounts as the computed
    /// value clamps them: `grayscale`, `invert`, `opacity` and `sepia` within 0..=1, the others
    /// non-negative).
    pub fn is_valid(&self) -> bool {
        match *self {
            FilterOp::Blur(s) => s.is_finite() && s >= 0.0,
            FilterOp::Brightness(a) | FilterOp::Contrast(a) | FilterOp::Saturate(a) => {
                a.is_finite() && a >= 0.0
            }
            FilterOp::Grayscale(a)
            | FilterOp::Invert(a)
            | FilterOp::Opacity(a)
            | FilterOp::Sepia(a) => a.is_finite() && (0.0..=1.0).contains(&a),
            FilterOp::HueRotate(d) => d.is_finite(),
            FilterOp::DropShadow { offset, sigma, .. } => {
                offset.x.is_finite() && offset.y.is_finite() && sigma.is_finite() && sigma >= 0.0
            }
            FilterOp::Svg(ref f) => f.is_valid(),
        }
    }

    /// The images the function draws (an SVG filter's `feImage`s).
    pub fn images(&self) -> impl Iterator<Item = crate::ImageId> + '_ {
        let svg = match self {
            FilterOp::Svg(f) => Some(f.images()),
            _ => None,
        };
        svg.into_iter().flatten()
    }

    /// Whether the function reads pixels away from the one it writes (a blur or a drop shadow):
    /// its result reaches outside its input.
    pub fn moves_pixels(&self) -> bool {
        matches!(
            self,
            FilterOp::Blur(_) | FilterOp::DropShadow { .. } | FilterOp::Svg(_)
        )
    }

    /// Where the function's result of content within `r` can show (`r` for a function that
    /// changes colors only).
    pub fn map_rect(&self, r: Rect) -> Rect {
        match *self {
            FilterOp::Blur(sigma) => {
                let o = blur_outset(sigma);
                r.inflate(o, o)
            }
            FilterOp::DropShadow { offset, sigma, .. } => {
                let o = blur_outset(sigma);
                r.union(&r.translate(offset.x, offset.y).inflate(o, o))
            }
            FilterOp::Svg(ref f) => f.map_rect(r),
            _ => r,
        }
    }

    /// The function as a color matrix on non-premultiplied `[r, g, b, a, 1]` (Filter Effects 1
    /// §10's `feColorMatrix` and `feComponentTransfer` equivalents), by rows (each row the
    /// coefficients of one output channel, the last its offset); `None` for a function that
    /// moves pixels.
    pub fn color_matrix(&self) -> Option<[[f32; 5]; 4]> {
        let mut m = [
            [1.0, 0.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0, 0.0],
        ];
        match *self {
            FilterOp::Blur(_) | FilterOp::DropShadow { .. } | FilterOp::Svg(_) => return None,
            FilterOp::Brightness(a) => {
                for row in m.iter_mut().take(3) {
                    for v in row.iter_mut().take(3) {
                        *v *= a;
                    }
                }
            }
            FilterOp::Contrast(a) => {
                // (a linear transfer with slope `a` and intercept `-0.5a + 0.5`)
                for row in m.iter_mut().take(3) {
                    for v in row.iter_mut().take(3) {
                        *v *= a;
                    }
                    row[4] = -0.5 * a + 0.5;
                }
            }
            FilterOp::Invert(a) => {
                // (a table transfer from `a` to `1 - a`: `a + (1 - 2a) * c`)
                for row in m.iter_mut().take(3) {
                    for v in row.iter_mut().take(3) {
                        *v *= 1.0 - 2.0 * a;
                    }
                    row[4] = a;
                }
            }
            FilterOp::Opacity(a) => m[3][3] = a,
            FilterOp::Grayscale(a) => {
                let v = (1.0 - a).clamp(0.0, 1.0);
                m[0][..3].copy_from_slice(&[
                    0.2126 + 0.7874 * v,
                    0.7152 - 0.7152 * v,
                    0.0722 - 0.0722 * v,
                ]);
                m[1][..3].copy_from_slice(&[
                    0.2126 - 0.2126 * v,
                    0.7152 + 0.2848 * v,
                    0.0722 - 0.0722 * v,
                ]);
                m[2][..3].copy_from_slice(&[
                    0.2126 - 0.2126 * v,
                    0.7152 - 0.7152 * v,
                    0.0722 + 0.9278 * v,
                ]);
            }
            FilterOp::Sepia(a) => {
                let v = (1.0 - a).clamp(0.0, 1.0);
                m[0][..3].copy_from_slice(&[
                    0.393 + 0.607 * v,
                    0.769 - 0.769 * v,
                    0.189 - 0.189 * v,
                ]);
                m[1][..3].copy_from_slice(&[
                    0.349 - 0.349 * v,
                    0.686 + 0.314 * v,
                    0.168 - 0.168 * v,
                ]);
                m[2][..3].copy_from_slice(&[
                    0.272 - 0.272 * v,
                    0.534 - 0.534 * v,
                    0.131 + 0.869 * v,
                ]);
            }
            FilterOp::Saturate(s) => {
                m[0][..3].copy_from_slice(&[
                    0.213 + 0.787 * s,
                    0.715 - 0.715 * s,
                    0.072 - 0.072 * s,
                ]);
                m[1][..3].copy_from_slice(&[
                    0.213 - 0.213 * s,
                    0.715 + 0.285 * s,
                    0.072 - 0.072 * s,
                ]);
                m[2][..3].copy_from_slice(&[
                    0.213 - 0.213 * s,
                    0.715 - 0.715 * s,
                    0.072 + 0.928 * s,
                ]);
            }
            FilterOp::HueRotate(degrees) => {
                let (sin, cos) = degrees.to_radians().sin_cos();
                m[0][..3].copy_from_slice(&[
                    0.213 + cos * 0.787 - sin * 0.213,
                    0.715 - cos * 0.715 - sin * 0.715,
                    0.072 - cos * 0.072 + sin * 0.928,
                ]);
                m[1][..3].copy_from_slice(&[
                    0.213 - cos * 0.213 + sin * 0.143,
                    0.715 + cos * 0.285 + sin * 0.140,
                    0.072 - cos * 0.072 - sin * 0.283,
                ]);
                m[2][..3].copy_from_slice(&[
                    0.213 - cos * 0.213 - sin * 0.787,
                    0.715 - cos * 0.715 + sin * 0.715,
                    0.072 + cos * 0.928 + sin * 0.072,
                ]);
            }
        }
        Some(m)
    }
}

/// How far a Gaussian blur of standard deviation `sigma` reaches past its input: three standard
/// deviations hold 99.7% of the kernel.
pub fn blur_outset(sigma: f32) -> f32 {
    if sigma > 0.0 {
        (3.0 * sigma).ceil()
    } else {
        0.0
    }
}

/// Where a filter list's result of content within `r` can show (the functions applied in order).
pub fn map_rect(ops: &[FilterOp], r: Rect) -> Rect {
    ops.iter().fold(r, |r, op| op.map_rect(r))
}

/// Where the content whose result through the list can land within `r` lies (the reverse of
/// [`map_rect`]: what a rasterizer must paint of the content to show `r` right).
pub fn source_rect(ops: &[FilterOp], r: Rect) -> Rect {
    ops.iter().rev().fold(r, |r, op| match *op {
        FilterOp::DropShadow { offset, sigma, .. } => {
            let o = blur_outset(sigma);
            r.union(&r.translate(-offset.x, -offset.y).inflate(o, o))
        }
        // (an SVG filter can move any of its input within its region to any other place)
        FilterOp::Svg(ref f) => f.map_rect(r),
        _ => op.map_rect(r),
    })
}

/// Whether any function of the list moves pixels.
pub fn moves_pixels(ops: &[FilterOp]) -> bool {
    ops.iter().any(FilterOp::moves_pixels)
}

/// A filter function that changes colors only (no pixel moves): what a compositor can apply to a
/// layer's tiles one pixel at a time, and interpolate on its own (a composited filter animation).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ColorFilter {
    Brightness(f32),
    Contrast(f32),
    Grayscale(f32),
    HueRotate(f32),
    Invert(f32),
    Opacity(f32),
    Saturate(f32),
    Sepia(f32),
}

impl ColorFilter {
    /// The function of `op`, if it changes colors only.
    pub fn of(op: &FilterOp) -> Option<ColorFilter> {
        Some(match *op {
            FilterOp::Brightness(a) => ColorFilter::Brightness(a),
            FilterOp::Contrast(a) => ColorFilter::Contrast(a),
            FilterOp::Grayscale(a) => ColorFilter::Grayscale(a),
            FilterOp::HueRotate(a) => ColorFilter::HueRotate(a),
            FilterOp::Invert(a) => ColorFilter::Invert(a),
            FilterOp::Opacity(a) => ColorFilter::Opacity(a),
            FilterOp::Saturate(a) => ColorFilter::Saturate(a),
            FilterOp::Sepia(a) => ColorFilter::Sepia(a),
            FilterOp::Blur(_) | FilterOp::DropShadow { .. } | FilterOp::Svg(_) => return None,
        })
    }

    pub fn op(self) -> FilterOp {
        match self {
            ColorFilter::Brightness(a) => FilterOp::Brightness(a),
            ColorFilter::Contrast(a) => FilterOp::Contrast(a),
            ColorFilter::Grayscale(a) => FilterOp::Grayscale(a),
            ColorFilter::HueRotate(a) => FilterOp::HueRotate(a),
            ColorFilter::Invert(a) => FilterOp::Invert(a),
            ColorFilter::Opacity(a) => FilterOp::Opacity(a),
            ColorFilter::Saturate(a) => FilterOp::Saturate(a),
            ColorFilter::Sepia(a) => FilterOp::Sepia(a),
        }
    }

    /// The function's kind (0 to 7, in the order of the variants) and amount.
    fn parts(self) -> (u8, f32) {
        match self {
            ColorFilter::Brightness(a) => (0, a),
            ColorFilter::Contrast(a) => (1, a),
            ColorFilter::Grayscale(a) => (2, a),
            ColorFilter::HueRotate(a) => (3, a),
            ColorFilter::Invert(a) => (4, a),
            ColorFilter::Opacity(a) => (5, a),
            ColorFilter::Saturate(a) => (6, a),
            ColorFilter::Sepia(a) => (7, a),
        }
    }

    /// The function of kind `kind` with the amount clamped to its range (Filter Effects 1 §10:
    /// `grayscale`, `invert`, `opacity` and `sepia` within 0..=1, the others non-negative, the
    /// angle any).
    fn with(kind: u8, a: f32) -> ColorFilter {
        let unit = a.clamp(0.0, 1.0);
        let positive = a.max(0.0);
        match kind {
            0 => ColorFilter::Brightness(positive),
            1 => ColorFilter::Contrast(positive),
            2 => ColorFilter::Grayscale(unit),
            3 => ColorFilter::HueRotate(a),
            4 => ColorFilter::Invert(unit),
            5 => ColorFilter::Opacity(unit),
            6 => ColorFilter::Saturate(positive),
            _ => ColorFilter::Sepia(unit),
        }
    }

    /// The function's initial value for interpolation (Filter Effects 1 §14: 1 for brightness,
    /// contrast, opacity and saturate, 0 for the others): what it is in the list that lacks it.
    fn initial(self) -> ColorFilter {
        let (kind, _) = self.parts();
        ColorFilter::with(
            kind,
            if matches!(kind, 0 | 1 | 5 | 6) {
                1.0
            } else {
                0.0
            },
        )
    }

    pub fn is_valid(self) -> bool {
        self.op().is_valid()
    }
}

/// Up to [`ColorFilters::MAX`] color functions, in order: a compositor's filter state step and a
/// composited filter animation's keyframe value. Fixed in size, so that the state step is `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorFilters {
    fns: [ColorFilter; ColorFilters::MAX],
    len: u8,
}

impl Default for ColorFilters {
    fn default() -> Self {
        ColorFilters {
            fns: [ColorFilter::Opacity(1.0); ColorFilters::MAX],
            len: 0,
        }
    }
}

impl ColorFilters {
    /// The most functions a composited filter has (a longer list stays with the page).
    pub const MAX: usize = 8;

    /// `ops` as color functions: `None` when one moves pixels or there are more than [`Self::MAX`].
    pub fn of(ops: &[FilterOp]) -> Option<ColorFilters> {
        if ops.len() > Self::MAX {
            return None;
        }
        let mut out = ColorFilters::default();
        for (i, op) in ops.iter().enumerate() {
            out.fns[i] = ColorFilter::of(op)?;
        }
        out.len = ops.len() as u8;
        Some(out)
    }

    pub fn as_slice(&self) -> &[ColorFilter] {
        &self.fns[..(self.len as usize).min(Self::MAX)]
    }

    pub fn ops(&self) -> Vec<FilterOp> {
        self.as_slice().iter().map(|f| f.op()).collect()
    }

    /// The list with each amount clamped to its function's range (Filter Effects 1 §10).
    pub fn clamped(&self) -> ColorFilters {
        ColorFilters::blend(self, self, 0.0)
    }

    /// Whether the length is in range and each function's amount finite and in its range.
    pub fn is_valid(&self) -> bool {
        (self.len as usize) <= Self::MAX && self.as_slice().iter().all(|f| f.is_valid())
    }

    /// Whether `a` and `b` interpolate function by function (Filter Effects 1 §14): the shorter
    /// list (`none` is empty) is a prefix of the longer one by function kind.
    pub fn interpolate(a: &ColorFilters, b: &ColorFilters) -> bool {
        a.as_slice()
            .iter()
            .zip(b.as_slice())
            .all(|(x, y)| x.parts().0 == y.parts().0)
    }

    /// `a` to `b` at `t` (any real number; each amount clamped to its range) for lists that
    /// [`ColorFilters::interpolate`]: function by function, a function the shorter list lacks taken
    /// at its initial value.
    pub fn blend(a: &ColorFilters, b: &ColorFilters, t: f64) -> ColorFilters {
        let (a, b) = (a.as_slice(), b.as_slice());
        let n = a.len().max(b.len());
        let mut out = ColorFilters {
            len: n as u8,
            ..ColorFilters::default()
        };
        for i in 0..n {
            let (x, y) = match (a.get(i), b.get(i)) {
                (Some(x), Some(y)) => (*x, *y),
                (Some(x), None) => (*x, x.initial()),
                (None, Some(y)) => (y.initial(), *y),
                (None, None) => unreachable!(),
            };
            let ((kind, from), (_, to)) = (x.parts(), y.parts());
            let v = from as f64 + (to as f64 - from as f64) * t;
            out.fns[i] = ColorFilter::with(kind, v as f32);
        }
        out
    }
}

/// Applies a color matrix to a non-premultiplied color with 0..=1 channels, clamping the result.
pub fn apply_matrix(m: &[[f32; 5]; 4], c: [f32; 4]) -> [f32; 4] {
    let input = [c[0], c[1], c[2], c[3], 1.0];
    std::array::from_fn(|o| {
        m[o].iter()
            .zip(input)
            .map(|(k, v)| k * v)
            .sum::<f32>()
            .clamp(0.0, 1.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(ops: &[FilterOp]) -> ColorFilters {
        ColorFilters::of(ops).unwrap()
    }

    /// Filter Effects 1 §14: function by function when one list is a prefix of the other by
    /// function, the missing functions at their initial values (1 for brightness, contrast,
    /// opacity and saturate, 0 for the others), `none` as the empty list; amounts clamped to their
    /// ranges when extrapolated.
    #[test]
    fn color_filter_lists_interpolate_as_section_14_says() {
        use FilterOp as F;
        let none = list(&[]);
        let a = list(&[F::Brightness(0.5), F::Grayscale(1.0)]);
        let b = list(&[F::Brightness(1.5)]);
        assert!(ColorFilters::interpolate(&a, &b) && ColorFilters::interpolate(&none, &a));
        assert!(!ColorFilters::interpolate(&a, &list(&[F::Grayscale(1.0)])));
        assert_eq!(
            ColorFilters::blend(&a, &b, 0.5).ops(),
            vec![F::Brightness(1.0), F::Grayscale(0.5)]
        );
        assert_eq!(
            ColorFilters::blend(&none, &list(&[F::Saturate(3.0), F::HueRotate(90.0)]), 0.25).ops(),
            vec![F::Saturate(1.5), F::HueRotate(22.5)]
        );
        assert_eq!(
            ColorFilters::blend(&a, &b, 2.0).ops(),
            vec![F::Brightness(2.5), F::Grayscale(0.0)]
        );
        assert_eq!(
            ColorFilters::blend(&a, &b, -2.0).ops(),
            vec![F::Brightness(0.0), F::Grayscale(1.0)]
        );
        assert!(ColorFilters::of(&[F::Blur(1.0)]).is_none());
        assert!(ColorFilters::of(&vec![F::Opacity(1.0); 9]).is_none());
    }

    fn near(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.002)
    }

    #[test]
    fn color_matrices_follow_the_spec_equivalents() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let apply = |op: FilterOp| apply_matrix(&op.color_matrix().unwrap(), red);
        // grayscale(1): the luminance of red in every channel
        assert!(near(
            apply(FilterOp::Grayscale(1.0)),
            [0.2126, 0.2126, 0.2126, 1.0]
        ));
        // grayscale(0), sepia(0), saturate(1), hue-rotate(0), invert(0), brightness(1),
        // contrast(1), opacity(1): unchanged
        for op in [
            FilterOp::Grayscale(0.0),
            FilterOp::Sepia(0.0),
            FilterOp::Saturate(1.0),
            FilterOp::HueRotate(0.0),
            FilterOp::Invert(0.0),
            FilterOp::Brightness(1.0),
            FilterOp::Contrast(1.0),
            FilterOp::Opacity(1.0),
        ] {
            assert!(near(apply(op.clone()), red), "{op:?}");
        }
        assert!(near(
            apply(FilterOp::Sepia(1.0)),
            [0.393, 0.349, 0.272, 1.0]
        ));
        assert!(near(apply(FilterOp::Invert(1.0)), [0.0, 1.0, 1.0, 1.0]));
        assert!(near(apply(FilterOp::Opacity(0.25)), [1.0, 0.0, 0.0, 0.25]));
        assert!(near(apply(FilterOp::Brightness(0.5)), [0.5, 0.0, 0.0, 1.0]));
        // contrast(0): mid gray
        assert!(near(apply(FilterOp::Contrast(0.0)), [0.5, 0.5, 0.5, 1.0]));
        // saturate(0): the same gray as grayscale(1) (the §10 matrix's weights)
        assert!(near(
            apply(FilterOp::Saturate(0.0)),
            [0.213, 0.213, 0.213, 1.0]
        ));
        // hue-rotate(180deg) takes red to the teal of the §10 matrix (its luminance kept)
        assert!(near(
            apply(FilterOp::HueRotate(180.0)),
            [0.0, 0.426, 0.426, 1.0]
        ));
        assert!(FilterOp::Blur(1.0).color_matrix().is_none());
    }

    #[test]
    fn blur_and_drop_shadow_extend_the_painted_area() {
        let r = Rect::new(10.0, 10.0, 20.0, 20.0);
        assert_eq!(FilterOp::Blur(2.0).map_rect(r), r.inflate(6.0, 6.0));
        assert_eq!(FilterOp::Grayscale(1.0).map_rect(r), r);
        let shadow = FilterOp::DropShadow {
            offset: Point::new(5.0, -3.0),
            sigma: 1.0,
            color: Color::BLACK,
        };
        assert_eq!(shadow.map_rect(r), Rect::from_ltrb(10.0, 4.0, 38.0, 30.0));
        // a list applies in order: the shadow of the blurred content
        let both = [FilterOp::Blur(1.0), shadow.clone()];
        assert_eq!(map_rect(&both, r), Rect::from_ltrb(7.0, 1.0, 41.0, 33.0));
        assert!(moves_pixels(&both) && !moves_pixels(&[FilterOp::Sepia(1.0)]));
        // what to paint for a shown area: the shadow of content 5 px left and 3 px below it
        assert_eq!(
            source_rect(&[shadow], r),
            Rect::from_ltrb(2.0, 10.0, 30.0, 36.0)
        );
        assert_eq!(blur_outset(0.0), 0.0);
    }

    #[test]
    fn ranges_of_the_amounts() {
        assert!(FilterOp::Blur(0.0).is_valid() && !FilterOp::Blur(-1.0).is_valid());
        assert!(FilterOp::Brightness(3.0).is_valid() && !FilterOp::Brightness(f32::NAN).is_valid());
        assert!(FilterOp::Grayscale(1.0).is_valid() && !FilterOp::Grayscale(1.5).is_valid());
        assert!(
            FilterOp::HueRotate(-400.0).is_valid()
                && !FilterOp::HueRotate(f32::INFINITY).is_valid()
        );
    }
}
