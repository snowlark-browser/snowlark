// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Typed OM Level 1, the parts that need no platform objects: numeric values and their types
//! (§4.3: "create a type", adding and multiplying types, sum values, `to()`, `toSum()`, the arithmetic
//! operations), their serialization (§6.3-6.5), and the reification (§5) of specified and computed
//! values through stylo's `ToTyped` into owned trees the DOM side turns into objects.

use std::collections::BTreeMap;

use style::properties::{ComputedValues, PropertyId};
use style::typed_om::{
    ImageValue, NumericValue, PerspectiveValue, TransformComponent, TypedValue, TypedValueList,
    UnparsedSegment,
};
use style_traits::ToCss;

/// A base type of the numeric type of a value (§4.3.2), in the order of the spec's
/// `CSSNumericBaseType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Base {
    Length,
    Angle,
    Time,
    Frequency,
    Resolution,
    Flex,
    Percent,
}

pub const BASES: [Base; 7] = [
    Base::Length,
    Base::Angle,
    Base::Time,
    Base::Frequency,
    Base::Resolution,
    Base::Flex,
    Base::Percent,
];

impl Base {
    pub fn name(self) -> &'static str {
        match self {
            Base::Length => "length",
            Base::Angle => "angle",
            Base::Time => "time",
            Base::Frequency => "frequency",
            Base::Resolution => "resolution",
            Base::Flex => "flex",
            Base::Percent => "percent",
        }
    }

    pub fn from_name(name: &str) -> Option<Base> {
        BASES.into_iter().find(|b| b.name() == name)
    }
}

/// What a unit is: its base type (none for "number") and, for units convertible to others, the
/// canonical unit of its set of compatible units with the ratio of one of it to the canonical unit.
struct UnitKind {
    base: Option<Base>,
    canonical: Option<(&'static str, f64)>,
}

fn unit_kind(unit: &str) -> Option<UnitKind> {
    let (base, canonical) = match unit {
        "number" => (None, None),
        "percent" => (Some(Base::Percent), None),
        // (absolute lengths: CSS Values 4 §6.2)
        "px" => (Some(Base::Length), Some(("px", 1.0))),
        "cm" => (Some(Base::Length), Some(("px", 96.0 / 2.54))),
        "mm" => (Some(Base::Length), Some(("px", 96.0 / 25.4))),
        "q" => (Some(Base::Length), Some(("px", 96.0 / 101.6))),
        "in" => (Some(Base::Length), Some(("px", 96.0))),
        "pt" => (Some(Base::Length), Some(("px", 96.0 / 72.0))),
        "pc" => (Some(Base::Length), Some(("px", 16.0))),
        // (relative lengths: compatible only with themselves)
        "em" | "ex" | "ch" | "ic" | "cap" | "lh" | "rem" | "rex" | "rch" | "ric" | "rcap"
        | "rlh" | "vw" | "vh" | "vi" | "vb" | "vmin" | "vmax" | "svw" | "svh" | "svi" | "svb"
        | "svmin" | "svmax" | "lvw" | "lvh" | "lvi" | "lvb" | "lvmin" | "lvmax" | "dvw" | "dvh"
        | "dvi" | "dvb" | "dvmin" | "dvmax" | "cqw" | "cqh" | "cqi" | "cqb" | "cqmin" | "cqmax" => {
            (Some(Base::Length), None)
        }
        "deg" => (Some(Base::Angle), Some(("deg", 1.0))),
        "grad" => (Some(Base::Angle), Some(("deg", 0.9))),
        "rad" => (
            Some(Base::Angle),
            Some(("deg", 180.0 / std::f64::consts::PI)),
        ),
        "turn" => (Some(Base::Angle), Some(("deg", 360.0))),
        "s" => (Some(Base::Time), Some(("s", 1.0))),
        "ms" => (Some(Base::Time), Some(("s", 0.001))),
        "hz" => (Some(Base::Frequency), Some(("hz", 1.0))),
        "khz" => (Some(Base::Frequency), Some(("hz", 1000.0))),
        "dppx" | "x" => (Some(Base::Resolution), Some(("dppx", 1.0))),
        "dpi" => (Some(Base::Resolution), Some(("dppx", 1.0 / 96.0))),
        "dpcm" => (Some(Base::Resolution), Some(("dppx", 2.54 / 96.0))),
        "fr" => (Some(Base::Flex), None),
        _ => return None,
    };
    Some(UnitKind { base, canonical })
}

/// A unit as `CSSUnitValue.unit` holds it: ASCII lowercase (units are case-insensitive), "%" as
/// "percent". `None` when it is not a unit.
pub fn normalize_unit(unit: &str) -> Option<String> {
    let lower = if unit == "%" {
        "percent".to_owned()
    } else {
        unit.to_ascii_lowercase()
    };
    unit_kind(&lower).map(|_| lower)
}

/// Converts `value` of `from` to `to` ("convert a CSSUnitValue"), if the units are compatible.
fn convert(value: f64, from: &str, to: &str) -> Option<f64> {
    if from == to {
        return Some(value);
    }
    let (a, b) = (unit_kind(from)?, unit_kind(to)?);
    match (a.canonical, b.canonical) {
        (Some((ca, ra)), Some((cb, rb))) if ca == cb => Some(value * ra / rb),
        _ => None,
    }
}

/// A numeric type (§4.3.2): the exponent of each base type, zero for those absent, and the
/// percent hint.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NumType {
    pub exp: [i32; 7],
    pub hint: Option<Base>,
}

impl NumType {
    /// "Create a type" from a unit (lowercase).
    pub fn from_unit(unit: &str) -> Option<NumType> {
        let kind = unit_kind(unit)?;
        let mut t = NumType::default();
        if let Some(base) = kind.base {
            t.exp[base as usize] = 1;
        }
        Some(t)
    }

