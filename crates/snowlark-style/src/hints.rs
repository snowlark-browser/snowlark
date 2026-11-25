// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Presentational hints (HTML §15 "Rendering": the attributes that "map to" CSS properties), produced as a
//! declaration list in CSS syntax per element and parsed once per attribute change. They enter the
//! cascade at stylo's `PresHints` level: after the user-agent sheet, before author rules.
//!
//! A table's `cellpadding` is a hint on its cells ([`cell_table`]); the engine recomputes the cells'
//! hints when the table's attribute changes. `tr bgcolor` inherited by cells is not modelled.

use std::borrow::Cow;
use std::fmt::Write;

use smallvec::SmallVec;
use snowlark_dom::{local_name, ns, Dom, LocalName, NodeId};

/// HTML §2.3.4.1 "rules for parsing integers".
pub(crate) fn parse_integer(input: &str) -> Option<i64> {
    let s = input.trim_start_matches(is_html_space);
    let (negative, digits) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let value: i64 = digits[..end].parse().ok()?;
    Some(if negative { -value } else { value })
}

/// HTML §2.3.4.2 "rules for parsing non-negative integers".
pub(crate) fn parse_non_negative_integer(input: &str) -> Option<i64> {
    parse_integer(input).filter(|&n| n >= 0)
}

fn is_html_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')
}

/// A dimension value (HTML §2.3.4.6): a length in CSS pixels or a percentage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Dimension {
    Px(f64),
    Percent(f64),
}

impl Dimension {
    fn css(self) -> String {
        match self {
            Dimension::Px(v) => format!("{v}px"),
            Dimension::Percent(v) => format!("{v}%"),
        }
    }
}

/// HTML §2.3.4.6 "rules for parsing dimension values".
pub(crate) fn parse_dimension(input: &str) -> Option<Dimension> {
    let s = input.trim_start_matches(is_html_space);
    let bytes = s.as_bytes();
    let int_end = bytes
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    if int_end == 0 {
        return None;
    }
    let mut value: f64 = s[..int_end].parse().ok()?;
    let mut pos = int_end;
    if bytes.get(pos) == Some(&b'.') {
        let frac_start = pos + 1;
        let frac_end = bytes[frac_start..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .map_or(bytes.len(), |p| frac_start + p);
        if frac_end > frac_start {
            let frac: f64 = format!("0.{}", &s[frac_start..frac_end]).parse().ok()?;
            value += frac;
        }
        pos = frac_end;
    }
    if bytes.get(pos) == Some(&b'%') {
        Some(Dimension::Percent(value))
    } else {
        Some(Dimension::Px(value))
    }
}

/// HTML §2.3.4.6 "rules for parsing nonzero dimension values".
pub(crate) fn parse_nonzero_dimension(input: &str) -> Option<Dimension> {
    parse_dimension(input).filter(|d| match d {
        Dimension::Px(v) | Dimension::Percent(v) => *v != 0.0,
    })
}

/// HTML §2.3.6 "rules for parsing a legacy color value", as `#rrggbb`.
pub(crate) fn parse_legacy_color(input: &str) -> Option<String> {
    let s = input.trim_matches(is_html_space);
    if s.is_empty() || s.eq_ignore_ascii_case("transparent") {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    if cssparser::color::parse_named_color(&lower).is_ok() {
        return Some(lower);
    }
    let b = s.as_bytes();
    if b.len() == 4 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_hexdigit) {
        let d = |c: u8| (c as char).to_digit(16).unwrap() * 17;
        return Some(format!("#{:02x}{:02x}{:02x}", d(b[1]), d(b[2]), d(b[3])));
    }
    // Replace non-BMP code points with "00", truncate to 128 code units, drop a leading '#', replace
    // non-hex characters with '0'.
    let mut chars: Vec<char> = Vec::new();
    for c in s.chars() {
        if (c as u32) > 0xffff {
            chars.extend(['0', '0']);
        } else {
            chars.push(c);
        }
    }
    chars.truncate(128);
    if chars.first() == Some(&'#') {
        chars.remove(0);
    }
    for c in chars.iter_mut() {
        if !c.is_ascii_hexdigit() {
            *c = '0';
        }
    }
    while chars.is_empty() || !chars.len().is_multiple_of(3) {
        chars.push('0');
    }
    let len = chars.len() / 3;
    let mut parts: Vec<&[char]> = chars.chunks(len).collect();
    let mut length = len;
    if length > 8 {
        let drop = length - 8;
        for p in parts.iter_mut() {
            *p = &p[drop..];
        }
        length = 8;
    }
    while length > 2 && parts.iter().all(|p| p[0] == '0') {
        for p in parts.iter_mut() {
            *p = &p[1..];
        }
        length -= 1;
    }
    let mut out = String::from("#");
    for p in parts {
        let mut hex: String = p.iter().take(2).collect();
        if hex.len() < 2 {
            hex.insert(0, '0');
        }
        out.push_str(&hex.to_ascii_lowercase());
    }
    Some(out)
}

struct Hints<'a> {
    dom: &'a Dom,
    id: NodeId,
    out: String,
}

