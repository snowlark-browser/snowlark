// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Form controls as layout renders them ([`snowlark_layout::style::Control`]): what an `input`,
//! `select`, `textarea`, `progress` or `meter` element shows, from its attributes and children (HTML
//! §4.10; without scripts the values are the default values): the input type's widget, value
//! sanitization, the placeholder, a select's options and selectedness, `rows`/`cols`/`size`, a
//! meter's gauge region, disabledness (including a disabled fieldset's), `readonly`.
//!
//! A control's box, font, colors and borders come from the computed style (the UA sheet's form
//! control rules give the native look); `appearance: none` turns off the widget's own painted parts.

use std::sync::Arc;

use snowlark_dom::{local_name, ns, Dom, NodeId};
use snowlark_layout::style::{Color, Control, ControlKind, MeterLevel, SelectOption};
use style::properties::ComputedValues;

/// HTML §4.10.5: an input element's type state (the `type` attribute ASCII-lowercased; unknown
/// values are the Text state).
pub fn input_type(dom: &Dom, el: NodeId) -> &'static str {
    const TYPES: [&str; 22] = [
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
    let t = dom.get_attribute(el, "type").unwrap_or("");
    TYPES
        .iter()
        .find(|k| t.eq_ignore_ascii_case(k))
        .copied()
        .unwrap_or("text")
}

/// HTML §2.3.4.1 "rules for parsing non-negative integers".
fn non_negative_integer(value: Option<&str>) -> Option<u32> {
    let v = value?.trim_start_matches([' ', '\t', '\n', '\u{c}', '\r']);
    let v = v.strip_prefix('+').unwrap_or(v);
    let digits = v.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    v[..digits]
        .parse::<u64>()
        .ok()
        .map(|n| n.min(u32::MAX as u64) as u32)
}

/// HTML §2.3.4.3 "rules for parsing floating-point number values" (the valid-number subset is what
/// value sanitization accepts).
fn valid_float(value: &str) -> Option<f64> {
    let v = value;
    if v.is_empty() || v.starts_with('+') || v.ends_with('.') || v.starts_with('.') && v.len() == 1
    {
        return None;
    }
    if !v
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'e' | b'E' | b'+'))
    {
        return None;
    }
    v.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// The "rules for parsing floating-point number values" (leading number, whitespace skipped).
fn parse_float(value: Option<&str>) -> Option<f64> {
    let v = value?.trim_matches([' ', '\t', '\n', '\u{c}', '\r']);
    let end = v
        .char_indices()
        .find(|&(i, c)| {
            !(c.is_ascii_digit()
                || c == '.'
                || (c == '-' && i == 0)
                || ((c == 'e' || c == 'E') && i > 0)
                || ((c == '-' || c == '+') && i > 0 && matches!(&v[i - 1..i], "e" | "E")))
        })
        .map_or(v.len(), |(i, _)| i);
    v[..end].parse::<f64>().ok().filter(|f| f.is_finite())
}

/// HTML §4.10.5.1.18: a valid simple color, lowercased; black otherwise.
fn simple_color(value: &str) -> Color {
    let v = value.as_bytes();
    if v.len() == 7 && v[0] == b'#' && v[1..].iter().all(u8::is_ascii_hexdigit) {
        let h = |i: usize| u8::from_str_radix(&value[i..i + 2], 16).unwrap_or(0);
        return Color::rgb(h(1), h(3), h(5));
    }
    Color::BLACK
}

