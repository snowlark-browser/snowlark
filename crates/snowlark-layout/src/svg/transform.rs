// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Transform lists as SVG's `transform`, `gradientTransform` and `patternTransform` attributes hold
//! them (CSS Transforms §7.1: the SVG `transform` attribute grammar), and the `SVGTransform`
//! values of SVG 2 §8.14.1.

use super::geom::Matrix;
use super::parse::{fmt_f64, Scanner};

/// `SVGTransform.type` (SVG 2 §8.14.1 `SVG_TRANSFORM_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformKind {
    Matrix = 1,
    Translate = 2,
    Scale = 3,
    Rotate = 4,
    SkewX = 5,
    SkewY = 6,
}

/// One transform function: its type, its matrix and the parameters the matrix does not keep
/// (a rotation's angle and centre, a skew's angle).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub kind: TransformKind,
    pub matrix: Matrix,
    /// The angle of a rotate or skew, in degrees.
    pub angle: f64,
    /// A rotation's centre.
    pub cx: f64,
    pub cy: f64,
}

impl Default for Transform {
    /// `createSVGTransform()`: the identity as a `matrix(…)`.
    fn default() -> Transform {
        Transform::matrix(Matrix::IDENTITY)
    }
}

impl Transform {
    pub fn matrix(m: Matrix) -> Transform {
        Transform {
            kind: TransformKind::Matrix,
            matrix: m,
            angle: 0.0,
            cx: 0.0,
            cy: 0.0,
        }
    }

    pub fn translate(tx: f64, ty: f64) -> Transform {
        Transform {
            kind: TransformKind::Translate,
            matrix: Matrix::translate((tx, ty)),
            ..Transform::default()
        }
    }

    pub fn scale(sx: f64, sy: f64) -> Transform {
        Transform {
            kind: TransformKind::Scale,
            matrix: Matrix::scale_non_uniform(sx, sy),
            ..Transform::default()
        }
    }

    /// `rotate(angle, cx, cy)`: translate(cx, cy) rotate(angle) translate(-cx, -cy).
    pub fn rotate(angle: f64, cx: f64, cy: f64) -> Transform {
        let m = Matrix::translate((cx, cy))
            * Matrix::rotate(angle.to_radians())
            * Matrix::translate((-cx, -cy));
        Transform {
            kind: TransformKind::Rotate,
            matrix: m,
            angle,
            cx,
            cy,
        }
    }

    pub fn skew_x(angle: f64) -> Transform {
        Transform {
            kind: TransformKind::SkewX,
            matrix: Matrix::new([1.0, 0.0, angle.to_radians().tan(), 1.0, 0.0, 0.0]),
            angle,
            ..Transform::default()
        }
    }

    pub fn skew_y(angle: f64) -> Transform {
        Transform {
            kind: TransformKind::SkewY,
            matrix: Matrix::new([1.0, angle.to_radians().tan(), 0.0, 1.0, 0.0, 0.0]),
            angle,
            ..Transform::default()
        }
    }

    /// The function's serialization (SVG 2 §8.5's grammar: the function's parameters, space
    /// separated).
    pub fn to_css(&self) -> String {
        let c = self.matrix.as_coeffs();
        let args: Vec<f64> = match self.kind {
            TransformKind::Matrix => c.to_vec(),
            TransformKind::Translate => vec![c[4], c[5]],
            TransformKind::Scale => vec![c[0], c[3]],
            TransformKind::Rotate => vec![self.angle, self.cx, self.cy],
            TransformKind::SkewX | TransformKind::SkewY => vec![self.angle],
        };
        let name = match self.kind {
            TransformKind::Matrix => "matrix",
            TransformKind::Translate => "translate",
            TransformKind::Scale => "scale",
            TransformKind::Rotate => "rotate",
            TransformKind::SkewX => "skewX",
            TransformKind::SkewY => "skewY",
        };
        let args: Vec<String> = args.into_iter().map(fmt_f64).collect();
        format!("{name}({})", args.join(" "))
    }
}

/// Parses a transform list (`none` and the empty string are the empty list); `None` when the value
/// is in error.
pub fn parse_transform_list(s: &str) -> Option<Vec<Transform>> {
    let mut sc = Scanner::new(s);
    let mut out = Vec::new();
    sc.skip_wsp();
    if sc.rest() == b"none" {
        return Some(out);
    }
    {
        let mut t = sc.clone();
        if t.rest().starts_with(b"none") {
            t.i += 4;
            t.skip_wsp();
            if t.at_end() {
                return Some(out);
            }
        }
    }
    while !sc.at_end() {
        let name_start = sc.i;
        while sc.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
            sc.i += 1;
        }
        let name = std::str::from_utf8(&sc.s[name_start..sc.i]).ok()?;
        sc.skip_wsp();
        if !sc.eat(b'(') {
            return None;
        }
        sc.skip_wsp();
        let mut args = Vec::new();
        loop {
            if sc.eat(b')') {
                break;
            }
            if !args.is_empty() {
                sc.skip_comma_wsp();
            }
            args.push(sc.number()?);
            sc.skip_wsp();
            if args.len() > 6 {
                return None;
            }
        }
        let t = match (name, args.as_slice()) {
            ("matrix", &[a, b, c, d, e, f]) => Transform::matrix(Matrix::new([a, b, c, d, e, f])),
            ("translate", &[tx]) => Transform::translate(tx, 0.0),
            ("translate", &[tx, ty]) => Transform::translate(tx, ty),
            ("scale", &[s]) => Transform::scale(s, s),
            ("scale", &[sx, sy]) => Transform::scale(sx, sy),
            ("rotate", &[a]) => Transform::rotate(a, 0.0, 0.0),
            ("rotate", &[a, cx, cy]) => Transform::rotate(a, cx, cy),
            ("skewX", &[a]) => Transform::skew_x(a),
            ("skewY", &[a]) => Transform::skew_y(a),
            _ => return None,
        };
        out.push(t);
        // (functions are separated by comma-wsp, a trailing comma is an error)
        let comma = sc.skip_comma_wsp();
        if comma && sc.at_end() {
            return None;
        }
    }
    Some(out)
}

/// A list's serialization: its functions separated by spaces.
pub fn serialize_transform_list(list: &[Transform]) -> String {
    list.iter()
        .map(Transform::to_css)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The matrix of a whole list (post-multiplied in order).
pub fn list_matrix(list: &[Transform]) -> Matrix {
    list.iter().fold(Matrix::IDENTITY, |acc, t| acc * t.matrix)
}