    /// "Apply the percent hint".
    fn apply_hint(&mut self, hint: Base) {
        self.hint = Some(hint);
        if hint != Base::Percent {
            let p = Base::Percent as usize;
            self.exp[hint as usize] += self.exp[p];
            self.exp[p] = 0;
        }
    }

    fn reconcile_hints(a: &mut NumType, b: &mut NumType) -> Option<()> {
        match (a.hint, b.hint) {
            (Some(x), Some(y)) if x != y => return None,
            (Some(x), None) => b.apply_hint(x),
            (None, Some(y)) => a.apply_hint(y),
            _ => {}
        }
        Some(())
    }

    /// "Add two types".
    pub fn add(a: &NumType, b: &NumType) -> Option<NumType> {
        let (mut t1, mut t2) = (a.clone(), b.clone());
        NumType::reconcile_hints(&mut t1, &mut t2)?;
        if t1.exp == t2.exp {
            return Some(t1);
        }
        let p = Base::Percent as usize;
        let has_percent = t1.exp[p] != 0 || t2.exp[p] != 0;
        let has_other = |t: &NumType| (0..p).any(|i| t.exp[i] != 0);
        if has_percent && (has_other(&t1) || has_other(&t2)) {
            for hint in &BASES[..p] {
                let (mut p1, mut p2) = (t1.clone(), t2.clone());
                p1.apply_hint(*hint);
                p2.apply_hint(*hint);
                if p1.exp == p2.exp {
                    return Some(p1);
                }
            }
        }
        None
    }

    /// "Multiply two types".
    pub fn multiply(a: &NumType, b: &NumType) -> Option<NumType> {
        let (mut t1, mut t2) = (a.clone(), b.clone());
        NumType::reconcile_hints(&mut t1, &mut t2)?;
        for i in 0..7 {
            t1.exp[i] += t2.exp[i];
        }
        Some(t1)
    }

    /// "Invert a type".
    pub fn invert(&self) -> NumType {
        NumType {
            exp: self.exp.map(|e| -e),
            hint: self.hint,
        }
    }

    /// Whether the type matches `<number>`.
    pub fn is_number(&self) -> bool {
        self.exp == [0; 7] && self.hint.is_none()
    }

    /// Whether the only non-zero entry is `base` with exponent 1 (with a percent hint allowed,
    /// as for `<length-percentage>` when `percent_ok`).
    pub fn matches(&self, base: Base, percent_ok: bool) -> bool {
        let mut want = [0; 7];
        want[base as usize] = 1;
        self.exp == want && (self.hint.is_none() || (percent_ok && self.hint == Some(base)))
    }
}

fn add_all<'a>(mut types: impl Iterator<Item = &'a NumType>) -> Option<NumType> {
    let first = types.next()?.clone();
    types.try_fold(first, |acc, t| NumType::add(&acc, t))
}

fn multiply_all<'a>(mut types: impl Iterator<Item = &'a NumType>) -> Option<NumType> {
    let first = types.next()?.clone();
    types.try_fold(first, |acc, t| NumType::multiply(&acc, t))
}

/// A `CSSNumericValue` as a tree (the DOM objects are made from it and read into it).
#[derive(Clone, Debug, PartialEq)]
pub enum Num {
    /// A `CSSUnitValue`: its value and (lowercase) unit.
    Unit(f64, String),
    Sum(Vec<Num>),
    Product(Vec<Num>),
    Negate(Box<Num>),
    Invert(Box<Num>),
    Min(Vec<Num>),
    Max(Vec<Num>),
    Clamp(Box<[Num; 3]>),
}

/// The exception an operation on numeric values throws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumError {
    Type,
    Syntax,
    Range,
}

/// One item of a sum value: a value and a unit map (canonical units to their powers).
type SumItem = (f64, BTreeMap<String, i32>);

impl Num {
    pub fn number(v: f64) -> Num {
        Num::Unit(v, "number".into())
    }

    pub fn px(v: f64) -> Num {
        Num::Unit(v, "px".into())
    }

    /// The value's numeric type; `None` when its parts cannot be combined.
    pub fn ty(&self) -> Option<NumType> {
        match self {
            Num::Unit(_, u) => NumType::from_unit(u),
            Num::Sum(v) | Num::Min(v) | Num::Max(v) => {
                let types = v.iter().map(Num::ty).collect::<Option<Vec<_>>>()?;
                add_all(types.iter())
            }
            Num::Product(v) => {
                let types = v.iter().map(Num::ty).collect::<Option<Vec<_>>>()?;
                multiply_all(types.iter())
            }
            Num::Negate(v) => v.ty(),
            Num::Invert(v) => v.ty().map(|t| t.invert()),
            Num::Clamp(v) => {
                let types = v.iter().map(Num::ty).collect::<Option<Vec<_>>>()?;
                add_all(types.iter())
            }
        }
    }