impl Hints<'_> {
    fn attr(&self, name: &LocalName) -> Option<&str> {
        self.dom.attr(self.id, &ns!(), name)
    }

    fn set(&mut self, property: &str, value: &str) {
        let _ = write!(self.out, "{property}:{value};");
    }

    fn dimension(&mut self, attr: LocalName, property: &str, ignore_zero: bool) {
        let parsed = self.attr(&attr).and_then(|v| {
            if ignore_zero {
                parse_nonzero_dimension(v)
            } else {
                parse_dimension(v)
            }
        });
        if let Some(d) = parsed {
            self.set(property, &d.css());
        }
    }

    fn color(&mut self, attr: LocalName, property: &str) {
        if let Some(c) = self.attr(&attr).and_then(parse_legacy_color) {
            self.set(property, &c);
        }
    }

    fn text_align(&mut self) {
        let value = match self.attr(&local_name!("align")) {
            Some(v) if v.eq_ignore_ascii_case("left") => "-webkit-left",
            Some(v) if v.eq_ignore_ascii_case("right") => "-webkit-right",
            Some(v) if v.eq_ignore_ascii_case("center") || v.eq_ignore_ascii_case("middle") => {
                "-webkit-center"
            }
            Some(v) if v.eq_ignore_ascii_case("justify") => "justify",
            _ => return,
        };
        self.set("text-align", value);
    }

    fn valign(&mut self) {
        if let Some(v) = self.attr(&local_name!("valign")) {
            let v = v.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "top" | "middle" | "bottom" | "baseline") {
                self.set("vertical-align", &v);
            }
        }
    }

    /// HTML §15.4.3: a `border` greater than zero is a solid border of that many pixels.
    fn image_border(&mut self) {
        if let Some(n) = self
            .attr(&local_name!("border"))
            .and_then(parse_non_negative_integer)
            .filter(|&n| n > 0)
        {
            self.set("border-width", &format!("{n}px"));
            self.set("border-style", "solid");
        }
    }

    /// HTML §15.2 "map to the aspect-ratio property" (`dimension_rules`: "using dimension rules").
    fn aspect_ratio(&mut self, dimension_rules: bool) {
        let parse = |v: &str| -> Option<f64> {
            if dimension_rules {
                match parse_dimension(v)? {
                    Dimension::Px(n) => Some(n),
                    Dimension::Percent(_) => None,
                }
            } else {
                parse_non_negative_integer(v).map(|n| n as f64)
            }
        };
        let w = self.attr(&local_name!("width")).and_then(parse);
        let h = self.attr(&local_name!("height")).and_then(parse);
        if let (Some(w), Some(h)) = (w, h) {
            self.set("aspect-ratio", &format!("auto {w} / {h}"));
        }
    }

    /// HTML §15.4.3 "attributes for embedded content and images" (`align`, `hspace`, `vspace`, sizes).
    fn embedded(&mut self) {
        self.dimension(local_name!("width"), "width", false);
        self.dimension(local_name!("height"), "height", false);
        if let Some(d) = self.attr(&local_name!("hspace")).and_then(parse_dimension) {
            let v = d.css();
            self.set("margin-left", &v);
            self.set("margin-right", &v);
        }
        if let Some(d) = self.attr(&local_name!("vspace")).and_then(parse_dimension) {
            let v = d.css();
            self.set("margin-top", &v);
            self.set("margin-bottom", &v);
        }
        match self
            .attr(&local_name!("align"))
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("left") => self.set("float", "left"),
            Some("right") => self.set("float", "right"),
            Some("top") => self.set("vertical-align", "top"),
            Some("middle") => self.set("vertical-align", "middle"),
            Some("bottom" | "baseline") => self.set("vertical-align", "baseline"),
            _ => {}
        }
    }
}

/// The presentational hints of `element` as a CSS declaration list, or `None` when it has none.
/// The table a `td`/`th` belongs to (HTML §4.9.11 "cell's table"): its row's parent table, directly or
/// through a row group.
pub(crate) fn cell_table(dom: &Dom, cell: NodeId) -> Option<NodeId> {
    let row = dom.parent_element(cell)?;
    if !dom.is_html_element_named(row, &local_name!("tr")) {
        return None;
    }
    let p = dom.parent_element(row)?;
    if dom.is_html_element_named(p, &local_name!("table")) {
        return Some(p);
    }
    let group = matches!(
        dom.element(p).map(|e| e.name().local.clone()),
        Some(local_name!("tbody") | local_name!("thead") | local_name!("tfoot"))
    );
    if !group {
        return None;
    }
    dom.parent_element(p)
        .filter(|&t| dom.is_html_element_named(t, &local_name!("table")))
}

