// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lexical layer of SVG attribute values (SVG 2 §4.2 and the path data grammar §9.3.9):
//! white space, `comma-wsp`, numbers with CSS's syntax, number lists and point lists.

/// SVG white space (`wsp`): tab, space, line feed, form feed, carriage return.
#[inline]
pub fn is_wsp(b: u8) -> bool {
    matches!(b, b'\t' | b' ' | b'\n' | b'\x0c' | b'\r')
}

/// A cursor over an attribute value's bytes.
#[derive(Clone, Debug)]
pub struct Scanner<'a> {
    pub s: &'a [u8],
    pub i: usize,
}

impl<'a> Scanner<'a> {
    pub fn new(s: &'a str) -> Scanner<'a> {
        Scanner {
            s: s.as_bytes(),
            i: 0,
        }
    }

    #[inline]
    pub fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }

    #[inline]
    pub fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    pub fn skip_wsp(&mut self) {
        while self.peek().is_some_and(is_wsp) {
            self.i += 1;
        }
    }

    /// `comma-wsp?`: white space, at most one comma, white space. Returns whether a comma was
    /// consumed.
    pub fn skip_comma_wsp(&mut self) -> bool {
        self.skip_wsp();
        let comma = self.peek() == Some(b',');
        if comma {
            self.i += 1;
            self.skip_wsp();
        }
        comma
    }

    /// Consumes `c` if it is next.
    pub fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// A number with CSS's syntax (sign, digits, fraction, exponent), as the path grammar's
    /// `coordinate` reads it: as much as matches, so `100-200` is two numbers and `0.6.5` is
    /// `0.6` and `.5`. A trailing `.` without digits is not part of a number.
    pub fn number(&mut self) -> Option<f64> {
        let start = self.i;
        let s = self.s;
        let mut i = self.i;
        if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
            i += 1;
        }
        let int_start = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        let mut digits = i > int_start;
        if i + 1 < s.len() && s[i] == b'.' && s[i + 1].is_ascii_digit() {
            i += 1;
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
            digits = true;
        }
        if !digits {
            self.i = start;
            return None;
        }
        // An exponent only when digits follow (`1em` is a number and a unit).
        if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
            let mut j = i + 1;
            if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
                j += 1;
            }
            if j < s.len() && s[j].is_ascii_digit() {
                while j < s.len() && s[j].is_ascii_digit() {
                    j += 1;
                }
                i = j;
            }
        }
        let text = std::str::from_utf8(&s[start..i]).ok()?;
        let v: f64 = text.parse().ok()?;
        self.i = i;
        v.is_finite().then_some(v)
    }

    /// A path data `flag`: one `0` or `1` character.
    pub fn flag(&mut self) -> Option<bool> {
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                Some(false)
            }
            Some(b'1') => {
                self.i += 1;
                Some(true)
            }
            _ => None,
        }
    }

    /// The rest of the input.
    pub fn rest(&self) -> &'a [u8] {
        &self.s[self.i.min(self.s.len())..]
    }
}

/// A `<number>` alone (leading and trailing white space allowed), as `float` content attributes
/// hold it.
pub fn parse_number(s: &str) -> Option<f64> {
    let mut sc = Scanner::new(s);
    sc.skip_wsp();
    let v = sc.number()?;
    sc.skip_wsp();
    sc.at_end().then_some(v)
}

/// A `<number-optional-number>` (`stdDeviation`, `radius`, `order`...): one or two numbers.
pub fn parse_number_optional_number(s: &str) -> Option<(f64, Option<f64>)> {
    let list = parse_number_list(s)?;
    match list.as_slice() {
        [a] => Some((*a, None)),
        [a, b] => Some((*a, Some(*b))),
        _ => None,
    }
}

/// A `<list-of-numbers>` (numbers separated by `comma-wsp`); `None` when the value is in error
/// (a trailing comma, a token that is not a number).
pub fn parse_number_list(s: &str) -> Option<Vec<f64>> {
    let mut sc = Scanner::new(s);
    let mut out = Vec::new();
    sc.skip_wsp();
    while !sc.at_end() {
        out.push(sc.number()?);
        let comma = sc.skip_comma_wsp();
        if comma && sc.at_end() {
            return None;
        }
    }
    Some(out)
}

/// The `points` attribute (SVG 2 §10.7): its coordinate pairs, an unpaired last coordinate
/// dropped; `None` when a token is not a number (the value is invalid and the initial value, the
/// empty list, applies).
/// A URL fragment percent-decoded: the id it names.
pub fn percent_decoded(fragment: &str) -> String {
    let b = fragment.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_points(s: &str) -> Option<Vec<(f64, f64)>> {
    let mut sc = Scanner::new(s);
    let mut coords = Vec::new();
    sc.skip_wsp();
    while !sc.at_end() {
        coords.push(sc.number()?);
        sc.skip_comma_wsp();
    }
    Some(
        coords
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[x, y]| (*x, *y))
            .collect(),
    )
}

/// A number as SVG serializes it: the shortest string that parses back to the same `float`, with
/// no exponent and no negative zero.
pub fn fmt_number(v: f32) -> String {
    if v == 0.0 || !v.is_finite() {
        return "0".into();
    }
    format!("{v}")
}

/// [`fmt_number`] for a value kept as `f64` (rounded to `float` first, as the IDL attributes are).
pub fn fmt_f64(v: f64) -> String {
    fmt_number(v as f32)
}

/// Splits on SVG white space (the token lists of `requiredExtensions`, `systemLanguage` use
/// commas too: see [`split_string_list`]).
pub fn split_wsp(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c.is_ascii() && is_wsp(c as u8))
        .filter(|t| !t.is_empty())
}

/// The items of an `SVGStringList` attribute: `systemLanguage` is a set of comma-separated tokens
/// (SVG 2 §5.6.5), the others are white space separated.
pub fn split_string_list(s: &str, commas: bool) -> Vec<String> {
    if commas {
        // (HTML "split a string on commas": empty tokens count, "" is one empty token)
        s.split(',')
            .map(|t| t.trim_matches(|c: char| c.is_ascii() && is_wsp(c as u8)))
            .map(str::to_owned)
            .collect()
    } else {
        split_wsp(s).map(str::to_owned).collect()
    }
}