    /// "Create a sum value".
    fn sum_value(&self) -> Option<Vec<SumItem>> {
        match self {
            Num::Unit(v, u) => {
                let (mut value, mut unit) = (*v, u.as_str());
                if let Some((canonical, ratio)) = unit_kind(u)?.canonical {
                    value *= ratio;
                    unit = canonical;
                }
                let mut map = BTreeMap::new();
                if unit != "number" {
                    map.insert(unit.to_owned(), 1);
                }
                Some(vec![(value, map)])
            }
            Num::Sum(items) => {
                let mut values: Vec<SumItem> = Vec::new();
                for item in items {
                    for (v, map) in item.sum_value()? {
                        match values.iter_mut().find(|(_, m)| *m == map) {
                            Some(existing) => existing.0 += v,
                            None => values.push((v, map)),
                        }
                    }
                }
                let types = values
                    .iter()
                    .map(|(_, m)| type_of_unit_map(m))
                    .collect::<Option<Vec<_>>>()?;
                add_all(types.iter())?;
                Some(values)
            }
            Num::Negate(v) => {
                let mut values = v.sum_value()?;
                for (v, _) in &mut values {
                    *v = -*v;
                }
                Some(values)
            }
            Num::Product(items) => {
                let mut values: Vec<SumItem> = vec![(1.0, BTreeMap::new())];
                for item in items {
                    let new = item.sum_value()?;
                    let mut temp = Vec::new();
                    for (v1, m1) in &values {
                        for (v2, m2) in &new {
                            let mut map = m1.clone();
                            for (u, p) in m2 {
                                *map.entry(u.clone()).or_insert(0) += p;
                            }
                            map.retain(|_, p| *p != 0);
                            temp.push((v1 * v2, map));
                        }
                    }
                    values = temp;
                }
                Some(values)
            }
            Num::Invert(v) => {
                let mut values = v.sum_value()?;
                if values.len() != 1 {
                    return None;
                }
                let (value, map) = &mut values[0];
                *value = 1.0 / *value;
                for p in map.values_mut() {
                    *p = -*p;
                }
                Some(values)
            }
            Num::Min(items) | Num::Max(items) => {
                let args = items
                    .iter()
                    .map(single_sum_item)
                    .collect::<Option<Vec<_>>>()?;
                let first_map = &args.first()?.1;
                if args.iter().any(|(_, m)| m != first_map) {
                    return None;
                }
                let pick = if matches!(self, Num::Min(_)) {
                    args.iter()
                        .cloned()
                        .reduce(|a, b| if b.0 < a.0 { b } else { a })
                } else {
                    args.iter()
                        .cloned()
                        .reduce(|a, b| if b.0 > a.0 { b } else { a })
                }?;
                Some(vec![pick])
            }
            Num::Clamp(v) => {
                let [lower, value, upper] = [
                    single_sum_item(&v[0])?,
                    single_sum_item(&v[1])?,
                    single_sum_item(&v[2])?,
                ];
                if lower.1 != value.1 || value.1 != upper.1 {
                    return None;
                }
                let clamped = value.0.min(upper.0).max(lower.0);
                Some(vec![(clamped, value.1)])
            }
        }
    }

    /// `to(unit)`.
    pub fn to(&self, unit: &str) -> Result<Num, NumError> {
        let unit = normalize_unit(unit).ok_or(NumError::Syntax)?;
        let sum = self.sum_value().ok_or(NumError::Type)?;
        if sum.len() != 1 {
            return Err(NumError::Type);
        }
        let (value, from) = unit_value_from_sum_item(&sum[0]).ok_or(NumError::Type)?;
        convert(value, &from, &unit)
            .map(|v| Num::Unit(v, unit))
            .ok_or(NumError::Type)
    }

    /// `toSum(...units)`.
    pub fn to_sum(&self, units: &[String]) -> Result<Num, NumError> {
        let units = units
            .iter()
            .map(|u| normalize_unit(u).ok_or(NumError::Syntax))
            .collect::<Result<Vec<_>, _>>()?;
        // (units whose types cannot be added cannot make one sum)
        let types = units
            .iter()
            .filter_map(|u| NumType::from_unit(u))
            .collect::<Vec<_>>();
        if !types.is_empty() && add_all(types.iter()).is_none() {
            return Err(NumError::Type);
        }
        let sum = self.sum_value().ok_or(NumError::Type)?;
        let mut values = sum
            .iter()
            .map(unit_value_from_sum_item)
            .collect::<Option<Vec<_>>>()
            .ok_or(NumError::Type)?;
        if units.is_empty() {
            values.sort_by(|a, b| a.1.cmp(&b.1));
            return Ok(Num::Sum(
                values.into_iter().map(|(v, u)| Num::Unit(v, u)).collect(),
            ));
        }
        let mut result = Vec::new();
        for unit in units {
            let mut temp = 0.0;
            values.retain(|(v, u)| match convert(*v, u, &unit) {
                Some(c) => {
                    temp += c;
                    false
                }
                None => true,
            });
            result.push(Num::Unit(temp, unit));
        }
        if !values.is_empty() {
            return Err(NumError::Type);
        }
        Ok(Num::Sum(result))
    }

    /// "Negate a CSSNumericValue".
    pub fn negate(self) -> Num {
        match self {
            Num::Negate(v) => *v,
            Num::Unit(v, u) => Num::Unit(-v, u),
            other => Num::Negate(Box::new(other)),
        }
    }

    /// "Invert a CSSNumericValue".
    pub fn invert(self) -> Result<Num, NumError> {
        match self {
            Num::Invert(v) => Ok(*v),
            Num::Unit(v, u) if u == "number" => {
                if v == 0.0 {
                    Err(NumError::Range)
                } else {
                    Ok(Num::number(1.0 / v))
                }
            }
            other => Ok(Num::Invert(Box::new(other))),
        }
    }

    /// `add(...values)`.
    pub fn add_values(self, values: Vec<Num>) -> Result<Num, NumError> {
        let mut all = match self {
            Num::Sum(v) => v,
            other => vec![other],
        };
        all.extend(values);
        if let Some(unit) = same_unit(&all) {
            let sum = all.iter().fold(0.0, |acc, n| acc + unit_value(n));
            return Ok(Num::Unit(sum, unit));
        }
        Num::Sum(all).checked()
    }

