// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Forms (HTML §4.10): form-associated elements, their value, checkedness and selectedness state, the
//! `select` element's options, constraint validation flags, form submission and reset (without
//! navigation, which needs browsing contexts).

use smallvec::SmallVec;
use snowlark_bindings::generated::{
    HTMLButtonElementImpl, HTMLElementOrLong, HTMLFieldSetElementImpl, HTMLFormElementImpl,
    HTMLInputElementImpl, HTMLLabelElementImpl, HTMLLegendElementImpl, HTMLMeterElementImpl,
    HTMLOptionElementImpl, HTMLOptionElementOrHTMLOptGroupElement, HTMLOutputElementImpl,
    HTMLProgressElementImpl, HTMLSelectElementImpl, HTMLTextAreaElementImpl,
    RadioNodeListOrElement, SelectionMode, ValidityStateImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsObject};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, ns, Dom, ElementData, LocalName, NodeId};
use snowlark_style::ElementState;

use super::collections::{form_owner, Filter};
use super::fetch::FormValue;
use super::html::is_disabled;
use super::internals::SubmissionValue;
use super::{exception, node_handle, Engine, Object};

/// A numeric input's constraints (HTML §4.10.5.4), as numbers of its type.
struct Numeric {
    kind: String,
    min: Option<f64>,
    max: Option<f64>,
    /// The allowed value step (none: "any").
    step: Option<f64>,
    base: f64,
    /// Decimal places the attributes are written with (number and range), to round results to.
    places: usize,
}

/// The state of a form control that is not in its attributes.
#[derive(Clone, Debug)]
pub struct ControlState {
    /// The value when its dirty value flag is set.
    pub value: Option<String>,
    /// The checkedness when its dirty checkedness flag is set.
    pub checked: Option<bool>,
    pub indeterminate: bool,
    /// An option's selectedness when its dirtiness is set.
    pub selected: Option<bool>,
    pub custom_validity: String,
    pub selection: (u32, u32, &'static str),
    /// `output`'s default value override.
    pub default_value: Option<String>,
    /// How far a one-line field's text is scrolled to keep the caret in view (CSS px).
    pub scroll_x: f32,
    /// A select whose selection script emptied (`selectedIndex = -1`, a `value` no option has): no
    /// option is selected until the selectedness setting algorithm runs again.
    pub no_selection: bool,
    /// A number field's text as the user typed it, when it is not its value: input the field could
    /// not convert to a number (bad input), or a form it converts (`1.` is 1). Setting the value
    /// replaces it.
    pub editor: Option<String>,
    /// HTML §4.10.18 "user validity": the user committed a change, or the form was submitted.
    pub user_validity: bool,
}

impl Default for ControlState {
    fn default() -> ControlState {
        ControlState {
            value: None,
            checked: None,
            indeterminate: false,
            selected: None,
            custom_validity: String::new(),
            // (HTML §4.10.20: a text control's selection direction starts as "none")
            selection: (0, 0, "none"),
            default_value: None,
            scroll_x: 0.0,
            no_selection: false,
            editor: None,
            user_validity: false,
        }
    }
}

fn is(dom: &Dom, id: NodeId, local: &LocalName) -> bool {
    dom.is_html_element_named(id, local)
}

fn attr<'a>(dom: &'a Dom, id: NodeId, local: &LocalName) -> Option<&'a str> {
    dom.attr(id, &ns!(), local)
}

/// Which of the elements marked since the last sync an [`Engine::sync_form_states`] brings up to
/// date: what its caller reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormSync {
    /// All of them: DOM selector matching that reads the state, form submission.
    All,
    /// Those in a document (style and layout reads, the rendering update); the others wait for a
    /// selector match that reads them.
    Connected,
    /// Those whose state needs a script context (a `pattern`), after each DOM operation: the
    /// reads without one then see them up to date.
    Script,
}

/// Elements with a state of [`Engine::sync_form_states`]: the listed elements, options and
/// optgroups, and forms.
fn has_form_state(dom: &Dom, id: NodeId) -> bool {
    dom.element(id).is_some_and(element_has_form_state)
}

/// The [`snowlark_dom::subtree`] bit of the engine's reaction filter for a subtree that holds an
/// element that has (or, an undefined custom element, may come to have) a form state: without it,
/// an inserted or removed subtree's descendants are not searched for them.
pub(crate) const SUBTREE_FORM_STATE: u8 = 2;

/// [`SUBTREE_FORM_STATE`] for `e` (the reaction filter's one match gives it with its other bits).
#[cfg(test)]
pub(crate) fn form_state_bit(e: &ElementData) -> u8 {
    if element_has_form_state(e) || e.custom_state() == snowlark_dom::CustomElementState::Undefined
    {
        SUBTREE_FORM_STATE
    } else {
        0
    }
}

fn element_has_form_state(e: &ElementData) -> bool {
    e.name().ns == ns!(html)
        && (matches!(
            e.name().local,
            local_name!("button")
                | local_name!("fieldset")
                | local_name!("input")
                | local_name!("object")
                | local_name!("output")
                | local_name!("select")
                | local_name!("textarea")
                | local_name!("option")
                | local_name!("optgroup")
                | local_name!("form")
        ) || e.flags() & snowlark_dom::element_flags::FORM_ASSOCIATED_CUSTOM != 0)
}

/// The select an option or optgroup is in (its parent, or its optgroup parent's).
fn select_of(dom: &Dom, id: NodeId) -> Option<NodeId> {
    if !is(dom, id, &local_name!("option")) && !is(dom, id, &local_name!("optgroup")) {
        return None;
    }
    dom.ancestors(id)
        .take(2)
        .find(|&a| is(dom, a, &local_name!("select")))
}

/// Forms and fieldsets: their `:valid`/`:invalid` are their controls'.
fn is_container(dom: &Dom, id: NodeId) -> bool {
    is(dom, id, &local_name!("form")) || is(dom, id, &local_name!("fieldset"))
}

/// The input element's type keywords (HTML §4.10.5).
const INPUT_TYPES: &[&str] = &[
    "hidden",
    "text",
    "search",
    "tel",
    "url",
    "email",
    "password",
    "date",
    "month",
    "week",
    "time",
    "datetime-local",
    "number",
    "range",
    "color",
    "checkbox",
    "radio",
    "file",
    "submit",
    "image",
    "reset",
    "button",
];

/// The number a number field's typed text stands for, as a string (HTML §4.10.5.1.12: the user
/// agent converts the user's input to a valid floating-point number): ASCII whitespace and a
/// leading `+` dropped, and a trailing `.` after digits. Text with an exponent, or with characters a
/// number does not have, is left as it is (and fails sanitization unless it is a number).
fn number_from_user(text: &str) -> String {
    if text.is_empty() || text.contains(['e', 'E']) {
        return text.to_owned();
    }
    let mut t: String = text
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r'))
        .collect();
    if !t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
    {
        return text.to_owned();
    }
    if t.len() >= 2 && t.starts_with('+') {
        t.remove(0);
    }
    if t.len() >= 2 && t.ends_with('.') && t.matches('.').count() < 2 {
        t.pop();
    }
    t
}

/// The value sanitization algorithm (HTML §4.10.5.1) of an input type.
fn sanitize(kind: &str, value: &str) -> String {
    let strip_newlines = |s: &str| {
        s.chars()
            .filter(|&c| c != '\n' && c != '\r')
            .collect::<String>()
    };
    let trim = |s: &str| {
        s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r'))
            .to_owned()
    };
    match kind {
        "text" | "search" | "tel" | "password" => strip_newlines(value),
        "url" => trim(&strip_newlines(value)),
        "email" => {
            let v = strip_newlines(value);
            trim(&v)
        }
        "number" => match rt::parse_float(value) {
            Some(_) if is_valid_float(value) => value.to_owned(),
            _ => String::new(),
        },
        // (an invalid value becomes the default value, which depends on the element: see
        // `Engine::range_value`)
        "range" => {
            if is_valid_float(value) {
                value.to_owned()
            } else {
                String::new()
            }
        }
        "color" => {
            let l = value.to_ascii_lowercase();
            if l.len() == 7 && l.starts_with('#') && l[1..].bytes().all(|b| b.is_ascii_hexdigit()) {
                l
            } else {
                "#000000".to_owned()
            }
        }
        "date" => {
            if is_valid_date(value) {
                value.to_owned()
            } else {
                String::new()
            }
        }
        "month" => {
            if is_valid_month(value) {
                value.to_owned()
            } else {
                String::new()
            }
        }
        "week" => {
            if is_valid_week(value) {
                value.to_owned()
            } else {
                String::new()
            }
        }
        "time" => {
            if is_valid_time(value) {
                value.to_owned()
            } else {
                String::new()
            }
        }
        // (a valid local date and time string becomes the normalized one: "T", the shortest time)
        "datetime-local" => string_to_number("datetime-local", value)
            .and_then(|v| number_to_string("datetime-local", v))
            .unwrap_or_default(),
        _ => value.to_owned(),
    }
}

/// Milliseconds in a day.
const DAY_MS: f64 = 86_400_000.0;

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date `z` days after 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// Monday 0 .. Sunday 6 of the day `z` days after 1970-01-01 (a Thursday).
fn weekday(z: i64) -> i64 {
    (z + 3).rem_euclid(7)
}

/// The day (since 1970-01-01) of the Monday of week 1 of ISO week-year `y` (the week with 4 January).
fn week_one_monday(y: i64) -> i64 {
    let jan4 = days_from_civil(y, 1, 4);
    jan4 - weekday(jan4)
}

/// HTML §2.3.5.8: the weeks in `y` (53 when 1 January is a Thursday, or a Wednesday of a leap year).
fn weeks_in_year(y: i64) -> u32 {
    ((week_one_monday(y + 1) - week_one_monday(y)) / 7) as u32
}

fn parse_month(s: &str) -> Option<(i64, u32)> {
    if !is_valid_month(s) {
        return None;
    }
    let (y, m) = s.rsplit_once('-')?;
    Some((y.parse().ok()?, m.parse().ok()?))
}

fn parse_date(s: &str) -> Option<(i64, u32, u32)> {
    if !is_valid_date(s) {
        return None;
    }
    let (ym, d) = s.rsplit_once('-')?;
    let (y, m) = parse_month(ym)?;
    Some((y, m, d.parse().ok()?))
}

/// Milliseconds after midnight of a valid time string.
fn parse_time(s: &str) -> Option<f64> {
    if !is_valid_time(s) {
        return None;
    }
    let mut parts = s.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = match parts.next() {
        Some(x) => {
            let (w, f) = x.split_once('.').unwrap_or((x, ""));
            let frac = format!("{f:0<3}");
            w.parse::<f64>().ok()? + frac.parse::<f64>().ok()? / 1000.0
        }
        None => 0.0,
    };
    Some(((h * 60.0 + m) * 60.0 + sec) * 1000.0)
}

/// The shortest valid time string for `ms` after midnight (seconds and milliseconds only when not
/// zero).
fn format_time(ms: f64) -> String {
    let ms = ms.rem_euclid(DAY_MS).round() as i64;
    let (h, m, s, f) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let mut out = format!("{h:02}:{m:02}");
    if s != 0 || f != 0 {
        out += &format!(":{s:02}");
        if f != 0 {
            let frac = format!("{f:03}");
            out += ".";
            out += frac.trim_end_matches('0');
        }
    }
    out
}

fn format_year(y: i64) -> String {
    format!("{y:04}")
}

/// HTML §4.10.5.1.7-13 "the algorithm to convert a string to a number" of each type.
fn string_to_number(kind: &str, s: &str) -> Option<f64> {
    match kind {
        // (the rules for parsing floating-point number values, not "valid floating-point number")
        "number" | "range" => rt::parse_float(s),
        "date" => parse_date(s).map(|(y, m, d)| days_from_civil(y, m, d) as f64 * DAY_MS),
        "month" => parse_month(s).map(|(y, m)| ((y - 1970) * 12 + m as i64 - 1) as f64),
        "week" => {
            let (y, w) = s.split_once("-W")?;
            if !is_valid_week(s) {
                return None;
            }
            let (y, w): (i64, i64) = (y.parse().ok()?, w.parse().ok()?);
            Some((week_one_monday(y) + (w - 1) * 7) as f64 * DAY_MS)
        }
        "time" => parse_time(s),
        "datetime-local" => {
            let (d, t) = s.split_once(['T', ' '])?;
            let (y, m, dd) = parse_date(d)?;
            Some(days_from_civil(y, m, dd) as f64 * DAY_MS + parse_time(t)?)
        }
        _ => None,
    }
}

/// HTML §4.10.5.1.7-13 "the algorithm to convert a number to a string" of each type.
fn number_to_string(kind: &str, v: f64) -> Option<String> {
    if !v.is_finite() {
        return None;
    }
    let date = |ms: f64| {
        let (y, m, d) = civil_from_days((ms / DAY_MS).floor() as i64);
        format!("{}-{m:02}-{d:02}", format_year(y))
    };
    Some(match kind {
        "number" | "range" => js_number_string(v),
        "date" => date(v),
        "month" => {
            let months = v.floor() as i64;
            let (y, m) = (1970 + months.div_euclid(12), months.rem_euclid(12) + 1);
            format!("{}-{m:02}", format_year(y))
        }
        "week" => {
            let day = (v / DAY_MS).floor() as i64;
            let monday = day - weekday(day);
            let (y, _, _) = civil_from_days(monday + 3);
            let w = (monday - week_one_monday(y)) / 7 + 1;
            format!("{}-W{w:02}", format_year(y))
        }
        "time" => format_time(v),
        "datetime-local" => format!("{}T{}", date(v), format_time(v.rem_euclid(DAY_MS))),
        _ => return None,
    })
}

/// ECMAScript Number::toString(10) for the finite numbers forms produce.
fn js_number_string(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let a = v.abs();
    if (1e-6..1e21).contains(&a) {
        return format!("{v}");
    }
    let s = format!("{v:e}");
    match s.split_once('e') {
        Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
        _ => s,
    }
}

/// The number of digits after the decimal point in a floating-point number string (for results that
/// should not show binary rounding: 0.1 stepped three times is 0.3).
fn decimals(s: &str) -> usize {
    let mantissa = s.split(['e', 'E']).next().unwrap_or("");
    mantissa.split_once('.').map_or(0, |(_, f)| f.len()).min(16)
}

/// `v` rounded to `places` decimals.
fn round_to(v: f64, places: usize) -> f64 {
    format!("{v:.places$}").parse().unwrap_or(v)
}

/// A numeric input type's step facts (HTML §4.10.5.1.7-13): (default step, step scale factor,
/// default step base).
fn step_facts(kind: &str) -> Option<(f64, f64, f64)> {
    Some(match kind {
        "number" | "range" => (1.0, 1.0, 0.0),
        "date" => (1.0, DAY_MS, 0.0),
        "month" => (1.0, 1.0, 0.0),
        "week" => (1.0, 7.0 * DAY_MS, -259_200_000.0),
        "time" | "datetime-local" => (60.0, 1000.0, 0.0),
        _ => return None,
    })
}

/// The autofill field names' categories (HTML §4.10.18.7.1's table): (category, maximum tokens).
fn autofill_category(field: &str) -> Option<(&'static str, usize)> {
    const NORMAL: &[&str] = &[
        "name",
        "honorific-prefix",
        "given-name",
        "additional-name",
        "family-name",
        "honorific-suffix",
        "nickname",
        "username",
        "new-password",
        "current-password",
        "one-time-code",
        "organization-title",
        "organization",
        "street-address",
        "address-line1",
        "address-line2",
        "address-line3",
        "address-level4",
        "address-level3",
        "address-level2",
        "address-level1",
        "country",
        "country-name",
        "postal-code",
        "cc-name",
        "cc-given-name",
        "cc-additional-name",
        "cc-family-name",
        "cc-number",
        "cc-exp",
        "cc-exp-month",
        "cc-exp-year",
        "cc-csc",
        "cc-type",
        "transaction-currency",
        "transaction-amount",
        "language",
        "bday",
        "bday-day",
        "bday-month",
        "bday-year",
        "sex",
        "url",
        "photo",
    ];
    const CONTACT: &[&str] = &[
        "tel",
        "tel-country-code",
        "tel-national",
        "tel-area-code",
        "tel-local",
        "tel-local-prefix",
        "tel-local-suffix",
        "tel-extension",
        "email",
        "impp",
    ];
    Some(match field {
        "off" => ("off", 1),
        "on" => ("automatic", 1),
        "webauthn" => ("credential", 5),
        f if NORMAL.contains(&f) => ("normal", 3),
        f if CONTACT.contains(&f) => ("contact", 4),
        _ => return None,
    })
}