/// SVG 2 §6.6 presentation attributes: the attributes of SVG elements that are CSS properties of the
/// same name, at the presentational hints' level of the cascade (their specificity is 0, author rules
/// and `style` override them), on every SVG element. The geometry properties and `transform` have
/// their own rules ([`svg_special_presentation`]).
macro_rules! svg_presentation {
    ($($name:literal,)*) => {
        #[cfg(test)]
        const SVG_PRESENTATION: &[&str] = &[$($name),*];

        /// The property of the presentation attribute `attribute` that is one on every SVG element.
        /// (A match: the attribute names of every SVG element prepared go through it.)
        fn svg_common_presentation(attribute: &str) -> Option<&'static str> {
            match attribute {
                $($name => Some($name),)*
                _ => None,
            }
        }
    };
}

svg_presentation! {
    "alignment-baseline",
    "baseline-shift",
    "clip-path",
    "clip-rule",
    "color",
    "color-interpolation",
    "color-interpolation-filters",
    "cursor",
    "direction",
    "display",
    "dominant-baseline",
    "fill",
    "fill-opacity",
    "fill-rule",
    "filter",
    "flood-color",
    "flood-opacity",
    "font-family",
    "font-size",
    "font-size-adjust",
    "font-stretch",
    "font-style",
    "font-variant",
    "font-weight",
    "image-rendering",
    "letter-spacing",
    "lighting-color",
    "marker-end",
    "marker-mid",
    "marker-start",
    "mask",
    "mask-type",
    "opacity",
    "overflow",
    "paint-order",
    "pointer-events",
    "shape-rendering",
    "stop-color",
    "stop-opacity",
    "stroke",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-opacity",
    "stroke-width",
    "text-anchor",
    "text-decoration",
    "text-overflow",
    "text-rendering",
    "transform-origin",
    "unicode-bidi",
    "vector-effect",
    "visibility",
    "white-space",
    "word-spacing",
    "writing-mode",
}

/// SVG 2 §6.6 (the presentation attributes table): the attributes that are presentation attributes
/// only on some elements, as (attribute, property, unitless lengths are px): the geometry
/// properties (SVG 2 §9.2-9.5, §11.2) and the spellings of `transform` (§8.5: `patternTransform` on
/// `pattern`, `gradientTransform` on the gradients, `transform` on the others).
fn svg_special_presentation(element: &str, attribute: &str) -> Option<&'static str> {
    Some(match (attribute, element) {
        ("cx" | "cy", "circle" | "ellipse") => {
            if attribute == "cx" {
                "cx"
            } else {
                "cy"
            }
        }
        ("r", "circle") => "r",
        ("rx", "ellipse" | "rect") => "rx",
        ("ry", "ellipse" | "rect") => "ry",
        ("x", "foreignObject" | "image" | "rect" | "svg" | "symbol" | "use") => "x",
        ("y", "foreignObject" | "image" | "rect" | "svg" | "symbol" | "use") => "y",
        ("width", "foreignObject" | "image" | "rect" | "svg" | "symbol" | "use") => "width",
        ("height", "foreignObject" | "image" | "rect" | "svg" | "symbol" | "use") => "height",
        ("d", "path") => "d",
        ("patternTransform", "pattern") => "transform",
        ("gradientTransform", "linearGradient" | "radialGradient") => "transform",
        ("transform", e) if !matches!(e, "pattern" | "linearGradient" | "radialGradient") => {
            "transform"
        }
        _ => return None,
    })
}