    /// `sub(...values)`.
    pub fn sub_values(self, values: Vec<Num>) -> Result<Num, NumError> {
        self.add_values(values.into_iter().map(Num::negate).collect())
    }

    /// `mul(...values)`.
    pub fn mul_values(self, values: Vec<Num>) -> Result<Num, NumError> {
        let mut all = match self {
            Num::Product(v) => v,
            other => vec![other],
        };
        all.extend(values);
        if all.iter().all(|n| matches!(n, Num::Unit(..))) {
            let non_number: Vec<&Num> = all
                .iter()
                .filter(|n| !matches!(n, Num::Unit(_, u) if u == "number"))
                .collect();
            if non_number.len() <= 1 {
                let unit = match non_number.first() {
                    Some(Num::Unit(_, u)) => u.clone(),
                    _ => "number".to_owned(),
                };
                let product = all.iter().fold(1.0, |acc, n| acc * unit_value(n));
                return Ok(Num::Unit(product, unit));
            }
        }
        Num::Product(all).checked()
    }

    /// `div(...values)`.
    pub fn div_values(self, values: Vec<Num>) -> Result<Num, NumError> {
        let inverted = values
            .into_iter()
            .map(Num::invert)
            .collect::<Result<Vec<_>, _>>()?;
        self.mul_values(inverted)
    }

    /// `min(...values)` and `max(...values)`.
    pub fn min_max(self, values: Vec<Num>, max: bool) -> Result<Num, NumError> {
        let mut all = match (self, max) {
            (Num::Min(v), false) | (Num::Max(v), true) => v,
            (other, _) => vec![other],
        };
        all.extend(values);
        if let Some(unit) = same_unit(&all) {
            let values = all.iter().map(unit_value);
            let pick = if max {
                values.fold(f64::NEG_INFINITY, f64::max)
            } else {
                values.fold(f64::INFINITY, f64::min)
            };
            return Ok(Num::Unit(pick, unit));
        }
        if max {
            Num::Max(all).checked()
        } else {
            Num::Min(all).checked()
        }
    }

    /// The value when its type is valid, a TypeError otherwise (the math value constructors and
    /// operations).
    pub fn checked(self) -> Result<Num, NumError> {
        match self.ty() {
            Some(_) => Ok(self),
            None => Err(NumError::Type),
        }
    }

    /// §6.3 "serialize a CSSNumericValue".
    pub fn serialize(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, false, false);
        s
    }

    /// Serialized with `minimum` (a `CSSUnitValue` below it is wrapped in `calc()`).
    pub fn serialize_with_min(&self, minimum: f64) -> String {
        match self {
            Num::Unit(v, _) if *v < minimum => format!("calc({})", self.serialize()),
            _ => self.serialize(),
        }
    }

    fn write(&self, s: &mut String, nested: bool, paren_less: bool) {
        let open = |s: &mut String| {
            if !paren_less {
                s.push_str(if nested { "(" } else { "calc(" });
            }
        };
        let close = |s: &mut String| {
            if !paren_less {
                s.push(')');
            }
        };
        match self {
            Num::Unit(v, u) => write_unit(s, *v, u),
            Num::Min(items) | Num::Max(items) => {
                s.push_str(if matches!(self, Num::Min(_)) {
                    "min("
                } else {
                    "max("
                });
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    item.write(s, true, true);
                }
                s.push(')');
            }
            Num::Clamp(items) => {
                s.push_str("clamp(");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    item.write(s, true, true);
                }
                s.push(')');
            }
            Num::Sum(items) => {
                open(s);
                for (i, item) in items.iter().enumerate() {
                    match item {
                        Num::Negate(v) if i > 0 => {
                            s.push_str(" - ");
                            v.write(s, true, false);
                        }
                        _ => {
                            if i > 0 {
                                s.push_str(" + ");
                            }
                            item.write(s, true, false);
                        }
                    }
                }
                close(s);
            }
            Num::Negate(v) => {
                open(s);
                s.push('-');
                v.write(s, true, false);
                close(s);
            }
            Num::Product(items) => {
                open(s);
                for (i, item) in items.iter().enumerate() {
                    match item {
                        Num::Invert(v) if i > 0 => {
                            s.push_str(" / ");
                            v.write(s, true, false);
                        }
                        _ => {
                            if i > 0 {
                                s.push_str(" * ");
                            }
                            item.write(s, true, false);
                        }
                    }
                }
                close(s);
            }
            Num::Invert(v) => {
                open(s);
                s.push_str("1 / ");
                v.write(s, true, false);
                close(s);
            }
        }
    }
}

fn type_of_unit_map(map: &BTreeMap<String, i32>) -> Option<NumType> {
    let mut t = NumType::default();
    for (unit, power) in map {
        let base = NumType::from_unit(unit)?;
        for i in 0..7 {
            t.exp[i] += base.exp[i] * power;
        }
    }
    Some(t)
}

fn single_sum_item(n: &Num) -> Option<SumItem> {
    let mut v = n.sum_value()?;
    (v.len() == 1).then(|| v.remove(0))
}

/// "Create a CSSUnitValue from a sum value item".
fn unit_value_from_sum_item((value, map): &SumItem) -> Option<(f64, String)> {
    match map.len() {
        0 => Some((*value, "number".into())),
        1 => {
            let (unit, power) = map.iter().next()?;
            (*power == 1).then(|| (*value, unit.clone()))
        }
        _ => None,
    }
}

fn unit_value(n: &Num) -> f64 {
    match n {
        Num::Unit(v, _) => *v,
        _ => f64::NAN,
    }
}