/// HTML §4.10.18.7.1 "autofill processing model": the IDL-exposed autofill value of an
/// `autocomplete` attribute's tokens (`anchor`: the element wears the autofill anchor mantle, a
/// hidden input). The empty string where the processing model reaches "default".
fn autofill_idl_value(value: &str, anchor: bool) -> String {
    let tokens: Vec<String> = value
        .split([' ', '\t', '\n', '\x0c', '\r'])
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let Some(mut index) = tokens.len().checked_sub(1) else {
        return String::new();
    };
    let Some((mut category, max)) = autofill_category(&tokens[index]) else {
        return String::new();
    };
    if tokens.len() > max {
        return String::new();
    }
    match category {
        "off" | "automatic" if anchor => return String::new(),
        "off" | "automatic" => return tokens[index].clone(),
        _ => {}
    }
    let mut out = tokens[index].clone();
    if category == "credential" {
        // ("webauthn" after a normal or contact field name)
        if index == 0 {
            return out;
        }
        index -= 1;
        match autofill_category(&tokens[index]) {
            Some((c @ ("normal" | "contact"), _)) => category = c,
            _ => return String::new(),
        }
        out = format!("{} {out}", tokens[index]);
    }
    let Some(i) = index.checked_sub(1) else {
        return out;
    };
    index = i;
    if category == "contact"
        && matches!(
            tokens[index].as_str(),
            "home" | "work" | "mobile" | "fax" | "pager"
        )
    {
        out = format!("{} {out}", tokens[index]);
        let Some(i) = index.checked_sub(1) else {
            return out;
        };
        index = i;
    }
    if matches!(tokens[index].as_str(), "shipping" | "billing") {
        out = format!("{} {out}", tokens[index]);
        let Some(i) = index.checked_sub(1) else {
            return out;
        };
        index = i;
    }
    if index != 0 || !tokens[index].starts_with("section-") {
        return String::new();
    }
    format!("{} {out}", tokens[index])
}

/// Every CR not followed by LF and every LF not preceded by CR as CRLF (the form submission
/// encodings' newline normalization).
pub(crate) fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}

/// Strips ASCII whitespace from both ends (HTML's "strip leading and trailing ASCII whitespace").
fn trim_ascii_ws(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r'))
}

/// HTML §4.10.5.1.5 "valid email address": the ABNF's `1*( atext / "." ) "@" label *( "." label )`,
/// a label 1 to 63 letters, digits and hyphens that neither starts nor ends with a hyphen.
fn is_valid_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    let atext = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~-.".contains(c);
    if local.is_empty() || !local.chars().all(atext) {
        return false;
    }
    domain.split('.').all(|label| {
        let b = label.as_bytes();
        !b.is_empty()
            && b.len() <= 63
            && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
            && b[0] != b'-'
            && b[b.len() - 1] != b'-'
    })
}

/// Tests `values` against an input's `pattern` (HTML §4.10.5.3.8 "compiled pattern regular
/// expression": the pattern must compile alone with the "v" flag, and is then anchored as
/// `^(?:pattern)$`). True when every value matches; none when the pattern does not compile.
fn pattern_regexp_matches(cx: &mut Cx, pattern: &str, values: &[&str]) -> Option<bool> {
    let scope = cx.scope();
    v8::tc_scope!(let tc, scope);
    let flags = v8::RegExpCreationFlags::UNICODE_SETS;
    let bare = v8::String::new(tc, pattern)?;
    if v8::RegExp::new(tc, bare, flags).is_none() {
        tc.reset();
        return None;
    }
    let anchored = v8::String::new(tc, &format!("^(?:{pattern})$"))?;
    let Some(re) = v8::RegExp::new(tc, anchored, flags) else {
        tc.reset();
        return None;
    };
    for v in values {
        let subject = v8::String::new(tc, v)?;
        match re.exec(tc, subject) {
            Some(m) if !m.is_null_or_undefined() => {}
            Some(_) => return Some(false),
            None => {
                tc.reset();
                return None;
            }
        }
    }
    Some(true)
}

/// HTML §2.3.4.3 "valid floating-point number".
fn is_valid_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'-') {
        i = 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i > start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == f {
            return false;
        }
    } else if !int_digits {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return false;
        }
    }
    i == b.len()
}

fn digits(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        _ => 28,
    }
}

fn is_valid_month(s: &str) -> bool {
    let Some((y, m)) = s.rsplit_once('-') else {
        return false;
    };
    y.len() >= 4
        && y.bytes().all(|b| b.is_ascii_digit())
        && y.parse::<u32>().is_ok_and(|y| y > 0)
        && digits(m, 2).is_some_and(|m| (1..=12).contains(&m))
}

fn is_valid_date(s: &str) -> bool {
    let Some((ym, d)) = s.rsplit_once('-') else {
        return false;
    };
    if !is_valid_month(ym) {
        return false;
    }
    let (y, m) = ym.rsplit_once('-').unwrap();
    let (y, m): (u32, u32) = (y.parse().unwrap_or(0), m.parse().unwrap_or(0));
    digits(d, 2).is_some_and(|d| d >= 1 && d <= days_in_month(y, m))
}

fn is_valid_week(s: &str) -> bool {
    let Some((y, w)) = s.split_once("-W") else {
        return false;
    };
    let Some(year) = (y.len() >= 4 && y.bytes().all(|b| b.is_ascii_digit()))
        .then(|| y.parse::<i64>().ok())
        .flatten()
        .filter(|y| *y > 0)
    else {
        return false;
    };
    digits(w, 2).is_some_and(|w| w >= 1 && w <= weeks_in_year(year))
}

fn is_valid_time(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return false;
    }
    if !digits(parts[0], 2).is_some_and(|h| h < 24) || !digits(parts[1], 2).is_some_and(|m| m < 60)
    {
        return false;
    }
    if let Some(sec) = parts.get(2) {
        let (whole, frac) = match sec.split_once('.') {
            Some((w, f)) => (w, Some(f)),
            None => (*sec, None),
        };
        if !digits(whole, 2).is_some_and(|s| s < 60) {
            return false;
        }
        if let Some(f) = frac {
            if f.is_empty() || f.len() > 3 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
        }
    }
    true
}

impl Engine {
    pub(crate) fn ctl<R>(&self, el: NodeId, f: impl FnOnce(&mut ControlState) -> R) -> R {
        // (what the control shows follows at the next rendering update: `Engine::sync_controls`)
        self.controls_dirty.borrow_mut().insert(el);
        let mut h = self.html.borrow_mut();
        f(h.controls.entry(el).or_default())
    }

    /// An attribute of `el` changed: a control with page state shows it at the next rendering
    /// update (`checked`, `value` and `type` decide what a field with no dirty state shows), as
    /// does an option's select (`selected`).
    pub(crate) fn control_attribute_changed(&self, el: NodeId) {
        let mark = {
            let h = self.html.borrow();
            let dom = self.dom();
            h.controls.contains_key(&el)
                || (is(&dom, el, &local_name!("option"))
                    && self
                        .select_of(&dom, el)
                        .is_some_and(|s| h.controls.contains_key(&s)))
        };
        if mark {
            self.controls_dirty.borrow_mut().insert(el);
        }
    }

    /// Reads a control's state without giving it an entry (one it has none for reads as the
    /// default).
    pub(crate) fn ctl_read<R>(&self, el: NodeId, f: impl FnOnce(&ControlState) -> R) -> R {
        let h = self.html.borrow();
        match h.controls.get(&el) {
            Some(c) => f(c),
            None => f(&ControlState::default()),
        }
    }

    /// The input element's type state (the `type` attribute's keyword, or "text").
    pub(crate) fn input_type(&self, el: NodeId) -> Option<String> {
        let dom = self.dom();
        if !is(&dom, el, &local_name!("input")) {
            return None;
        }
        let t = attr(&dom, el, &local_name!("type")).map(str::to_ascii_lowercase);
        Some(match t {
            Some(t) if INPUT_TYPES.contains(&t.as_str()) => t,
            _ => "text".to_owned(),
        })
    }

