// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SVG Animations (SMIL) values: clock values, and the animation function of an animation element
//! (§3.7, SMIL 3 §3.4: `values`, `from`/`to`/`by`, `calcMode`, `keyTimes`, `keySplines`, `additive`,
//! `accumulate`; the timing model is [`super::timegraph`]) over values interpolated as templates of
//! numbers: two values interpolate when they are the same text with
//! different numbers (lengths in one unit, lists, points, `viewBox`, path data with the same
//! commands), and switch discretely otherwise.

use super::parse::{fmt_f64, Scanner};

/// A clock value (SMIL 3 §5.4.1 "Clock values", as SVG Animations §3.6.7 uses it): a full
/// (`hh:mm:ss`) or partial (`mm:ss`) clock value, minutes and seconds two digits below 60 and an
/// optional fraction with digits, or a timecount (digits, an optional fraction, an optional `h`,
/// `min`, `s` or `ms` metric).
pub fn clock_value(s: &str) -> Option<f64> {
    let s = s.trim();
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    // `NN` or `NN.F+`, below 60 in the integer part.
    let two = |t: &str, fraction: bool| -> Option<f64> {
        let (int, frac) = match t.split_once('.') {
            Some((a, b)) if fraction => (a, Some(b)),
            Some(_) => return None,
            None => (t, None),
        };
        if int.len() != 2 || !digits(int) || frac.is_some_and(|f| !digits(f)) {
            return None;
        }
        let v: f64 = t.parse().ok()?;
        (v < 60.0).then_some(v)
    };
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        return match parts.as_slice() {
            [m, sec] => Some(two(m, false)? * 60.0 + two(sec, true)?),
            [h, m, sec] => {
                if !digits(h) {
                    return None;
                }
                let h: f64 = h.parse().ok()?;
                Some(h * 3600.0 + two(m, false)? * 60.0 + two(sec, true)?)
            }
            _ => None,
        };
    }
    let (num, scale) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix("min") {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600.0)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else {
        (s, 1.0)
    };
    let ok = match num.split_once('.') {
        Some((a, b)) => digits(a) && digits(b),
        None => digits(num),
    };
    if !ok {
        return None;
    }
    let v: f64 = num.parse().ok()?;
    v.is_finite().then_some(v * scale)
}

/// An offset value (SMIL 3 §5.4.1 "Offset values"): a clock value with an optional sign.
pub fn offset_value(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('-') {
        return clock_value(rest).map(|v| -v);
    }
    clock_value(s.strip_prefix('+').unwrap_or(s))
}

/// Where an animation is at a time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// The begin of the current (or last) interval.
    pub begin: f64,
    /// The progress through the simple duration, 0 to 1.
    pub progress: f64,
    /// The repeat iteration.
    pub iteration: u32,
}

/// A value as a template: the text between its numbers, and its numbers.
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub parts: Vec<String>,
    pub numbers: Vec<f64>,
}

impl Template {
    /// Splits a value into its numbers and the text between them (white space and commas
    /// normalized to one space).
    pub fn parse(s: &str) -> Template {
        let mut parts = vec![String::new()];
        let mut numbers = Vec::new();
        let mut sc = Scanner::new(s.trim());
        let mut prev_sep = false;
        while !sc.at_end() {
            let c = sc.peek().unwrap();
            // (a number starts at a digit, a dot, or a sign before one, after a non-letter)
            let starts = c.is_ascii_digit()
                || ((c == b'-' || c == b'+' || c == b'.')
                    && sc
                        .s
                        .get(sc.i + 1)
                        .is_some_and(|n| n.is_ascii_digit() || *n == b'.'));
            let after_letter = parts
                .last()
                .and_then(|p| p.chars().last())
                .is_some_and(|l| l.is_ascii_alphabetic() || l == '#');
            if starts && !(after_letter && c.is_ascii_digit() && !prev_sep) {
                if let Some(v) = sc.number() {
                    numbers.push(v);
                    parts.push(String::new());
                    prev_sep = false;
                    continue;
                }
            }
            sc.i += 1;
            let last = parts.last_mut().unwrap();
            if c == b',' || super::parse::is_wsp(c) {
                if !prev_sep {
                    last.push(' ');
                }
                prev_sep = true;
            } else {
                last.push(c as char);
                prev_sep = false;
            }
        }
        Template { parts, numbers }
    }