/// The unit shared by every item, when all are `CSSUnitValue`s of one unit.
fn same_unit(items: &[Num]) -> Option<String> {
    let Some(Num::Unit(_, first)) = items.first() else {
        return None;
    };
    items
        .iter()
        .all(|n| matches!(n, Num::Unit(_, u) if u == first))
        .then(|| first.clone())
}

/// CSSOM §6.7.2 "serialize a `<number>`", to six significant digits as engines do.
pub fn serialize_number(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let magnitude = v.abs().log10().floor() as i32;
    let decimals = (5 - magnitude).clamp(0, 20) as usize;
    let mut s = format!("{v:.decimals$}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".into();
    }
    s
}

/// §6.4 "serialize a CSSUnitValue" (a value that is not finite as `calc()` of infinity or NaN,
/// as CSS Values 4 §10.12 serializes it).
fn write_unit(s: &mut String, v: f64, unit: &str) {
    let suffix = match unit {
        "number" => "",
        "percent" => "%",
        u => u,
    };
    if !v.is_finite() {
        let word = if v.is_nan() {
            "NaN"
        } else if v > 0.0 {
            "infinity"
        } else {
            "-infinity"
        };
        if suffix.is_empty() {
            s.push_str(&format!("calc({word})"));
        } else {
            s.push_str(&format!("calc({word} * 1{suffix})"));
        }
        return;
    }
    s.push_str(&serialize_number(v));
    s.push_str(suffix);
}

// ---------------------------------------------------------------------------------------------
// Reification (§5).

/// A segment of a `CSSUnparsedValue`.
#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    Str(String),
    /// A `CSSVariableReferenceValue`: the custom property and the fallback, if any.
    Var(String, Option<Vec<Seg>>),
}

/// A `CSSTransformComponent`.
#[derive(Clone, Debug, PartialEq)]
pub enum Comp {
    Translate([Num; 3], bool),
    /// x, y, z, angle, is2D.
    Rotate([Num; 4], bool),
    Scale([Num; 3], bool),
    Skew(Num, Num),
    SkewX(Num),
    SkewY(Num),
    /// The length, or the keyword (`none`).
    Perspective(Result<Num, String>),
    /// The 16 entries in column-major order (m11, m12, ... m44) and is2D.
    Matrix([f64; 16], bool),
}

/// A reified value of a `CSSStyleValue` subclass.
#[derive(Clone, Debug, PartialEq)]
pub enum Typed {
    Keyword(String),
    Numeric(Num),
    Unparsed(Vec<Seg>),
    Transform(Vec<Comp>),
    /// A `CSSImageValue`, by its serialization.
    Image(String),
}

/// A property's value reified: the values of its iterations, or (§5.2) a value no subclass
/// represents, by its serialization.
#[derive(Clone, Debug, PartialEq)]
pub enum Reified {
    Typed(Vec<Typed>),
    Unsupported(String),
}

/// stylo keeps values in `f32`; the shortest decimal that is the same `f32` gives the `f64` a page
/// would have written (3.14, not 3.140000104904175).
fn f(v: f32) -> f64 {
    if !v.is_finite() {
        return v as f64;
    }
    format!("{v}").parse().unwrap_or(v as f64)
}

fn num_from(v: &NumericValue) -> Num {
    use style::typed_om::MathValue as M;
    match v {
        NumericValue::Unit(u) => {
            let unit = normalize_unit(&u.unit).unwrap_or_else(|| u.unit.to_ascii_lowercase());
            Num::Unit(f(u.value), unit)
        }
        NumericValue::Math(m) => match m {
            M::Sum(s) => Num::Sum(s.values.iter().map(num_from).collect()),
            M::Product(p) => Num::Product(p.values.iter().map(num_from).collect()),
            M::Negate(n) => Num::Negate(Box::new(num_from(&n.value))),
            M::Invert(i) => Num::Invert(Box::new(num_from(&i.value))),
            M::Min(m) => Num::Min(m.values.iter().map(num_from).collect()),
            M::Max(m) => Num::Max(m.values.iter().map(num_from).collect()),
            M::Clamp(c) => {
                let v = &c.values;
                Num::Clamp(Box::new([
                    num_from(&v[0]),
                    num_from(&v[1]),
                    num_from(&v[2]),
                ]))
            }
        },
    }
}

fn segs_from(v: &[UnparsedSegment]) -> Vec<Seg> {
    v.iter()
        .map(|s| match s {
            UnparsedSegment::String(s) => Seg::Str(s.to_string()),
            UnparsedSegment::VariableReference(r) => Seg::Var(
                r.variable.to_string(),
                r.has_fallback.then(|| segs_from(&r.fallback)),
            ),
        })
        .collect()
}

fn comp_from(c: &TransformComponent) -> Comp {
    match c {
        TransformComponent::Translate(t) => {
            Comp::Translate([num_from(&t.x), num_from(&t.y), num_from(&t.z)], t.is_2d)
        }
        TransformComponent::Rotate(r) => Comp::Rotate(
            [
                num_from(&r.x),
                num_from(&r.y),
                num_from(&r.z),
                num_from(&r.angle),
            ],
            r.is_2d,
        ),
        TransformComponent::Scale(s) => {
            Comp::Scale([num_from(&s.x), num_from(&s.y), num_from(&s.z)], s.is_2d)
        }
        TransformComponent::Skew(s) => Comp::Skew(num_from(&s.ax), num_from(&s.ay)),
        TransformComponent::SkewX(a) => Comp::SkewX(num_from(a)),
        TransformComponent::SkewY(a) => Comp::SkewY(num_from(a)),
        TransformComponent::Perspective(p) => Comp::Perspective(match &p.length {
            PerspectiveValue::Numeric(n) => Ok(num_from(n)),
            PerspectiveValue::Keyword(k) => Err(k.0.to_string()),
        }),
        TransformComponent::Matrix(m) => {
            let x = &m.matrix;
            Comp::Matrix(
                [
                    x.m11, x.m12, x.m13, x.m14, x.m21, x.m22, x.m23, x.m24, x.m31, x.m32, x.m33,
                    x.m34, x.m41, x.m42, x.m43, x.m44,
                ]
                .map(f),
                m.is_2d,
            )
        }
    }
}