    /// The input's value mode (HTML §4.10.5.4 "value" IDL attribute modes).
    fn value_mode(kind: &str) -> &'static str {
        match kind {
            "hidden" | "submit" | "image" | "reset" | "button" => "default",
            "checkbox" | "radio" => "default/on",
            "file" => "filename",
            _ => "value",
        }
    }

    /// HTML §4.10.5: the steps when an input element's `type` attribute changes state (the value
    /// moves between the value modes, the value is sanitized for the new state, and a control that
    /// becomes selectable has its cursor at the start).
    pub(crate) fn input_attribute_changed(
        &self,
        cx: &mut Cx,
        el: NodeId,
        local: &LocalName,
        old: Option<&str>,
        new: Option<&str>,
    ) {
        if !is(&self.dom(), el, &local_name!("input")) {
            return;
        }
        // (an email field's `multiple` changes its value sanitization algorithm, which runs again)
        if &**local == "multiple" && self.input_type(el).as_deref() == Some("email") {
            if let Some(v) = self.ctl(el, |c| c.value.clone()) {
                let v = self.sanitize_for(el, "email", &v);
                self.ctl(el, |c| c.value = Some(v));
                self.form_state_changed(el);
            }
            return;
        }
        if &**local != "type" {
            return;
        }
        let state = |v: Option<&str>| match v.map(str::to_ascii_lowercase) {
            Some(t) if INPUT_TYPES.contains(&t.as_str()) => t,
            _ => "text".to_owned(),
        };
        let (before, after) = (state(old), state(new));
        if before == after {
            return;
        }
        self.ctl(el, |c| c.editor = None);
        let (was, now) = (Engine::value_mode(&before), Engine::value_mode(&after));
        if was == "value" && matches!(now, "default" | "default/on") {
            // 1. The value moves to the value content attribute.
            let mut value = self.ctl(el, |c| c.value.clone()).unwrap_or_else(|| {
                let default = attr(&self.dom(), el, &local_name!("value"))
                    .unwrap_or("")
                    .to_owned();
                self.sanitize_for(el, &before, &default)
            });
            if before == "range" {
                value = self.range_value(el, &value);
            }
            if !value.is_empty() {
                self.set_attr(cx, el, "value", &value);
            }
        } else if was != "value" && now == "value" {
            // 2. The value is the value content attribute's, and not dirty.
            self.ctl(el, |c| c.value = None);
            self.form_state_changed(el);
        } else if was != "filename" && now == "filename" {
            // 3. A file input starts empty.
            self.ctl(el, |c| c.value = None);
            self.form_state_changed(el);
        }
        // 6. The value sanitization algorithm of the new state.
        if now == "value" {
            if let Some(v) = self.ctl(el, |c| c.value.clone()) {
                let mut v = self.sanitize_for(el, &after, &v);
                if after == "range" {
                    v = self.range_value(el, &v);
                }
                self.ctl(el, |c| c.value = Some(v));
                self.form_state_changed(el);
            }
        }
        // 7-9. A control that `setRangeText()` now applies to, and did not, has its cursor at the
        // start.
        let selectable = |t: &str| matches!(t, "text" | "search" | "url" | "tel" | "password");
        if !selectable(&before) && selectable(&after) {
            self.ctl(el, |c| c.selection = (0, 0, "none"));
        }
    }

    /// The utocomplete IDL attribute of a control: its IDL-exposed autofill value.
    fn autofill_idl(&self, el: NodeId) -> String {
        let value = attr(&self.dom(), el, &LocalName::from("autocomplete")).map(str::to_owned);
        let anchor = self.input_type(el).as_deref() == Some("hidden");
        value.map_or_else(String::new, |v| autofill_idl_value(&v, anchor))
    }

    /// The value sanitization algorithm of `el` as a `kind` input: an email field with `multiple`
    /// splits its value on commas, strips each token's ASCII whitespace and joins them with commas
    /// (HTML §4.10.5.1.5); the others as [`sanitize`].
    fn sanitize_for(&self, el: NodeId, kind: &str, value: &str) -> String {
        if kind == "email" && attr(&self.dom(), el, &local_name!("multiple")).is_some() {
            return value
                .split(',')
                .map(trim_ascii_ws)
                .collect::<Vec<_>>()
                .join(",");
        }
        sanitize(kind, value)
    }

    /// A text control's value (input in value mode, textarea): the dirty value or the default.
    pub(crate) fn control_value(&self, el: NodeId) -> String {
        let value = match self.ctl_read(el, |c| c.value.clone()) {
            Some(v) => v,
            None => {
                let dom = self.dom();
                if is(&dom, el, &local_name!("textarea")) {
                    return dom.child_text_content(el);
                }
                let default = attr(&dom, el, &local_name!("value"))
                    .unwrap_or("")
                    .to_owned();
                drop(dom);
                let kind = self.input_type(el).unwrap_or_default();
                self.sanitize_for(el, &kind, &default)
            }
        };
        if self.input_type(el).as_deref() == Some("range") {
            return self.range_value(el, &value);
        }
        value
    }

    /// The text a text control shows and the user edits: a number field's typed text when it is
    /// not the value, else the value.
    pub(crate) fn editor_value(&self, el: NodeId) -> String {
        self.ctl_read(el, |c| c.editor.clone())
            .unwrap_or_else(|| self.control_value(el))
    }

    /// The user changed a text control's text to `text` (HTML's user edits): the value is dirty. A
    /// number field keeps the text and takes as its value the number the text converts to,
    /// sanitized (none, for input that is not a number: bad input).
    pub(crate) fn set_edited_text(&self, el: NodeId, text: String) {
        self.form_state_changed(el);
        if self.input_type(el).as_deref() == Some("number") {
            let value = sanitize("number", &number_from_user(&text));
            self.ctl(el, |c| {
                c.editor = (value != text).then(|| text.clone());
                c.value = Some(value);
            });
        } else {
            self.ctl(el, |c| {
                c.value = Some(text);
                c.editor = None;
            });
        }
    }

    /// HTML §4.10.21.1 suffering from bad input: a number field holds text that does not convert to
    /// a number.
    fn bad_input_of(&self, el: NodeId) -> bool {
        if self.input_type(el).as_deref() != Some("number") {
            return false;
        }
        self.ctl_read(el, |c| c.editor.clone()).is_some_and(|t| {
            let n = number_from_user(&t);
            !n.is_empty() && sanitize("number", &n).is_empty()
        })
    }

    /// A numeric input's minimum, maximum, allowed value step and step base (HTML §4.10.5.4), from
    /// its attributes; none for other types.
    fn numeric(&self, el: NodeId) -> Option<Numeric> {
        self.numeric_for(el, self.input_type(el)?)
    }

    /// [Engine::numeric] for the input as if its type were `kind`.
    fn numeric_for(&self, el: NodeId, kind: String) -> Option<Numeric> {
        let (default_step, scale, default_base) = step_facts(&kind)?;
        let dom = self.dom();
        let raw = |name: &str| attr(&dom, el, &LocalName::from(name));
        let num = |name: &str| raw(name).and_then(|s| string_to_number(&kind, s));
        let (mut min, mut max) = (num("min"), num("max"));
        if kind == "range" {
            // (the default minimum is 0 and maximum 100; a maximum below the minimum is the minimum)
            let lo = min.unwrap_or(0.0);
            min = Some(lo);
            max = Some(max.unwrap_or(100.0).max(lo));
        }
        let step = match raw("step") {
            Some(s) if s.eq_ignore_ascii_case("any") => None,
            Some(s) => Some(
                rt::parse_float(s)
                    .filter(|v| *v > 0.0)
                    .map_or(default_step * scale, |v| v * scale),
            ),
            None => Some(default_step * scale),
        };
        let base = num("min").or_else(|| num("value")).unwrap_or(default_base);
        let places = if matches!(kind.as_str(), "number" | "range") {
            ["step", "min", "max", "value"]
                .iter()
                .filter_map(|n| raw(n))
                .map(decimals)
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        Some(Numeric {
            kind,
            min,
            max,
            step,
            base,
            places,
        })
    }

    /// HTML §4.10.5.1.13: a range control's value: the default (the middle of the range) for none,
    /// clamped to the range and moved to the nearest allowed step (the larger on a tie, the smaller
    /// when that is past the maximum).
    fn range_value(&self, el: NodeId, value: &str) -> String {
        let Some(n) = self.numeric_for(el, "range".into()) else {
            return value.to_owned();
        };
        let (min, max) = (n.min.unwrap_or(0.0), n.max.unwrap_or(100.0));
        let mut v = rt::parse_float(value)
            .filter(|_| !value.is_empty())
            .unwrap_or(min + (max - min) / 2.0);
        v = v.clamp(min, max);
        if let Some(step) = n.step {
            let q = ((v - n.base) / step + 0.5).floor();
            v = n.base + q * step;
            if v > max {
                v -= step;
            }
            if v < min {
                v += step;
            }
        }
        js_number_string(round_to(v, n.places.max(decimals(value))))
    }

    pub(crate) fn checkedness(&self, el: NodeId) -> bool {
        match self.ctl_read(el, |c| c.checked) {
            Some(c) => c,
            None => attr(&self.dom(), el, &local_name!("checked")).is_some(),
        }
    }

    pub(crate) fn indeterminate(&self, el: NodeId) -> bool {
        self.ctl_read(el, |c| c.indeterminate)
    }

    pub(crate) fn set_indeterminate_state(&self, el: NodeId, v: bool) {
        self.ctl(el, |c| c.indeterminate = v);
        self.sync_checked_state(el);
    }

    /// Selectors 4 `:checked` (a checkbox's or radio button's checkedness, an option's selectedness)
    /// and `:indeterminate` (a checkbox's indeterminate IDL state) as the page has them, for the
    /// style engine, which otherwise derives them from the content attributes.
    pub(crate) fn sync_checked_state(&self, el: NodeId) {
        let (checkable, option) = {
            let dom = self.dom();
            if !dom.is_alive(el) {
                return;
            }
            let checkable = is(&dom, el, &local_name!("input"))
                && attr(&dom, el, &local_name!("type")).is_some_and(|t| {
                    t.eq_ignore_ascii_case("checkbox") || t.eq_ignore_ascii_case("radio")
                });
            (checkable, is(&dom, el, &local_name!("option")))
        };
        if checkable {
            let mut bits = ElementState::empty();
            bits.set(ElementState::CHECKED, self.checkedness(el));
            bits.set(ElementState::INDETERMINATE, self.indeterminate(el));
            self.set_element_states(
                el,
                ElementState::CHECKED | ElementState::INDETERMINATE,
                bits,
            );
        } else if option {
            let on = self.option_selected_in(&self.dom(), el);
            self.set_element_state(el, ElementState::CHECKED, on);
        }
    }

    /// The radio buttons of `el`'s group (HTML §4.10.5.1.18), `el` included.
    fn radio_group(&self, el: NodeId) -> Vec<NodeId> {
        let dom = self.dom();
        let Some(name) = attr(&dom, el, &local_name!("name")).filter(|n| !n.is_empty()) else {
            return vec![el];
        };
        let form = form_owner(&dom, el);
        let root = dom.root(el);
        dom.descendants(root)
            .chain(std::iter::once(root))
            .filter(|&d| {
                is(&dom, d, &local_name!("input"))
                    && attr(&dom, d, &local_name!("type"))
                        .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
                    && attr(&dom, d, &local_name!("name")) == Some(name)
                    && form_owner(&dom, d) == form
            })
            .collect()
    }

    pub(crate) fn checked_radio_in_group(&self, el: NodeId) -> Option<NodeId> {
        self.radio_group(el)
            .into_iter()
            .find(|&r| self.checkedness(r))
    }

    /// Sets checkedness (and the dirty checkedness flag); a checked radio unchecks its group.
    pub(crate) fn set_checkedness(&self, _cx: &mut Cx, el: NodeId, v: bool) {
        if v && self.input_type(el).as_deref() == Some("radio") {
            for r in self.radio_group(el) {
                if r != el {
                    self.ctl(r, |c| c.checked = Some(false));
                    self.form_state_changed(r);
                    self.sync_checked_state(r);
                }
            }
        }
        self.ctl(el, |c| c.checked = Some(v));
        self.form_state_changed(el);
        self.sync_checked_state(el);
    }

    pub(crate) fn matches_checked(&self, dom: &Dom, el: NodeId) -> bool {
        if is(dom, el, &local_name!("input")) {
            let t = attr(dom, el, &local_name!("type")).map(str::to_ascii_lowercase);
            if matches!(t.as_deref(), Some("checkbox") | Some("radio")) {
                return match self.html.borrow().controls.get(&el).and_then(|c| c.checked) {
                    Some(c) => c,
                    None => attr(dom, el, &local_name!("checked")).is_some(),
                };
            }
            return false;
        }
        if is(dom, el, &local_name!("option")) {
            return self.option_selected_in(dom, el);
        }
        false
    }

    pub(crate) fn matches_indeterminate(&self, dom: &Dom, el: NodeId) -> bool {
        if is(dom, el, &local_name!("input")) {
            return self
                .html
                .borrow()
                .controls
                .get(&el)
                .is_some_and(|c| c.indeterminate);
        }
        if is(dom, el, &local_name!("progress")) {
            return attr(dom, el, &local_name!("value")).is_none();
        }
        false
    }

    pub(crate) fn form_owner_of(&self, el: NodeId) -> Option<NodeId> {
        form_owner(&self.dom(), el)
    }

    fn validity_handle(&self, el: NodeId) -> Handle {
        self.alloc(Object::ValidityState { element: el })
    }

    fn labels_handle(&self, el: NodeId) -> Handle {
        self.collection(el, Filter::Labels(el))
    }

    fn opt_form(&self, el: NodeId) -> Option<Handle> {
        self.opt_node(self.form_owner_of(el))
    }

    // ----- options and selects ------------------------------------------------------------------------

    fn select_of(&self, dom: &Dom, option: NodeId) -> Option<NodeId> {
        let p = dom.parent(option)?;
        if is(dom, p, &local_name!("select")) || is(dom, p, &local_name!("datalist")) {
            return Some(p);
        }
        if is(dom, p, &local_name!("optgroup")) {
            let gp = dom.parent(p)?;
            if is(dom, gp, &local_name!("select")) {
                return Some(gp);
            }
        }
        None
    }

    pub(crate) fn options_of(&self, select: NodeId) -> Vec<NodeId> {
        let h = self.collection(select, Filter::Options);
        let n = self.coll_len(h);
        (0..n)
            .filter_map(|i| self.coll_item(h, i).and_then(super::handle_node))
            .collect()
    }

    /// Raw selectedness: the dirty value, else the `selected` attribute.
    fn raw_selected(&self, dom: &Dom, option: NodeId) -> bool {
        match self
            .html
            .borrow()
            .controls
            .get(&option)
            .and_then(|c| c.selected)
        {
            Some(s) => s,
            None => attr(dom, option, &local_name!("selected")).is_some(),
        }
    }

    /// The selectedness of each option of a select after HTML's "selectedness setting algorithm".
    fn select_selection(&self, dom: &Dom, select: NodeId, options: &[NodeId]) -> Vec<bool> {
        let mut sel: Vec<bool> = options.iter().map(|&o| self.raw_selected(dom, o)).collect();
        let multiple = attr(dom, select, &local_name!("multiple")).is_some();
        if !multiple {
            let size = attr(dom, select, &local_name!("size"))
                .and_then(rt::parse_non_negative_integer)
                .unwrap_or(1);
            match sel.iter().rposition(|&s| s) {
                Some(last) => {
                    for (i, s) in sel.iter_mut().enumerate() {
                        *s = i == last;
                    }
                }
                // (a select script emptied keeps no selection until the algorithm runs again)
                None if size <= 1
                    && !self
                        .html
                        .borrow()
                        .controls
                        .get(&select)
                        .is_some_and(|c| c.no_selection) =>
                {
                    if let Some(first) = options.iter().position(|&o| !is_disabled(dom, o)) {
                        sel[first] = true;
                    }
                }
                None => {}
            }
        }
        sel
    }

    pub(crate) fn option_selected_in(&self, dom: &Dom, option: NodeId) -> bool {
        match self.select_of(dom, option) {
            Some(select) if is(dom, select, &local_name!("select")) => {
                let options = {
                    let mut out = Vec::new();
                    for c in dom.children(select) {
                        if is(dom, c, &local_name!("option")) {
                            out.push(c);
                        } else if is(dom, c, &local_name!("optgroup")) {
                            out.extend(
                                dom.children(c)
                                    .filter(|&o| is(dom, o, &local_name!("option"))),
                            );
                        }
                    }
                    out
                };
                let sel = self.select_selection(dom, select, &options);
                options
                    .iter()
                    .position(|&o| o == option)
                    .is_some_and(|i| sel[i])
            }
            _ => self.raw_selected(dom, option),
        }
    }

    /// The selected options of a select, in tree order (its `selectedOptions`).
    pub(crate) fn selected_options_of(&self, select: NodeId) -> Vec<NodeId> {
        let options = self.options_of(select);
        let dom = self.dom();
        let sel = self.select_selection(&dom, select, &options);
        options
            .iter()
            .zip(sel)
            .filter(|(_, s)| *s)
            .map(|(o, _)| *o)
            .collect()
    }

    pub(crate) fn select_selected_index(&self, select: NodeId) -> i32 {
        let options = self.options_of(select);
        let dom = self.dom();
        let sel = self.select_selection(&dom, select, &options);
        sel.iter().position(|&s| s).map_or(-1, |i| i as i32)
    }

    pub(crate) fn select_set_selected_index(&self, _cx: &mut Cx, select: NodeId, index: i32) {
        let options = self.options_of(select);
        for (i, &o) in options.iter().enumerate() {
            let v = i as i32 == index;
            self.ctl(o, |c| c.selected = Some(v));
            self.form_state_changed(o);
        }
        let none = index < 0 || index as usize >= options.len();
        self.ctl(select, |c| c.no_selection = none);
    }

    /// An option or optgroup was inserted into, or removed from, `parent`: the select it is (or
    /// was) in runs its selectedness setting algorithm.
    pub(crate) fn option_tree_changed(&self, node: NodeId, parent: NodeId) {
        let select = {
            let dom = self.dom();
            if !is(&dom, node, &local_name!("option")) && !is(&dom, node, &local_name!("optgroup"))
            {
                return;
            }
            if is(&dom, parent, &local_name!("select")) {
                Some(parent)
            } else if is(&dom, parent, &local_name!("optgroup")) {
                dom.parent(parent)
                    .filter(|&g| is(&dom, g, &local_name!("select")))
            } else {
                None
            }
        };
        if let Some(s) = select {
            self.select_options_changed(s);
            // (its options are what it shows)
            if self.html.borrow().controls.contains_key(&s) {
                self.controls_dirty.borrow_mut().insert(s);
            }
        }
    }

    /// HTML §4.10.7: the selectedness setting algorithm runs again for `select` (an option was
    /// inserted or removed, the form reset): a selection script emptied is no longer kept empty.
    pub(crate) fn select_options_changed(&self, select: NodeId) {
        if self
            .html
            .borrow()
            .controls
            .get(&select)
            .is_some_and(|c| c.no_selection)
        {
            self.ctl(select, |c| c.no_selection = false);
        }
    }

    fn set_option_selected(&self, option: NodeId, v: bool) {
        let select = self.select_of(&self.dom(), option);
        if v {
            if let Some(s) = select {
                let multiple = self
                    .dom()
                    .attr(s, &ns!(), &local_name!("multiple"))
                    .is_some();
                if !multiple {
                    for o in self.options_of(s) {
                        self.ctl(o, |c| c.selected = Some(false));
                        self.form_state_changed(o);
                    }
                }
            }
        }
        self.ctl(option, |c| c.selected = Some(v));
        self.form_state_changed(option);
    }

    pub(crate) fn set_options_length(
        &self,
        cx: &mut Cx,
        select: NodeId,
        length: u32,
    ) -> rt::Result<()> {
        let options = self.options_of(select);
        let n = options.len() as u32;
        if length > 100_000 {
            return Ok(());
        }
        if length > n {
            let doc = self.dom().node_document(select);
            for _ in n..length {
                let o = self
                    .dom_mut()
                    .create_html_element(doc, local_name!("option"));
                self.dom_mut()
                    .append_child(select, o)
                    .map_err(super::dom_err)?;
            }
        } else {
            for &o in &options[length as usize..] {
                self.dom_mut().remove_node(o);
            }
        }
        self.after_mutation(cx);
        Ok(())
    }

    pub(crate) fn set_option_at(
        &self,
        cx: &mut Cx,
        select: NodeId,
        index: u32,
        value: Option<Handle>,
    ) -> rt::Result<()> {
        let options = self.options_of(select);
        let Some(value) = value else {
            if let Some(&o) = options.get(index as usize) {
                self.dom_mut().remove_node(o);
                self.after_mutation(cx);
            }
            return Ok(());
        };
        let new = self.n(value);
        if index as usize >= options.len() {
            let doc = self.dom().node_document(select);
            for _ in options.len()..index as usize {
                let o = self
                    .dom_mut()
                    .create_html_element(doc, local_name!("option"));
                self.dom_mut()
                    .append_child(select, o)
                    .map_err(super::dom_err)?;
            }
            self.dom_mut()
                .append_child(select, new)
                .map_err(super::dom_err)?;
        } else {
            let old = options[index as usize];
            let parent = self.dom().parent(old).unwrap_or(select);
            self.dom_mut()
                .replace_child(parent, new, old)
                .map_err(super::dom_err)?;
        }
        self.after_mutation(cx);
        Ok(())
    }

    /// `HTMLOptionsCollection.add` / `HTMLSelectElement.add` (HTML §2.7.3).
    pub(crate) fn select_add(
        &self,
        cx: &mut Cx,
        select: NodeId,
        element: HTMLOptionElementOrHTMLOptGroupElement,
        before: Option<HTMLElementOrLong>,
    ) -> rt::Result<()> {
        let el = match element {
            HTMLOptionElementOrHTMLOptGroupElement::HTMLOptionElement(h)
            | HTMLOptionElementOrHTMLOptGroupElement::HTMLOptGroupElement(h) => self.n(h),
        };
        if self.dom().is_inclusive_ancestor_of(el, select) {
            return Err(exception(
                ExceptionName::HierarchyRequestError,
                "The element is an ancestor of the select",
            ));
        }
        let reference = match before {
            None => None,
            Some(HTMLElementOrLong::HTMLElement(h)) => {
                let b = self.n(h);
                if !self.dom().is_ancestor_of(select, b) {
                    return Err(exception(
                        ExceptionName::NotFoundError,
                        "The reference element is not in the select",
                    ));
                }
                Some(b)
            }
            Some(HTMLElementOrLong::Long(i)) => {
                if i < 0 {
                    None
                } else {
                    self.options_of(select).get(i as usize).copied()
                }
            }
        };
        if reference == Some(el) {
            return Ok(());
        }
        let parent = match reference {
            Some(r) => self.dom().parent(r).unwrap_or(select),
            None => select,
        };
        let r = self.dom_mut().insert_before(parent, el, reference);
        self.after_mutation(cx);
        r.map(drop).map_err(super::dom_err)
    }

    pub(crate) fn select_remove_index(
        &self,
        cx: &mut Cx,
        select: NodeId,
        index: i32,
    ) -> rt::Result<()> {
        if index < 0 {
            return Ok(());
        }
        if let Some(&o) = self.options_of(select).get(index as usize) {
            self.dom_mut().remove_node(o);
            self.after_mutation(cx);
        }
        Ok(())
    }

    // ----- submission and reset -----------------------------------------------------------------------

    /// HTML §4.10.21.3 "submit" up to firing `submit`; navigation is not available headless.
    /// HTML §4.10.21.3 "submit" `form` from `submitter` (the form itself when `None`); `fire` is false
    /// for `form.submit()` ("submitted from submit() method").
    pub(crate) fn submit_form(
        &self,
        cx: &mut Cx,
        form: NodeId,
        submitter: Option<NodeId>,
        fire: bool,
    ) {
        // 1. A form that cannot navigate (not connected) does nothing.
        if !self.dom().is_connected(form) {
            return;
        }
        // 2. Constructing its entry list.
        if self.html.borrow().constructing_entry_list.contains(&form) {
            return;
        }
        let doc = self.dom().node_document(form);
        // 3. A document whose (CSP) sandbox lacks allow-forms submits nothing.
        if self.forms_sandboxed(doc) {
            self.console_print(
                "error",
                "Blocked form submission because the form's frame is sandboxed and the 'allow-forms' permission is not set.",
            );
            return;
        }
        let sub = submitter.unwrap_or(form);
        let attr_of = |el: NodeId, name: &str| -> Option<String> {
            self.dom().get_attribute(el, name).map(str::to_owned)
        };
        // A submitter's form* attributes override the form's (only buttons and inputs have them).
        let over = |name: &str, form_name: &str| -> Option<String> {
            submitter
                .filter(|&s| s != form)
                .and_then(|s| attr_of(s, form_name))
                .or_else(|| attr_of(form, name))
        };
        if fire {
            // 5.1-5.2: firing submission events already.
            if !self.html.borrow_mut().firing_submission.insert(form) {
                return;
            }
            // (each submittable element's user validity becomes true)
            {
                let h = self.collection(form, Filter::FormElements(form));
                for i in 0..self.coll_len(h) {
                    if let Some(c) = self.coll_item(h, i).and_then(super::handle_node) {
                        self.set_user_validity(c, true);
                    }
                }
                self.sync_form_states(Some(cx), FormSync::All);
            }
            // 5.3 Interactive validation unless novalidate.
            let novalidate = attr_of(form, "novalidate").is_some()
                || submitter
                    .filter(|&s| s != form)
                    .is_some_and(|s| attr_of(s, "formnovalidate").is_some());
            if !novalidate {
                let h = self.collection(form, Filter::FormElements(form));
                let n = self.coll_len(h);
                let mut valid = true;
                for i in 0..n {
                    if let Some(c) = self.coll_item(h, i).and_then(super::handle_node) {
                        valid &= self.check_validity_steps(cx, c);
                    }
                }
                if !valid {
                    self.html.borrow_mut().firing_submission.remove(&form);
                    return;
                }
            }
            // 5.6-5.7 The submit event, cancelable.
            let e = self.trusted_event(
                InterfaceId::SubmitEvent,
                "submit",
                true,
                true,
                super::events::EventExt::Submit((sub != form).then(|| node_handle(sub))),
            );
            let go = self.dispatch(cx, e, node_handle(form), false);
            self.html.borrow_mut().firing_submission.remove(&form);
            if !go || !self.dom().is_connected(form) {
                return;
            }
        }
        // 9. The method.
        let method = over("method", "formmethod")
            .map(|m| m.to_ascii_lowercase())
            .unwrap_or_default();
        let method = match method.as_str() {
            "post" => "post",
            "dialog" => "dialog",
            _ => "get",
        };
        if method == "dialog" {
            // HTML §4.10.21.3: the form's nearest ancestor dialog closes, its result the
            // submitter's value (an image button's selected coordinate as "x,y").
            let subject = {
                let dom = self.dom();
                dom.ancestors(form)
                    .find(|&a| is(&dom, a, &local_name!("dialog")))
            };
            let Some(subject) = subject else {
                return;
            };
            let result = submitter.filter(|&s| s != form).and_then(|s| {
                if self.input_type(s).as_deref() == Some("image") {
                    Some("0,0".to_owned())
                } else {
                    attr_of(s, "value")
                }
            });
            self.close_the_dialog(cx, subject, result.as_deref(), None);
            return;
        }
        // 11. The action: empty is the document's URL; else encoding-parsed relative to the
        // submitter's node document.
        let action = over("action", "formaction").unwrap_or_default();
        let url = if action.trim().is_empty() {
            url::Url::parse(self.dom().document_url(doc)).ok()
        } else {
            self.encoding_parse_url(sub, action.trim())
        };
        let Some(mut url) = url else {
            return;
        };
        // 6-7. The form's encoding, and the entry list through the `formdata` event; none while
        // constructing one.
        let encoding = self.pick_form_encoding(form);
        let Some(entries) =
            self.construct_entry_list(cx, form, (sub != form).then_some(sub), encoding)
        else {
            return;
        };
        // 8. A form no longer connected after the event submits nothing.
        if !self.dom().is_connected(form) {
            return;
        }
        let enctype = over("enctype", "formenctype")
            .map(|e| e.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let enctype = match enctype.as_str() {
            "multipart/form-data" => "multipart/form-data",
            "text/plain" => "text/plain",
            _ => "application/x-www-form-urlencoded",
        };
        // HTML §4.10.21.6 "convert to a list of name-value pairs": a File is its name, and names
        // and values have their newlines normalized to CRLF.
        let pairs: Vec<(String, String)> = entries
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    FormValue::Str(s) => s.clone(),
                    FormValue::File(f) => self.blob_file_name(*f).unwrap_or_default(),
                };
                (normalize_newlines(k), normalize_newlines(&v))
            })
            .collect();
        let urlencoded = |entries: &[(String, String)]| urlencoded_serialize(entries, encoding);
        let post: Option<(Vec<u8>, String)> = match (url.scheme(), method) {
            ("http" | "https" | "data" | "file", "get") => {
                // Mutate action URL: the query is the entry list.
                url.set_query(Some(&urlencoded(&pairs)));
                None
            }
            ("http" | "https", "post") => Some(match enctype {
                "multipart/form-data" => {
                    let boundary = format!(
                        "----SnowlarkFormBoundary{}",
                        &crate::engine::crypto::random_uuid().replace('-', "")[..16]
                    );
                    (
                        self.multipart_entries(&entries, &boundary, encoding),
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                }
                "text/plain" => {
                    let mut body = String::new();
                    for (k, v) in &pairs {
                        body.push_str(&format!("{k}={v}\r\n"));
                    }
                    (
                        encode_html(encoding, &body).into_owned(),
                        "text/plain".into(),
                    )
                }
                _ => (
                    urlencoded(&pairs).into_bytes(),
                    "application/x-www-form-urlencoded".into(),
                ),
            }),
            (s, "get") if self.embedder_scheme(s) => {
                url.set_query(Some(&urlencoded(&pairs)));
                None
            }
            ("javascript", _) => None,
            ("data" | "file", _) => None,
            // (mailto: and other schemes: nothing)
            _ => return,
        };
        // 12-13. The target: the submitter's formtarget, the form's target, the base element's.
        let target = over("target", "formtarget")
            .or_else(|| self.base_target(doc))
            .unwrap_or_default();
        // CSP form-action for a `javascript:` action, which is not fetched (the network layer checks
        // the others, again at each redirect).
        if url.scheme() == "javascript" && self.form_action_blocked(cx, doc, &url) {
            return;
        }
        // 22-23. Replace the form's planned navigation (see `cancel_planned_form_navigation`),
        // then plan to navigate the chosen navigable.
        if fire {
            self.cancel_planned_form_navigation(form);
        }
        if let Some(id) = self.navigate_element_target(cx, form, &target, url, post, true) {
            let planned = self.frames.borrow().list[id].navigation_request.clone();
            if let Some(planned) = planned {
                self.html
                    .borrow_mut()
                    .planned_navigations
                    .insert(form, (id, planned));
            }
        }
    }

    /// HTML §4.10.22 "reset" a form.
    pub(crate) fn reset_form(&self, cx: &mut Cx, form: NodeId) {
        if !self.fire_event(cx, node_handle(form), "reset", true, true) {
            return;
        }
        let h = self.collection(form, Filter::FormElements(form));
        let n = self.coll_len(h);
        let controls: Vec<NodeId> = (0..n)
            .filter_map(|i| self.coll_item(h, i).and_then(super::handle_node))
            .collect();
        for c in controls {
            self.ctl(c, |s| {
                s.value = None;
                s.editor = None;
                s.checked = None;
                s.selected = None;
                s.indeterminate = false;
                s.no_selection = false;
                s.user_validity = false;
            });
            self.form_state_changed(c);
            if self.dom().is_html_element_named(c, &local_name!("select")) {
                for o in self.options_of(c) {
                    self.ctl(o, |s| s.selected = None);
                }
                for o in self.options_of(c) {
                    self.sync_checked_state(o);
                }
            }
            self.sync_checked_state(c);
            if self.dom().is_html_element_named(c, &local_name!("output")) {
                self.ctl(c, |s| s.default_value = None);
            }
        }
    }

    fn will_validate(&self, el: NodeId) -> bool {
        let dom = self.dom();
        if is_disabled(&dom, el) || attr(&dom, el, &local_name!("readonly")).is_some() {
            return false;
        }
        if dom
            .ancestors(el)
            .any(|a| is(&dom, a, &local_name!("datalist")))
        {
            return false;
        }
        if is(&dom, el, &local_name!("input")) {
            drop(dom);
            return !matches!(
                self.input_type(el).as_deref(),
                Some("hidden") | Some("reset") | Some("button")
            );
        }
        if is(&dom, el, &local_name!("button")) {
            return button_type_state(&dom, el) == "submit";
        }
        is(&dom, el, &local_name!("select")) || is(&dom, el, &local_name!("textarea"))
    }

    /// "Suffering from being missing" (HTML §4.10.5.1 per type, §4.10.7, §4.10.11): a required
    /// control with no value. Text-like fields and `textarea` only count while mutable; checkboxes,
    /// radio groups, file inputs and `select` whatever their state.
    fn value_missing(&self, el: NodeId) -> bool {
        let dom = self.dom();
        let required = |e: NodeId| attr(&dom, e, &local_name!("required")).is_some();
        let mutable =
            |e: NodeId| !is_disabled(&dom, e) && attr(&dom, e, &local_name!("readonly")).is_none();
        if is(&dom, el, &local_name!("input")) {
            let (required_el, mutable_el) = (required(el), mutable(el));
            drop(dom);
            return match self.input_type(el).as_deref() {
                Some("checkbox") => required_el && !self.checkedness(el),
                // (a radio button without a name is in no group, and never missing)
                Some("radio") => {
                    let named =
                        attr(&self.dom(), el, &local_name!("name")).is_some_and(|n| !n.is_empty());
                    let group = self.radio_group(el);
                    let any_required = {
                        let dom = self.dom();
                        group
                            .iter()
                            .any(|&r| attr(&dom, r, &local_name!("required")).is_some())
                    };
                    named && any_required && !group.iter().any(|&r| self.checkedness(r))
                }
                Some("file") => required_el,
                Some("hidden" | "range" | "color" | "submit" | "image" | "reset" | "button") => {
                    false
                }
                _ => required_el && mutable_el && self.control_value(el).is_empty(),
            };
        }
        if !required(el) {
            return false;
        }
        if is(&dom, el, &local_name!("textarea")) {
            let mutable_el = mutable(el);
            drop(dom);
            return mutable_el && self.control_value(el).is_empty();
        }
        if is(&dom, el, &local_name!("select")) {
            drop(dom);
            // HTML §4.10.7: no selected option, or only the placeholder label option selected (the
            // first option, a child of the select itself, with an empty value, when the select
            // has neither `multiple` nor a display size above 1).
            let selected = self.selected_options_of(el);
            let Some(&first_selected) = selected.first() else {
                return true;
            };
            if selected.len() > 1 {
                return false;
            }
            let options = self.options_of(el);
            let dom = self.dom();
            let single = attr(&dom, el, &local_name!("multiple")).is_none()
                && attr(&dom, el, &local_name!("size"))
                    .and_then(rt::parse_non_negative_integer)
                    .is_none_or(|s| s <= 1);
            let placeholder = options.first().copied().filter(|&o| {
                dom.parent(o) == Some(el)
                    && attr(&dom, o, &local_name!("value"))
                        .map(str::to_owned)
                        .unwrap_or_else(|| self_text(&dom, o))
                        .is_empty()
            });
            return single && placeholder == Some(first_selected);
        }
        false
    }

    /// HTML §4.10.5.1.4-5 "suffering from a type mismatch": a URL field's value that is not a valid
    /// absolute URL, an email field's that is not a valid email address (or list of them).
    fn type_mismatch_of(&self, el: NodeId) -> bool {
        let kind = self.input_type(el);
        if !matches!(kind.as_deref(), Some("url") | Some("email")) {
            return false;
        }
        let value = self.control_value(el);
        if value.is_empty() {
            return false;
        }
        if kind.as_deref() == Some("url") {
            return url::Url::parse(&value).is_err();
        }
        if attr(&self.dom(), el, &local_name!("multiple")).is_some() {
            return !value.split(',').map(trim_ascii_ws).all(is_valid_email);
        }
        !is_valid_email(&value)
    }

    /// HTML §4.10.5.3.8 "suffering from a pattern mismatch": the `pattern` attribute's compiled
    /// regular expression (the "v" flag, anchored) does not match the value (each value of an
    /// email field with `multiple`). A pattern that does not compile is no constraint.
    fn pattern_mismatch_of(&self, cx: &mut Cx, el: NodeId) -> bool {
        let kind = self.input_type(el);
        if !matches!(
            kind.as_deref(),
            Some("text" | "search" | "tel" | "url" | "email" | "password")
        ) {
            return false;
        }
        let Some(pattern) = attr(&self.dom(), el, &local_name!("pattern")).map(str::to_owned)
        else {
            return false;
        };
        let value = self.control_value(el);
        if value.is_empty() {
            return false;
        }
        let multiple = kind.as_deref() == Some("email")
            && attr(&self.dom(), el, &local_name!("multiple")).is_some();
        let values: Vec<&str> = if multiple {
            value.split(',').map(trim_ascii_ws).collect()
        } else {
            vec![value.as_str()]
        };
        match pattern_regexp_matches(cx, &pattern, &values) {
            Some(all) => !all,
            None => false,
        }
    }

    /// HTML §4.10.5.3.7-9 for a numeric field: (underflow, overflow, step mismatch), with its
    /// value, minimum and maximum as numbers of its type. A time field whose maximum is below its
    /// minimum has a reversed range: a value between the two is both.
    fn range_and_step_of(&self, el: NodeId) -> (bool, bool, bool) {
        let Some(n) = self.numeric(el) else {
            return (false, false, false);
        };
        let Some(v) = string_to_number(&n.kind, &self.control_value(el)) else {
            return (false, false, false);
        };
        let (under, over) = match (n.min, n.max) {
            (Some(lo), Some(hi)) if n.kind == "time" && hi < lo => {
                let out = v > hi && v < lo;
                (out, out)
            }
            (lo, hi) => (lo.is_some_and(|m| v < m), hi.is_some_and(|m| v > m)),
        };
        let mismatch = n.step.is_some_and(|s| {
            let q = (v - n.base) / s;
            (q - q.round()).abs() > 1e-9 * q.abs().max(1.0)
        });
        (under, over, mismatch)
    }

    /// Whether the element satisfies its constraints, but for the pattern (which needs a context).
    fn is_valid(&self, el: NodeId) -> bool {
        let (under, over, step) = self.range_and_step_of(el);
        !self.value_missing(el)
            && !self.bad_input_of(el)
            && !self.type_mismatch_of(el)
            && !(under || over || step)
            && self.ctl_read(el, |c| c.custom_validity.is_empty())
    }

    /// HTML §4.10.21.1 "satisfies its constraints".
    fn is_valid_cx(&self, cx: &mut Cx, el: NodeId) -> bool {
        self.is_valid(el) && !self.pattern_mismatch_of(cx, el)
    }

    fn check_validity_steps(&self, cx: &mut Cx, el: NodeId) -> bool {
        if !self.will_validate(el) || self.is_valid_cx(cx, el) {
            return true;
        }
        self.fire_event(cx, node_handle(el), "invalid", false, true);
        false
    }

    // ----- the form pseudo-classes' state (HTML §4.16.3) ---------------------------------------------

    /// Something `el`'s `:valid`/`:invalid`, `:in-range`/`:out-of-range`, `:user-valid`/
    /// `:user-invalid` or `:enabled`/`:disabled` depend on may have changed: they follow at the next
    /// [`Engine::sync_form_states`]. An element that is neither a form control nor a container
    /// stands for the fieldsets and form around it (a control left it).
    pub(crate) fn form_state_changed(&self, el: NodeId) {
        self.mark_form_states(&[el]);
    }

    /// Marks elements for [`Engine::sync_form_states`]: a flag on each, and the list of the
    /// flagged (a mark of a marked element costs a flag test, and the list has no repeats). One
    /// whose constraint validation may need a script context (a `pattern` to test) is also noted
    /// for [`FormSync::Script`], which brings it up to date after each DOM operation.
    fn mark_form_states(&self, nodes: &[NodeId]) {
        use snowlark_dom::element_flags::FORM_STATE_MARKED;
        let mut dom = self.dom_mut();
        let mut list = self.form_states_dirty.borrow_mut();
        for &n in nodes {
            if !dom.is_alive(n) || !dom.is_element(n) {
                continue;
            }
            if dom.element_flags(n) & FORM_STATE_MARKED == 0 {
                dom.set_element_flag(n, FORM_STATE_MARKED, true);
                list.push(n);
            }
            if is(&dom, n, &local_name!("input"))
                && attr(&dom, n, &local_name!("pattern")).is_some()
            {
                self.form_states_dirty_script.borrow_mut().insert(n);
            }
        }
    }

    /// [`Engine::form_state_changed`] for `root` and every element in its subtree whose state is
    /// one of those (an inserted subtree, a fieldset or datalist whose descendants' state follows).
    pub(crate) fn form_state_changed_subtree(&self, root: NodeId) {
        let nodes: SmallVec<[NodeId; 8]> = {
            let dom = self.dom();
            std::iter::once(root)
                .chain(dom.descendants(root).filter(|&d| has_form_state(&dom, d)))
                .collect()
        };
        self.mark_form_states(&nodes);
    }

    /// Brings the form pseudo-classes up to date before DOM selector matching of a list, when it
    /// reads them (`reads`: [`super::select::reads_form_state`]).
    pub(crate) fn sync_form_states_if(&self, cx: &mut Cx, reads: bool) {
        if reads {
            self.sync_form_states(Some(cx), FormSync::All);
        }
    }

    /// The number of times a control's form pseudo-class state was computed (a cost counter).
    pub fn form_states_computed(&self) -> u64 {
        self.form_states_computed.get()
    }

    /// The last computed form pseudo-class state of `el` (validity, range, user validity and
    /// disabledness bits), for the DOM's own selector matching.
    pub(crate) fn form_state(&self, el: NodeId) -> ElementState {
        self.form_states
            .borrow()
            .get(&el)
            .copied()
            .unwrap_or(ElementState::empty())
    }

    /// Brings the form pseudo-classes up to date for the elements [`Engine::form_state_changed`]
    /// named, in the style engine and for [`Engine::form_state`]: a control's candidacy for
    /// constraint validation and whether it satisfies its constraints, a numeric field's range
    /// limitations, its user validity; then the forms and fieldsets around them. The pattern
    /// constraint needs a script context: without `cx`, a control whose pattern applies keeps its
    /// state and stays dirty for the next call with one.
    ///
    /// Like validity in HTML's own model, the state is computed when something reads it, not at
    /// each change: `scope` says which of the marked elements the reader needs (a list rebuilt
    /// several times before a rendering update computes only the controls still in the document).
    pub(crate) fn sync_form_states(&self, mut cx: Option<&mut Cx>, scope: FormSync) {
        let dirty: Vec<NodeId> = match scope {
            FormSync::Script => {
                if self.form_states_dirty_script.borrow().is_empty() {
                    return;
                }
                // (and they leave the list the reads take, whose entries count only while
                // flagged: a pattern is tested once per change, here, where a script context
                // is, and a runaway one stopped with the script)
                let script: Vec<NodeId> =
                    self.form_states_dirty_script.borrow_mut().drain().collect();
                let mut dom = self.dom_mut();
                for &n in &script {
                    if dom.is_alive(n) && dom.is_element(n) {
                        dom.set_element_flag(
                            n,
                            snowlark_dom::element_flags::FORM_STATE_MARKED,
                            false,
                        );
                    }
                }
                script
            }
            FormSync::Connected => {
                if self.form_states_dirty.borrow().is_empty() {
                    return;
                }
                let mut dom = self.dom_mut();
                let mut detached = self.form_states_dirty_detached.borrow_mut();
                let mut connected = Vec::new();
                let marked = std::mem::take(&mut *self.form_states_dirty.borrow_mut());
                for n in marked {
                    // (an entry whose flag went is done: `FormSync::Script` computed it)
                    if !dom.is_alive(n)
                        || dom.element_flags(n) & snowlark_dom::element_flags::FORM_STATE_MARKED
                            == 0
                    {
                        continue;
                    }
                    // (a detached one leaves the list too: marked again, it is listed again)
                    dom.set_element_flag(n, snowlark_dom::element_flags::FORM_STATE_MARKED, false);
                    if dom.is_connected(n) {
                        connected.push(n);
                    } else {
                        detached.push(n);
                    }
                }
                // (a control marked again while out of the document is listed again: the repeats
                // and the dead go now and then)
                if detached.len() >= 4096
                    && detached.len() >= 2 * self.form_states_detached_len.get()
                {
                    detached.sort_unstable();
                    detached.dedup();
                    detached.retain(|&n| dom.is_alive(n));
                    self.form_states_detached_len.set(detached.len());
                }
                connected
            }
            FormSync::All => {
                let mut all = std::mem::take(&mut *self.form_states_dirty.borrow_mut());
                let mut dom = self.dom_mut();
                // (an entry whose flag went is done: `FormSync::Script` computed it)
                all.retain(|&n| {
                    dom.is_alive(n)
                        && dom.element_flags(n) & snowlark_dom::element_flags::FORM_STATE_MARKED
                            != 0
                });
                for &n in &all {
                    dom.set_element_flag(n, snowlark_dom::element_flags::FORM_STATE_MARKED, false);
                }
                drop(dom);
                all.append(&mut self.form_states_dirty_detached.borrow_mut());
                self.form_states_detached_len.set(0);
                all.sort_unstable();
                all.dedup();
                all
            }
        };
        if dirty.is_empty() {
            return;
        }
        let mut seen: rustc_hash::FxHashSet<NodeId> = Default::default();
        let mut controls: Vec<NodeId> = Vec::new();
        let mut containers: Vec<NodeId> = Vec::new();
        let mut radios: Vec<NodeId> = Vec::new();
        {
            let dom = self.dom();
            for n in dirty {
                if !dom.is_alive(n) || !dom.is_element(n) {
                    continue;
                }
                if is_container(&dom, n) {
                    // (and the forms and fieldsets around it, whose controls it holds)
                    containers.push(n);
                    containers.extend(dom.ancestors(n).filter(|&a| is_container(&dom, a)));
                } else if has_form_state(&dom, n) {
                    // (an option's selectedness and label are its select's value)
                    if let Some(s) = select_of(&dom, n) {
                        if seen.insert(s) {
                            controls.push(s);
                        }
                    }
                    if seen.insert(n) {
                        controls.push(n);
                        if is(&dom, n, &local_name!("input"))
                            && attr(&dom, n, &local_name!("type"))
                                .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
                        {
                            radios.push(n);
                        }
                    }
                } else {
                    containers.extend(dom.ancestors(n).filter(|&a| is_container(&dom, a)));
                }
            }
        }
        // (a radio button's missing value is its group's)
        let mut grouped: rustc_hash::FxHashSet<NodeId> = Default::default();
        for r in radios {
            if !grouped.insert(r) {
                continue;
            }
            for m in self.radio_group(r) {
                grouped.insert(m);
                if seen.insert(m) {
                    controls.push(m);
                }
            }
        }
        for c in controls {
            // (`:checked` and `:indeterminate` as checkedness and selectedness have them)
            self.sync_checked_state(c);
            self.form_states_computed
                .set(self.form_states_computed.get() + 1);
            let Some(state) = self.control_form_state(cx.as_deref_mut(), c) else {
                // (marked again, and for the next operation's end, which has a context)
                self.mark_form_states(&[c]);
                self.form_states_dirty_script.borrow_mut().insert(c);
                continue;
            };
            self.apply_form_state(c, state);
            let dom = self.dom();
            containers.extend(
                dom.ancestors(c)
                    .filter(|&a| is(&dom, a, &local_name!("fieldset"))),
            );
            drop(dom);
            containers.extend(self.form_owner_of(c));
        }
        containers.sort_unstable();
        containers.dedup();
        for k in containers {
            let invalid = self.container_has_invalid(k);
            let mut state = if invalid {
                ElementState::INVALID
            } else {
                ElementState::VALID
            };
            if is(&self.dom(), k, &local_name!("fieldset")) {
                // (its own `:disabled` is its attribute's and its fieldset ancestors')
                state |= self.control_form_state_disabled(k);
            }
            self.apply_form_state(k, state);
        }
    }

    /// The bits of `FORM_STATE_BITS` that are on for a control (a listed element, an option or an
    /// optgroup); `None` when only the pattern constraint is undecided and there is no `cx`.
    fn control_form_state(&self, cx: Option<&mut Cx>, el: NodeId) -> Option<ElementState> {
        let mut state = self.control_form_state_disabled(el);
        if !self.will_validate(el) {
            return Some(state);
        }
        let (under, over, step) = self.range_and_step_of(el);
        let mut valid = !self.value_missing(el)
            && !self.bad_input_of(el)
            && !self.type_mismatch_of(el)
            && !(under || over || step)
            && self.ctl_read(el, |c| c.custom_validity.is_empty());
        if valid && self.pattern_applies(el) {
            valid = !self.pattern_mismatch_of(cx?, el);
        }
        state |= if valid {
            ElementState::VALID
        } else {
            ElementState::INVALID
        };
        // Selectors 4 §11.4: a candidate with range limitations (a minimum or a maximum).
        if self
            .numeric(el)
            .is_some_and(|n| n.min.is_some() || n.max.is_some())
        {
            state |= if under || over {
                ElementState::OUTOFRANGE
            } else {
                ElementState::INRANGE
            };
        }
        // HTML §4.16.3 `:user-valid`/`:user-invalid`: a candidate whose user validity is true.
        if self.ctl_read(el, |c| c.user_validity) {
            state |= if valid {
                ElementState::USER_VALID
            } else {
                ElementState::USER_INVALID
            };
        }
        Some(state)
    }

    /// `:enabled`/`:disabled` for an element that can be disabled ("actually disabled": its
    /// attribute, a disabled optgroup parent, or a disabled fieldset ancestor outside its first
    /// legend); nothing for the others.
    fn control_form_state_disabled(&self, el: NodeId) -> ElementState {
        let dom = self.dom();
        if !super::html::can_be_disabled(&dom, el) {
            return ElementState::empty();
        }
        if is_disabled(&dom, el) {
            ElementState::DISABLED
        } else {
            ElementState::ENABLED
        }
    }

    /// Whether a form or fieldset has a candidate for constraint validation that does not satisfy
    /// its constraints (HTML §4.16.3 `:invalid`): a form among the controls it owns, a fieldset
    /// among its descendants. Reads the controls' computed state.
    fn container_has_invalid(&self, k: NodeId) -> bool {
        let states = self.form_states.borrow();
        let invalid = |c: NodeId| {
            states
                .get(&c)
                .is_some_and(|s| s.contains(ElementState::INVALID))
        };
        let dom = self.dom();
        if is(&dom, k, &local_name!("fieldset")) {
            return dom
                .descendants(k)
                .any(|d| !is_container(&dom, d) && invalid(d));
        }
        // (the controls a form owns: its descendants that name no other form, and, when it has an
        // ID, any control in its tree that names it)
        if attr(&dom, k, &local_name!("id")).is_none_or(str::is_empty) {
            return dom.descendants(k).any(|d| {
                !is_container(&dom, d)
                    && invalid(d)
                    && (attr(&dom, d, &local_name!("form")).is_none()
                        || form_owner(&dom, d) == Some(k))
            });
        }
        dom.descendants(dom.root(k))
            .any(|d| !is_container(&dom, d) && invalid(d) && form_owner(&dom, d) == Some(k))
    }

    /// Records `state` as `el`'s form pseudo-class state and hands it to the style engine.
    fn apply_form_state(&self, el: NodeId, state: ElementState) {
        let old = self
            .form_states
            .borrow_mut()
            .insert(el, state)
            .unwrap_or(ElementState::empty());
        if old == state && old != ElementState::empty() {
            return;
        }
        let mut mask = ElementState::VALID
            | ElementState::INVALID
            | ElementState::INRANGE
            | ElementState::OUTOFRANGE
            | ElementState::USER_VALID
            | ElementState::USER_INVALID;
        if super::html::can_be_disabled(&self.dom(), el) {
            mask |= ElementState::ENABLED | ElementState::DISABLED;
        }
        // (one call for all of them: each reaches the style engine through the renderer)
        self.set_element_states(el, mask, state);
    }

    /// Whether the `pattern` attribute constrains `el` now (a type it applies to, a value to test).
    fn pattern_applies(&self, el: NodeId) -> bool {
        matches!(
            self.input_type(el).as_deref(),
            Some("text" | "search" | "tel" | "url" | "email" | "password")
        ) && attr(&self.dom(), el, &local_name!("pattern")).is_some()
            && !self.control_value(el).is_empty()
    }

    /// HTML §4.10.5.5 / §4.10.11 / §4.10.7: the user committed a change to a control (the
    /// `change` event it fires for the user): its user validity becomes true.
    pub(crate) fn set_user_validity(&self, el: NodeId, v: bool) {
        if self.ctl_read(el, |c| c.user_validity) != v {
            self.ctl(el, |c| c.user_validity = v);
            self.form_state_changed(el);
        }
    }

    /// The insertion of `node` (with its subtree when the batch gives its descendants no reactions
    /// of their own): its form-state elements, and the select an inserted option belongs to.
    pub(crate) fn form_state_inserted(&self, node: NodeId, subtree: bool) {
        let mut marks: SmallVec<[NodeId; 8]> = SmallVec::new();
        {
            let dom = self.dom();
            let mut add = |d: NodeId| {
                if has_form_state(&dom, d) {
                    marks.push(d);
                    if let Some(s) = select_of(&dom, d) {
                        marks.push(s);
                    }
                }
            };
            add(node);
            if subtree {
                dom.descendants(node).for_each(add);
            }
        }
        if !marks.is_empty() {
            self.mark_form_states(&marks);
        }
    }

    /// The removal of `node` from `old_parent`: the forms and fieldsets it left, the select it left,
    /// and the radio buttons of a group it left.
    ///
    /// `descendants` are the node's shadow-including descendants as its removal collected them
    /// (`Reaction::Removed`): the elements of its shadow trees add marks that change nothing but
    /// the work [`Engine::sync_form_states`] does, which a second walk of the subtree would cost.
    pub(crate) fn form_state_removed(
        &self,
        node: NodeId,
        old_parent: NodeId,
        descendants: &[NodeId],
    ) {
        if let Some(marks) = self.form_state_removed_marks(node, old_parent, descendants) {
            self.mark_form_states(&marks);
        }
    }

    /// What [`Engine::form_state_removed`] marks (`None`: nothing with a form state left).
    fn form_state_removed_marks(
        &self,
        node: NodeId,
        old_parent: NodeId,
        descendants: &[NodeId],
    ) -> Option<SmallVec<[NodeId; 8]>> {
        let dom = self.dom();
        let radio_name = |d: NodeId| {
            (is(&dom, d, &local_name!("input"))
                && attr(&dom, d, &local_name!("type"))
                    .is_some_and(|t| t.eq_ignore_ascii_case("radio")))
            .then(|| attr(&dom, d, &local_name!("name")))
            .flatten()
            .filter(|n| !n.is_empty())
        };
        // (one pass over the removed subtree: whether it held an element with a form state, and
        // the groups of the radio buttons in it)
        let mut held = false;
        let mut names: SmallVec<[&str; 2]> = SmallVec::new();
        for d in std::iter::once(node).chain(descendants.iter().copied()) {
            if dom.is_alive(d) && has_form_state(&dom, d) {
                held = true;
                if let Some(n) = radio_name(d) {
                    names.push(n);
                }
            }
        }
        if !held {
            return None;
        }
        let mut marks: SmallVec<[NodeId; 8]> = SmallVec::new();
        marks.push(old_parent);
        if let Some(s) = select_of(&dom, old_parent)
            .or_else(|| is(&dom, old_parent, &local_name!("select")).then_some(old_parent))
        {
            marks.push(s);
        }
        if !names.is_empty() {
            let root = dom.root(old_parent);
            for d in dom.descendants(root) {
                if radio_name(d).is_some_and(|n| names.contains(&n)) {
                    marks.push(d);
                }
            }
        }
        Some(marks)
    }

    /// An attribute of `el` changed: its own state, a fieldset's or datalist's descendants', an
    /// optgroup's options', an option's select's.
    pub(crate) fn form_state_attribute_changed(&self, el: NodeId) {
        let dom = self.dom();
        let datalist = is(&dom, el, &local_name!("datalist"));
        if !has_form_state(&dom, el) && !datalist {
            return;
        }
        if datalist || is(&dom, el, &local_name!("fieldset")) {
            drop(dom);
            self.form_state_changed_subtree(el);
            return;
        }
        let mut marks: SmallVec<[NodeId; 8]> = SmallVec::new();
        marks.push(el);
        if let Some(s) = select_of(&dom, el) {
            marks.push(s);
        }
        if is(&dom, el, &local_name!("optgroup")) {
            marks.extend(dom.element_children(el));
        }
        drop(dom);
        self.mark_form_states(&marks);
    }

    /// `parent`'s children changed: a `textarea`'s default value, an option's label (its select's
    /// placeholder option).
    pub(crate) fn form_state_children_changed(&self, parent: NodeId) {
        let mark = {
            let dom = self.dom();
            if is(&dom, parent, &local_name!("textarea")) {
                Some(parent)
            } else {
                select_of(&dom, parent)
                    .or_else(|| is(&dom, parent, &local_name!("select")).then_some(parent))
            }
        };
        if let Some(m) = mark {
            self.mark_form_states(&[m]);
        }
    }

    fn validation_message_of(&self, el: NodeId) -> String {
        if !self.will_validate(el) {
            return String::new();
        }
        let custom = self.ctl(el, |c| c.custom_validity.clone());
        if !custom.is_empty() {
            return custom;
        }
        if self.value_missing(el) {
            return "Please fill out this field.".into();
        }
        if self.bad_input_of(el) {
            return "Please enter a number.".into();
        }
        if self.type_mismatch_of(el) {
            return match self.input_type(el).as_deref() {
                Some("email") => "Please enter an email address.".into(),
                _ => "Please enter a URL.".into(),
            };
        }
        match self.range_and_step_of(el) {
            (true, _, _) => "Value must be greater than or equal to the minimum.".into(),
            (_, true, _) => "Value must be less than or equal to the maximum.".into(),
            (_, _, true) => "Please enter a valid value.".into(),
            _ => String::new(),
        }
    }
}