    /// Whether two templates differ only in their numbers.
    pub fn compatible(&self, other: &Template) -> bool {
        self.numbers.len() == other.numbers.len()
            && self
                .parts
                .iter()
                .zip(&other.parts)
                .all(|(a, b)| a.trim() == b.trim())
    }

    pub fn to_css(&self) -> String {
        let mut out = String::new();
        for (i, p) in self.parts.iter().enumerate() {
            out.push_str(p);
            if let Some(n) = self.numbers.get(i) {
                out.push_str(&fmt_f64(*n));
            }
        }
        out.trim().to_owned()
    }

    fn with_numbers(&self, numbers: Vec<f64>) -> Template {
        Template {
            parts: self.parts.clone(),
            numbers,
        }
    }
}

/// `a + (b - a) * t` over templates (discrete at the half when they do not interpolate).
pub fn interpolate(a: &str, b: &str, t: f64) -> String {
    let (ta, tb) = (Template::parse(a), Template::parse(b));
    if !ta.compatible(&tb) {
        return if t < 0.5 { a } else { b }.trim().to_owned();
    }
    let n = ta
        .numbers
        .iter()
        .zip(&tb.numbers)
        .map(|(x, y)| x + (y - x) * t)
        .collect();
    tb.with_numbers(n).to_css()
}

/// `a + b * k` over templates (`None` when they do not add up).
pub fn add(a: &str, b: &str, k: f64) -> Option<String> {
    let (ta, tb) = (Template::parse(a), Template::parse(b));
    if !ta.compatible(&tb) {
        return None;
    }
    let n = ta
        .numbers
        .iter()
        .zip(&tb.numbers)
        .map(|(x, y)| x + y * k)
        .collect();
    Some(tb.with_numbers(n).to_css())
}

/// The distance between two values (for `calcMode="paced"`): the Euclidean distance of their
/// numbers.
fn distance(a: &str, b: &str) -> f64 {
    let (ta, tb) = (Template::parse(a), Template::parse(b));
    if !ta.compatible(&tb) {
        return 0.0;
    }
    ta.numbers
        .iter()
        .zip(&tb.numbers)
        .map(|(x, y)| (y - x) * (y - x))
        .sum::<f64>()
        .sqrt()
}

/// The animation function of an `animate`, `set` or `animateTransform` element.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Animator {
    pub values: Vec<String>,
    /// A to-animation (only `to`): from the base value.
    pub to_animation: bool,
    /// A by-animation (`by` alone): additive.
    pub by_animation: bool,
    /// A from-by animation: the `by` value (the values are `from` and their sum).
    pub by: Option<String>,
    pub calc_mode: CalcMode,
    pub key_times: Vec<f64>,
    pub key_splines: Vec<[f64; 4]>,
    pub additive: bool,
    pub accumulate: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CalcMode {
    Discrete,
    #[default]
    Linear,
    Paced,
    Spline,
}