fn typed_from(v: &TypedValue) -> Typed {
    match v {
        TypedValue::Unparsed(u) => Typed::Unparsed(segs_from(u)),
        TypedValue::Keyword(k) => Typed::Keyword(k.0.to_string()),
        TypedValue::Numeric(n) => Typed::Numeric(num_from(n)),
        TypedValue::Transform(t) => Typed::Transform(t.iter().map(comp_from).collect()),
        TypedValue::Image(i) => Typed::Image(image_css(i)),
    }
}

fn image_css(i: &ImageValue) -> String {
    let mut out = String::new();
    let _ = i.to_css(&mut style_traits::CssWriter::new(&mut out));
    out
}

fn is_css_wide(k: &str) -> bool {
    ["initial", "inherit", "unset", "revert", "revert-layer"]
        .iter()
        .any(|w| k.eq_ignore_ascii_case(w))
}

/// The kinds of value `property` (or the property it is an alias of) takes as subclasses.
fn kinds(property: &str) -> Option<u16> {
    crate::typed_props::types(property).or_else(|| {
        let id = crate::computed::property_id(property)?;
        let canonical = id.non_custom_id()?.unaliased().name();
        (canonical != property)
            .then(|| crate::typed_props::types(canonical))
            .flatten()
    })
}

/// Whether a numeric value of type `t` is one `property` takes (as a `CSSStyleValue` subclass).
pub fn takes_numeric(property: &str, t: &NumType) -> bool {
    use crate::typed_props as p;
    let Some(kinds) = kinds(property) else {
        return false;
    };
    (kinds & p::LENGTH != 0 && t.matches(Base::Length, true))
        || (kinds & p::PERCENTAGE != 0 && t.matches(Base::Percent, false))
        || (kinds & p::NUMBER != 0 && t.is_number())
        || (kinds & p::TIME != 0 && t.matches(Base::Time, false))
        || (kinds & p::ANGLE != 0 && t.matches(Base::Angle, false))
        || (kinds & p::FLEX != 0 && t.matches(Base::Flex, false))
        || (kinds & p::RESOLUTION != 0 && t.matches(Base::Resolution, false))
}

/// Whether `property` takes transform lists (`TRANSFORM`) or images (`IMAGE`) as values.
pub fn takes(property: &str, kind: u16) -> bool {
    kinds(property).is_some_and(|k| k & kind != 0)
}

/// Whether a reified value is one `property` represents with a subclass (§5.1): any keyword or
/// numeric value of a property Typed OM represents, with these exceptions. A color property's only
/// keyword is `currentcolor` (and `auto` for `accent-color` and `caret-color`): its colors are
/// `CSSStyleValue`s. The computed `border-image` sides are four values (two for `-repeat`), never
/// one subclass. `will-change` represents `auto` alone. A numeric value needs a numeric type the
/// property takes (a grid line's integer is not one).
fn represented(property: &str, v: &Typed, computed: bool) -> bool {
    match v {
        Typed::Unparsed(_) => return true,
        Typed::Keyword(k) if is_css_wide(k) => return true,
        _ if property.starts_with("--") => return false,
        _ => {}
    }
    if kinds(property).is_none() {
        return false;
    }
    let keyword = |allowed: &[&str]| match v {
        Typed::Keyword(k) => allowed.iter().any(|a| k.eq_ignore_ascii_case(a)),
        _ => false,
    };
    match property {
        "accent-color" | "caret-color" => keyword(&["currentcolor", "auto"]),
        "background-color"
        | "border-bottom-color"
        | "border-left-color"
        | "border-right-color"
        | "border-top-color"
        | "color"
        | "column-rule-color"
        | "flood-color"
        | "lighting-color"
        | "outline-color"
        | "stop-color"
        | "text-decoration-color"
        | "text-emphasis-color" => keyword(&["currentcolor"]),
        "border-image-outset"
        | "border-image-slice"
        | "border-image-width"
        | "border-image-repeat"
            if computed =>
        {
            false
        }
        "will-change" => keyword(&["auto"]),
        _ => match v {
            Typed::Numeric(n) => n.ty().is_some_and(|t| takes_numeric(property, &t)),
            _ => true,
        },
    }
}

/// A value stylo could not reify that is a single keyword or numeric value (as engines reify any
/// identifier or number of a property they represent).
fn single_component(property: &str, css: &str, computed: bool) -> Option<Typed> {
    let mut parser = cssparser::Parser::new(css);
    let ident = parser
        .parse_entirely(|p| {
            p.expect_ident_cloned()
                .map_err(cssparser::ParseError::<()>::from)
        })
        .ok()
        .map(|i| i.to_string());
    let v = match ident {
        Some(i) => Typed::Keyword(i),
        None => Typed::Numeric(parse_numeric(css.trim()).ok()?),
    };
    represented(property, &v, computed).then_some(v)
}