/// An option's text (HTML §4.10.10): child text, stripped and collapsed, excluding script elements.
fn self_text(dom: &Dom, option: NodeId) -> String {
    let mut s = String::new();
    for d in dom.descendants(option) {
        // (descendants of HTML and SVG `script` elements are left out; a MathML or null-namespace
        // element named "script" is not one)
        if dom.ancestors(d).take_while(|&a| a != option).any(|a| {
            dom.element(a).is_some_and(|e| {
                &*e.name().local == "script"
                    && (e.name().ns == ns!(html) || e.name().ns == ns!(svg))
            })
        }) {
            continue;
        }
        if let Some(t) = dom.character_data(d) {
            if dom.is_text(d) {
                s.push_str(t);
            }
        }
    }
    super::document::strip_and_collapse(&s)
}

macro_rules! validation_members {
    () => {
        fn will_validate(&self, this: Handle) -> bool {
            Engine::will_validate(self, self.n(this))
        }
        fn validity(&self, this: Handle) -> Handle {
            self.validity_handle(self.n(this))
        }
        fn validation_message(&self, this: Handle) -> DomStr<'_> {
            DomStr::Owned(self.validation_message_of(self.n(this)))
        }
        fn check_validity(&self, cx: &mut Cx, this: Handle) -> rt::Result<bool> {
            Ok(self.check_validity_steps(cx, self.n(this)))
        }
        fn report_validity(&self, cx: &mut Cx, this: Handle) -> rt::Result<bool> {
            Ok(self.check_validity_steps(cx, self.n(this)))
        }
        fn set_custom_validity(&self, _cx: &mut Cx, this: Handle, error: &str) -> rt::Result<()> {
            self.ctl(self.n(this), |c| c.custom_validity = error.to_owned());
            self.form_state_changed(self.n(this));
            Ok(())
        }
    };
}