/// A valid date string `yyyy-mm-dd` (HTML §2.3.5.2) in the en-US display format.
fn display_date(value: &str) -> Option<String> {
    let mut parts = value.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() < 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (yn, mn, dn): (u32, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let leap = yn % 4 == 0 && (yn % 100 != 0 || yn % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&mn) || dn == 0 || dn > days[mn as usize - 1] || yn == 0 {
        return None;
    }
    Some(format!("{m}/{d}/{y}"))
}

/// HTML "strip and collapse ASCII whitespace".
fn collapse(text: &str) -> String {
    text.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// HTML §4.10.18.5: disabled by its `disabled` attribute or by a disabled fieldset ancestor (except
/// inside that fieldset's first legend).
pub fn is_disabled(dom: &Dom, el: NodeId) -> bool {
    if dom.get_attribute(el, "disabled").is_some() {
        return true;
    }
    let mut child = el;
    while let Some(p) = dom.parent_element(child) {
        if dom.is_html_element_named(p, &local_name!("fieldset"))
            && dom.get_attribute(p, "disabled").is_some()
        {
            let first_legend = dom
                .element_children(p)
                .find(|&c| dom.is_html_element_named(c, &local_name!("legend")));
            if first_legend != Some(child) {
                return true;
            }
        }
        child = p;
    }
    false
}

/// An option's label (HTML §4.10.10): its `label` attribute, else its text stripped and collapsed.
pub fn option_label(dom: &Dom, option: NodeId) -> String {
    match dom.get_attribute(option, "label") {
        Some(l) if !l.is_empty() => l.to_owned(),
        _ => collapse(&dom.text_content(option).unwrap_or_default()),
    }
}

/// A select's rows (its list of options in tree order, with optgroup labels) and whether it is a
/// list box with how many rows (HTML §4.10.7 display size; §15.5.16 rendering).
/// A control's state that is not in the DOM (the page's dirty value, checkedness and selectedness,
/// and the caret of a focused text field), which overrides the defaults the attributes give.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ControlState {
    /// A text control's current value.
    pub value: Option<String>,
    pub checked: Option<bool>,
    pub indeterminate: bool,
    /// A select's options' selectedness, in tree order.
    pub selected: Option<Vec<bool>>,
    /// A focused text field's selection start, end and caret, in UTF-16 code units of the value.
    pub editing: Option<(u32, u32, u32)>,
    /// How far a focused one-line field's text is scrolled (CSS px).
    pub scroll_x: f32,
    /// The IME composition's range in the value (UTF-16), shown underlined.
    pub composition: Option<(u32, u32)>,
}

/// The UTF-16 offset in `value` of byte offset `byte` of its displayed text (a password's bullets:
/// one per character), the inverse of how a field's caret is shown.
pub fn utf16_of_display(value: &str, byte: usize, masked: bool) -> u32 {
    let mut n = 0u32;
    let bullet = '\u{2022}'.len_utf8();
    for (chars, (i, c)) in value.char_indices().enumerate() {
        let at = if masked { chars * bullet } else { i };
        if at >= byte {
            return n;
        }
        n += c.len_utf16() as u32;
    }
    n
}

/// UTF-16 offset → byte offset of the displayed text (a password's bullets: one per character).
pub fn display_byte(value: &str, utf16: u32, masked: bool) -> usize {
    let mut n = 0u32;
    for (chars, (i, c)) in value.char_indices().enumerate() {
        if n >= utf16 {
            return if masked {
                chars * '\u{2022}'.len_utf8()
            } else {
                i
            };
        }
        n += c.len_utf16() as u32;
    }
    if masked {
        value.chars().count() * '\u{2022}'.len_utf8()
    } else {
        value.len()
    }
}

fn select_kind(dom: &Dom, el: NodeId, state: Option<&ControlState>) -> ControlKind {
    let multiple = dom.get_attribute(el, "multiple").is_some();
    let size = non_negative_integer(dom.get_attribute(el, "size")).filter(|&n| n > 0);
    let display_size = size.unwrap_or(if multiple { 4 } else { 1 });
    let mut rows: Vec<SelectOption> = Vec::new();
    // Indices into `rows` of the real options, with whether each has `selected`.
    let mut options: Vec<(usize, bool, bool)> = Vec::new();
    let mut push_option = |rows: &mut Vec<SelectOption>, o: NodeId, group_disabled: bool| {
        let disabled = group_disabled || dom.get_attribute(o, "disabled").is_some();
        options.push((
            rows.len(),
            dom.get_attribute(o, "selected").is_some(),
            disabled,
        ));
        rows.push(SelectOption {
            label: Arc::from(option_label(dom, o)),
            selected: false,
            disabled,
            group: false,
            indented: false,
        });
    };
    for c in dom.element_children(el) {
        if dom.is_html_element_named(c, &local_name!("option")) {
            push_option(&mut rows, c, false);
        } else if dom.is_html_element_named(c, &local_name!("optgroup")) {
            let disabled = dom.get_attribute(c, "disabled").is_some();
            rows.push(SelectOption {
                label: Arc::from(dom.get_attribute(c, "label").unwrap_or("")),
                selected: false,
                disabled,
                group: true,
                indented: false,
            });
            for o in dom.element_children(c) {
                if dom.is_html_element_named(o, &local_name!("option")) {
                    push_option(&mut rows, o, disabled);
                    if let Some(r) = rows.last_mut() {
                        r.indented = true;
                    }
                }
            }
        } else if dom.is_html_element_named(c, &local_name!("hr")) {
            continue;
        }
    }
    // The page's selectedness, when it has one.
    if let Some(sel) = state.and_then(|s| s.selected.as_ref()) {
        for (k, &(i, _, _)) in options.iter().enumerate() {
            rows[i].selected = sel.get(k).copied().unwrap_or(false);
        }
    } else if multiple {
        for &(i, sel, _) in &options {
            rows[i].selected = sel;
        }
    } else {
        let chosen = options
            .iter()
            .rev()
            .find(|o| o.1)
            .or_else(|| {
                (display_size == 1)
                    .then(|| options.iter().find(|o| !o.2))
                    .flatten()
            })
            .map(|o| o.0);
        if let Some(i) = chosen {
            rows[i].selected = true;
        }
    }
    let list = multiple || display_size > 1;
    if !list {
        // A drop-down box shows the selected option; optgroup labels are not part of it.
        rows.retain(|r| !r.group);
    }
    ControlKind::Select {
        options: rows.into(),
        list_rows: list.then_some(display_size),
    }
}

/// HTML §4.10.13: a progress bar's position, the current value over the maximum (`None`:
/// indeterminate, without a `value` attribute). The maximum is 1 unless `max` parses to more than
/// zero; the value is 0 unless it parses to more, and at most the maximum.
pub(crate) fn progress_position(dom: &Dom, el: NodeId) -> Option<f32> {
    let max = parse_float(dom.get_attribute(el, "max"))
        .filter(|m| *m > 0.0)
        .unwrap_or(1.0);
    dom.get_attribute(el, "value")
        .map(|v| (parse_float(Some(v)).unwrap_or(0.0).clamp(0.0, max) / max) as f32)
}

/// HTML §4.10.14: the region of the gauge a meter's value is in, and the fraction it fills.
pub(crate) fn meter_kind(dom: &Dom, el: NodeId) -> ControlKind {
    let attr = |n: &str| parse_float(dom.get_attribute(el, n));
    let min = attr("min").unwrap_or(0.0);
    let max = attr("max").unwrap_or(1.0).max(min);
    let value = attr("value").unwrap_or(0.0).clamp(min, max);
    let low = attr("low").unwrap_or(min).clamp(min, max);
    let high = attr("high").unwrap_or(max).clamp(low, max);
    let optimum = attr("optimum").unwrap_or((min + max) / 2.0).clamp(min, max);
    let level = if optimum < low {
        if value <= low {
            MeterLevel::Optimum
        } else if value <= high {
            MeterLevel::Suboptimum
        } else {
            MeterLevel::EvenLessGood
        }
    } else if optimum > high {
        if value >= high {
            MeterLevel::Optimum
        } else if value >= low {
            MeterLevel::Suboptimum
        } else {
            MeterLevel::EvenLessGood
        }
    } else if value >= low && value <= high {
        MeterLevel::Optimum
    } else {
        MeterLevel::Suboptimum
    };
    let fraction = if max > min {
        ((value - min) / (max - min)) as f32
    } else {
        0.0
    };
    ControlKind::Meter { fraction, level }
}

/// The widget an input element renders as, or `None` (hidden and image inputs).
fn input_kind(dom: &Dom, el: NodeId, state: Option<&ControlState>) -> Option<ControlKind> {
    let ty = input_type(dom, el);
    let value = match state.and_then(|s| s.value.as_deref()) {
        Some(v) => Some(v),
        None => dom.get_attribute(el, "value"),
    };
    let placeholder = dom
        .get_attribute(el, "placeholder")
        .map(|p| p.replace(['\r', '\n'], ""));
    // §4.10.5.1: value sanitization.
    let strip_newlines = |v: &str| v.replace(['\r', '\n'], "");
    let text_field = |text: String| {
        let size = non_negative_integer(dom.get_attribute(el, "size"))
            .filter(|&n| n > 0)
            .unwrap_or(20);
        match (text.is_empty(), placeholder.clone()) {
            (true, Some(p)) => ControlKind::TextField {
                text: Arc::from(p),
                placeholder: true,
                size,
            },
            _ => ControlKind::TextField {
                text: Arc::from(text),
                placeholder: false,
                size,
            },
        }
    };
    Some(match ty {
        "hidden" | "image" => return None,
        "text" | "search" | "tel" => text_field(strip_newlines(value.unwrap_or(""))),
        "url" | "email" => text_field(
            strip_newlines(value.unwrap_or(""))
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .to_owned(),
        ),
        "password" => {
            let v = strip_newlines(value.unwrap_or(""));
            text_field("\u{2022}".repeat(v.chars().count()))
        }
        "number" => text_field(
            value
                .filter(|v| valid_float(v).is_some())
                .unwrap_or("")
                .to_owned(),
        ),
        "date" => ControlKind::DateTime {
            text: Arc::from(
                value
                    .and_then(display_date)
                    .unwrap_or_else(|| "mm/dd/yyyy".to_owned()),
            ),
        },
        "time" => ControlKind::DateTime {
            text: Arc::from(value.filter(|v| !v.is_empty()).unwrap_or("--:-- --")),
        },
        "month" => ControlKind::DateTime {
            text: Arc::from(value.filter(|v| !v.is_empty()).unwrap_or("--------- ----")),
        },
        "week" => ControlKind::DateTime {
            text: Arc::from(value.filter(|v| !v.is_empty()).unwrap_or("Week --, ----")),
        },
        "datetime-local" => ControlKind::DateTime {
            text: Arc::from(
                value
                    .filter(|v| !v.is_empty())
                    .unwrap_or("mm/dd/yyyy --:-- --"),
            ),
        },
        "checkbox" => ControlKind::Checkbox {
            checked: state
                .and_then(|s| s.checked)
                .unwrap_or_else(|| dom.get_attribute(el, "checked").is_some()),
            indeterminate: state.is_some_and(|s| s.indeterminate),
        },
        "radio" => ControlKind::Radio {
            checked: state
                .and_then(|s| s.checked)
                .unwrap_or_else(|| dom.get_attribute(el, "checked").is_some()),
        },
        "submit" | "reset" | "button" => ControlKind::Button {
            label: Arc::from(match (ty, value) {
                (_, Some(v)) => v,
                ("submit", None) => "Submit",
                ("reset", None) => "Reset",
                _ => "",
            }),
        },
        "file" => ControlKind::File {
            button: Arc::from("Choose File"),
            label: Arc::from(if dom.get_attribute(el, "multiple").is_some() {
                "No files chosen"
            } else {
                "No file chosen"
            }),
        },
        "color" => ControlKind::Color(simple_color(&value.unwrap_or("").to_ascii_lowercase())),
        "range" => {
            // §4.10.5.1.13: min 0, max 100, the default value halfway.
            let min = parse_float(dom.get_attribute(el, "min")).unwrap_or(0.0);
            let max = parse_float(dom.get_attribute(el, "max"))
                .unwrap_or(100.0)
                .max(min);
            let v = value
                .and_then(valid_float)
                .unwrap_or(if max < min {
                    min
                } else {
                    min + (max - min) / 2.0
                })
                .clamp(min, max);
            ControlKind::Range {
                fraction: if max > min {
                    ((v - min) / (max - min)) as f32
                } else {
                    0.0
                },
            }
        }
        _ => text_field(strip_newlines(value.unwrap_or(""))),
    })
}

/// The control an element renders as (HTML §15.5), with `values` its computed style and
/// `placeholder_color` its `::placeholder` color. `None` for elements that aren't controls.
pub fn control(
    dom: &Dom,
    el: NodeId,
    values: &ComputedValues,
    placeholder_color: Color,
    state: Option<&ControlState>,
) -> Option<Control> {
    let e = dom.element(el)?;
    if e.name().ns != ns!(html) {
        return None;
    }
    let readonly_applies;
    let kind = match e.name().local {
        local_name!("input") => {
            let ty = input_type(dom, el);
            readonly_applies = matches!(
                ty,
                "text"
                    | "search"
                    | "tel"
                    | "url"
                    | "email"
                    | "password"
                    | "number"
                    | "date"
                    | "month"
                    | "week"
                    | "time"
                    | "datetime-local"
            );
            input_kind(dom, el, state)?
        }
        local_name!("textarea") => {
            readonly_applies = true;
            // The value: the page's, else the default value, the child text (the parser drops a
            // leading newline).
            let text = match state.and_then(|s| s.value.clone()) {
                Some(v) => v,
                None => dom.child_text_content(el).replace("\r\n", "\n"),
            };
            let placeholder = dom.get_attribute(el, "placeholder");
            let rows = non_negative_integer(dom.get_attribute(el, "rows"))
                .filter(|&n| n > 0)
                .unwrap_or(2);
            let cols = non_negative_integer(dom.get_attribute(el, "cols"))
                .filter(|&n| n > 0)
                .unwrap_or(20);
            let (text, placeholder) = match placeholder {
                Some(p) if text.is_empty() => (p.to_owned(), true),
                _ => (text, false),
            };
            ControlKind::TextArea {
                text: Arc::from(text),
                placeholder,
                rows,
                cols,
            }
        }
        local_name!("select") => {
            readonly_applies = false;
            select_kind(dom, el, state)
        }
        local_name!("progress") => {
            readonly_applies = false;
            ControlKind::Progress {
                fraction: progress_position(dom, el),
            }
        }
        local_name!("meter") => {
            readonly_applies = false;
            meter_kind(dom, el)
        }
        _ => return None,
    };
    use style::values::specified::box_::Appearance;
    // A focused text field's caret and selection, in the displayed text.
    let editing = state.and_then(|s| s.editing).map(|(start, end, caret)| {
        let masked = input_type(dom, el) == "password";
        let value = state
            .and_then(|s| s.value.clone())
            .unwrap_or_else(|| match &kind {
                ControlKind::TextField {
                    text,
                    placeholder: false,
                    ..
                }
                | ControlKind::TextArea {
                    text,
                    placeholder: false,
                    ..
                } => text.to_string(),
                _ => String::new(),
            });
        snowlark_layout::style::TextEditing {
            selection: display_byte(&value, start, masked)..display_byte(&value, end, masked),
            caret: display_byte(&value, caret, masked),
            scroll: state.map_or(0.0, |s| s.scroll_x),
            composition: state
                .and_then(|s| s.composition)
                .map(|(a, b)| display_byte(&value, a, masked)..display_byte(&value, b, masked)),
        }
    });
    Some(Control {
        editing,
        native: values.get_box().appearance != Appearance::None,
        disabled: is_disabled(dom, el),
        readonly: readonly_applies && dom.get_attribute(el, "readonly").is_some(),
        placeholder_color,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_dom::ParseOptions;

    fn parse(html: &str) -> (Dom, NodeId) {
        let mut dom = Dom::new();
        let doc =
            dom.parse_html_document(html.as_bytes(), encoding_rs::UTF_8, ParseOptions::default());
        (dom, doc)
    }

    fn by_id(dom: &Dom, doc: NodeId, id: &str) -> NodeId {
        dom.descendants(doc)
            .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some(id))
            .unwrap()
    }

    fn kind(html: &str) -> Option<ControlKind> {
        let (dom, doc) = parse(html);
        let el = by_id(&dom, doc, "x");
        match dom.element(el).unwrap().name().local {
            local_name!("input") => input_kind(&dom, el, None),
            local_name!("select") => Some(select_kind(&dom, el, None)),
            local_name!("meter") => Some(meter_kind(&dom, el)),
            _ => None,
        }
    }

    fn text(k: Option<ControlKind>) -> (String, bool, u32) {
        match k {
            Some(ControlKind::TextField {
                text,
                placeholder,
                size,
            }) => (text.to_string(), placeholder, size),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn input_values_are_sanitized() {
        assert_eq!(
            text(kind("<input id=x value='a&#10;b'>")),
            ("ab".into(), false, 20)
        );
        assert_eq!(
            text(kind("<input id=x size=5 placeholder=P>")),
            ("P".into(), true, 5)
        );
        assert_eq!(text(kind("<input id=x size=0>")).2, 20);
        assert_eq!(text(kind("<input id=x type=NUMBER value=1e3>")).0, "1e3");
        assert_eq!(text(kind("<input id=x type=number value=abc>")).0, "");
        assert_eq!(text(kind("<input id=x type=email value=' a@b '>")).0, "a@b");
        assert_eq!(
            text(kind("<input id=x type=password value=abc>")).0,
            "\u{2022}\u{2022}\u{2022}"
        );
        // Unknown types are text fields.
        assert_eq!(text(kind("<input id=x type=bogus value=v>")).0, "v");
        assert_eq!(kind("<input id=x type=hidden>"), None);
        assert_eq!(
            kind("<input id=x type=color value='#FF8000'>"),
            Some(ControlKind::Color(Color::rgb(255, 128, 0)))
        );
        assert_eq!(
            kind("<input id=x type=color value=red>"),
            Some(ControlKind::Color(Color::BLACK))
        );
        assert_eq!(
            kind("<input id=x type=range>"),
            Some(ControlKind::Range { fraction: 0.5 })
        );
        assert_eq!(
            kind("<input id=x type=range min=10 max=20 value=30>"),
            Some(ControlKind::Range { fraction: 1.0 })
        );
        assert_eq!(
            kind("<input id=x type=submit>"),
            Some(ControlKind::Button {
                label: "Submit".into()
            })
        );
        assert_eq!(
            kind("<input id=x type=date value=2024-02-29>"),
            Some(ControlKind::DateTime {
                text: "02/29/2024".into()
            })
        );
        assert_eq!(
            kind("<input id=x type=date value=2023-02-29>"),
            Some(ControlKind::DateTime {
                text: "mm/dd/yyyy".into()
            })
        );
        assert_eq!(
            kind("<input id=x type=checkbox checked>"),
            Some(ControlKind::Checkbox {
                checked: true,
                indeterminate: false
            })
        );
    }

    fn select(html: &str) -> (Vec<(String, bool, bool)>, Option<u32>) {
        match kind(html) {
            Some(ControlKind::Select { options, list_rows }) => (
                options
                    .iter()
                    .map(|o| (o.label.to_string(), o.selected, o.group))
                    .collect(),
                list_rows,
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn select_selectedness_and_display_size() {
        // A drop-down: the first non-disabled option when none is selected.
        let (o, rows) = select("<select id=x><option disabled>A<option> B  b <option>C</select>");
        assert_eq!(rows, None);
        assert_eq!(
            o,
            vec![
                ("A".into(), false, false),
                ("B b".into(), true, false),
                ("C".into(), false, false)
            ]
        );
        // The last option with `selected` wins; a label attribute is the label; optgroup labels
        // are not rows of a drop-down.
        let (o, _) = select(
            "<select id=x><option selected>A<optgroup label=G><option label=L selected>x</optgroup></select>",
        );
        assert_eq!(
            o,
            vec![("A".into(), false, false), ("L".into(), true, false)]
        );
        // multiple: 4 rows, every selected option; size > 1 without selection: none.
        let (o, rows) =
            select("<select id=x multiple><option selected>A<option selected>B</select>");
        assert_eq!(rows, Some(4));
        assert!(o.iter().all(|r| r.1));
        let (o, rows) =
            select("<select id=x size=3><optgroup label=G><option>A</optgroup><option>B</select>");
        assert_eq!(rows, Some(3));
        assert_eq!(
            o,
            vec![
                ("G".into(), false, true),
                ("A".into(), false, false),
                ("B".into(), false, false)
            ]
        );
    }

    #[test]
    fn meter_regions() {
        let level = |html: &str| match kind(html) {
            Some(ControlKind::Meter { fraction, level }) => (fraction, level),
            other => panic!("{other:?}"),
        };
        assert_eq!(level("<meter id=x value=0.5>"), (0.5, MeterLevel::Optimum));
        assert_eq!(
            level("<meter id=x value=0.9 low=0.3 high=0.7 optimum=0.1>"),
            (0.9, MeterLevel::EvenLessGood)
        );
        assert_eq!(
            level("<meter id=x value=0.5 low=0.3 high=0.7 optimum=0.1>").1,
            MeterLevel::Suboptimum
        );
        assert_eq!(
            level("<meter id=x value=0.9 low=0.3 high=0.7>").1,
            MeterLevel::Suboptimum
        );
        assert_eq!(level("<meter id=x min=10 max=20 value=15>").0, 0.5);
    }

    #[test]
    fn fieldset_disables_except_first_legend() {
        let (dom, doc) = parse(
            "<fieldset disabled><legend><input id=a></legend><legend><input id=b></legend><input id=c></fieldset><input id=d>",
        );
        let d = |id| is_disabled(&dom, by_id(&dom, doc, id));
        assert_eq!((d("a"), d("b"), d("c"), d("d")), (false, true, true, false));
    }

    #[test]
    fn number_parsing() {
        assert_eq!(non_negative_integer(Some(" +12px")), Some(12));
        assert_eq!(non_negative_integer(Some("-1")), None);
        assert_eq!(parse_float(Some(" 1.5e1x")), Some(15.0));
        assert_eq!(valid_float("1."), None);
        assert_eq!(valid_float("-0.5"), Some(-0.5));
    }

    #[test]
    fn display_offsets_map_back_to_utf16() {
        for (value, masked) in [("a\u{1F600}b", false), ("a\u{1F600}b", true)] {
            for u in [0u32, 1, 3, 4] {
                let byte = display_byte(value, u, masked);
                assert_eq!(
                    utf16_of_display(value, byte, masked),
                    u,
                    "{value:?} {u} {masked}"
                );
            }
        }
    }
}