/// The reified iterations of a list stylo reified for `property`, or the serialization when no
/// subclass represents the value.
pub(crate) fn reified(
    property: &str,
    list: Option<TypedValueList>,
    computed: bool,
    css: impl FnOnce() -> String,
) -> Reified {
    let values: Vec<Typed> = match list {
        Some(list) if !list.values.is_empty() => list.values.iter().map(typed_from).collect(),
        _ => {
            let css = css();
            return match single_component(property, &css, computed) {
                Some(v) => Reified::Typed(vec![v]),
                None => Reified::Unsupported(css),
            };
        }
    };
    if values.iter().all(|v| represented(property, v, computed)) {
        Reified::Typed(values)
    } else {
        Reified::Unsupported(css())
    }
}

/// The computed value of `property` reified (§3.1 computed style map), `None` for an unknown
/// property.
pub fn computed(values: &ComputedValues, property: &str) -> Option<Reified> {
    let id = crate::computed::property_id(property)?;
    let css = || crate::computed::computed_property(values, &id);
    Some(match id.longhand_id() {
        Some(longhand) => {
            let physical = longhand.to_physical(values.writing_mode);
            reified(
                property,
                values.property_value_to_typed_value_list(physical),
                true,
                css,
            )
        }
        // (a custom property's computed value is its tokens, with no var() left; one with no
        // value is not in the map)
        None if matches!(id, PropertyId::Custom(_)) => {
            let s = css();
            if s.is_empty() {
                return None;
            }
            Reified::Typed(vec![Typed::Unparsed(vec![Seg::Str(s)])])
        }
        None => Reified::Unsupported(css()),
    })
}

/// The tokens of `pieces`, each tokenized on its own, serialized as one list (CSS Syntax 3 §9: an
/// empty comment between two tokens that would otherwise read as one; comments dropped).
pub fn serialize_tokens(pieces: &[String]) -> String {
    use cssparser::{Token, TokenSerializationType};

    fn walk(
        parser: &mut cssparser::Parser<'_>,
        out: &mut String,
        last: &mut TokenSerializationType,
    ) {
        loop {
            let token = match parser.next_including_whitespace() {
                Ok(t) => t.clone(),
                Err(_) => return,
            };
            let ty = token.serialization_type();
            if last.needs_separator_when_before(ty) {
                out.push_str("/**/");
            }
            let _ = cssparser::ToCss::to_css(&token, out);
            *last = ty;
            let close = match token {
                Token::Function(_) | Token::ParenthesisBlock => Some(Token::CloseParenthesis),
                Token::SquareBracketBlock => Some(Token::CloseSquareBracket),
                Token::CurlyBracketBlock => Some(Token::CloseCurlyBracket),
                _ => None,
            };
            if let Some(close) = close {
                let _ = parser.parse_nested_block(|p| {
                    walk(p, out, last);
                    Ok::<(), cssparser::ParseError<()>>(())
                });
                let _ = cssparser::ToCss::to_css(&close, out);
                *last = close.serialization_type();
            }
        }
    }

    let mut out = String::new();
    let mut last = TokenSerializationType::Nothing;
    for piece in pieces {
        let mut parser = cssparser::Parser::new(piece);
        walk(&mut parser, &mut out, &mut last);
    }
    out
}

