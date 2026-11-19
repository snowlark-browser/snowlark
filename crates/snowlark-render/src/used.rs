// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layout's used values for CSSOM's resolved values (CSSOM §6.7.2): `width`/`height` and paddings from
//! the box's fragment, margins and insets from its position in its containing block, `line-height`
//! in px, `transform-origin` against the border box.

use servo_arc::Arc as ServoArc;
use snowlark_layout::BoxRects;
use snowlark_paint::Rect;
use snowlark_style::computed::UsedValues;
use snowlark_style::LonghandId;
use style::properties::ComputedValues;
use style::values::computed::{Length, PositionProperty};
use style::values::generics::length::GenericMargin;
use style::values::generics::position::GenericInset;

/// The used values of one element with a box.
pub struct NodeUsedValues {
    values: ServoArc<ComputedValues>,
    geometry: BoxRects,
    /// The containing block (the padding box of a positioned ancestor for absolute boxes, the parent's
    /// content box otherwise, the viewport for fixed ones).
    cb: Rect,
    /// For a sticky box, the size its insets resolve against: its scrollports' (see
    /// `Renderer::used_values`).
    pub sticky_basis: Option<(f32, f32)>,
    /// A grid container's resolved `grid-template-columns` and `grid-template-rows`.
    grid: Option<(String, String)>,
    /// An element without a CSS box: only `transform` has a used value.
    boxless: bool,
}

/// A CSS `<number>` as CSSOM serializes it in a matrix: six significant digits at most (as the
/// engines print them), trailing zeros dropped, no negative zero; what rounding left of a zero
/// (`cos(90deg)`) is zero.
pub(crate) fn number(v: f32) -> String {
    if v.abs() < 1e-6 || !v.is_finite() {
        return "0".into();
    }
    let magnitude = v.abs().log10().floor() as i32;
    let decimals = (5 - magnitude).clamp(0, 12) as usize;
    let s = format!("{:.*}", decimals, v);
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        &s
    };
    if s == "-0" {
        "0".into()
    } else {
        s.to_owned()
    }
}

/// CSS Transforms 1 §11 / 2 §6: the resolved value of `transform` for a box is its used value, the
/// matrix of the transform functions over the reference box (the border box), serialized as
/// `matrix()` or, when 3D, `matrix3d()`; `none` stays `none`.
pub(crate) fn transform_matrix(values: &ComputedValues, reference: Rect) -> Option<String> {
    let t = &values.get_box().transform;
    if t.0.is_empty() {
        return None;
    }
    let basis = euclid::default::Rect::new(
        euclid::default::Point2D::new(Length::new(0.0), Length::new(0.0)),
        euclid::default::Size2D::new(Length::new(reference.width), Length::new(reference.height)),
    );
    let (m, is_3d) = t.to_transform_3d_matrix(Some(&basis)).ok()?;
    let n = number;
    Some(if is_3d {
        format!(
            "matrix3d({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            n(m.m11),
            n(m.m12),
            n(m.m13),
            n(m.m14),
            n(m.m21),
            n(m.m22),
            n(m.m23),
            n(m.m24),
            n(m.m31),
            n(m.m32),
            n(m.m33),
            n(m.m34),
            n(m.m41),
            n(m.m42),
            n(m.m43),
            n(m.m44)
        )
    } else {
        format!(
            "matrix({}, {}, {}, {}, {}, {})",
            n(m.m11),
            n(m.m12),
            n(m.m21),
            n(m.m22),
            n(m.m41),
            n(m.m42)
        )
    })
}

/// A CSS `<number>` in px the way CSSOM serializes it.
pub(crate) fn px(v: f32) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    // Six decimals at most, trailing zeros dropped.
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}px")
}

impl NodeUsedValues {
    pub fn new(values: ServoArc<ComputedValues>, geometry: BoxRects, cb: Rect) -> Self {
        NodeUsedValues {
            values,
            geometry,
            cb,
            sticky_basis: None,
            grid: None,
            boxless: false,
        }
    }