impl HTMLInputElementImpl for Engine {
    fn files(&self, this: Handle) -> Option<Handle> {
        let el = self.n(this);
        if self.input_type(el).as_deref() != Some("file") {
            return None;
        }
        if let Some(h) = self.html.borrow().files.get(&el) {
            return Some(*h);
        }
        let h = self.alloc(Object::FileList(Vec::new()));
        self.html.borrow_mut().files.insert(el, h);
        Some(h)
    }
    fn set_files(&self, _cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        let el = self.n(this);
        if let (Some(v), Some("file")) = (value, self.input_type(el).as_deref()) {
            self.html.borrow_mut().files.insert(el, v);
        }
        Ok(())
    }
    fn autocomplete(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.autofill_idl(self.n(this)))
    }
    fn checked(&self, this: Handle) -> bool {
        self.checkedness(self.n(this))
    }
    fn set_checked(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_checkedness(cx, self.n(this), value);
        Ok(())
    }
    fn color_space(&self, this: Handle) -> DomStr<'_> {
        let v = self
            .attr_str(self.n(this), "colorspace")
            .map(|v| v.to_ascii_lowercase());
        DomStr::Borrowed(if v.as_deref() == Some("display-p3") {
            "display-p3"
        } else {
            "limited-srgb"
        })
    }
    fn set_color_space(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "colorspace", value);
        Ok(())
    }
    fn form(&self, this: Handle) -> Option<Handle> {
        self.opt_form(self.n(this))
    }
    fn form_action(&self, this: Handle) -> DomStr<'_> {
        self.form_action_of(self.n(this))
    }
    fn height(&self, this: Handle) -> u32 {
        self.attr_str(self.n(this), "height")
            .and_then(|v| rt::parse_non_negative_integer(&v))
            .unwrap_or(0) as u32
    }
    fn indeterminate(&self, this: Handle) -> bool {
        Engine::indeterminate(self, self.n(this))
    }
    fn set_indeterminate(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_indeterminate_state(self.n(this), value);
        Ok(())
    }
    fn list(&self, this: Handle) -> Option<Handle> {
        let el = self.n(this);
        let dom = self.dom();
        let id = attr(&dom, el, &LocalName::from("list"))?;
        let root = dom.root(el);
        dom.descendants(root)
            .find(|&d| dom.is_element(d) && attr(&dom, d, &local_name!("id")) == Some(id))
            .filter(|&d| is(&dom, d, &local_name!("datalist")))
            .map(node_handle)
    }
    fn value(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        let kind = self.input_type(el).unwrap_or_default();
        match Engine::value_mode(&kind) {
            "value" => DomStr::Owned(self.control_value(el)),
            "default" => self.attr_str(el, "value").unwrap_or(DomStr::Borrowed("")),
            "default/on" => self.attr_str(el, "value").unwrap_or(DomStr::Borrowed("on")),
            _ => DomStr::Borrowed(""),
        }
    }
    fn set_value(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let el = self.n(this);
        let kind = self.input_type(el).unwrap_or_default();
        match Engine::value_mode(&kind) {
            "value" => {
                // HTML §4.10.5.4 value mode: the sanitized value, dirty; the cursor moves to the
                // end (selection direction "none") only if the value changed.
                let old = self.control_value(el);
                let mut v = self.sanitize_for(el, &kind, value);
                if kind == "range" {
                    v = self.range_value(el, &v);
                }
                let len = v.encode_utf16().count() as u32;
                let changed = v != old;
                self.form_state_changed(el);
                self.ctl(el, |c| {
                    c.value = Some(v);
                    c.editor = None;
                    if changed {
                        c.selection = (len, len, "none");
                    }
                });
            }
            "default" | "default/on" => self.set_attr(cx, el, "value", value),
            _ => {
                if !value.is_empty() {
                    return Err(exception(
                        ExceptionName::InvalidStateError,
                        "A file input's value can only be cleared",
                    ));
                }
            }
        }
        Ok(())
    }
    /// HTML §4.10.5.4 `valueAsDate`: the value as a Date (UTC), for date, month, week and time.
    fn value_as_date(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<JsObject>> {
        let el = self.n(this);
        let kind = self.input_type(el).unwrap_or_default();
        if !matches!(kind.as_str(), "date" | "month" | "week" | "time") {
            return Ok(None);
        }
        let Some(v) = string_to_number(&kind, &self.control_value(el)) else {
            return Ok(None);
        };
        // (a month's number is months since 1970: its Date is the first day of the month)
        let ms = if kind == "month" {
            let months = v as i64;
            let (y, m) = (
                1970 + months.div_euclid(12),
                months.rem_euclid(12) as u32 + 1,
            );
            days_from_civil(y, m, 1) as f64 * DAY_MS
        } else {
            v
        };
        let scope = cx.scope();
        let Some(date) = v8::Date::new(scope, ms) else {
            return Ok(None);
        };
        Ok(Some(JsObject::new(scope, date.into())))
    }
    fn set_value_as_date(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: Option<JsObject>,
    ) -> rt::Result<()> {
        let el = self.n(this);
        let kind = self.input_type(el).unwrap_or_default();
        if !matches!(kind.as_str(), "date" | "month" | "week" | "time") {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "valueAsDate does not apply to this input type",
            ));
        }
        let ms = match value {
            None => f64::NAN,
            Some(o) => {
                let scope = cx.scope();
                let local = v8::Local::new(scope, o.global());
                match v8::Local::<v8::Date>::try_from(local) {
                    Ok(d) => d.value_of(),
                    Err(_) => return Err(rt::Error::Type("The value is not a Date".into())),
                }
            }
        };
        let s = if ms.is_nan() {
            String::new()
        } else if kind == "month" {
            let (y, m, _) = civil_from_days((ms / DAY_MS).floor() as i64);
            number_to_string("month", ((y - 1970) * 12 + m as i64 - 1) as f64).unwrap_or_default()
        } else {
            number_to_string(&kind, ms).unwrap_or_default()
        };
        self.set_value_from_api(el, &kind, s);
        Ok(())
    }
    /// HTML §4.10.5.4 `valueAsNumber`: the value as a number of its type, NaN when it has none.
    fn value_as_number(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f64> {
        let el = self.n(this);
        let kind = self.input_type(el).unwrap_or_default();
        if step_facts(&kind).is_none() {
            return Ok(f64::NAN);
        }
        Ok(string_to_number(&kind, &self.control_value(el)).unwrap_or(f64::NAN))
    }
    fn set_value_as_number(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        let el = self.n(this);
        if value.is_infinite() {
            return Err(rt::Error::Type("The value is not finite".into()));
        }
        let kind = self.input_type(el).unwrap_or_default();
        if !matches!(
            kind.as_str(),
            "number" | "range" | "date" | "month" | "week" | "time" | "datetime-local"
        ) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "valueAsNumber does not apply to this input type",
            ));
        }
        let _ = cx;
        let s = if value.is_nan() {
            String::new()
        } else {
            number_to_string(&kind, value).unwrap_or_default()
        };
        self.set_value_from_api(el, &kind, s);
        Ok(())
    }
    fn width(&self, this: Handle) -> u32 {
        self.attr_str(self.n(this), "width")
            .and_then(|v| rt::parse_non_negative_integer(&v))
            .unwrap_or(0) as u32
    }
    validation_members!();
    fn labels(&self, this: Handle) -> Option<Handle> {
        let el = self.n(this);
        if self.input_type(el).as_deref() == Some("hidden") {
            return None;
        }
        Some(self.labels_handle(el))
    }
    fn selection_start(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<u32>> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Ok(None);
        }
        Ok(Some(self.ctl(el, |c| c.selection.0)))
    }
    fn set_selection_start(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: Option<u32>,
    ) -> rt::Result<()> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Selection does not apply",
            ));
        }
        let len = self.control_value(el).encode_utf16().count() as u32;
        let start = value.unwrap_or(0).min(len);
        let (_, end, d) = self.ctl(el, |c| c.selection);
        self.change_selection(el, (start, end.max(start), d));
        Ok(())
    }
    fn selection_end(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<u32>> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Ok(None);
        }
        Ok(Some(self.ctl(el, |c| c.selection.1)))
    }
    fn set_selection_end(&self, _cx: &mut Cx, this: Handle, value: Option<u32>) -> rt::Result<()> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Selection does not apply",
            ));
        }
        let len = self.control_value(el).encode_utf16().count() as u32;
        let end = value.unwrap_or(0).min(len);
        let (start, _, d) = self.ctl(el, |c| c.selection);
        self.change_selection(el, (start.min(end), end, d));
        Ok(())
    }
    fn selection_direction(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<DomStr<'_>>> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Ok(None);
        }
        Ok(Some(DomStr::Borrowed(self.ctl(el, |c| c.selection.2))))
    }
    fn set_selection_direction(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: Option<&str>,
    ) -> rt::Result<()> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Selection does not apply",
            ));
        }
        let d = match value {
            Some("forward") => "forward",
            Some("backward") => "backward",
            _ => "none",
        };
        let (start, end, _) = self.ctl(el, |c| c.selection);
        self.change_selection(el, (start, end, d));
        Ok(())
    }
    fn webkitdirectory(&self, this: Handle) -> bool {
        self.attr_str(self.n(this), "webkitdirectory").is_some()
    }
    fn set_webkitdirectory(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        let el = self.n(this);
        if value {
            self.set_attr(cx, el, "webkitdirectory", "");
        } else {
            self.remove_attr(cx, el, "webkitdirectory");
        }
        Ok(())
    }
    fn capture(&self, this: Handle) -> DomStr<'_> {
        let v = self
            .attr_str(self.n(this), "capture")
            .map(|v| v.to_ascii_lowercase());
        DomStr::Borrowed(match v.as_deref() {
            None => "",
            Some("user") => "user",
            Some(_) => "environment",
        })
    }
    fn set_capture(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr(cx, self.n(this), "capture", value);
        Ok(())
    }
    fn step_up(&self, _cx: &mut Cx, this: Handle, n: i32) -> rt::Result<()> {
        self.step(self.n(this), n as f64)
    }
    fn step_down(&self, _cx: &mut Cx, this: Handle, n: i32) -> rt::Result<()> {
        self.step(self.n(this), -(n as f64))
    }
    fn select(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let el = self.n(this);
        if self.selection_applies(el) {
            let len = self.control_value(el).encode_utf16().count() as u32;
            self.change_selection(el, (0, len, "none"));
        }
        Ok(())
    }
    fn set_range_text_replacement(
        &self,
        cx: &mut Cx,
        this: Handle,
        replacement: &str,
    ) -> rt::Result<()> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Selection does not apply",
            ));
        }
        let (s, e, _) = self.ctl(el, |c| c.selection);
        self.set_range_text(cx, el, replacement, s, e, SelectionMode::Preserve)
    }
    fn set_range_text_replacement_start_end_selection_mode(
        &self,
        cx: &mut Cx,
        this: Handle,
        replacement: &str,
        start: u32,
        end: u32,
        selection_mode: SelectionMode,
    ) -> rt::Result<()> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Selection does not apply",
            ));
        }
        self.set_range_text(cx, el, replacement, start, end, selection_mode)
    }
    fn set_selection_range(
        &self,
        _cx: &mut Cx,
        this: Handle,
        start: u32,
        end: u32,
        direction: Option<&str>,
    ) -> rt::Result<()> {
        let el = self.n(this);
        if !self.selection_applies(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "Selection does not apply",
            ));
        }
        self.set_selection(el, start, end, direction);
        Ok(())
    }
    /// HTML §4.10.5 `showPicker()`: file and color pickers may be shown from a cross-origin frame.
    fn show_picker(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let el = self.n(this);
        let any_origin = matches!(self.input_type(el).as_deref(), Some("file" | "color"));
        self.show_picker_checks(el, any_origin)?;
        self.show_picker_if_applicable(el);
        Ok(())
    }
}