/// An SVG transform list (SVG 2 §8.5, the `transform` attribute's grammar: unitless translations
/// and angles, whitespace or commas between arguments, `rotate(a cx cy)`) as a CSS `transform`
/// value; `None` when it does not parse that way (the value is then taken as CSS).
fn svg_transform_to_css(v: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = v.trim();
    while !rest.is_empty() {
        let open = rest.find('(')?;
        let name = rest[..open].trim().trim_start_matches(',').trim();
        let close = rest[open..].find(')')? + open;
        let args: Vec<f64> = rest[open + 1..close]
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|a| !a.is_empty())
            .map(|a| a.parse::<f64>().ok())
            .collect::<Option<_>>()?;
        if !out.is_empty() {
            out.push(' ');
        }
        let n = |i: usize| args[i];
        match (name, args.len()) {
            ("matrix", 6) => write!(
                out,
                "matrix({}, {}, {}, {}, {}, {})",
                n(0),
                n(1),
                n(2),
                n(3),
                n(4),
                n(5)
            ),
            ("translate", 1) => write!(out, "translate({}px)", n(0)),
            ("translate", 2) => write!(out, "translate({}px, {}px)", n(0), n(1)),
            ("scale", 1) => write!(out, "scale({})", n(0)),
            ("scale", 2) => write!(out, "scale({}, {})", n(0), n(1)),
            ("rotate", 1) => write!(out, "rotate({}deg)", n(0)),
            ("rotate", 3) => write!(
                out,
                "translate({}px, {}px) rotate({}deg) translate({}px, {}px)",
                n(1),
                n(2),
                n(0),
                -n(1),
                -n(2)
            ),
            ("skewX", 1) => write!(out, "skewX({}deg)", n(0)),
            ("skewY", 1) => write!(out, "skewY({}deg)", n(0)),
            _ => return None,
        }
        .ok()?;
        rest = rest[close + 1..].trim_start();
    }
    (!out.is_empty()).then_some(out)
}

/// The presentation attributes of an SVG element as declarations. A presentation attribute's value is
/// parsed as the property's value, with SVG's unitless lengths read as px.
/// The property a presentation attribute of an SVG element sets (SVG 2 §6.6), if it is one.
pub fn svg_presentation_property(
    element: &str,
    outermost_svg: bool,
    attribute: &str,
) -> Option<&'static str> {
    let special = svg_special_presentation(element, attribute).filter(|p| {
        if element != "svg" {
            return true;
        }
        match *p {
            "width" | "height" => outermost_svg,
            "transform" => !outermost_svg,
            _ => true,
        }
    });
    special.or_else(|| svg_common_presentation(attribute))
}

/// The property a presentation attribute's `value` (trimmed, not empty) sets and its value in CSS
/// syntax: the attribute's grammars allow unitless lengths and SVG transform lists (SVG 2 §6.6, §8.5).
pub fn svg_presentation_declaration<'a>(
    element: &str,
    outermost_svg: bool,
    attribute: &str,
    value: &'a str,
) -> Option<(&'static str, Cow<'a, str>)> {
    let property = svg_presentation_property(element, outermost_svg, attribute)?;
    Some((property, svg_presentation_css(property, value)?))
}

/// The value in CSS syntax of a presentation attribute that sets `property` and has the value `value`
/// (trimmed, not empty), `None` when it is not given to the cascade. The conversion depends on the
/// property alone: the attributes with grammars of their own (the geometry properties and the
/// spellings of `transform`, [`svg_special_presentation`]) set properties no other attribute sets.
pub(crate) fn svg_presentation_css<'a>(property: &str, value: &'a str) -> Option<Cow<'a, str>> {
    let css = if matches!(
        property,
        "cx" | "cy" | "r" | "rx" | "ry" | "x" | "y" | "width" | "height" | "d" | "transform"
    ) {
        if property == "transform" {
            // (css/css-transforms/skew*/svg-skew*-with-units.html: the skews take unitless angles
            // only in the attribute, not CSS's)
            match svg_transform_to_css(value) {
                Some(css) => Cow::Owned(css),
                None if value.contains("skew") => return None,
                None => Cow::Borrowed(value),
            }
        } else if property == "d" {
            // (`d`'s value is a path string)
            Cow::Owned(format!("path(\"{}\")", value.replace(['"', '\\'], "")))
        } else if value.parse::<f64>().is_ok() {
            Cow::Owned(format!("{value}px"))
        } else {
            Cow::Borrowed(value)
        }
    } else if matches!(
        property,
        "stroke-width"
            | "stroke-dashoffset"
            | "stroke-dasharray"
            | "font-size"
            | "letter-spacing"
            | "word-spacing"
            | "baseline-shift"
            | "transform-origin"
    ) {
        let mut out = String::new();
        for part in value.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
            if part.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(part);
            if part.parse::<f64>().is_ok() {
                out.push_str("px");
            }
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(value)
    };
    Some(css)
}

/// The `d` presentation attribute's declaration (SVG 2 §9.4.1: the attribute's value is path data,
/// and the `d` property's value is that path): the path data parsed once, with the attribute's
/// grammar (§9.3.9, white space and newlines included), into the property's value, which the shape
/// is then drawn and measured from. `None` when the data holds an error: the path then comes from
/// the attribute, which keeps what precedes the error (§9.5.4).
pub(crate) fn d_declaration(path_data: &str) -> Option<style::properties::PropertyDeclaration> {
    use style::values::specified::{DProperty, SVGPathData};
    let (path, ok) = SVGPathData::parse_bytes(path_data.as_bytes());
    ok.then(|| style::properties::PropertyDeclaration::D(DProperty::Path(path)))
}