    /// The used values of an SVG element, which has no CSS box.
    pub fn boxless(values: ServoArc<ComputedValues>) -> Self {
        let none = BoxRects {
            border_box: Rect::ZERO,
            padding_box: Rect::ZERO,
            content_box: Rect::ZERO,
        };
        NodeUsedValues {
            boxless: true,
            ..NodeUsedValues::new(values, none, Rect::ZERO)
        }
    }

    /// The used track lists of a grid container ([`snowlark_layout::LayoutTree::grid_track_lists`]).
    pub fn with_grid(mut self, grid: Option<(String, String)>) -> Self {
        self.grid = grid;
        self
    }

    fn margin(&self, side: usize) -> f32 {
        let m = self.values.get_margin();
        let v = [
            &m.margin_top,
            &m.margin_right,
            &m.margin_bottom,
            &m.margin_left,
        ][side];
        match v {
            GenericMargin::LengthPercentage(l) => l.resolve(Length::new(self.cb.width)).px(),
            _ => {
                // `auto`: what layout gave it. For an absolutely positioned box, between the border
                // box and its inset-modified containing block, and zero when an inset in that axis
                // is `auto` (css-position-3 §4.2).
                let b = self.geometry.border_box;
                if matches!(
                    self.values.get_box().position,
                    PositionProperty::Absolute | PositionProperty::Fixed
                ) {
                    let p = self.values.get_position();
                    let resolve = |v: &style::values::computed::Inset, basis: f32| match v {
                        GenericInset::LengthPercentage(l) => {
                            Some(l.resolve(Length::new(basis)).px())
                        }
                        _ => None,
                    };
                    let (start, end) = if side.is_multiple_of(2) {
                        (
                            resolve(&p.top, self.cb.height),
                            resolve(&p.bottom, self.cb.height),
                        )
                    } else {
                        (
                            resolve(&p.left, self.cb.width),
                            resolve(&p.right, self.cb.width),
                        )
                    };
                    let (Some(start), Some(end)) = (start, end) else {
                        return 0.0;
                    };
                    return match side {
                        0 => b.y - (self.cb.y + start),
                        1 => self.cb.right() - end - b.right(),
                        2 => self.cb.bottom() - end - b.bottom(),
                        _ => b.x - (self.cb.x + start),
                    };
                }
                match side {
                    0 => b.y - self.cb.y,
                    1 => self.cb.right() - b.right(),
                    2 => self.cb.bottom() - b.bottom(),
                    _ => b.x - self.cb.x,
                }
            }
        }
    }

    fn inset(&self, side: usize) -> Option<f32> {
        let position = self.values.get_box().position;
        if position == PositionProperty::Static {
            return None;
        }
        let p = self.values.get_position();
        let v = [&p.top, &p.right, &p.bottom, &p.left][side];
        let basis = if side.is_multiple_of(2) {
            self.cb.height
        } else {
            self.cb.width
        };
        let given = match v {
            GenericInset::LengthPercentage(l) => Some(l.resolve(Length::new(basis)).px()),
            _ => None,
        };
        if position == PositionProperty::Sticky {
            // Against the scrollport; `auto` stays `auto` (no offset in that direction).
            let (w, h) = self.sticky_basis.unwrap_or((self.cb.width, self.cb.height));
            let basis = if side.is_multiple_of(2) { h } else { w };
            return match v {
                GenericInset::LengthPercentage(l) => Some(l.resolve(Length::new(basis)).px()),
                _ => None,
            };
        }
        if position == PositionProperty::Relative {
            // CSS 2 §9.4.3: `auto` is the negation of the opposite side, or 0.
            if let Some(g) = given {
                return Some(g);
            }
            let opposite = [&p.bottom, &p.left, &p.top, &p.right][side];
            let basis = if side.is_multiple_of(2) {
                self.cb.height
            } else {
                self.cb.width
            };
            return Some(match opposite {
                GenericInset::LengthPercentage(l) => -l.resolve(Length::new(basis)).px(),
                _ => 0.0,
            });
        }
        // Absolute and fixed: the distance from the containing block's edge to the margin box.
        let b = self.geometry.border_box;
        Some(match side {
            0 => b.y - self.margin(0) - self.cb.y,
            1 => self.cb.right() - (b.right() + self.margin(1)),
            2 => self.cb.bottom() - (b.bottom() + self.margin(2)),
            _ => b.x - self.margin(3) - self.cb.x,
        })
    }
}