impl Engine {
    /// The checks `showPicker()` makes before showing the picker (HTML §4.10.5 and §4.10.7):
    /// a mutable element, a document same origin with its top-level one (unless `any_origin`),
    /// then transient activation.
    fn show_picker_checks(&self, el: NodeId, any_origin: bool) -> rt::Result<()> {
        if !self.picker_control_is_mutable(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "showPicker() needs a mutable element",
            ));
        }
        let doc = self.dom().node_document(el);
        if !any_origin {
            let origin = self.origin_of_document(doc);
            let mut top = doc;
            while let Some(parent) = self.parent_document(top) {
                top = parent;
            }
            if self.origin_of_document(top) != origin {
                return Err(exception(
                    ExceptionName::SecurityError,
                    "showPicker() is not allowed in a cross-origin frame",
                ));
            }
        }
        if !self.has_transient_activation(doc) {
            return Err(exception(
                ExceptionName::NotAllowedError,
                "showPicker() needs transient user activation",
            ));
        }
        Ok(())
    }

    fn form_action_of(&self, el: NodeId) -> DomStr<'_> {
        let action = self
            .dom()
            .attr(el, &ns!(), &LocalName::from("formaction"))
            .map(str::to_owned);
        let doc_url = {
            let dom = self.dom();
            dom.document_url(dom.node_document(el)).to_owned()
        };
        match action.filter(|a| !a.is_empty()) {
            Some(a) => DomStr::Owned(self.base_url_of(el).join(&a).map(String::from).unwrap_or(a)),
            None => DomStr::Owned(doc_url),
        }
    }

    fn selection_applies(&self, el: NodeId) -> bool {
        if self
            .dom()
            .is_html_element_named(el, &local_name!("textarea"))
        {
            return true;
        }
        matches!(
            self.input_type(el).as_deref(),
            Some("text") | Some("search") | Some("url") | Some("tel") | Some("password")
        )
    }

    pub(crate) fn set_selection(&self, el: NodeId, start: u32, end: u32, direction: Option<&str>) {
        let len = self.editor_value(el).encode_utf16().count() as u32;
        let end = end.min(len);
        let start = start.min(end);
        let d = match direction {
            Some("forward") => "forward",
            Some("backward") => "backward",
            _ => "none",
        };
        self.change_selection(el, (start, end, d));
    }

    /// HTML §4.10.20 "set the selection range", last step: a change of the selection (its extent
    /// or direction) queues a task on the user interaction task source to fire `select` at the
    /// element, bubbling; and (Selection API §5.3) schedules a `selectionchange` at it.
    fn change_selection(&self, el: NodeId, new: (u32, u32, &'static str)) {
        let old = self.ctl(el, |c| std::mem::replace(&mut c.selection, new));
        if old != new {
            self.schedule_selectionchange(el);
            self.queue_task(
                crate::event_loop::TaskSource::UserInteraction,
                move |e, cx| {
                    if e.dom().is_alive(el) {
                        e.fire_event(cx, node_handle(el), "select", true, false);
                    }
                },
            );
        }
    }

    /// HTML §4.10.20 `setRangeText`.
    fn set_range_text(
        &self,
        _cx: &mut Cx,
        el: NodeId,
        replacement: &str,
        start: u32,
        end: u32,
        mode: SelectionMode,
    ) -> rt::Result<()> {
        if start > end {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "start is after end",
            ));
        }
        let value: Vec<u16> = self.control_value(el).encode_utf16().collect();
        let len = value.len() as u32;
        let start = start.min(len);
        let end = end.min(len);
        let (sel_start, sel_end, _) = self.ctl(el, |c| c.selection);
        let rep: Vec<u16> = replacement.encode_utf16().collect();
        let mut new = value[..start as usize].to_vec();
        new.extend_from_slice(&rep);
        new.extend_from_slice(&value[end as usize..]);
        let new_len = rep.len() as u32;
        let new_end = start + new_len;
        let (s, e) = match mode {
            SelectionMode::Select => (start, new_end),
            SelectionMode::Start => (start, start),
            SelectionMode::End => (new_end, new_end),
            SelectionMode::Preserve => {
                let old_len = end - start;
                let delta = new_len as i64 - old_len as i64;
                let mut s = sel_start as i64;
                let mut e = sel_end as i64;
                if s > end as i64 {
                    s += delta;
                } else if s > start as i64 {
                    s = start as i64;
                }
                if e > end as i64 {
                    e += delta;
                } else if e > start as i64 {
                    e = new_end as i64;
                }
                (s.max(0) as u32, e.max(0) as u32)
            }
        };
        let v = String::from_utf16_lossy(&new);
        self.ctl(el, |c| c.value = Some(v));
        self.form_state_changed(el);
        self.change_selection(el, (s, e, "none"));
        Ok(())
    }

    /// Sets the value as the value IDL attribute's setter does in the value mode (sanitized, dirty,
    /// the cursor at the end), for `valueAsNumber`, `valueAsDate` and `stepUp`/`stepDown`.
    fn set_value_from_api(&self, el: NodeId, kind: &str, value: String) {
        let mut v = self.sanitize_for(el, kind, &value);
        if kind == "range" {
            v = self.range_value(el, &v);
        }
        let len = v.encode_utf16().count() as u32;
        self.form_state_changed(el);
        self.ctl(el, |c| {
            c.value = Some(v);
            c.editor = None;
            c.selection = (len, len, "none");
        });
    }

    /// HTML §4.10.5.4 `stepUp(n)` and `stepDown(n)` (`n` negative for down), for the numeric types.
    fn step(&self, el: NodeId, n: f64) -> rt::Result<()> {
        let not_applicable = || {
            exception(
                ExceptionName::InvalidStateError,
                "stepUp/stepDown do not apply",
            )
        };
        // 1-2. They apply to the numeric types, with an allowed value step.
        let num = self.numeric(el).ok_or_else(not_applicable)?;
        let step = num.step.ok_or_else(|| {
            exception(
                ExceptionName::InvalidStateError,
                "The element has no allowed value step",
            )
        })?;
        let base = num.base;
        let on_step = |v: f64| {
            let q = (v - base) / step;
            (q - q.round()).abs() <= 1e-9 * q.abs().max(1.0)
        };
        // 3-4. A minimum above the maximum, or no allowed value between them: nothing.
        if let (Some(lo), Some(hi)) = (num.min, num.max) {
            if lo > hi {
                return Ok(());
            }
            let first = base + ((lo - base) / step - 1e-9).ceil() * step;
            if first > hi + 1e-9 * hi.abs().max(1.0) {
                return Ok(());
            }
        }
        // 5-6. The current value, or zero.
        let current = string_to_number(&num.kind, &self.control_value(el));
        let value = current.unwrap_or(0.0);
        let before = value;
        let up = n > 0.0 || (n == 0.0 && n.is_sign_positive());
        // 7. Off a step: to the nearest step in the method's direction; on one: n steps.
        let mut v = if !on_step(value) {
            let q = (value - base) / step;
            base + if up { q.ceil() } else { q.floor() } * step
        } else {
            value + step * n
        };
        // 8-9. Into the range, on a step.
        if let Some(lo) = num.min {
            if v < lo {
                v = base + ((lo - base) / step - 1e-9).ceil() * step;
            }
        }
        if let Some(hi) = num.max {
            if v > hi {
                v = base + ((hi - base) / step + 1e-9).floor() * step;
            }
        }
        // 10. Never against the method's direction (from a value that was a number: an empty
        // field steps into the range, as the web platform tests and engines have it).
        if current.is_some() && ((up && v < before) || (!up && v > before)) {
            return Ok(());
        }
        // 11-12. The value, as a string of the type.
        if matches!(num.kind.as_str(), "number" | "range") {
            let value_places = decimals(&self.control_value(el));
            v = round_to(v, num.places.max(value_places));
        }
        let s = number_to_string(&num.kind, v).unwrap_or_default();
        self.set_value_from_api(el, &num.kind, s);
        Ok(())
    }
}