impl Animator {
    /// Reads the animation value attributes through `attr` (`set`: discrete `to`).
    pub fn parse(attr: impl Fn(&str) -> Option<String>, set: bool) -> Option<Animator> {
        let mut a = Animator {
            calc_mode: match attr("calcMode").as_deref().map(str::trim) {
                Some("discrete") => CalcMode::Discrete,
                Some("paced") => CalcMode::Paced,
                Some("spline") => CalcMode::Spline,
                _ => CalcMode::Linear,
            },
            additive: attr("additive").as_deref().map(str::trim) == Some("sum"),
            accumulate: attr("accumulate").as_deref().map(str::trim) == Some("sum"),
            ..Animator::default()
        };
        if set {
            a.values = vec![attr("to")?];
            a.calc_mode = CalcMode::Discrete;
            a.additive = false;
            a.accumulate = false;
            return Some(a);
        }
        if let Some(v) = attr("values") {
            // (SMIL Animation §3.2.2: a list of values; what follows the last separator is not one)
            a.values = v.split(';').map(|s| s.trim().to_owned()).collect();
            if a.values.last().is_some_and(|l| l.is_empty()) {
                a.values.pop();
            }
            if a.values.is_empty() {
                return None;
            }
        } else {
            match (attr("from"), attr("to"), attr("by")) {
                (Some(f), Some(t), _) => a.values = vec![f, t],
                (None, Some(t), _) => {
                    a.values = vec![t];
                    a.to_animation = true;
                    a.additive = false;
                }
                (Some(f), None, Some(b)) => {
                    let to = add(&f, &b, 1.0).unwrap_or_else(|| b.clone());
                    a.values = vec![f, to];
                    a.by = Some(b);
                }
                (None, None, Some(b)) => {
                    a.values = vec![b];
                    a.by_animation = true;
                    a.additive = true;
                }
                _ => return None,
            }
        }
        a.key_times = attr("keyTimes")
            .map(|v| {
                v.split(';')
                    .filter_map(|t| t.trim().parse::<f64>().ok())
                    .collect()
            })
            .unwrap_or_default();
        a.key_splines = attr("keySplines")
            .map(|v| {
                v.split(';')
                    .filter_map(|s| {
                        let n: Vec<f64> = s
                            .split(|c: char| c == ',' || c.is_ascii_whitespace())
                            .filter(|t| !t.is_empty())
                            .filter_map(|t| t.parse().ok())
                            .collect();
                        (n.len() == 4).then(|| [n[0], n[1], n[2], n[3]])
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(a)
    }

    /// Whether `keyTimes` or `keySplines` are in error for the value count (SMIL Animation §3.2.2:
    /// an animation in error has no effect).
    pub fn in_error(&self) -> bool {
        let n = if self.to_animation || self.by_animation {
            2
        } else {
            self.values.len()
        };
        !key_times_valid(self.calc_mode, &self.key_times, n)
            || (self.calc_mode == CalcMode::Spline && !key_splines_valid(&self.key_splines, n))
    }

    /// The keyframe offsets of the values (`keyTimes`, else even, else by distance when paced).
    pub fn offsets(&self) -> Vec<f64> {
        let n = self.values.len();
        if self.calc_mode == CalcMode::Paced {
            let d: Vec<f64> = self
                .values
                .windows(2)
                .map(|w| distance(&w[0], &w[1]))
                .collect();
            let total: f64 = d.iter().fold(0.0, |a, b| a + b);
            if total > 0.0 {
                let mut at = 0.0;
                let mut out = vec![0.0];
                for seg in d {
                    at += seg / total;
                    out.push(at.min(1.0));
                }
                return out;
            }
        } else if self.key_times.len() == n {
            return self.key_times.clone();
        }
        let steps = if self.calc_mode == CalcMode::Discrete {
            n
        } else {
            n.saturating_sub(1).max(1)
        };
        (0..n).map(|i| i as f64 / steps as f64).collect()
    }

    /// The animated value at `sample` over `base` (the underlying value), per the sandwich
    /// position's `underlying` value.
    pub fn value(&self, sample: &Sample, underlying: &str) -> String {
        let p = sample.progress.clamp(0.0, 1.0);
        let values: Vec<String> = if self.to_animation {
            vec![underlying.to_owned(), self.values[0].clone()]
        } else if self.by_animation {
            // (from zero: the by value scaled)
            let zero = Template::parse(&self.values[0]);
            let z = zero.with_numbers(vec![0.0; zero.numbers.len()]).to_css();
            vec![z, self.values[0].clone()]
        } else {
            self.values.clone()
        };
        let mut v = self.interpolate_values(&values, p);
        // Accumulation (SMIL 3 §3.4.4): the end value added per completed iteration.
        if self.accumulate && sample.iteration > 0 && !self.to_animation {
            let last = values.last().cloned().unwrap_or_default();
            if let Some(acc) = add(&v, &last, sample.iteration as f64) {
                v = acc;
            }
        }
        if self.additive && !self.to_animation {
            if let Some(sum) = add(underlying, &v, 1.0) {
                return sum;
            }
        }
        v
    }

    fn interpolate_values(&self, values: &[String], p: f64) -> String {
        let n = values.len();
        if n == 1 {
            return values[0].clone();
        }
        let mode = if self.calc_mode != CalcMode::Discrete
            && values
                .windows(2)
                .any(|w| !Template::parse(&w[0]).compatible(&Template::parse(&w[1])))
        {
            CalcMode::Discrete
        } else {
            self.calc_mode
        };
        match mode {
            CalcMode::Discrete => {
                let i = if self.key_times.len() == n {
                    self.key_times.iter().rposition(|k| *k <= p).unwrap_or(0)
                } else {
                    ((p * n as f64).floor() as usize).min(n - 1)
                };
                // (the end of the simple duration shows the last value)
                let i = if p >= 1.0 { n - 1 } else { i };
                values[i].clone()
            }
            CalcMode::Paced => {
                let d: Vec<f64> = values.windows(2).map(|w| distance(&w[0], &w[1])).collect();
                let total: f64 = d.iter().sum();
                if total <= 0.0 {
                    return values[0].clone();
                }
                let mut target = p * total;
                for (i, seg) in d.iter().enumerate() {
                    if target <= *seg || i == d.len() - 1 {
                        let t = if *seg > 0.0 {
                            (target / seg).min(1.0)
                        } else {
                            0.0
                        };
                        return interpolate(&values[i], &values[i + 1], t);
                    }
                    target -= seg;
                }
                values[n - 1].clone()
            }
            CalcMode::Linear | CalcMode::Spline => {
                let times: Vec<f64> = if self.key_times.len() == n {
                    self.key_times.clone()
                } else {
                    (0..n).map(|i| i as f64 / (n - 1) as f64).collect()
                };
                let i = (0..n - 1).rev().find(|&i| times[i] <= p).unwrap_or(0);
                let span = times[i + 1] - times[i];
                let mut t = if span > 0.0 {
                    (p - times[i]) / span
                } else {
                    1.0
                };
                t = t.clamp(0.0, 1.0);
                if mode == CalcMode::Spline {
                    if let Some(s) = self.key_splines.get(i) {
                        t = cubic_bezier(s, t);
                    }
                }
                interpolate(&values[i], &values[i + 1], t)
            }
        }
    }
}

/// SMIL Animation §3.2.3 `keyTimes`: as many as there are values, each within 0 to 1 and not below
/// the one before, the first 0, and for linear and spline animation the last 1 (paced ignores them).
pub fn key_times_valid(mode: CalcMode, times: &[f64], n: usize) -> bool {
    if times.is_empty() || mode == CalcMode::Paced {
        return true;
    }
    times.len() == n
        && times[0] == 0.0
        && times.iter().all(|t| (0.0..=1.0).contains(t))
        && times.windows(2).all(|w| w[0] <= w[1])
        && (mode == CalcMode::Discrete || times[n - 1] == 1.0)
}

/// SMIL Animation §3.2.3 `keySplines`: one set fewer than values, each within 0 to 1.
pub fn key_splines_valid(splines: &[[f64; 4]], n: usize) -> bool {
    splines.len() + 1 == n && splines.iter().flatten().all(|v| (0.0..=1.0).contains(v))
}

/// The y of a unit cubic Bézier timing function at x.
pub fn cubic_bezier(s: &[f64; 4], x: f64) -> f64 {
    let [x1, y1, x2, y2] = *s;
    let bez = |a: f64, b: f64, t: f64| {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    };
    // (bisection on x)
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..50 {
        let mid = (lo + hi) / 2.0;
        if bez(x1, x2, mid) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bez(y1, y2, (lo + hi) / 2.0)
}

/// An `animateTransform` value of `type` (`translate`, `scale`, `rotate`, `skewX`, `skewY`) as a
/// transform function.
pub fn transform_function(kind: &str, value: &str) -> String {
    let t = Template::parse(value);
    let nums: Vec<String> = t.numbers.iter().map(|n| fmt_f64(*n)).collect();
    format!("{kind}({})", nums.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values() {
        let attrs = |a: &str| -> Option<String> {
            match a {
                "values" => Some("0; 30; 20".into()),
                "accumulate" => Some("sum".into()),
                _ => None,
            }
        };
        let anim = Animator::parse(attrs, false).unwrap();
        let at = |progress: f64, iteration: u32| {
            anim.value(
                &Sample {
                    begin: 0.0,
                    progress,
                    iteration,
                },
                "20",
            )
        };
        assert_eq!(at(0.0, 0), "0");
        assert_eq!(at(0.5, 0), "30");
        assert_eq!(at(1.0, 0), "20");
        assert_eq!(at(0.5, 1), "50");
        assert_eq!(at(1.0, 4), "100");
        assert_eq!(interpolate("10px", "20px", 0.5), "15px");
        assert_eq!(interpolate("10px", "2em", 0.25), "10px");
        assert_eq!(
            interpolate("M 0 0 L 10 10", "M 10 10 L 20 20", 0.5),
            "M 5 5 L 15 15"
        );
        assert_eq!(clock_value("1.5s"), Some(1.5));
        assert_eq!(clock_value("00:01.5"), Some(1.5));
        assert_eq!(clock_value("200ms"), Some(0.2));
        assert_eq!(clock_value("99:01"), None);
        assert_eq!(clock_value("5:30"), None);
        assert_eq!(clock_value("00:59:9.9"), None);
        assert_eq!(clock_value("1."), None);
        assert_eq!(offset_value("-1s"), Some(-1.0));
    }
}