impl UsedValues for NodeUsedValues {
    fn used_value(&self, property: LonghandId) -> Option<String> {
        use style::properties::longhands::box_sizing::computed_value::T as BoxSizing;
        use style::values::generics::font::GenericLineHeight;
        if self.boxless && property != LonghandId::Transform {
            return None;
        }
        let g = &self.geometry;
        let border_box = self.values.get_position().box_sizing == BoxSizing::BorderBox;
        let v = match property {
            LonghandId::Width => {
                if border_box {
                    g.border_box.width
                } else {
                    g.content_box.width
                }
            }
            LonghandId::Height => {
                if border_box {
                    g.border_box.height
                } else {
                    g.content_box.height
                }
            }
            LonghandId::PaddingTop => g.content_box.y - g.padding_box.y,
            LonghandId::PaddingRight => g.padding_box.right() - g.content_box.right(),
            LonghandId::PaddingBottom => g.padding_box.bottom() - g.content_box.bottom(),
            LonghandId::PaddingLeft => g.content_box.x - g.padding_box.x,
            LonghandId::MarginTop => self.margin(0),
            LonghandId::MarginRight => self.margin(1),
            LonghandId::MarginBottom => self.margin(2),
            LonghandId::MarginLeft => self.margin(3),
            LonghandId::Top => self.inset(0)?,
            LonghandId::Right => self.inset(1)?,
            LonghandId::Bottom => self.inset(2)?,
            LonghandId::Left => self.inset(3)?,
            LonghandId::LineHeight => match &self.values.get_font().line_height {
                GenericLineHeight::Normal => return None,
                GenericLineHeight::Number(n) => {
                    n.0 * self.values.get_font().font_size.computed_size().px()
                }
                GenericLineHeight::Length(l) => l.0.px(),
            },
            LonghandId::Transform => return transform_matrix(&self.values, g.border_box),
            LonghandId::GridTemplateColumns => return self.grid.as_ref().map(|g| g.0.clone()),
            LonghandId::GridTemplateRows => return self.grid.as_ref().map(|g| g.1.clone()),
            LonghandId::TransformOrigin => {
                let o = &self.values.get_box().transform_origin;
                let x = o.horizontal.resolve(Length::new(g.border_box.width)).px();
                let y = o.vertical.resolve(Length::new(g.border_box.height)).px();
                let z = o.depth.px();
                return Some(if z == 0.0 {
                    format!("{} {}", px(x), px(y))
                } else {
                    format!("{} {} {}", px(x), px(y), px(z))
                });
            }
            _ => return None,
        };
        Some(px(v))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn px_serialization() {
        assert_eq!(super::px(120.0), "120px");
        assert_eq!(super::px(12.5), "12.5px");
        assert_eq!(super::px(-0.0), "0px");
        assert_eq!(super::px(1.0 / 3.0), "0.333333px");
    }

    #[test]
    fn matrix_numbers_have_six_significant_digits() {
        assert_eq!(super::number(std::f32::consts::FRAC_1_SQRT_2), "0.707107");
        assert_eq!(super::number(-0.1), "-0.1");
        assert_eq!(super::number(-0.0), "0");
        assert_eq!(super::number(1.0), "1");
        assert_eq!(super::number(-1504.0), "-1504");
        assert_eq!(super::number(-50.078125), "-50.0781");
        assert_eq!(super::number(1e-9), "0");
    }
}