impl HTMLTextAreaElementImpl for Engine {
    fn autocomplete(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.autofill_idl(self.n(this)))
    }
    fn form(&self, this: Handle) -> Option<Handle> {
        self.opt_form(self.n(this))
    }
    fn type_(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("textarea")
    }
    fn default_value(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.dom().child_text_content(self.n(this)))
    }
    fn set_default_value(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.dom_mut().string_replace_all(value, self.n(this));
        self.after_mutation(cx);
        Ok(())
    }
    fn value(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.control_value(self.n(this))
                .replace("\r\n", "\n")
                .replace('\r', "\n"),
        )
    }
    /// HTML §4.10.11 `value` setter: the raw value (newlines normalized), dirty; the cursor moves to
    /// the end only if the API value changed.
    fn set_value(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let el = self.n(this);
        let old = self.control_value(el);
        let v = value.replace("\r\n", "\n").replace('\r', "\n");
        let len = v.encode_utf16().count() as u32;
        let changed = v != old;
        self.form_state_changed(el);
        self.ctl(el, |c| {
            c.value = Some(v);
            if changed {
                c.selection = (len, len, "none");
            }
        });
        Ok(())
    }
    fn text_length(&self, this: Handle) -> u32 {
        self.control_value(self.n(this)).encode_utf16().count() as u32
    }
    validation_members!();
    fn labels(&self, this: Handle) -> Handle {
        self.labels_handle(self.n(this))
    }
    fn selection_start(&self, this: Handle) -> u32 {
        self.ctl(self.n(this), |c| c.selection.0)
    }
    fn set_selection_start(&self, _cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        let el = self.n(this);
        let end = self.ctl(el, |c| c.selection.1).max(value);
        self.set_selection(el, value, end, None);
        Ok(())
    }
    fn selection_end(&self, this: Handle) -> u32 {
        self.ctl(self.n(this), |c| c.selection.1)
    }
    fn set_selection_end(&self, _cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        let el = self.n(this);
        let start = self.ctl(el, |c| c.selection.0).min(value);
        self.set_selection(el, start, value, None);
        Ok(())
    }
    fn selection_direction(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.ctl(self.n(this), |c| c.selection.2))
    }
    fn set_selection_direction(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let el = self.n(this);
        let (s, e, _) = self.ctl(el, |c| c.selection);
        self.set_selection(el, s, e, Some(value));
        Ok(())
    }
    fn select(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let el = self.n(this);
        let len = self.control_value(el).encode_utf16().count() as u32;
        self.change_selection(el, (0, len, "none"));
        Ok(())
    }
    fn set_range_text_replacement(
        &self,
        cx: &mut Cx,
        this: Handle,
        replacement: &str,
    ) -> rt::Result<()> {
        let el = self.n(this);
        let (s, e, _) = self.ctl(el, |c| c.selection);
        self.set_range_text(cx, el, replacement, s, e, SelectionMode::Preserve)
    }
    fn set_range_text_replacement_start_end_selection_mode(
        &self,
        cx: &mut Cx,
        this: Handle,
        replacement: &str,
        start: u32,
        end: u32,
        selection_mode: SelectionMode,
    ) -> rt::Result<()> {
        self.set_range_text(cx, self.n(this), replacement, start, end, selection_mode)
    }
    fn set_selection_range(
        &self,
        _cx: &mut Cx,
        this: Handle,
        start: u32,
        end: u32,
        direction: Option<&str>,
    ) -> rt::Result<()> {
        self.set_selection(self.n(this), start, end, direction);
        Ok(())
    }
}

impl HTMLSelectElementImpl for Engine {
    fn autocomplete(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.autofill_idl(self.n(this)))
    }
    fn form(&self, this: Handle) -> Option<Handle> {
        self.opt_form(self.n(this))
    }
    fn type_(&self, this: Handle) -> DomStr<'_> {
        let multiple = self.attr_str(self.n(this), "multiple").is_some();
        DomStr::Borrowed(if multiple {
            "select-multiple"
        } else {
            "select-one"
        })
    }
    fn options(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::Options)
    }
    fn length(&self, this: Handle) -> u32 {
        self.options_of(self.n(this)).len() as u32
    }
    fn set_length(&self, cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        self.set_options_length(cx, self.n(this), value)
    }
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        self.options_of(self.n(this))
            .get(index as usize)
            .copied()
            .map(node_handle)
    }
    fn indexed_setter(
        &self,
        cx: &mut Cx,
        this: Handle,
        index: u32,
        value: Option<Handle>,
    ) -> rt::Result<()> {
        self.set_option_at(cx, self.n(this), index, value)
    }
    /// HTML §4.10.7 `selectedOptions`: a live HTMLCollection of the selected options.
    fn selected_options(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::SelectedOptions)
    }
    fn selected_index(&self, this: Handle) -> i32 {
        self.select_selected_index(self.n(this))
    }
    fn set_selected_index(&self, cx: &mut Cx, this: Handle, value: i32) -> rt::Result<()> {
        self.select_set_selected_index(cx, self.n(this), value);
        Ok(())
    }
    fn value(&self, this: Handle) -> DomStr<'_> {
        let select = self.n(this);
        let i = self.select_selected_index(select);
        if i < 0 {
            return DomStr::Borrowed("");
        }
        let o = self.options_of(select)[i as usize];
        let dom = self.dom();
        DomStr::Owned(
            attr(&dom, o, &local_name!("value"))
                .map(str::to_owned)
                .unwrap_or_else(|| self_text(&dom, o)),
        )
    }
    fn set_value(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let select = self.n(this);
        let options = self.options_of(select);
        let mut found = false;
        for o in options {
            let v = {
                let dom = self.dom();
                attr(&dom, o, &local_name!("value"))
                    .map(str::to_owned)
                    .unwrap_or_else(|| self_text(&dom, o))
            };
            let hit = !found && v == value;
            if hit {
                found = true;
            }
            self.ctl(o, |c| c.selected = Some(hit));
            self.form_state_changed(o);
        }
        self.ctl(select, |c| c.no_selection = !found);
        Ok(())
    }
    validation_members!();
    fn labels(&self, this: Handle) -> Handle {
        self.labels_handle(self.n(this))
    }
    fn named_item(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Option<Handle>> {
        let select = self.n(this);
        let dom = self.dom();
        Ok(self
            .options_of(select)
            .into_iter()
            .find(|&o| {
                attr(&dom, o, &local_name!("id")) == Some(name)
                    || attr(&dom, o, &local_name!("name")) == Some(name)
            })
            .map(node_handle))
    }
    fn add(
        &self,
        cx: &mut Cx,
        this: Handle,
        element: HTMLOptionElementOrHTMLOptGroupElement,
        before: Option<HTMLElementOrLong>,
    ) -> rt::Result<()> {
        self.select_add(cx, self.n(this), element, before)
    }
    fn remove_0(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.dom_mut().remove_node(self.n(this));
        self.after_mutation(cx);
        Ok(())
    }
    fn remove_index(&self, cx: &mut Cx, this: Handle, index: i32) -> rt::Result<()> {
        self.select_remove_index(cx, self.n(this), index)
    }
    /// HTML §4.10.7 `showPicker()`: a select that is not being rendered (no boxes, or in contents
    /// `content-visibility: hidden` skips) has no picker to show.
    fn show_picker(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let el = self.n(this);
        self.show_picker_checks(el, false)?;
        if !self.being_rendered(el) || self.in_skipped_contents(el) {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "showPicker() needs a select that is being rendered",
            ));
        }
        self.show_picker_if_applicable(el);
        Ok(())
    }
}

impl HTMLOptionElementImpl for Engine {
    fn form(&self, this: Handle) -> Option<Handle> {
        let o = self.n(this);
        let select = self.select_of(&self.dom(), o)?;
        self.opt_form(select)
    }
    fn label(&self, this: Handle) -> DomStr<'_> {
        let o = self.n(this);
        match self.attr_str(o, "label") {
            Some(l) => l,
            None => DomStr::Owned(self_text(&self.dom(), o)),
        }
    }
    fn selected(&self, this: Handle) -> bool {
        let o = self.n(this);
        self.option_selected_in(&self.dom(), o)
    }
    fn set_selected(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_option_selected(self.n(this), value);
        Ok(())
    }
    fn value(&self, this: Handle) -> DomStr<'_> {
        let o = self.n(this);
        match self.attr_str(o, "value") {
            Some(v) => v,
            None => DomStr::Owned(self_text(&self.dom(), o)),
        }
    }
    fn text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self_text(&self.dom(), self.n(this)))
    }
    fn set_text(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.dom_mut().string_replace_all(value, self.n(this));
        self.after_mutation(cx);
        Ok(())
    }
    fn index(&self, this: Handle) -> i32 {
        let o = self.n(this);
        let select = self.select_of(&self.dom(), o);
        match select {
            Some(s) if self.dom().is_html_element_named(s, &local_name!("select")) => self
                .options_of(s)
                .iter()
                .position(|&x| x == o)
                .map_or(0, |i| i as i32),
            _ => 0,
        }
    }
    fn factory_option(
        &self,
        cx: &mut Cx,
        text: &str,
        value: Option<&str>,
        default_selected: bool,
        selected: bool,
    ) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        let o = self
            .dom_mut()
            .create_html_element(doc, local_name!("option"));
        if !text.is_empty() {
            let t = self.dom_mut().create_text_node(doc, text);
            self.dom_mut().append_child(o, t).map_err(super::dom_err)?;
        }
        if let Some(v) = value {
            self.set_attr(cx, o, "value", v);
        }
        if default_selected {
            self.set_attr(cx, o, "selected", "");
        }
        // (the constructor sets the selectedness, not the dirtiness: while it matches the
        // `selected` attribute, the option keeps following that attribute)
        if selected != default_selected {
            self.ctl(o, |c| c.selected = Some(selected));
            self.form_state_changed(o);
        }
        Ok(node_handle(o))
    }
}

impl HTMLButtonElementImpl for Engine {
    /// HTML §4.10.6 `command`: a custom command ("--" and more) as written, a known command's
    /// keyword (ASCII case-insensitively), else the empty string.
    fn command(&self, this: Handle) -> DomStr<'_> {
        const KNOWN: &[&str] = &[
            "toggle-popover",
            "show-popover",
            "hide-popover",
            "close",
            "request-close",
            "show-modal",
        ];
        let Some(v) = self.attr_str(self.n(this), "command") else {
            return DomStr::Borrowed("");
        };
        if v.starts_with("--") {
            return v;
        }
        let lower = v.to_ascii_lowercase();
        DomStr::Borrowed(KNOWN.iter().copied().find(|k| *k == lower).unwrap_or(""))
    }
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(button_type_state(&self.dom(), self.n(this)))
    }
    fn command_for_element(&self, this: Handle) -> Option<Handle> {
        self.element_ref(self.n(this), "commandfor")
    }
    fn set_command_for_element(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: Option<Handle>,
    ) -> rt::Result<()> {
        self.set_element_ref(cx, self.n(this), "commandfor", value);
        Ok(())
    }
    fn form(&self, this: Handle) -> Option<Handle> {
        self.opt_form(self.n(this))
    }
    fn form_action(&self, this: Handle) -> DomStr<'_> {
        self.form_action_of(self.n(this))
    }
    validation_members!();
    fn labels(&self, this: Handle) -> Handle {
        self.labels_handle(self.n(this))
    }
}

impl HTMLOutputElementImpl for Engine {
    fn form(&self, this: Handle) -> Option<Handle> {
        self.opt_form(self.n(this))
    }
    fn type_(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("output")
    }
    fn default_value(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        match self.ctl(el, |c| c.default_value.clone()) {
            Some(d) => DomStr::Owned(d),
            None => DomStr::Owned(self.dom().descendant_text_content(el)),
        }
    }
    fn set_default_value(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let el = self.n(this);
        if self.ctl(el, |c| c.default_value.is_none()) {
            self.dom_mut().string_replace_all(value, el);
            self.after_mutation(cx);
        } else {
            self.ctl(el, |c| c.default_value = Some(value.to_owned()));
        }
        Ok(())
    }
    fn value(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.dom().descendant_text_content(self.n(this)))
    }
    fn set_value(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let el = self.n(this);
        if self.ctl(el, |c| c.default_value.is_none()) {
            let d = self.dom().descendant_text_content(el);
            self.ctl(el, |c| c.default_value = Some(d));
        }
        self.dom_mut().string_replace_all(value, el);
        self.after_mutation(cx);
        Ok(())
    }
    fn will_validate(&self, _this: Handle) -> bool {
        false
    }
    fn validity(&self, this: Handle) -> Handle {
        self.validity_handle(self.n(this))
    }
    fn validation_message(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn labels(&self, this: Handle) -> Handle {
        self.labels_handle(self.n(this))
    }
    fn check_validity(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(true)
    }
    fn report_validity(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(true)
    }
    fn set_custom_validity(&self, _cx: &mut Cx, this: Handle, error: &str) -> rt::Result<()> {
        self.ctl(self.n(this), |c| c.custom_validity = error.to_owned());
        self.form_state_changed(self.n(this));
        Ok(())
    }
}

impl HTMLFieldSetElementImpl for Engine {
    fn form(&self, this: Handle) -> Option<Handle> {
        self.opt_form(self.n(this))
    }
    fn type_(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("fieldset")
    }
    fn elements(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::FieldsetElements)
    }
    fn will_validate(&self, _this: Handle) -> bool {
        false
    }
    fn validity(&self, this: Handle) -> Handle {
        self.validity_handle(self.n(this))
    }
    fn validation_message(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn check_validity(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(true)
    }
    fn report_validity(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(true)
    }
    fn set_custom_validity(&self, _cx: &mut Cx, this: Handle, error: &str) -> rt::Result<()> {
        self.ctl(self.n(this), |c| c.custom_validity = error.to_owned());
        self.form_state_changed(self.n(this));
        Ok(())
    }
}

impl HTMLFormElementImpl for Engine {
    fn action(&self, this: Handle) -> DomStr<'_> {
        let el = self.n(this);
        let action = self
            .dom()
            .attr(el, &ns!(), &LocalName::from("action"))
            .map(str::to_owned);
        match action.filter(|a| !a.is_empty()) {
            Some(a) => DomStr::Owned(self.base_url_of(el).join(&a).map(String::from).unwrap_or(a)),
            None => {
                let dom = self.dom();
                DomStr::Owned(dom.document_url(dom.node_document(el)).to_owned())
            }
        }
    }
    fn elements(&self, this: Handle) -> Handle {
        let f = self.n(this);
        self.collection(f, Filter::FormElements(f))
    }
    fn length(&self, this: Handle) -> u32 {
        let h = HTMLFormElementImpl::elements(self, this);
        self.coll_len(h)
    }
    fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
        let h = HTMLFormElementImpl::elements(self, this);
        self.coll_item(h, index)
    }
    fn named_getter(&self, this: Handle, name: &str) -> Option<RadioNodeListOrElement> {
        let h = HTMLFormElementImpl::elements(self, this);
        self.controls_named(h, name)
    }
    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        let h = HTMLFormElementImpl::elements(self, this);
        snowlark_bindings::generated::HTMLFormControlsCollectionImpl::supported_property_names(
            self, h,
        )
    }
    fn submit(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.submit_form(cx, self.n(this), None, false);
        Ok(())
    }
    fn request_submit(
        &self,
        cx: &mut Cx,
        this: Handle,
        submitter: Option<Handle>,
    ) -> rt::Result<()> {
        let form = self.n(this);
        if let Some(s) = submitter {
            let s = self.n(s);
            if self.form_owner_of(s) != Some(form) {
                return Err(exception(
                    ExceptionName::NotFoundError,
                    "The submitter is not in this form",
                ));
            }
        }
        self.submit_form(cx, form, submitter.map(|s| self.n(s)), true);
        Ok(())
    }
    fn reset(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.reset_form(cx, self.n(this));
        Ok(())
    }
    fn check_validity(&self, cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        let h = HTMLFormElementImpl::elements(self, this);
        let n = self.coll_len(h);
        let mut ok = true;
        for i in 0..n {
            if let Some(c) = self.coll_item(h, i).and_then(super::handle_node) {
                ok &= self.check_validity_steps(cx, c);
            }
        }
        Ok(ok)
    }
    fn report_validity(&self, cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        HTMLFormElementImpl::check_validity(self, cx, this)
    }
}

impl HTMLLabelElementImpl for Engine {
    fn form(&self, this: Handle) -> Option<Handle> {
        let control = super::html::labeled_control(&self.dom(), self.n(this))?;
        self.opt_form(control)
    }
    fn control(&self, this: Handle) -> Option<Handle> {
        self.opt_node(super::html::labeled_control(&self.dom(), self.n(this)))
    }
}

impl HTMLLegendElementImpl for Engine {
    fn form(&self, this: Handle) -> Option<Handle> {
        let el = self.n(this);
        let parent = self.dom().parent(el)?;
        if !self
            .dom()
            .is_html_element_named(parent, &local_name!("fieldset"))
        {
            return None;
        }
        self.opt_form(parent)
    }
}

impl Engine {
    fn num_attr(&self, el: NodeId, name: &str) -> Option<f64> {
        self.dom()
            .attr(el, &ns!(), &LocalName::from(name))
            .and_then(rt::parse_float)
    }
}