/// An SVG element's presentation attributes, in order (SVG 2 §6.6): each one's property and its
/// value as written (trimmed), then the animated values of presentation attributes. A later one of a
/// property overrides an earlier one. The value becomes a declaration of the property through
/// [`svg_presentation_css`] (`d`'s through [`d_declaration`]), which depends on the property and the
/// value alone, so the list is all a cache of the declarations needs as its key.
pub(crate) fn svg_presentation_list<'a>(
    dom: &'a Dom,
    element: NodeId,
    out: &mut SmallVec<[(&'static str, &'a str); 8]>,
) {
    let Some(e) = dom.element(element) else {
        return;
    };
    let name = &*e.name().local;
    // (on the animation elements `fill` is the animation's fill mode, SVG Animation §3.2)
    let animation = matches!(
        name,
        "animate" | "animateMotion" | "animateTransform" | "set"
    );
    // (svgwg#1057: `width` and `height` are presentation attributes of the outermost `svg` only;
    // its `transform` is applied by the SVG image the box shows, not as a CSS transform of the box)
    let outermost_svg = name == "svg"
        && !dom
            .parent_element(element)
            .and_then(|p| dom.element(p))
            .is_some_and(|p| p.name().ns == ns!(svg));
    let presented = e
        .attrs()
        .iter()
        .filter(|a| *a.namespace() == ns!())
        .filter(|a| !(animation && &**a.local_name() == "fill"))
        .map(|a| (&**a.local_name(), a.value()));
    // (an animated value of a presentation attribute that animations cannot reach, as `display`)
    let animated = dom
        .animated_attrs(element)
        .iter()
        .map(|(local, value)| (&**local, value.as_str()));
    for (local, value) in presented.chain(animated) {
        let v = value.trim();
        // (one declaration per attribute: nothing that could end it or start another)
        if v.is_empty() || v.contains([';', '{', '}', '!']) {
            continue;
        }
        if let Some(property) = svg_presentation_property(name, outermost_svg, local) {
            out.push((property, v));
        }
    }
}

/// HTML §3.2.6.2: an element's own language attribute (`xml:lang`, else `lang` in no namespace), as
/// the language its text is shaped and broken in (CSS Text 3 §1.3 "content language"): stylo's
/// internal `-x-lang`, which inherits as the language does. The engine adds it to the hints, since
/// no author CSS can set it.
pub(crate) fn content_language_hint(dom: &Dom, element: NodeId) -> Option<String> {
    dom.element(element)?;
    dom.attr(element, &ns!(xml), &local_name!("lang"))
        .or_else(|| dom.attr(element, &ns!(), &local_name!("lang")))
        .map(|v| v.to_string())
}

/// Whether a change of the attribute `local` in no namespace can change `element`'s presentational
/// hints. SVG's are the presentation attributes; HTML's are spread over its elements, so only the
/// attributes that are hints on no element are ruled out. `lang` is one on every element.
pub(crate) fn can_change_hints(dom: &Dom, element: NodeId, local: &str) -> bool {
    let Some(e) = dom.element(element) else {
        return false;
    };
    if local == "lang" {
        return true;
    }
    let name = e.name();
    if name.ns == ns!(svg) {
        // (either way: an `svg`'s being outermost is not looked up for every attribute change)
        return svg_presentation_property(&name.local, true, local).is_some()
            || svg_presentation_property(&name.local, false, local).is_some();
    }
    name.ns == ns!(html)
        && !matches!(local, "class" | "id" | "style" | "part" | "exportparts")
        && !local.starts_with("data-")
        && !local.starts_with("aria-")
}

