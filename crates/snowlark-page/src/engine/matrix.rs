// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The math behind Geometry Interfaces Module Level 1's `DOMMatrix`: a 4×4 matrix with its "is 2D"
//! flag, the transformations its methods post-multiply, inversion, point transformation, the
//! `<transform-list>` parsing of "parse a string into an abstract matrix" (CSS Transforms 1 and 2), and
//! the serialization of its stringifier (ECMAScript Number::toString of each element).

/// A matrix as Geometry §6 names its elements: `m[i][j]` is `m{i+1}{j+1}`, so `m[i]` is the i-th column
/// of the mathematical matrix (`m41`, `m42`, `m43` are the translation).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix {
    pub m: [[f64; 4]; 4],
    pub is_2d: bool,
}

impl Default for Matrix {
    fn default() -> Matrix {
        Matrix::identity()
    }
}

impl Matrix {
    pub fn identity() -> Matrix {
        let mut m = [[0.0; 4]; 4];
        for (i, col) in m.iter_mut().enumerate() {
            col[i] = 1.0;
        }
        Matrix { m, is_2d: true }
    }

    /// The 2D matrix `matrix(a, b, c, d, e, f)`.
    pub fn from_2d(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Matrix {
        let mut r = Matrix::identity();
        r.m[0][0] = a;
        r.m[0][1] = b;
        r.m[1][0] = c;
        r.m[1][1] = d;
        r.m[3][0] = e;
        r.m[3][1] = f;
        r
    }

    /// The 3D matrix of 16 elements in the order m11, m12, ..., m44.
    pub fn from_3d(v: &[f64]) -> Matrix {
        let mut r = Matrix::identity();
        for (k, x) in v.iter().take(16).enumerate() {
            r.m[k / 4][k % 4] = *x;
        }
        r.is_2d = false;
        r
    }

    /// The 16 elements, m11 first.
    pub fn elements(&self) -> [f64; 16] {
        let mut out = [0.0; 16];
        for (k, x) in out.iter_mut().enumerate() {
            *x = self.m[k / 4][k % 4];
        }
        out
    }

    /// `self × other` (other applied first to a point, as CSS composes transform functions).
    pub fn multiply(&self, other: &Matrix) -> Matrix {
        let a = &self.m;
        let b = &other.m;
        let mut m = [[0.0; 4]; 4];
        for (c, col) in m.iter_mut().enumerate() {
            for (r, x) in col.iter_mut().enumerate() {
                *x = (0..4).map(|k| a[k][r] * b[c][k]).sum();
            }
        }
        Matrix {
            m,
            is_2d: self.is_2d && other.is_2d,
        }
    }

    /// Post-multiplies `other` (the current matrix × other), keeping the 2D flag as given.
    fn post(&mut self, other: &Matrix) {
        let is_2d = self.is_2d;
        *self = self.multiply(other);
        self.is_2d = is_2d;
    }

    pub fn translation(tx: f64, ty: f64, tz: f64) -> Matrix {
        let mut r = Matrix::identity();
        r.m[3][0] = tx;
        r.m[3][1] = ty;
        r.m[3][2] = tz;
        r.is_2d = tz == 0.0;
        r
    }

    pub fn scaling(sx: f64, sy: f64, sz: f64) -> Matrix {
        let mut r = Matrix::identity();
        r.m[0][0] = sx;
        r.m[1][1] = sy;
        r.m[2][2] = sz;
        r.is_2d = sz == 1.0;
        r
    }

    /// CSS Transforms 2 `rotate3d(x, y, z, angle)` (degrees); the identity for a zero vector.
    pub fn rotation(x: f64, y: f64, z: f64, degrees: f64) -> Matrix {
        let len = (x * x + y * y + z * z).sqrt();
        let mut r = Matrix::identity();
        if len == 0.0 || len.is_nan() {
            r.is_2d = x == 0.0 && y == 0.0;
            return r;
        }
        let (x, y, z) = (x / len, y / len, z / len);
        let half = degrees.to_radians() / 2.0;
        let sc = half.sin() * half.cos();
        let sq = half.sin() * half.sin();
        r.m[0][0] = 1.0 - 2.0 * (y * y + z * z) * sq;
        r.m[0][1] = 2.0 * (x * y * sq + z * sc);
        r.m[0][2] = 2.0 * (x * z * sq - y * sc);
        r.m[1][0] = 2.0 * (x * y * sq - z * sc);
        r.m[1][1] = 1.0 - 2.0 * (x * x + z * z) * sq;
        r.m[1][2] = 2.0 * (y * z * sq + x * sc);
        r.m[2][0] = 2.0 * (x * z * sq + y * sc);
        r.m[2][1] = 2.0 * (y * z * sq - x * sc);
        r.m[2][2] = 1.0 - 2.0 * (x * x + y * y) * sq;
        r.is_2d = x == 0.0 && y == 0.0;
        r
    }

    /// A rotation about the z axis, with exact values at multiples of 90°.
    pub fn rotation_z(degrees: f64) -> Matrix {
        let (s, c) = exact_sin_cos(degrees);
        Matrix::from_2d(c, s, -s, c, 0.0, 0.0)
    }

    /// `skew(ax, ay)` in degrees.
    pub fn skew(ax: f64, ay: f64) -> Matrix {
        Matrix::from_2d(
            1.0,
            ay.to_radians().tan(),
            ax.to_radians().tan(),
            1.0,
            0.0,
            0.0,
        )
    }

    /// `perspective(d)`: the identity for `none` (infinity); lengths below 1px are clamped to 1px
    /// (CSS Transforms 2 §12.1).
    pub fn perspective(d: f64) -> Matrix {
        let mut r = Matrix::identity();
        r.is_2d = false;
        if d.is_finite() {
            r.m[2][3] = -1.0 / d.max(1.0);
        }
        r
    }

    // ----- the mutable transform methods (Geometry §6.5) ----------------------------------------

    pub fn translate_self(&mut self, tx: f64, ty: f64, tz: f64) {
        self.post(&Matrix::translation(tx, ty, tz));
        if tz != 0.0 {
            self.is_2d = false;
        }
    }

    pub fn scale_self(&mut self, sx: f64, sy: Option<f64>, sz: f64, ox: f64, oy: f64, oz: f64) {
        self.translate_self(ox, oy, oz);
        let sy = sy.unwrap_or(sx);
        self.post(&Matrix::scaling(sx, sy, sz));
        self.translate_self(-ox, -oy, -oz);
        if sz != 1.0 || oz != 0.0 {
            self.is_2d = false;
        }
    }

    pub fn scale3d_self(&mut self, s: f64, ox: f64, oy: f64, oz: f64) {
        self.translate_self(ox, oy, oz);
        self.post(&Matrix::scaling(s, s, s));
        self.translate_self(-ox, -oy, -oz);
        if s != 1.0 {
            self.is_2d = false;
        }
    }

    pub fn rotate_self(&mut self, rot_x: f64, rot_y: Option<f64>, rot_z: Option<f64>) {
        let (rx, ry, rz) = match (rot_y, rot_z) {
            (None, None) => (0.0, 0.0, rot_x),
            (y, z) => (rot_x, y.unwrap_or(0.0), z.unwrap_or(0.0)),
        };
        if rx != 0.0 || ry != 0.0 {
            self.is_2d = false;
        }
        if rz != 0.0 {
            self.post(&Matrix::rotation_z(rz));
        }
        if ry != 0.0 {
            self.post(&Matrix::rotation(0.0, 1.0, 0.0, ry));
        }
        if rx != 0.0 {
            self.post(&Matrix::rotation(1.0, 0.0, 0.0, rx));
        }
    }

    pub fn rotate_from_vector_self(&mut self, x: f64, y: f64) {
        let angle = if x == 0.0 && y == 0.0 {
            0.0
        } else {
            y.atan2(x).to_degrees()
        };
        self.post(&Matrix::rotation_z(angle));
    }

    pub fn rotate_axis_angle_self(&mut self, x: f64, y: f64, z: f64, angle: f64) {
        self.post(&Matrix::rotation(x, y, z, angle));
        if x != 0.0 || y != 0.0 {
            self.is_2d = false;
        }
    }

    pub fn skew_x_self(&mut self, sx: f64) {
        self.post(&Matrix::skew(sx, 0.0));
    }

    pub fn skew_y_self(&mut self, sy: f64) {
        self.post(&Matrix::skew(0.0, sy));
    }

    pub fn multiply_self(&mut self, other: &Matrix) {
        let is_2d = self.is_2d && other.is_2d;
        *self = self.multiply(other);
        self.is_2d = is_2d;
    }

    pub fn pre_multiply_self(&mut self, other: &Matrix) {
        let is_2d = self.is_2d && other.is_2d;
        *self = other.multiply(self);
        self.is_2d = is_2d;
    }

    /// `invertSelf()`: a matrix that is not invertible becomes all NaN and 3D.
    pub fn invert_self(&mut self) {
        match self.inverse() {
            Some(inv) => {
                let is_2d = self.is_2d;
                *self = inv;
                self.is_2d = is_2d;
            }
            None => {
                self.m = [[f64::NAN; 4]; 4];
                self.is_2d = false;
            }
        }
    }

    /// The inverse (cofactor expansion), or `None` for a singular or non-finite matrix.
    pub fn inverse(&self) -> Option<Matrix> {
        let e = self.elements();
        if e.iter().any(|x| !x.is_finite()) {
            return None;
        }
        if self.is_2d {
            let [a, b, c, d, tx, ty] = [e[0], e[1], e[4], e[5], e[12], e[13]];
            let det = a * d - b * c;
            if det == 0.0 {
                return None;
            }
            return Some(Matrix::from_2d(
                d / det,
                -b / det,
                -c / det,
                a / det,
                (c * ty - d * tx) / det,
                (b * tx - a * ty) / det,
            ));
        }
        // The standard 4×4 inverse over the flattened column-major elements.
        let m = e;
        let mut inv = [0.0; 16];
        inv[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15]
            + m[9] * m[7] * m[14]
            + m[13] * m[6] * m[11]
            - m[13] * m[7] * m[10];
        inv[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15]
            - m[8] * m[7] * m[14]
            - m[12] * m[6] * m[11]
            + m[12] * m[7] * m[10];
        inv[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15]
            + m[8] * m[7] * m[13]
            + m[12] * m[5] * m[11]
            - m[12] * m[7] * m[9];
        inv[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14]
            - m[8] * m[6] * m[13]
            - m[12] * m[5] * m[10]
            + m[12] * m[6] * m[9];
        inv[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15]
            - m[9] * m[3] * m[14]
            - m[13] * m[2] * m[11]
            + m[13] * m[3] * m[10];
        inv[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15]
            + m[8] * m[3] * m[14]
            + m[12] * m[2] * m[11]
            - m[12] * m[3] * m[10];
        inv[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15]
            - m[8] * m[3] * m[13]
            - m[12] * m[1] * m[11]
            + m[12] * m[3] * m[9];
        inv[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14]
            + m[8] * m[2] * m[13]
            + m[12] * m[1] * m[10]
            - m[12] * m[2] * m[9];
        inv[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15]
            + m[5] * m[3] * m[14]
            + m[13] * m[2] * m[7]
            - m[13] * m[3] * m[6];
        inv[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15]
            - m[4] * m[3] * m[14]
            - m[12] * m[2] * m[7]
            + m[12] * m[3] * m[6];
        inv[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15]
            + m[4] * m[3] * m[13]
            + m[12] * m[1] * m[7]
            - m[12] * m[3] * m[5];
        inv[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14]
            - m[4] * m[2] * m[13]
            - m[12] * m[1] * m[6]
            + m[12] * m[2] * m[5];
        inv[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11]
            - m[5] * m[3] * m[10]
            - m[9] * m[2] * m[7]
            + m[9] * m[3] * m[6];
        inv[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11]
            + m[4] * m[3] * m[10]
            + m[8] * m[2] * m[7]
            - m[8] * m[3] * m[6];
        inv[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11]
            - m[4] * m[3] * m[9]
            - m[8] * m[1] * m[7]
            + m[8] * m[3] * m[5];
        inv[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10]
            + m[4] * m[2] * m[9]
            + m[8] * m[1] * m[6]
            - m[8] * m[2] * m[5];
        let det = m[0] * inv[0] + m[1] * inv[4] + m[2] * inv[8] + m[3] * inv[12];
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        for x in inv.iter_mut() {
            *x /= det;
        }
        Some(Matrix::from_3d(&inv))
    }

    /// Geometry §5 "transform a point with a matrix": the point (a column vector) multiplied by the
    /// matrix.
    pub fn transform_point(&self, p: [f64; 4]) -> [f64; 4] {
        let mut out = [0.0; 4];
        for (r, x) in out.iter_mut().enumerate() {
            *x = (0..4).map(|c| self.m[c][r] * p[c]).sum();
        }
        out
    }

    /// Geometry §6.4 `isIdentity`.
    pub fn is_identity(&self) -> bool {
        (0..4).all(|i| (0..4).all(|j| self.m[i][j] == if i == j { 1.0 } else { 0.0 }))
    }

    /// The stringifier's text, or `None` when an element is not finite (InvalidStateError).
    pub fn to_css_string(self) -> Option<String> {
        let e = self.elements();
        if e.iter().any(|x| !x.is_finite()) {
            return None;
        }
        let list: Vec<f64> = if self.is_2d {
            vec![e[0], e[1], e[4], e[5], e[12], e[13]]
        } else {
            e.to_vec()
        };
        let parts: Vec<String> = list.into_iter().map(number_to_string).collect();
        Some(format!(
            "{}({})",
            if self.is_2d { "matrix" } else { "matrix3d" },
            parts.join(", ")
        ))
    }
}

/// sin and cos of an angle in degrees, exact at multiples of 90°.
fn exact_sin_cos(degrees: f64) -> (f64, f64) {
    let r = degrees % 360.0;
    let r = if r < 0.0 { r + 360.0 } else { r };
    if r == 0.0 {
        (0.0, 1.0)
    } else if r == 90.0 {
        (1.0, 0.0)
    } else if r == 180.0 {
        (0.0, -1.0)
    } else if r == 270.0 {
        (-1.0, 0.0)
    } else {
        let rad = degrees.to_radians();
        (rad.sin(), rad.cos())
    }
}

/// ECMAScript Number::toString(10) (ECMA-262 §6.1.6.1.20), which the geometry stringifier uses.
pub fn number_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v == 0.0 {
        return "0".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sign = if v < 0.0 { "-" } else { "" };
    // Rust's `{:e}` gives the shortest digits that round-trip, as ECMAScript's k does.
    let sci = format!("{:e}", v.abs());
    let (mantissa, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent digits");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign_e = if e < 0 { "-" } else { "+" };
        if k == 1 {
            format!("{digits}e{sign_e}{}", e.abs())
        } else {
            format!("{}.{}e{sign_e}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    format!("{sign}{body}")
}

// ----- parsing (Geometry §6.2 "parse a string into an abstract matrix") ------------------------------

/// Why a string is not a transform list the geometry interfaces accept (a SyntaxError).
#[derive(Debug, PartialEq)]
pub struct ParseError;

/// Parses `<transform-list> | none` with absolute lengths only: the matrix, 2D when every transform function
/// in it is. The empty string is the identity (whitespace alone is a syntax error).
pub fn parse_transform_list(text: &str) -> Result<Matrix, ParseError> {
    use cssparser::Parser;
    if text.is_empty() {
        return Ok(Matrix::identity());
    }
    let mut p = Parser::new(text);
    if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() {
        p.expect_exhausted().map_err(|_| ParseError)?;
        return Ok(Matrix::identity());
    }
    let mut result = Matrix::identity();
    let mut any = false;
    loop {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        let name = match p.next() {
            Ok(cssparser::Token::Function(name)) => name.to_ascii_lowercase(),
            _ => return Err(ParseError),
        };
        let f = p
            .parse_nested_block(|p| {
                transform_function(&name, p).map_err(|_| p.new_error_for_next_token::<()>())
            })
            .map_err(|_| ParseError)?;
        let is_2d = result.is_2d && f.is_2d;
        result = result.multiply(&f);
        result.is_2d = is_2d;
        any = true;
    }
    if !any {
        return Err(ParseError);
    }
    Ok(result)
}

#[derive(Clone, Copy)]
enum Arg {
    Number(f64),
    /// A length in px.
    Length(f64),
    /// An angle in degrees.
    Angle(f64),
}

fn args(p: &mut cssparser::Parser) -> Result<Vec<Arg>, ParseError> {
    use cssparser::Token;
    let mut out = Vec::new();
    loop {
        let arg = match p.next().map_err(|_| ParseError)?.clone() {
            Token::Number { value, .. } => Arg::Number(value as f64),
            Token::Dimension { value, unit, .. } => {
                let v = value as f64;
                match unit.to_ascii_lowercase().as_str() {
                    "px" => Arg::Length(v),
                    "cm" => Arg::Length(v * 96.0 / 2.54),
                    "mm" => Arg::Length(v * 96.0 / 25.4),
                    "q" => Arg::Length(v * 96.0 / 101.6),
                    "in" => Arg::Length(v * 96.0),
                    "pt" => Arg::Length(v * 96.0 / 72.0),
                    "pc" => Arg::Length(v * 16.0),
                    "deg" => Arg::Angle(v),
                    "rad" => Arg::Angle(v.to_degrees()),
                    "grad" => Arg::Angle(v * 0.9),
                    "turn" => Arg::Angle(v * 360.0),
                    // Relative lengths (em, vw, ...) and other units cannot be resolved here.
                    _ => return Err(ParseError),
                }
            }
            _ => return Err(ParseError),
        };
        out.push(arg);
        if p.is_exhausted() {
            return Ok(out);
        }
        p.expect_comma().map_err(|_| ParseError)?;
    }
}

/// A `<length-percentage>` argument without percentages: a length, or 0.
fn length(a: Arg) -> Result<f64, ParseError> {
    match a {
        Arg::Length(v) => Ok(v),
        Arg::Number(0.0) => Ok(0.0),
        _ => Err(ParseError),
    }
}

/// An `<angle>` (or the unitless 0, CSS Transforms 1 §11's legacy allowance).
fn angle(a: Arg) -> Result<f64, ParseError> {
    match a {
        Arg::Angle(v) => Ok(v),
        Arg::Number(0.0) => Ok(0.0),
        _ => Err(ParseError),
    }
}

fn number(a: Arg) -> Result<f64, ParseError> {
    match a {
        Arg::Number(v) => Ok(v),
        _ => Err(ParseError),
    }
}

fn transform_function(name: &str, p: &mut cssparser::Parser) -> Result<Matrix, ParseError> {
    let a = args(p)?;
    let n = a.len();
    let count = |lo: usize, hi: usize| {
        if (lo..=hi).contains(&n) {
            Ok(())
        } else {
            Err(ParseError)
        }
    };
    Ok(match name {
        "matrix" => {
            count(6, 6)?;
            let v: Vec<f64> = a.into_iter().map(number).collect::<Result<_, _>>()?;
            Matrix::from_2d(v[0], v[1], v[2], v[3], v[4], v[5])
        }
        "matrix3d" => {
            count(16, 16)?;
            let v: Vec<f64> = a.into_iter().map(number).collect::<Result<_, _>>()?;
            Matrix::from_3d(&v)
        }
        "translate" => {
            count(1, 2)?;
            let tx = length(a[0])?;
            let ty = a.get(1).map(|x| length(*x)).transpose()?.unwrap_or(0.0);
            Matrix::translation(tx, ty, 0.0)
        }
        "translatex" => {
            count(1, 1)?;
            Matrix::translation(length(a[0])?, 0.0, 0.0)
        }
        "translatey" => {
            count(1, 1)?;
            Matrix::translation(0.0, length(a[0])?, 0.0)
        }
        "translatez" => {
            count(1, 1)?;
            let mut m = Matrix::translation(0.0, 0.0, length(a[0])?);
            m.is_2d = false;
            m
        }
        "translate3d" => {
            count(3, 3)?;
            let mut m = Matrix::translation(length(a[0])?, length(a[1])?, length(a[2])?);
            m.is_2d = false;
            m
        }
        "scale" => {
            count(1, 2)?;
            let sx = number(a[0])?;
            let sy = a.get(1).map(|x| number(*x)).transpose()?.unwrap_or(sx);
            Matrix::scaling(sx, sy, 1.0)
        }
        "scalex" => {
            count(1, 1)?;
            Matrix::scaling(number(a[0])?, 1.0, 1.0)
        }
        "scaley" => {
            count(1, 1)?;
            Matrix::scaling(1.0, number(a[0])?, 1.0)
        }
        "scalez" => {
            count(1, 1)?;
            let mut m = Matrix::scaling(1.0, 1.0, number(a[0])?);
            m.is_2d = false;
            m
        }
        "scale3d" => {
            count(3, 3)?;
            let mut m = Matrix::scaling(number(a[0])?, number(a[1])?, number(a[2])?);
            m.is_2d = false;
            m
        }
        "rotate" | "rotatez" => {
            count(1, 1)?;
            let mut m = Matrix::rotation_z(angle(a[0])?);
            m.is_2d = name == "rotate";
            m
        }
        "rotatex" | "rotatey" => {
            count(1, 1)?;
            let deg = angle(a[0])?;
            let mut m = if name == "rotatex" {
                Matrix::rotation(1.0, 0.0, 0.0, deg)
            } else {
                Matrix::rotation(0.0, 1.0, 0.0, deg)
            };
            m.is_2d = false;
            m
        }
        "rotate3d" => {
            count(4, 4)?;
            let mut m = Matrix::rotation(number(a[0])?, number(a[1])?, number(a[2])?, angle(a[3])?);
            m.is_2d = false;
            m
        }
        "skew" => {
            count(1, 2)?;
            let ax = angle(a[0])?;
            let ay = a.get(1).map(|x| angle(*x)).transpose()?.unwrap_or(0.0);
            Matrix::skew(ax, ay)
        }
        "skewx" => {
            count(1, 1)?;
            Matrix::skew(angle(a[0])?, 0.0)
        }
        "skewy" => {
            count(1, 1)?;
            Matrix::skew(0.0, angle(a[0])?)
        }
        "perspective" => {
            count(1, 1)?;
            let d = length(a[0])?;
            if d < 0.0 {
                return Err(ParseError);
            }
            Matrix::perspective(d)
        }
        _ => return Err(ParseError),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_serialize_as_ecmascript_does() {
        for (v, s) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-2.5, "-2.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1e+21"),
            (123456789012345680000.0, "123456789012345680000"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (1.5e-10, "1.5e-10"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
        ] {
            assert_eq!(number_to_string(v), s, "{v}");
        }
    }

    #[test]
    fn transform_lists_parse_into_matrices() {
        let m = parse_transform_list("matrix(1, 2, 3, 4, 5, 6)").unwrap();
        assert!(m.is_2d);
        assert_eq!(m.to_css_string().unwrap(), "matrix(1, 2, 3, 4, 5, 6)");
        let m = parse_transform_list("translate(10px, 20px) scale(2)").unwrap();
        assert_eq!(m.to_css_string().unwrap(), "matrix(2, 0, 0, 2, 10, 20)");
        let m = parse_transform_list("translateZ(5px)").unwrap();
        assert!(!m.is_2d);
        assert_eq!(m.m[3][2], 5.0);
        assert!(parse_transform_list("").unwrap().is_identity());
        assert!(parse_transform_list("none").unwrap().is_identity());
        assert!(parse_transform_list("translate(1em)").is_err());
        assert!(parse_transform_list("translate(10%)").is_err());
        assert!(parse_transform_list("banana(1)").is_err());
        assert!(parse_transform_list("none none").is_err());
        let m = parse_transform_list("rotate(90deg)").unwrap();
        assert_eq!(m.to_css_string().unwrap(), "matrix(0, 1, -1, 0, 0, 0)");
    }

    #[test]
    fn inverse_and_points() {
        let mut m = Matrix::from_2d(2.0, 0.0, 0.0, 4.0, 10.0, 20.0);
        let p = m.transform_point([1.0, 1.0, 0.0, 1.0]);
        assert_eq!(p, [12.0, 24.0, 0.0, 1.0]);
        m.invert_self();
        assert_eq!(
            m.transform_point([12.0, 24.0, 0.0, 1.0]),
            [1.0, 1.0, 0.0, 1.0]
        );
        let mut s = Matrix::from_2d(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        s.invert_self();
        assert!(s.m[0][0].is_nan() && !s.is_2d);
        let t = Matrix::translation(1.0, 2.0, 3.0);
        let inv = t.inverse().unwrap();
        assert_eq!(t.multiply(&inv).elements(), Matrix::identity().elements());
    }

    #[test]
    fn methods_follow_the_2d_flag_rules() {
        let mut m = Matrix::identity();
        m.translate_self(1.0, 2.0, 0.0);
        assert!(m.is_2d);
        m.rotate_self(45.0, None, None);
        assert!(m.is_2d);
        m.rotate_self(10.0, Some(0.0), None);
        assert!(!m.is_2d);
        let mut s = Matrix::identity();
        s.scale_self(2.0, None, 1.0, 0.0, 0.0, 0.0);
        assert_eq!(s.m[1][1], 2.0);
        assert!(s.is_2d);
    }
}
