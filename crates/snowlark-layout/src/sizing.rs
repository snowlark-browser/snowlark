// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Box edges and size resolution: CSS 2 §8 (margins, padding, borders), §10 (widths and heights,
//! min/max), css-sizing-3 (intrinsic size keywords, `box-sizing`, `aspect-ratio`, stretch-fit and
//! fit-content sizes).

use snowlark_paint::Sides;

use crate::style::*;

/// A box's resolved margins (`None` for `auto`), borders and padding, in px.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Edges {
    /// top, right, bottom, left
    pub margin: [Option<f32>; 4],
    pub border: Sides,
    pub padding: Sides,
}

impl Edges {
    /// Margins and padding resolve percentages against the containing block's width, in both axes
    /// (CSS 2 §8.3, §8.4).
    pub fn of<S: Style>(s: &S, cb_width: f32) -> Edges {
        let m = |side| s.margin(side).resolve(cb_width);
        let p = |side| s.padding(side).resolve(cb_width).max(0.0);
        Edges {
            margin: [m(Side::Top), m(Side::Right), m(Side::Bottom), m(Side::Left)],
            border: Sides::new(
                s.border_width(Side::Top),
                s.border_width(Side::Right),
                s.border_width(Side::Bottom),
                s.border_width(Side::Left),
            ),
            padding: Sides::new(p(Side::Top), p(Side::Right), p(Side::Bottom), p(Side::Left)),
        }
    }

    #[inline]
    pub fn pb_h(&self) -> f32 {
        self.border.left + self.border.right + self.padding.left + self.padding.right
    }

    #[inline]
    pub fn pb_v(&self) -> f32 {
        self.border.top + self.border.bottom + self.padding.top + self.padding.bottom
    }

    #[inline]
    pub fn mt(&self) -> f32 {
        self.margin[0].unwrap_or(0.0)
    }

    #[inline]
    pub fn mr(&self) -> f32 {
        self.margin[1].unwrap_or(0.0)
    }

    #[inline]
    pub fn mb(&self) -> f32 {
        self.margin[2].unwrap_or(0.0)
    }

    #[inline]
    pub fn ml(&self) -> f32 {
        self.margin[3].unwrap_or(0.0)
    }

    /// Left + right margins with `auto` as zero.
    #[inline]
    pub fn margin_h(&self) -> f32 {
        self.ml() + self.mr()
    }

    #[inline]
    pub fn margin_v(&self) -> f32 {
        self.mt() + self.mb()
    }
}

/// Converts a specified size to a border-box size under `box-sizing`.
#[inline]
pub(crate) fn to_border_box(size: f32, pb: f32, sizing: BoxSizing) -> f32 {
    match sizing {
        BoxSizing::ContentBox => size + pb,
        BoxSizing::BorderBox => size.max(pb),
    }
}

/// Content-box intrinsic inline sizes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ContentSizes {
    pub min: f32,
    pub max: f32,
}

impl ContentSizes {
    pub const ZERO: ContentSizes = ContentSizes { min: 0.0, max: 0.0 };

    /// The fit-content size for `available` (CSS 2 §10.3.5 shrink-to-fit): min(max(min, avail), max).
    #[inline]
    pub fn shrink_to_fit(&self, available: f32) -> f32 {
        self.max.min(available.max(self.min))
    }

    pub fn union(self, other: ContentSizes) -> ContentSizes {
        ContentSizes {
            min: self.min.max(other.min),
            max: self.max.max(other.max),
        }
    }

    pub fn add(self, v: f32) -> ContentSizes {
        ContentSizes {
            min: self.min + v,
            max: self.max + v,
        }
    }
}

/// Resolves a `width`/`min-width`-style value to a border-box width. `basis` is the containing block
/// width; `content` lazily gives the content-box min/max-content widths; `stretch` is the border-box
/// width that fills the available space. `None` for `auto`.
pub(crate) fn resolve_width(
    value: SizeValue,
    basis: Option<f32>,
    pb: f32,
    sizing: BoxSizing,
    stretch: f32,
    content: &mut dyn FnMut() -> ContentSizes,
) -> Option<f32> {
    match value {
        SizeValue::Auto => None,
        SizeValue::LengthPercentage(lp) => lp
            .resolve_opt(basis)
            .map(|v| to_border_box(v.max(0.0), pb, sizing)),
        SizeValue::MinContent => Some(content().min + pb),
        SizeValue::MaxContent => Some(content().max + pb),
        SizeValue::FitContent(limit) => {
            let c = content();
            let avail = match limit.and_then(|l| l.resolve_opt(basis)) {
                Some(l) => to_border_box(l, pb, sizing) - pb,
                None => stretch - pb,
            };
            Some(c.shrink_to_fit(avail.max(0.0)) + pb)
        }
        SizeValue::Stretch => Some(stretch.max(pb)),
    }
}