/// An HTML element's presentational hints as CSS declarations (SVG's: [`svg_presentation_list`]).
pub(crate) fn presentational_hints(dom: &Dom, element: NodeId, quirks: bool) -> Option<String> {
    let e = dom.element(element)?;
    let name = e.name();
    if name.ns != ns!(html) {
        return None;
    }
    if e.attrs().is_empty()
        && !matches!(
            name.local,
            local_name!("body") | local_name!("center") | local_name!("td") | local_name!("th")
        )
    {
        return None;
    }
    let mut h = Hints {
        dom,
        id: element,
        out: String::new(),
    };
    let local = name.local.clone();
    match local {
        // §15.3.3 flow content, §15.3.7 sections and headings, §15.3.8 tables.
        local_name!("div") => h.text_align(),
        // §15.3.3: `center` centers its text and aligns its descendants to the center.
        local_name!("center") => h.set("text-align", "-webkit-center"),
        local_name!("p")
        | local_name!("h1")
        | local_name!("h2")
        | local_name!("h3")
        | local_name!("h4")
        | local_name!("h5")
        | local_name!("h6") => {
            // §15.3.8: `p[align=left i] { text-align: left; }` and so on, as presentational hints.
            if let Some(v) = h.attr(&local_name!("align")) {
                let v = v.to_ascii_lowercase();
                if matches!(v.as_str(), "left" | "right" | "center" | "justify") {
                    h.set("text-align", &v);
                }
            }
        }
        local_name!("caption") => {
            if h.attr(&local_name!("align"))
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("bottom"))
            {
                h.set("caption-side", "bottom");
            } else {
                h.text_align();
            }
        }
        local_name!("body") => {
            h.color(local_name!("bgcolor"), "background-color");
            h.color(local_name!("text"), "color");
            // §15.3.2: the first of the attributes that exists maps to the pixel length margins, 8px
            // when none exists or it does not parse. (The container frame's attributes come from the
            // page layer, which has the navigable.)
            for (attrs, props) in [
                (
                    ["marginheight", "topmargin"],
                    ["margin-top", "margin-bottom"],
                ),
                (
                    ["marginwidth", "leftmargin"],
                    ["margin-left", "margin-right"],
                ),
            ] {
                let value = attrs
                    .iter()
                    .find_map(|a| h.attr(&LocalName::from(*a)))
                    .and_then(parse_non_negative_integer)
                    .unwrap_or(8);
                for prop in props {
                    h.set(prop, &format!("{value}px"));
                }
            }
            if let Some(bg) = h.attr(&local_name!("background")).filter(|v| !v.is_empty()) {
                let url = bg.replace('\\', "\\\\").replace('"', "\\\"");
                h.set("background-image", &format!("url(\"{url}\")"));
            }
        }
        local_name!("table") => {
            h.color(local_name!("bgcolor"), "background-color");
            h.dimension(local_name!("width"), "width", true);
            h.dimension(local_name!("height"), "height", false);
            if let Some(n) = h
                .attr(&local_name!("cellspacing"))
                .and_then(parse_non_negative_integer)
            {
                h.set("border-spacing", &format!("{n}px"));
            }
            // §15.3.8: pixel lengths, 1px when present but not a non-negative integer. The 'outset'
            // style is the UA sheet's `:-servo-nonzero-border` rule.
            if let Some(n) = h
                .attr(&local_name!("border"))
                .map(|v| parse_non_negative_integer(v).unwrap_or(1))
            {
                h.set("border-width", &format!("{n}px"));
            }
            h.color(LocalName::from("bordercolor"), "border-color");
            match h
                .attr(&local_name!("align"))
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref()
            {
                Some("left") => h.set("float", "left"),
                Some("right") => h.set("float", "right"),
                Some("center") => {
                    h.set("margin-inline-start", "auto");
                    h.set("margin-inline-end", "auto");
                }
                _ => {}
            }
        }
        local_name!("thead") | local_name!("tbody") | local_name!("tfoot") => {
            h.text_align();
            h.valign();
            h.color(local_name!("bgcolor"), "background-color");
            h.dimension(local_name!("height"), "height", false);
        }
        local_name!("tr") => {
            h.text_align();
            h.valign();
            h.color(local_name!("bgcolor"), "background-color");
            h.dimension(local_name!("height"), "height", false);
        }
        local_name!("td") | local_name!("th") => {
            // §15.3.8: the cell's table's `cellpadding`, as pixel padding on every side.
            if let Some(n) = cell_table(dom, element)
                .and_then(|t| dom.attr(t, &ns!(), &local_name!("cellpadding")))
                .and_then(parse_non_negative_integer)
            {
                h.set("padding", &format!("{n}px"));
            }
            h.text_align();
            h.valign();
            h.color(local_name!("bgcolor"), "background-color");
            h.dimension(local_name!("width"), "width", true);
            h.dimension(local_name!("height"), "height", true);
            // §15.3.8: `td[nowrap]` is the UA sheet's; in quirks mode a length `width` cancels it.
            if quirks
                && h.attr(&local_name!("nowrap")).is_some()
                && matches!(
                    h.attr(&local_name!("width"))
                        .and_then(parse_nonzero_dimension),
                    Some(Dimension::Px(_))
                )
            {
                h.set("white-space", "normal");
            }
        }
        local_name!("col") | local_name!("colgroup") => {
            h.dimension(local_name!("width"), "width", false);
            h.valign();
        }
        local_name!("img") | local_name!("object") | local_name!("embed") => {
            h.embedded();
            h.image_border();
            if local == local_name!("img") {
                h.aspect_ratio(true);
            }
        }
        local_name!("video") => {
            h.dimension(local_name!("width"), "width", false);
            h.dimension(local_name!("height"), "height", false);
            h.aspect_ratio(true);
        }
        local_name!("canvas") => h.aspect_ratio(false),
        local_name!("frameset") => {
            // §15.6 frames: the rows and columns of the frameset grid.
            for (attr, property) in [
                (local_name!("rows"), "grid-template-rows"),
                (local_name!("cols"), "grid-template-columns"),
            ] {
                if let Some(tracks) = h.attr(&attr).and_then(frameset_tracks) {
                    h.set(property, &tracks);
                }
            }
        }
        local_name!("frame") => {
            if let Some(v) = h.attr(&local_name!("frameborder")) {
                if parse_integer(v).is_none_or(|n| n == 0) {
                    h.set("border-width", "0");
                }
            }
        }
        local_name!("iframe") => {
            h.embedded();
            // §15.4.4: frameborder="0" (or not an integer) removes the border.
            if let Some(v) = h.attr(&local_name!("frameborder")) {
                if parse_integer(v).is_none_or(|n| n == 0) {
                    h.set("border-width", "0");
                }
            }
        }
        local_name!("input") => {
            if h.attr(&local_name!("type"))
                .is_some_and(|t| t.eq_ignore_ascii_case("image"))
            {
                h.embedded();
                h.image_border();
                h.aspect_ratio(true);
            }
        }
        local_name!("hr") => {
            // §15.3.11.
            match h
                .attr(&local_name!("align"))
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref()
            {
                Some("left") => {
                    h.set("margin-left", "0");
                    h.set("margin-right", "auto");
                }
                Some("right") => {
                    h.set("margin-left", "auto");
                    h.set("margin-right", "0");
                }
                Some("center") => {
                    h.set("margin-left", "auto");
                    h.set("margin-right", "auto");
                }
                _ => {}
            }
            let shaded = h.attr(&local_name!("color")).is_some()
                || h.attr(&local_name!("noshade")).is_some();
            if shaded {
                h.set("border-style", "solid");
            }
            if let Some(n) = h
                .attr(&local_name!("size"))
                .and_then(parse_non_negative_integer)
            {
                if shaded {
                    h.set("border-width", &format!("{}px", n as f64 / 2.0));
                } else if n == 1 {
                    h.set("border-bottom-width", "0");
                } else if n > 1 {
                    h.set("height", &format!("{}px", n - 2));
                }
            }
            h.dimension(local_name!("width"), "width", false);
            h.color(local_name!("color"), "color");
        }
        local_name!("font") => {
            // §15.3.4.
            h.color(local_name!("color"), "color");
            if let Some(face) = h
                .attr(&local_name!("face"))
                .filter(|f| !f.trim().is_empty())
            {
                let face = face.to_owned();
                h.set("font-family", &face);
            }
            if let Some(size) = h.attr(&local_name!("size")).and_then(legacy_font_size) {
                h.set("font-size", size);
            }
        }
        local_name!("ol") | local_name!("li") | local_name!("ul") | local_name!("dir") => {
            // §15.3.8 lists: the `type` attribute.
            if let Some(t) = h.attr(&local_name!("type")) {
                let value = match t {
                    "1" => Some("decimal"),
                    "a" => Some("lower-alpha"),
                    "A" => Some("upper-alpha"),
                    "i" => Some("lower-roman"),
                    "I" => Some("upper-roman"),
                    _ => match t.to_ascii_lowercase().as_str() {
                        "none" => Some("none"),
                        "disc" => Some("disc"),
                        "circle" => Some("circle"),
                        "square" => Some("square"),
                        _ => None,
                    },
                };
                if let Some(v) = value {
                    h.set("list-style-type", v);
                }
            }
            // §15.3.8: an `li` whose `value` parses as an integer sets its `list-item` counter.
            if local == local_name!("li") {
                if let Some(n) = h.attr(&local_name!("value")).and_then(parse_integer) {
                    let n = n.clamp(i32::MIN as i64, i32::MAX as i64);
                    h.set("counter-set", &format!("list-item {n}"));
                }
            }
        }
        // §15.3.3: `pre[wrap] { white-space: pre-wrap; }`.
        local_name!("pre") if h.attr(&local_name!("wrap")).is_some() => {
            h.set("white-space", "pre-wrap");
        }
        _ => {}
    }
    (!h.out.is_empty()).then_some(h.out)
}

