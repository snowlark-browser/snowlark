// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filter functions (Filter Effects 1 §10) and SVG filter primitives (§9) as Skia image filters.

use skia_safe as sk;

use super::Painter;
use crate::filter::FilterOp;
use crate::Rect;

/// The filters applied in order (the first to the content) as one image filter: `Some(None)` when
/// they change nothing, `None` when they cannot be made.
pub(super) fn image_filter(
    ops: &[FilterOp],
    bounds: &Rect,
    painter: &mut Painter,
) -> Option<Option<sk::ImageFilter>> {
    let mut filter: Option<sk::ImageFilter> = None;
    for op in ops {
        filter = Some(match op {
            FilterOp::Blur(sigma) => {
                if *sigma <= 0.0 {
                    continue;
                }
                sk::image_filters::blur((*sigma, *sigma), sk::TileMode::Decal, filter, None)?
            }
            FilterOp::DropShadow {
                offset,
                sigma,
                color,
            } => sk::image_filters::drop_shadow(
                (offset.x, offset.y),
                (*sigma, *sigma),
                sk::Color4f::from(super::color(*color)),
                None,
                filter,
                None,
            )?,
            FilterOp::Svg(svg) => super::svg::filter(svg, bounds, filter.clone(), painter)?,
            op => {
                let Some(m) = op.color_matrix() else {
                    continue;
                };
                sk::image_filters::color_filter(color_matrix(&m), filter, None)?
            }
        });
    }
    Some(filter)
}

/// A 4x5 color matrix on non-premultiplied colors (offsets in 0..=1) as a color filter.
pub(crate) fn color_matrix(m: &[[f32; 5]; 4]) -> sk::ColorFilter {
    let mut values = [0.0f32; 20];
    for (r, row) in m.iter().enumerate() {
        values[r * 5..r * 5 + 5].copy_from_slice(row);
    }
    sk::color_filters::matrix_row_major(&values, None)
}