impl HTMLMeterElementImpl for Engine {
    fn value(&self, this: Handle) -> f64 {
        let el = self.n(this);
        let min = HTMLMeterElementImpl::min(self, this);
        let max = HTMLMeterElementImpl::max(self, this);
        self.num_attr(el, "value").unwrap_or(0.0).clamp(min, max)
    }
    fn min(&self, this: Handle) -> f64 {
        self.num_attr(self.n(this), "min").unwrap_or(0.0)
    }
    fn max(&self, this: Handle) -> f64 {
        let min = HTMLMeterElementImpl::min(self, this);
        self.num_attr(self.n(this), "max").unwrap_or(1.0).max(min)
    }
    fn low(&self, this: Handle) -> f64 {
        let min = HTMLMeterElementImpl::min(self, this);
        let max = HTMLMeterElementImpl::max(self, this);
        self.num_attr(self.n(this), "low")
            .unwrap_or(min)
            .clamp(min, max)
    }
    fn high(&self, this: Handle) -> f64 {
        let low = HTMLMeterElementImpl::low(self, this);
        let max = HTMLMeterElementImpl::max(self, this);
        self.num_attr(self.n(this), "high")
            .unwrap_or(max)
            .clamp(low, max)
    }
    fn optimum(&self, this: Handle) -> f64 {
        let min = HTMLMeterElementImpl::min(self, this);
        let max = HTMLMeterElementImpl::max(self, this);
        self.num_attr(self.n(this), "optimum")
            .unwrap_or((min + max) / 2.0)
            .clamp(min, max)
    }
    fn labels(&self, this: Handle) -> Handle {
        self.labels_handle(self.n(this))
    }
}

impl HTMLProgressElementImpl for Engine {
    fn value(&self, this: Handle) -> f64 {
        let el = self.n(this);
        let max = self.num_attr(el, "max").filter(|m| *m > 0.0).unwrap_or(1.0);
        match self.num_attr(el, "value") {
            Some(v) if v >= 0.0 => v.min(max),
            _ => 0.0,
        }
    }
    fn position(&self, this: Handle) -> f64 {
        let el = self.n(this);
        if self.dom().attr(el, &ns!(), &local_name!("value")).is_none() {
            return -1.0;
        }
        let max = self.num_attr(el, "max").filter(|m| *m > 0.0).unwrap_or(1.0);
        HTMLProgressElementImpl::value(self, this) / max
    }
    fn labels(&self, this: Handle) -> Handle {
        self.labels_handle(self.n(this))
    }
}

impl ValidityStateImpl for Engine {
    fn value_missing(&self, this: Handle) -> bool {
        let el = self.validity_element(this);
        if let Some(f) = self.internals_flags(el) {
            return f.value_missing;
        }
        Engine::value_missing(self, el)
    }
    fn type_mismatch(&self, this: Handle) -> bool {
        let el = self.validity_element(this);
        if self.internals_flags(el).is_some() {
            return self.internals_flag(this, |f| f.type_mismatch);
        }
        self.type_mismatch_of(el)
    }
    fn pattern_mismatch(&self, cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        let el = self.validity_element(this);
        if self.internals_flags(el).is_some() {
            return Ok(self.internals_flag(this, |f| f.pattern_mismatch));
        }
        Ok(self.pattern_mismatch_of(cx, el))
    }
    fn too_long(&self, this: Handle) -> bool {
        self.internals_flag(this, |f| f.too_long)
    }
    fn too_short(&self, this: Handle) -> bool {
        self.internals_flag(this, |f| f.too_short)
    }
    fn range_underflow(&self, this: Handle) -> bool {
        let el = self.validity_element(this);
        if self.internals_flags(el).is_some() {
            return self.internals_flag(this, |f| f.range_underflow);
        }
        self.range_and_step_of(el).0
    }
    fn range_overflow(&self, this: Handle) -> bool {
        let el = self.validity_element(this);
        if self.internals_flags(el).is_some() {
            return self.internals_flag(this, |f| f.range_overflow);
        }
        self.range_and_step_of(el).1
    }
    fn step_mismatch(&self, this: Handle) -> bool {
        let el = self.validity_element(this);
        if self.internals_flags(el).is_some() {
            return self.internals_flag(this, |f| f.step_mismatch);
        }
        self.range_and_step_of(el).2
    }
    fn bad_input(&self, this: Handle) -> bool {
        let el = self.validity_element(this);
        if self.internals_flags(el).is_some() {
            return self.internals_flag(this, |f| f.bad_input);
        }
        self.bad_input_of(el)
    }
    fn custom_error(&self, this: Handle) -> bool {
        let el = self.validity_element(this);
        if let Some(f) = self.internals_flags(el) {
            return f.custom_error;
        }
        self.ctl(el, |c| !c.custom_validity.is_empty())
    }
    fn valid(&self, cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        let el = self.validity_element(this);
        if let Some(f) = self.internals_flags(el) {
            return Ok(!(f.value_missing
                || f.type_mismatch
                || f.pattern_mismatch
                || f.too_long
                || f.too_short
                || f.range_underflow
                || f.range_overflow
                || f.step_mismatch
                || f.bad_input
                || f.custom_error));
        }
        Ok(self.is_valid_cx(cx, el))
    }
}

impl Engine {
    /// A flag `ElementInternals.setValidity` set on the element of a ValidityState, else false.
    fn internals_flag(
        &self,
        this: Handle,
        f: impl FnOnce(&super::internals::ValidityFlags) -> bool,
    ) -> bool {
        let el = self.validity_element(this);
        self.internals_flags(el).is_some_and(|x| f(&x))
    }
}

impl Engine {
    fn validity_element(&self, h: Handle) -> NodeId {
        match &*self.obj(h) {
            Object::ValidityState { element } => *element,
            _ => panic!("not a ValidityState"),
        }
    }
}

impl Engine {
    /// HTML's cloning steps: form control state (HTML §4.10.5 "the cloning steps for input elements")
    /// and the script element's "already started".
    pub(crate) fn html_cloning_steps(&self, node: NodeId, copy: NodeId) {
        // (a copy of an `about:blank` or `about:srcdoc` document takes the original's document base
        // URL, `base` included, as its about base URL)
        if !self.dom().is_element(node) && self.dom().is_document(node) {
            let about = {
                let dom = self.dom();
                matches!(dom.document_url(node), "about:blank" | "about:srcdoc")
                    .then(|| self.document_base_url(&dom, node))
            };
            if let Some(u) = about {
                self.doc_state(copy, |s| s.about_base_url = Some(u));
            }
            return;
        }
        if !self.dom().is_element(node) || !self.dom().is_element(copy) {
            return;
        }
        self.script_cloning_steps(node, copy);
        if self.policy.borrow().any {
            self.csp_cloning_steps(node, copy);
        }
        // (HTML §4.10.5, §4.10.11 cloning steps: the value, checkedness, their dirty flags and the
        // indeterminate state; a copy's selection and scroll start afresh)
        let state = self.html.borrow().controls.get(&node).cloned();
        if let Some(s) = state {
            let copy_state = ControlState {
                selection: ControlState::default().selection,
                scroll_x: 0.0,
                ..s
            };
            self.html.borrow_mut().controls.insert(copy, copy_state);
        }
    }
}

/// Whether `el` is a submit button (a `button` in the Submit Button state, or an input of type submit
/// or image).
/// HTML §4.10.6 the `button` element's type state: the `type` keyword, else (the Auto state) the
/// Button state when it has a `command` or `commandfor` attribute, else Submit Button.
pub(crate) fn button_type_state(dom: &Dom, el: NodeId) -> &'static str {
    match attr(dom, el, &local_name!("type"))
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("submit") => "submit",
        Some("reset") => "reset",
        Some("button") => "button",
        _ if attr(dom, el, &LocalName::from("command")).is_some()
            || attr(dom, el, &LocalName::from("commandfor")).is_some() =>
        {
            "button"
        }
        _ => "submit",
    }
}

pub(crate) fn is_submit_button(dom: &Dom, el: NodeId) -> bool {
    if is(dom, el, &local_name!("button")) {
        return button_type_state(dom, el) == "submit";
    }
    if is(dom, el, &local_name!("input")) {
        let t = attr(dom, el, &local_name!("type")).map(str::to_ascii_lowercase);
        return matches!(t.as_deref(), Some("submit") | Some("image"));
    }
    false
}

impl Engine {
    /// HTML §4.10.21.4 "constructing the entry list" for `form` with `encoding` (the value of a
    /// hidden `_charset_` field).
    pub(crate) fn form_entries(
        &self,
        form: NodeId,
        submitter: Option<NodeId>,
        encoding: &'static encoding_rs::Encoding,
    ) -> Vec<(String, FormValue)> {
        let h = self.collection(form, Filter::FormElements(form));
        let n = self.coll_len(h);
        let controls: Vec<NodeId> = (0..n)
            .filter_map(|i| self.coll_item(h, i).and_then(super::handle_node))
            .collect();
        let mut out: Vec<(String, FormValue)> = Vec::new();
        for c in controls {
            // HTML §4.13.7.4 "entry construction algorithm" for a form-associated custom element:
            // its submission value's entries, or its name with its submission value.
            if self.is_form_associated_custom(c) {
                let skip = {
                    let dom = self.dom();
                    is_disabled(&dom, c)
                        || dom
                            .ancestors(c)
                            .any(|a| is(&dom, a, &local_name!("datalist")))
                };
                if skip {
                    continue;
                }
                match self.submission_value(c) {
                    Some(SubmissionValue::Entries(entries)) => out.extend(entries),
                    Some(v) => {
                        let name = attr(&self.dom(), c, &local_name!("name"))
                            .unwrap_or("")
                            .to_owned();
                        if !name.is_empty() {
                            out.push((
                                name,
                                match v {
                                    SubmissionValue::File(f) => FormValue::File(f),
                                    SubmissionValue::Str(s) => FormValue::Str(s),
                                    SubmissionValue::Entries(_) => unreachable!(),
                                },
                            ));
                        }
                    }
                    None => {}
                }
                continue;
            }
            let (kind, name, dirname) = {
                let dom = self.dom();
                let in_datalist = dom
                    .ancestors(c)
                    .any(|a| is(&dom, a, &local_name!("datalist")));
                if in_datalist || is_disabled(&dom, c) {
                    continue;
                }
                if is_submit_button(&dom, c) && Some(c) != submitter {
                    continue;
                }
                if is(&dom, c, &local_name!("button")) && !is_submit_button(&dom, c) {
                    continue;
                }
                if is(&dom, c, &local_name!("object"))
                    || is(&dom, c, &local_name!("fieldset"))
                    || is(&dom, c, &local_name!("output"))
                {
                    continue;
                }
                let name = attr(&dom, c, &local_name!("name")).unwrap_or("").to_owned();
                let dirname = attr(&dom, c, &LocalName::from("dirname")).map(str::to_owned);
                (self.input_type(c), name, dirname)
            };
            match kind.as_deref() {
                Some("checkbox") | Some("radio") => {
                    if !self.checkedness(c) {
                        continue;
                    }
                }
                Some("image") => {
                    let prefix = if name.is_empty() {
                        String::new()
                    } else {
                        format!("{name}.")
                    };
                    out.push((format!("{prefix}x"), FormValue::Str("0".into())));
                    out.push((format!("{prefix}y"), FormValue::Str("0".into())));
                    continue;
                }
                // (a file field: one entry per selected file, else an empty File named "" of type
                // application/octet-stream)
                Some("file") => {
                    if name.is_empty() {
                        continue;
                    }
                    let files = self.html.borrow().files.get(&c).copied();
                    let files = files
                        .map(|h| match &*self.obj(h) {
                            Object::FileList(v) => v.clone(),
                            _ => Vec::new(),
                        })
                        .unwrap_or_default();
                    if files.is_empty() {
                        let empty = self.new_file(Vec::new(), "application/octet-stream", "", 0);
                        out.push((name.clone(), FormValue::File(empty)));
                    } else {
                        out.extend(
                            files
                                .into_iter()
                                .map(|f| (name.clone(), FormValue::File(f))),
                        );
                    }
                    continue;
                }
                Some("reset") | Some("button") => continue,
                _ => {}
            }
            if name.is_empty() {
                continue;
            }
            let is_select = self.dom().is_html_element_named(c, &local_name!("select"));
            if is_select {
                for o in self.options_of(c) {
                    let dom = self.dom();
                    if self.option_selected_in(&dom, o) && !is_disabled(&dom, o) {
                        let v = attr(&dom, o, &local_name!("value"))
                            .map(str::to_owned)
                            .unwrap_or_else(|| self_text(&dom, o));
                        out.push((name.clone(), FormValue::Str(v)));
                    }
                }
                continue;
            }
            let value = match kind.as_deref() {
                Some("checkbox") | Some("radio") => attr(&self.dom(), c, &local_name!("value"))
                    .unwrap_or("on")
                    .to_owned(),
                Some("hidden") if name.eq_ignore_ascii_case("_charset_") => {
                    encoding.name().to_owned()
                }
                Some("submit") | Some("hidden") => attr(&self.dom(), c, &local_name!("value"))
                    .unwrap_or("")
                    .to_owned(),
                Some(_) => self.control_value(c),
                None if self.dom().is_html_element_named(c, &local_name!("button")) => {
                    attr(&self.dom(), c, &local_name!("value"))
                        .unwrap_or("")
                        .to_owned()
                }
                None => self.control_value(c),
            };
            out.push((name.clone(), FormValue::Str(value)));
            if let Some(d) = dirname.filter(|d| !d.is_empty()) {
                out.push((d, FormValue::Str("ltr".into())));
            }
        }
        // (newlines are normalized by the encodings, not here: FormData sees them as they are)
        out
    }
}

impl Engine {
    /// HTML §4.10.21.3 "pick an encoding for the form": the first `accept-charset` label that names
    /// an encoding (UTF-8 when the attribute names none), else the document's character encoding;
    /// then its output encoding (UTF-16 and replacement submit UTF-8).
    pub(crate) fn pick_form_encoding(&self, form: NodeId) -> &'static encoding_rs::Encoding {
        let dom = self.dom();
        let encoding = match attr(&dom, form, &local_name!("accept-charset")) {
            Some(labels) => labels
                .split_ascii_whitespace()
                .find_map(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
                .unwrap_or(encoding_rs::UTF_8),
            None => dom.encoding(dom.node_document(form)),
        };
        encoding.output_encoding()
    }
}

/// Encoding Standard "encode" (error mode html): `input` in `encoding`, each code point it cannot
/// represent written as a decimal character reference (`&#945;`).
pub(crate) fn encode_html<'a>(
    encoding: &'static encoding_rs::Encoding,
    input: &'a str,
) -> std::borrow::Cow<'a, [u8]> {
    use encoding_rs::EncoderResult;
    if encoding == encoding_rs::UTF_8 {
        return std::borrow::Cow::Borrowed(input.as_bytes());
    }
    let mut encoder = encoding.new_encoder();
    let mut out = Vec::with_capacity(input.len());
    let mut buffer = [0u8; 256];
    let mut rest = input;
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(rest, &mut buffer, true);
        out.extend_from_slice(&buffer[..written]);
        rest = &rest[read..];
        match result {
            EncoderResult::InputEmpty => return std::borrow::Cow::Owned(out),
            EncoderResult::OutputFull => {}
            EncoderResult::Unmappable(c) => {
                out.extend_from_slice(format!("&#{};", c as u32).as_bytes());
            }
        }
    }
}

/// URL Standard §5.2 the `application/x-www-form-urlencoded` serializer with an encoding override:
/// names and values encoded in `encoding` (unmappable code points as character references, which
/// are then percent-encoded), spaces as `+`.
pub(crate) fn urlencoded_serialize(
    pairs: &[(String, String)],
    encoding: &'static encoding_rs::Encoding,
) -> String {
    let encode: &dyn Fn(&str) -> std::borrow::Cow<'_, [u8]> = &|s| encode_html(encoding, s);
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    if encoding != encoding_rs::UTF_8 {
        s.encoding_override(Some(encode));
    }
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_encodings_write_unmappable_code_points_as_character_references() {
        let pairs = vec![("n \u{e9}".to_owned(), "\u{3b1}&\u{df}".to_owned())];
        assert_eq!(
            urlencoded_serialize(&pairs, encoding_rs::WINDOWS_1252),
            "n+%E9=%26%23945%3B%26%DF"
        );
        assert_eq!(
            urlencoded_serialize(&pairs, encoding_rs::UTF_8),
            "n+%C3%A9=%CE%B1%26%C3%9F"
        );
        assert_eq!(
            urlencoded_serialize(&[("k".into(), "\u{65e5}".into())], encoding_rs::SHIFT_JIS),
            "k=%93%FA"
        );
        assert_eq!(
            &*encode_html(encoding_rs::WINDOWS_1252, "a\u{1f600}"),
            b"a&#128512;"
        );
    }

    #[test]
    fn value_sanitization() {
        assert_eq!(sanitize("text", "a\nb\rc"), "abc");
        assert_eq!(sanitize("url", "  http://x/  "), "http://x/");
        assert_eq!(sanitize("number", "1e3"), "1e3");
        assert_eq!(sanitize("number", "1e"), "");
        assert_eq!(sanitize("color", "#ABCDEF"), "#abcdef");
        assert_eq!(sanitize("color", "red"), "#000000");
        assert_eq!(sanitize("date", "2024-02-29"), "2024-02-29");
        assert_eq!(sanitize("date", "2023-02-29"), "");
        assert_eq!(sanitize("time", "12:30:59.123"), "12:30:59.123");
        assert_eq!(sanitize("week", "2020-W53"), "2020-W53");
    }
}