/// HTML §15.3.4 "rules for parsing a legacy font size".
fn legacy_font_size(input: &str) -> Option<&'static str> {
    let s = input.trim_start_matches(is_html_space);
    let (mode, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (0, s),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let n: i64 = digits[..end].parse().ok()?;
    let value = match mode {
        1 => 3 + n,
        -1 => 3 - n,
        _ => n,
    }
    .clamp(1, 7);
    Some(match value {
        1 => "x-small",
        2 => "small",
        3 => "medium",
        4 => "large",
        5 => "x-large",
        6 => "xx-large",
        _ => "xxx-large",
    })
}

/// HTML §2.3.4.4 "rules for parsing a list of dimensions" of a frameset's `rows`/`cols`, as grid
/// tracks: absolute lengths in px, percentages, relative values (`*`, `2*`) in `fr`.
pub fn frameset_tracks(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches(',');
    if value.is_empty() {
        return None;
    }
    let mut tracks = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        let digits: String = item
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let rest = item[digits.len()..].trim_start();
        let n: f64 = digits.parse().unwrap_or(0.0);
        tracks.push(match rest.chars().next() {
            Some('%') => format!("{n}%"),
            Some('*') => format!("{}fr", if digits.is_empty() { 1.0 } else { n }),
            _ => format!("{n}px"),
        });
    }
    Some(tracks.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The match of the presentation attributes on every SVG element is the table's, and the
    /// attributes with rules of their own stay out of it.
    #[test]
    fn svg_common_presentation_is_the_table() {
        for name in SVG_PRESENTATION {
            assert_eq!(svg_common_presentation(name), Some(*name));
        }
        for name in ["x", "d", "transform", "width", "class", "Fill", "fill "] {
            assert_eq!(svg_common_presentation(name), None, "{name}");
        }
        assert_eq!(svg_presentation_property("rect", false, "x"), Some("x"));
        assert_eq!(svg_presentation_property("text", false, "x"), None);
        assert_eq!(
            svg_presentation_property("pattern", false, "patternTransform"),
            Some("transform")
        );
        assert_eq!(
            svg_presentation_property("pattern", false, "transform"),
            None
        );
    }

    /// A presentation attribute's CSS text depends on its property alone: the spellings of
    /// `transform` convert alike, unitless lengths of the geometry properties and the length lists
    /// are px, other values stay as written.
    #[test]
    fn svg_presentation_css_by_property() {
        let css = |p, v| svg_presentation_css(p, v).map(|c| c.into_owned());
        assert_eq!(css("y", "2").as_deref(), Some("2px"));
        assert_eq!(css("y", "2em").as_deref(), Some("2em"));
        assert_eq!(css("stroke-width", "1 2,3").as_deref(), Some("1px 2px 3px"));
        assert_eq!(css("fill", "2").as_deref(), Some("2"));
        assert_eq!(css("transform", "skewX(10deg)"), None);
        for (attribute, element) in [
            ("transform", "rect"),
            ("patternTransform", "pattern"),
            ("gradientTransform", "linearGradient"),
        ] {
            assert_eq!(
                svg_presentation_declaration(element, false, attribute, "translate(1 2)")
                    .map(|(p, c)| (p, c.into_owned())),
                Some(("transform", css("transform", "translate(1 2)").unwrap()))
            );
        }
    }

    #[test]
    fn dimensions() {
        assert_eq!(parse_dimension(" 100"), Some(Dimension::Px(100.0)));
        assert_eq!(parse_dimension("50%"), Some(Dimension::Percent(50.0)));
        assert_eq!(parse_dimension("12.5px"), Some(Dimension::Px(12.5)));
        assert_eq!(parse_dimension("x"), None);
        assert_eq!(parse_nonzero_dimension("0"), None);
    }

    #[test]
    fn legacy_colors() {
        assert_eq!(parse_legacy_color("#f00").as_deref(), Some("#ff0000"));
        assert_eq!(parse_legacy_color("ff0000").as_deref(), Some("#ff0000"));
        // The classic "chucknorris" is red.
        assert_eq!(
            parse_legacy_color("chucknorris").as_deref(),
            Some("#c00000")
        );
        assert_eq!(parse_legacy_color("transparent"), None);
        assert_eq!(parse_legacy_color("Red").as_deref(), Some("red"));
    }

    #[test]
    fn integers_and_font_sizes() {
        assert_eq!(parse_integer("  -12px"), Some(-12));
        assert_eq!(parse_integer("x"), None);
        assert_eq!(legacy_font_size("+2"), Some("x-large"));
        assert_eq!(legacy_font_size("1"), Some("x-small"));
        assert_eq!(legacy_font_size("9"), Some("xxx-large"));
    }

    #[test]
    fn frameset_dimension_lists() {
        assert_eq!(
            frameset_tracks("100, *, 2*, 30%").as_deref(),
            Some("100px 1fr 2fr 30%")
        );
        assert_eq!(frameset_tracks("50%,50%,").as_deref(), Some("50% 50%"));
        assert_eq!(frameset_tracks(" "), None);
    }
}
