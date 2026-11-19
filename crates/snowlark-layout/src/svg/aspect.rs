// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `viewBox` (SVG 2 §8.6) and `preserveAspectRatio` (§8.7): parsing, serialization and the
//! viewport transform they establish (§8.2 "equivalent transform of an SVG viewport").

use super::geom::Matrix;
use super::parse::{fmt_f64, split_wsp, Scanner};

/// A `viewBox` value: `min-x min-y width height`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ViewBox {
    /// Parses four numbers separated by `comma-wsp`; a negative width or height is an error.
    pub fn parse(s: &str) -> Option<ViewBox> {
        let mut sc = Scanner::new(s);
        sc.skip_wsp();
        let mut v = [0.0; 4];
        for (i, slot) in v.iter_mut().enumerate() {
            if i > 0 {
                sc.skip_comma_wsp();
            }
            *slot = sc.number()?;
        }
        sc.skip_wsp();
        if !sc.at_end() || v[2] < 0.0 || v[3] < 0.0 {
            return None;
        }
        Some(ViewBox {
            x: v[0],
            y: v[1],
            width: v[2],
            height: v[3],
        })
    }

    pub fn to_css(&self) -> String {
        [self.x, self.y, self.width, self.height]
            .map(fmt_f64)
            .join(" ")
    }
}

/// The alignment part of `preserveAspectRatio` (`SVG_PRESERVEASPECTRATIO_*` order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None = 1,
    XMinYMin = 2,
    XMidYMin = 3,
    XMaxYMin = 4,
    XMinYMid = 5,
    XMidYMid = 6,
    XMaxYMid = 7,
    XMinYMax = 8,
    XMidYMax = 9,
    XMaxYMax = 10,
}

const ALIGN_NAMES: [&str; 10] = [
    "none", "xMinYMin", "xMidYMin", "xMaxYMin", "xMinYMid", "xMidYMid", "xMaxYMid", "xMinYMax",
    "xMidYMax", "xMaxYMax",
];

const ALIGNS: [Align; 10] = [
    Align::None,
    Align::XMinYMin,
    Align::XMidYMin,
    Align::XMaxYMin,
    Align::XMinYMid,
    Align::XMidYMid,
    Align::XMaxYMid,
    Align::XMinYMax,
    Align::XMidYMax,
    Align::XMaxYMax,
];

impl Align {
    pub fn from_u16(v: u16) -> Option<Align> {
        ALIGNS.get((v as usize).checked_sub(1)?).copied()
    }

    pub fn name(self) -> &'static str {
        ALIGN_NAMES[self as usize - 1]
    }

    /// The x and y alignment fractions (0, 0.5, 1).
    fn fractions(self) -> (f64, f64) {
        let i = self as usize - 2;
        ((i % 3) as f64 / 2.0, (i / 3) as f64 / 2.0)
    }
}

/// A `preserveAspectRatio` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AspectRatio {
    pub align: Align,
    pub slice: bool,
}

impl Default for AspectRatio {
    /// The initial value, `xMidYMid meet`.
    fn default() -> AspectRatio {
        AspectRatio {
            align: Align::XMidYMid,
            slice: false,
        }
    }
}

impl AspectRatio {
    /// Parses `<align> [ meet | slice ]?`.
    pub fn parse(s: &str) -> Option<AspectRatio> {
        let mut parts = split_wsp(s);
        let a = parts.next()?;
        let i = ALIGN_NAMES.iter().position(|n| *n == a)?;
        let slice = match parts.next() {
            None | Some("meet") => false,
            Some("slice") => true,
            _ => return None,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(AspectRatio {
            align: ALIGNS[i],
            slice,
        })
    }

    pub fn to_css(&self) -> String {
        format!(
            "{} {}",
            self.align.name(),
            if self.slice { "slice" } else { "meet" }
        )
    }

    /// The transform that maps `vb` onto a viewport of `width` × `height` at the origin (§8.2).
    pub fn transform(&self, vb: &ViewBox, width: f64, height: f64) -> Matrix {
        if vb.width <= 0.0 || vb.height <= 0.0 {
            return Matrix::IDENTITY;
        }
        let mut sx = width / vb.width;
        let mut sy = height / vb.height;
        if self.align != Align::None {
            let s = if self.slice { sx.max(sy) } else { sx.min(sy) };
            sx = s;
            sy = s;
        }
        let mut tx = -vb.x * sx;
        let mut ty = -vb.y * sy;
        if self.align != Align::None {
            let (fx, fy) = self.align.fractions();
            tx += (width - vb.width * sx) * fx;
            ty += (height - vb.height * sy) * fy;
        }
        Matrix::new([sx, 0.0, 0.0, sy, tx, ty])
    }
}
