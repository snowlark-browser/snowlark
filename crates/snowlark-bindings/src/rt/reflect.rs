// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The reflection rules of HTML §2.6.1 and the microsyntaxes they parse with (HTML §2.3.4). The generated
//! getters and setters of `[Reflect]`-family and enumerated attributes call these over [`Reflect`].

const HTML_SPACE: &[char] = &[' ', '\t', '\n', '\u{c}', '\r'];
const INT_MAX: i64 = 2147483647;

/// The rules for parsing integers (HTML §2.3.4.1).
pub fn parse_integer(input: &str) -> Option<i64> {
    let s = input.trim_start_matches(HTML_SPACE);
    let (sign, s) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    // Values far outside the 32-bit range only need to compare as out of range.
    let mut v: i64 = 0;
    for b in s[..digits].bytes() {
        v = (v * 10 + (b - b'0') as i64).min(1 << 40);
    }
    Some(sign * v)
}

/// The rules for parsing non-negative integers.
pub fn parse_non_negative_integer(input: &str) -> Option<i64> {
    match parse_integer(input) {
        Some(v) if v >= 0 => Some(v),
        Some(0) => Some(0),
        _ => None,
    }
}

/// The rules for parsing floating-point number values (HTML §2.3.4.3).
pub fn parse_float(input: &str) -> Option<f64> {
    let s = input.trim_start_matches(HTML_SPACE);
    let b = s.as_bytes();
    let mut i = 0;
    let mut num = String::new();
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        if b[i] == b'-' {
            num.push('-');
        }
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    num.push_str(&s[int_start..i]);
    let mut had_digits = i > int_start;
    // A fraction; a '.' followed by an exponent marker goes on to the exponent.
    if i < b.len() && b[i] == b'.' {
        if b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            let frac_start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            num.push('.');
            num.push_str(&s[frac_start..i]);
            had_digits = true;
        } else if had_digits && matches!(b.get(i + 1), Some(b'e') | Some(b'E')) {
            i += 1;
        }
    }
    if !had_digits {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        let mut exp = String::from("e");
        if j < b.len() && (b[j] == b'-' || b[j] == b'+') {
            if b[j] == b'-' {
                exp.push('-');
            }
            j += 1;
        }
        let exp_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            exp.push_str(&s[exp_start..j]);
            num.push_str(&exp);
        }
    }
    if num == "-" || num.is_empty() {
        return None;
    }
    let v: f64 = num.parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    Some(if v == 0.0 { 0.0 } else { v })
}

/// The kinds of `unsigned long` reflection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsignedKind {
    Plain,
    /// `[ReflectPositive]`: limited to only positive numbers.
    Positive,
    /// `[ReflectPositiveWithFallback]`.
    PositiveWithFallback,
}

/// `long` getter.
pub fn long_get(value: Option<&str>, default: i32) -> i32 {
    match value.and_then(parse_integer) {
        Some(v) if (-INT_MAX - 1..=INT_MAX).contains(&v) => v as i32,
        _ => default,
    }
}

/// `long` `[ReflectNonNegative]` getter (default -1 unless given).
pub fn long_non_negative_get(value: Option<&str>, default: i32) -> i32 {
    match value.and_then(parse_non_negative_integer) {
        Some(v) if v <= INT_MAX => v as i32,
        _ => default,
    }
}

/// `unsigned long` getter, with an optional `[ReflectRange]` clamp.
pub fn unsigned_long_get(
    value: Option<&str>,
    kind: UnsignedKind,
    default: u32,
    range: Option<(u32, u32)>,
) -> u32 {
    let Some(v) = value.and_then(parse_non_negative_integer) else {
        return default;
    };
    if let Some((lo, hi)) = range {
        return v.clamp(lo as i64, hi as i64) as u32;
    }
    if v > INT_MAX || (kind != UnsignedKind::Plain && v == 0) {
        return default;
    }
    v as u32
}

/// `unsigned long` setter: the value to write, or `None` for the IndexSizeError of `[ReflectPositive]`.
pub fn unsigned_long_set(v: u32, kind: UnsignedKind, default: u32) -> Option<u32> {
    if kind == UnsignedKind::Positive && v == 0 {
        return None;
    }
    let positive = kind != UnsignedKind::Plain;
    if (v as i64) <= INT_MAX && !(positive && v == 0) {
        Some(v)
    } else {
        Some(default)
    }
}

/// `double` getter; `positive` for `[ReflectPositive]`.
pub fn double_get(value: Option<&str>, positive: bool, default: f64) -> f64 {
    match value.and_then(parse_float) {
        Some(v) if !positive || v > 0.0 => v,
        _ => default,
    }
}

/// An enumerated attribute's getter (HTML §2.3.3): the canonical keyword the value maps to (ASCII
/// case-insensitively), or the invalid value default, or the missing value default when absent.
/// `keywords` pairs each keyword with its canonical keyword.
pub fn enumerated_get(
    value: Option<&str>,
    keywords: &[(&str, &'static str)],
    missing: Option<&'static str>,
    invalid: Option<&'static str>,
) -> Option<&'static str> {
    let Some(v) = value else {
        return missing;
    };
    keywords
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(v))
        .map(|(_, c)| *c)
        .or(invalid)
}

/// The best representation of a number as a floating-point number, as HTML's reflection serializes
/// `double` values: ECMAScript Number::toString.
pub fn number_to_string(scope: &v8::PinScope<'_, '_>, v: f64) -> String {
    v8::Number::new(scope, v).to_rust_string_lossy(scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers() {
        assert_eq!(parse_integer("  42px"), Some(42));
        assert_eq!(parse_integer("-7"), Some(-7));
        assert_eq!(parse_integer("+3"), Some(3));
        assert_eq!(parse_integer("x"), None);
        assert_eq!(parse_non_negative_integer("-0"), Some(0));
        assert_eq!(parse_non_negative_integer("-1"), None);
        assert_eq!(long_get(Some("99999999999"), 5), 5);
    }

    #[test]
    fn floats() {
        assert_eq!(parse_float("1.5e2x"), Some(150.0));
        assert_eq!(parse_float(".5"), Some(0.5));
        assert_eq!(parse_float("+2"), Some(2.0));
        assert_eq!(parse_float("1."), Some(1.0));
        assert_eq!(parse_float("1e"), Some(1.0));
        assert_eq!(parse_float("abc"), None);
    }

    #[test]
    fn unsigned_rules() {
        use UnsignedKind::*;
        assert_eq!(unsigned_long_get(Some("0"), Positive, 1, None), 1);
        assert_eq!(
            unsigned_long_get(Some("5000"), Plain, 0, Some((1, 1000))),
            1000
        );
        assert_eq!(unsigned_long_set(0, Positive, 1), None);
        assert_eq!(unsigned_long_set(0, PositiveWithFallback, 2), Some(2));
        assert_eq!(unsigned_long_set(u32::MAX, Plain, 0), Some(0));
    }

    #[test]
    fn enumerated() {
        let kw = [("ltr", "ltr"), ("rtl", "rtl"), ("auto", "auto")];
        assert_eq!(
            enumerated_get(Some("RTL"), &kw, Some(""), Some("")),
            Some("rtl")
        );
        assert_eq!(enumerated_get(Some("x"), &kw, Some(""), Some("")), Some(""));
        assert_eq!(enumerated_get(None, &kw, None, Some("")), None);
    }
}