/// `max-width`: `None` for `none`.
pub(crate) fn resolve_max_width(
    value: MaxSizeValue,
    basis: Option<f32>,
    pb: f32,
    sizing: BoxSizing,
    stretch: f32,
    content: &mut dyn FnMut() -> ContentSizes,
) -> Option<f32> {
    let as_size = match value {
        MaxSizeValue::None => return None,
        MaxSizeValue::LengthPercentage(lp) => SizeValue::LengthPercentage(lp),
        MaxSizeValue::MinContent => SizeValue::MinContent,
        MaxSizeValue::MaxContent => SizeValue::MaxContent,
        MaxSizeValue::FitContent(l) => SizeValue::FitContent(l),
        MaxSizeValue::Stretch => SizeValue::Stretch,
    };
    resolve_width(as_size, basis, pb, sizing, stretch, content)
}

/// A height-axis length: `height`, `min-height`; intrinsic keywords behave as `auto` for block
/// containers (css-sizing-3 §3.1.1). `None` for auto or a percentage of an indefinite basis.
pub(crate) fn resolve_height(
    value: SizeValue,
    basis: Option<f32>,
    pb: f32,
    sizing: BoxSizing,
) -> Option<f32> {
    match value {
        SizeValue::LengthPercentage(lp) => lp
            .resolve_opt(basis)
            .map(|v| to_border_box(v.max(0.0), pb, sizing)),
        SizeValue::Stretch => basis.map(|b| b.max(pb)),
        _ => None,
    }
}

pub(crate) fn resolve_max_height(
    value: MaxSizeValue,
    basis: Option<f32>,
    pb: f32,
    sizing: BoxSizing,
) -> Option<f32> {
    match value {
        MaxSizeValue::LengthPercentage(lp) => lp
            .resolve_opt(basis)
            .map(|v| to_border_box(v.max(0.0), pb, sizing)),
        MaxSizeValue::Stretch => basis.map(|b| b.max(pb)),
        _ => None,
    }
}

/// CSS 2 §10.4 / §10.7: max first, then min (min wins over max).
#[inline]
pub(crate) fn clamp(v: f32, min: Option<f32>, max: Option<f32>) -> f32 {
    let mut v = v;
    if let Some(m) = max {
        v = v.min(m);
    }
    if let Some(m) = min {
        v = v.max(m);
    }
    v
}

/// The min/max border-box widths a style imposes (`min-width: auto` is 0 outside flex/grid).
pub(crate) fn width_bounds<S: Style>(
    s: &S,
    basis: Option<f32>,
    pb: f32,
    stretch: f32,
    content: &mut dyn FnMut() -> ContentSizes,
) -> (Option<f32>, Option<f32>) {
    let sizing = s.box_sizing();
    let min = resolve_width(s.min_width(), basis, pb, sizing, stretch, content);
    let max = resolve_max_width(s.max_width(), basis, pb, sizing, stretch, content);
    (min, max)
}

/// The min/max border-box heights.
pub(crate) fn height_bounds<S: Style>(
    s: &S,
    basis: Option<f32>,
    pb: f32,
) -> (Option<f32>, Option<f32>) {
    let sizing = s.box_sizing();
    (
        resolve_height(s.min_height(), basis, pb, sizing),
        resolve_max_height(s.max_height(), basis, pb, sizing),
    )
}

/// The preferred aspect ratio a non-replaced box's style gives (width / height), if any.
pub(crate) fn style_ratio<S: Style>(s: &S) -> Option<f32> {
    let r = s.aspect_ratio();
    r.ratio.filter(|v| *v > 0.0 && v.is_finite())
}

/// Resolves `auto` horizontal margins of a block-level box in normal flow (CSS 2 §10.3.3): with the
/// border-box width fixed, both auto margins center the box, one takes the remaining space, and an
/// over-constrained box ignores its end margin, unless its parent aligns descendants (`legacy`,
/// HTML §15.3.3), which places the leftover space as the keyword says. Returns (left, right).
pub(crate) fn resolve_auto_margins(
    edges: &Edges,
    width: f32,
    available: f32,
    rtl: bool,
    legacy: Option<LegacyAlign>,
) -> (f32, f32) {
    let free = available - width;
    match (edges.margin[3], edges.margin[1]) {
        (None, None) => {
            let m = (free / 2.0).max(0.0);
            (m, m)
        }
        (None, Some(r)) => (free - r, r),
        (Some(l), None) => (l, free - l),
        (Some(l), Some(r)) if legacy.is_some() && free - l - r > 0.0 => {
            let extra = free - l - r;
            match legacy {
                Some(LegacyAlign::Center) => (l + extra / 2.0, r + extra / 2.0),
                Some(LegacyAlign::Right) => (l + extra, r),
                _ => (l, r + extra),
            }
        }
        (Some(l), Some(r)) => {
            // Over-constrained: the end margin is ignored (recomputed).
            if rtl {
                (free - r, r)
            } else {
                (l, free - l)
            }
        }
    }
}