/// The custom properties an element's computed style has a value for, in code-point order (the
/// computed style map's last entries).
pub fn custom_property_names(values: &ComputedValues) -> Vec<String> {
    let props = values.custom_properties();
    let count = props.inherited.len() + props.non_inherited.len();
    let mut names: Vec<String> = (0..count)
        .filter_map(|i| props.property_at(i))
        .filter(|(_, v)| v.is_some())
        .map(|(name, _)| format!("--{name}"))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `css` reified as a list of component values (§5.3): its strings and `var()` references.
/// `None` when it is not a valid declaration value.
pub fn parse_unparsed(css: &str) -> Option<Vec<Seg>> {
    let base = url::Url::parse("about:blank").expect("valid URL");
    let mut block = crate::DeclarationBlock::new(&base);
    if !block.set_property("--x", css, "") {
        return None;
    }
    match block.typed("--x")? {
        Reified::Typed(mut v) => match v.pop() {
            Some(Typed::Unparsed(segs)) => Some(segs),
            _ => None,
        },
        Reified::Unsupported(_) => None,
    }
}

/// `CSSNumericValue.parse(cssText)`: a single number, percentage, dimension or math function.
pub fn parse_numeric(css: &str) -> Result<Num, NumError> {
    use style::parser::{Parse, ParserContext};
    use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
    use style::typed_om::numeric_declaration::NumericDeclaration;
    use style::typed_om::ToTyped;
    crate::prefs::init();
    let url = UrlExtraData::from(url::Url::parse("about:blank").expect("valid URL"));
    let context = ParserContext::new(
        Origin::Author,
        &url,
        Some(CssRuleType::Style),
        style_traits::ParsingMode::DEFAULT,
        style::context::QuirksMode::NoQuirks,
        Default::default(),
        None,
        None,
        Default::default(),
    );
    let mut parser = cssparser::Parser::new(css);
    let decl = parser
        .parse_entirely(|p| NumericDeclaration::parse(&context, p))
        .map_err(|_| NumError::Syntax)?;
    match decl.to_typed_value() {
        Some(TypedValue::Numeric(n)) => {
            let n = num_from(&n);
            n.ty().map(|_| n).ok_or(NumError::Syntax)
        }
        _ => Err(NumError::Syntax),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(v: f64, unit: &str) -> Num {
        Num::Unit(v, unit.into())
    }

    fn declared(property: &str, value: &str) -> Reified {
        let base = url::Url::parse("https://example.com/").unwrap();
        let mut b = crate::DeclarationBlock::new(&base);
        assert!(b.set_property(property, value, ""), "{property}: {value}");
        b.typed(property).unwrap()
    }

    fn one(v: Typed) -> Reified {
        Reified::Typed(vec![v])
    }

    #[test]
    fn reification_of_declared_values() {
        assert_eq!(
            declared("width", "10px"),
            one(Typed::Numeric(u(10.0, "px")))
        );
        assert_eq!(
            declared("width", "auto"),
            one(Typed::Keyword("auto".into()))
        );
        assert_eq!(
            declared("margin", "initial"),
            Reified::Unsupported("initial".into())
        );
        // (a color is no subclass, currentcolor is a keyword)
        assert_eq!(declared("color", "red"), Reified::Unsupported("red".into()));
        assert_eq!(
            declared("color", "currentcolor"),
            one(Typed::Keyword("currentcolor".into()))
        );
        assert_eq!(
            declared("margin", "1px"),
            Reified::Unsupported("1px".into())
        );
        // (a grid line's integer is not a numeric value of the property)
        assert_eq!(
            declared("grid-row-start", "3"),
            Reified::Unsupported("3".into())
        );
        // (an alias is reified as the property it names)
        assert_eq!(
            declared("word-wrap", "break-word"),
            one(Typed::Keyword("break-word".into()))
        );
    }

    #[test]
    fn var_fallbacks_keep_their_whitespace_and_references() {
        assert_eq!(
            declared("width", "var(--a, var(--b))"),
            one(Typed::Unparsed(vec![Seg::Var(
                "--a".into(),
                Some(vec![Seg::Str(" ".into()), Seg::Var("--b".into(), None)])
            )]))
        );
        assert_eq!(
            declared("width", "var(--a, 1em)"),
            one(Typed::Unparsed(vec![Seg::Var(
                "--a".into(),
                Some(vec![Seg::Str(" 1em".into())])
            )]))
        );
    }

    #[test]
    fn numeric_parse_takes_any_math_function() {
        assert_eq!(
            parse_numeric("calc(2 + 3)"),
            Ok(Num::Sum(vec![Num::number(5.0)]))
        );
        assert_eq!(parse_numeric("1px"), Ok(u(1.0, "px")));
        assert_eq!(parse_numeric("1px 2px"), Err(NumError::Syntax));
        assert_eq!(parse_numeric("calc(1px + 1s)"), Err(NumError::Syntax));
    }

    #[test]
    fn tokens_that_would_merge_are_separated() {
        let pieces = ["lem", "on", "ade"].map(String::from);
        assert_eq!(serialize_tokens(&pieces), "lem/**/on/**/ade");
        let pieces = ["bar/*c*/".to_string(), "var(--foo)".to_string()];
        assert_eq!(serialize_tokens(&pieces), "bar/**/var(--foo)");
    }

    #[test]
    fn types_add_with_percent_hint() {
        let length = NumType::from_unit("px").unwrap();
        let percent = NumType::from_unit("percent").unwrap();
        let t = NumType::add(&length, &percent).unwrap();
        assert_eq!(t.exp[Base::Length as usize], 1);
        assert_eq!(t.exp[Base::Percent as usize], 0);
        assert_eq!(t.hint, Some(Base::Length));
        assert!(NumType::add(&length, &NumType::from_unit("s").unwrap()).is_none());
    }

    #[test]
    fn to_and_to_sum() {
        assert_eq!(u(1.0, "in").to("px"), Ok(u(96.0, "px")));
        assert_eq!(u(1.0, "em").to("px"), Err(NumError::Type));
        assert_eq!(u(1.0, "px").to("nope"), Err(NumError::Syntax));
        let sum = Num::Sum(vec![u(1.0, "px"), u(1.0, "in"), u(2.0, "em")]);
        assert_eq!(
            sum.to_sum(&[]),
            Ok(Num::Sum(vec![u(2.0, "em"), u(97.0, "px")]))
        );
        assert_eq!(
            sum.to_sum(&["px".into(), "em".into()]),
            Ok(Num::Sum(vec![u(97.0, "px"), u(2.0, "em")]))
        );
        assert_eq!(sum.to_sum(&["px".into()]), Err(NumError::Type));
    }

    #[test]
    fn arithmetic() {
        assert_eq!(
            u(1.0, "px").add_values(vec![u(2.0, "px")]),
            Ok(u(3.0, "px"))
        );
        assert_eq!(
            u(1.0, "px").add_values(vec![u(2.0, "em")]),
            Ok(Num::Sum(vec![u(1.0, "px"), u(2.0, "em")]))
        );
        assert_eq!(
            u(1.0, "px").add_values(vec![u(2.0, "s")]),
            Err(NumError::Type)
        );
        assert_eq!(
            u(2.0, "px").mul_values(vec![Num::number(3.0)]),
            Ok(u(6.0, "px"))
        );
        assert_eq!(
            u(2.0, "px").div_values(vec![Num::number(0.0)]),
            Err(NumError::Range)
        );
    }

    #[test]
    fn serialization() {
        assert_eq!(u(2.5, "px").serialize(), "2.5px");
        assert_eq!(u(50.0, "percent").serialize(), "50%");
        assert_eq!(Num::number(0.1 + 0.2).serialize(), "0.3");
        let sum = Num::Sum(vec![
            u(1.0, "px"),
            Num::Negate(Box::new(Num::Product(vec![Num::number(2.0), u(3.0, "em")]))),
        ]);
        assert_eq!(sum.serialize(), "calc(1px - (2 * 3em))");
        let max = Num::Max(vec![
            Num::Sum(vec![Num::number(1.0), Num::number(2.0)]),
            Num::number(3.0),
        ]);
        assert_eq!(max.serialize(), "max(1 + 2, 3)");
        assert_eq!(u(f64::INFINITY, "px").serialize(), "calc(infinity * 1px)");
    }
}
