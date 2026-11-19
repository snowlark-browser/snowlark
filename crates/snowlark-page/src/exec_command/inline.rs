// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs §3 "Inline formatting commands": their definitions (effectively contained
//! nodes, modifiable and simple modifiable elements, formattable nodes, equivalent values), the
//! effective and specified command values, "reorder modifiable descendants", "record the values"
//! and "restore the values", "clear the value", "push down values", "force the value", "set the
//! selection's value", the commands themselves (`backColor`, `bold`, `createLink`, `fontName`,
//! `fontSize`, `foreColor`, `hiliteColor`, `italic`, `removeFormat`, `strikethrough`,
//! `subscript`, `superscript`, `underline`, `unlink`) with their states, values and
//! indeterminacy, and the state and value overrides (§2.6 "Recording and restoring overrides").
//!
//! The overrides are unset when the active range is no longer the one they were set at (the spec
//! unsets them whenever the selection's range changes).

use snowlark_bindings::rt::Cx;
use snowlark_dom::{ns, NodeId};

use super::nodes::Item;
use crate::engine::cssom::Pseudo;
use crate::engine::ranges::BoundaryPair;
use crate::engine::Engine;

/// A command value: a string, or null.
pub(crate) type Val = Option<String>;

/// The commands "record the values" covers, in its order.
const RECORDED: &[&str] = &[
    "subscript",
    "bold",
    "fontname",
    "fontsize",
    "forecolor",
    "hilitecolor",
    "italic",
    "strikethrough",
    "underline",
];

/// The inline formatting commands.
const INLINE: &[&str] = &[
    "backcolor",
    "bold",
    "createlink",
    "fontname",
    "fontsize",
    "forecolor",
    "hilitecolor",
    "italic",
    "removeformat",
    "strikethrough",
    "subscript",
    "superscript",
    "underline",
    "unlink",
];

/// The legacy font sizes 1 to 7 as keywords.
const SIZE_KEYWORDS: [&str; 7] = [
    "x-small",
    "small",
    "medium",
    "large",
    "x-large",
    "xx-large",
    "xxx-large",
];

/// The resolved `font-size` in pixels of a `font` element with `size` 1 to 7 (the keywords at the
/// initial `medium` of 16px).
const SIZE_PX: [f64; 7] = [10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0];

/// A document's state and value overrides, and the active range they were set at.
#[derive(Clone, Debug, Default)]
pub(crate) struct Overrides {
    range: Option<BoundaryPair>,
    state: Vec<(&'static str, bool)>,
    value: Vec<(&'static str, String)>,
}

/// A recorded override: a state, or a value (null when the command had none).
#[derive(Clone, Debug)]
pub(crate) enum Recorded {
    State(bool),
    Value(Val),
}

/// The static name of a command among the inline ones.
fn static_name(cmd: &str) -> &'static str {
    INLINE.iter().find(|c| **c == cmd).copied().unwrap_or("")
}

/// "The relevant CSS property" of a command.
fn relevant_css_property(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "bold" => "font-weight",
        "italic" => "font-style",
        "fontname" => "font-family",
        "fontsize" => "font-size",
        "forecolor" => "color",
        "backcolor" | "hilitecolor" => "background-color",
        _ => return None,
    })
}

/// "Inline command activated values".
fn activated_values(cmd: &str) -> Option<&'static [&'static str]> {
    Some(match cmd {
        "bold" => &["bold", "600", "700", "800", "900"],
        "italic" => &["italic", "oblique"],
        "strikethrough" => &["line-through"],
        "subscript" => &["subscript"],
        "superscript" => &["superscript"],
        "underline" => &["underline"],
        _ => return None,
    })
}

/// "A standard inline value command".
fn standard_inline_value(cmd: &str) -> bool {
    matches!(cmd, "backcolor" | "fontname" | "forecolor" | "hilitecolor")
}

/// Whether the command has a value.
fn has_value(cmd: &str) -> bool {
    standard_inline_value(cmd) || cmd == "fontsize"
}

/// A CSS color as its red, green, blue and alpha components; `None` when not a valid color or
/// `currentColor`.
fn normalize_color(color: &str) -> Option<(u8, u8, u8, u8)> {
    if color.trim().eq_ignore_ascii_case("currentcolor") {
        return None;
    }
    let c = snowlark_render::canvas::parse_color(color)?.to_color_u8();
    Some((c.red(), c.green(), c.blue(), c.alpha()))
}

/// The `#rrggbb` form of a fully opaque color ("simple color"), else `None`.
fn simple_color(color: &str) -> Option<String> {
    match normalize_color(color)? {
        (r, g, b, 255) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        _ => None,
    }
}

/// "Equivalent values" for a command.
pub(crate) fn equivalent(cmd: &str, a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match cmd {
            "bold" => {
                a == b
                    || matches!(
                        (a, b),
                        ("bold", "700") | ("700", "bold") | ("normal", "400") | ("400", "normal")
                    )
            }
            "backcolor" | "forecolor" | "hilitecolor" => normalize_color(a) == normalize_color(b),
            _ => a == b,
        },
        _ => false,
    }
}

/// "Loosely equivalent values": equivalent, or for `fontSize` a keyword and the pixel size of a
/// `font` element of the matching size.
pub(crate) fn loosely_equivalent(cmd: &str, a: Option<&str>, b: Option<&str>) -> bool {
    if equivalent(cmd, a, b) {
        return true;
    }
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    if cmd != "fontsize" {
        return false;
    }
    let px_of = |k: &str| {
        SIZE_KEYWORDS
            .iter()
            .position(|s| *s == k)
            .map(|i| format!("{}px", SIZE_PX[i]))
    };
    px_of(b).as_deref() == Some(a) || px_of(a).as_deref() == Some(b)
}

/// The fontSize action's value: a legacy size (absolute or relative to 3) as its keyword.
fn normalize_font_size(value: &str) -> Option<&'static str> {
    let v = value.trim_matches(|c: char| c.is_ascii_whitespace());
    let (mode, digits) = match v.as_bytes().first()? {
        b'+' => (1, &v[1..]),
        b'-' => (-1, &v[1..]),
        _ => (0, v),
    };
    // (a valid floating-point number: digits, an optional fraction, an optional exponent)
    let b = digits.as_bytes();
    let int_len = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if int_len == 0 {
        return None;
    }
    let mut i = int_len;
    if b.get(i) == Some(&b'.') {
        let f = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if f == 0 {
            return None;
        }
        i += 1 + f;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let e = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if e == 0 {
            return None;
        }
        i += e;
    }
    if i != b.len() {
        return None;
    }
    let n: i64 = digits[..int_len].parse().unwrap_or(i64::MAX / 2);
    let n = match mode {
        1 => n.saturating_add(3),
        -1 => 3 - n,
        _ => n,
    };
    Some(SIZE_KEYWORDS[(n.clamp(1, 7) - 1) as usize])
}

/// The `size` attribute value for a keyword.
fn css_size_to_legacy(keyword: &str) -> Option<usize> {
    SIZE_KEYWORDS
        .iter()
        .position(|s| *s == keyword)
        .map(|i| i + 1)
}

/// HTML "rules for parsing a legacy font size" of a `font` element's `size`.
fn parse_legacy_font_size(s: &str) -> Option<usize> {
    let s = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (mode, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (0, s),
    };
    let len = rest.bytes().take_while(|c| c.is_ascii_digit()).count();
    if len == 0 {
        return None;
    }
    let n: i64 = rest[..len].parse().unwrap_or(i64::MAX / 2);
    let n = match mode {
        1 => n.saturating_add(3),
        -1 => 3 - n,
        _ => n,
    };
    Some(n.clamp(1, 7) as usize)
}

/// "The legacy font size for" a size (a legacy size, a keyword or an absolute length); `None` for
/// one that has none (`2em`).
fn legacy_font_size(size: &str) -> Option<String> {
    if let Some(k) = normalize_font_size(size) {
        return css_size_to_legacy(k).map(|n| n.to_string());
    }
    let px = if let Some(i) = SIZE_KEYWORDS.iter().position(|s| *s == size) {
        SIZE_PX[i]
    } else {
        let unit_at = size.find(|c: char| c.is_ascii_alphabetic())?;
        let (num, unit) = size.split_at(unit_at);
        let ok = !num.is_empty()
            && !num.starts_with('.')
            && !num.ends_with('.')
            && num.bytes().all(|c| c.is_ascii_digit() || c == b'.')
            && num.bytes().filter(|&c| c == b'.').count() <= 1;
        if !ok {
            return None;
        }
        let n: f64 = num.parse().ok()?;
        let factor = match unit {
            "px" => 1.0,
            "pt" => 4.0 / 3.0,
            "pc" => 16.0,
            "in" => 96.0,
            "cm" => 96.0 / 2.54,
            "mm" => 96.0 / 25.4,
            _ => return None,
        };
        (n * factor).floor()
    };
    for returned in 1..7 {
        let average = (SIZE_PX[returned - 1] + SIZE_PX[returned]) / 2.0;
        if px < average {
            return Some(returned.to_string());
        }
    }
    Some("7".to_owned())
}

impl Engine {
    // ----- inline style -----------------------------------------------------------------------------------

    /// The value `el`'s `style` attribute sets `property` to ("" when none).
    pub(super) fn ec_style_value(&self, el: NodeId, property: &str) -> String {
        if !self.dom().is_element(el) || !self.dom().has_attribute(el, "style") {
            return String::new();
        }
        self.with_inline(el, |b| b.property_value(property))
    }

    /// The longhands `el`'s `style` attribute sets.
    pub(super) fn ec_style_longhands(&self, el: NodeId) -> Vec<String> {
        if !self.dom().has_attribute(el, "style") {
            return Vec::new();
        }
        self.with_inline(el, |b| (0..b.len()).filter_map(|i| b.item(i)).collect())
    }

    /// Whether `el`'s `style` sets exactly one property: one longhand, or the `text-decoration`
    /// shorthand's longhands only. Returns that property's name.
    fn ec_style_single_property(&self, el: NodeId) -> Option<String> {
        let props = self.ec_style_longhands(el);
        match props.len() {
            0 => None,
            1 => Some(props[0].clone()),
            _ if props.iter().all(|p| p.starts_with("text-decoration-")) => {
                Some("text-decoration".to_owned())
            }
            _ => None,
        }
    }

    /// Sets (or with "" unsets) `property` in `el`'s `style` attribute, removing the attribute
    /// when that leaves it empty.
    pub(super) fn ec_set_style(&self, cx: &mut Cx, el: NodeId, property: &str, value: &str) {
        self.set_inline_properties(cx, el, &[(property.to_owned(), value.to_owned())]);
        if self.dom().get_attribute(el, "style") == Some("") {
            self.remove_attr(cx, el, "style");
        }
    }

    /// The resolved value of `property` for an element.
    fn ec_resolved(&self, el: NodeId, property: &str) -> String {
        let connected = {
            let dom = self.dom();
            dom.is_alive(el) && dom.is_element(el) && dom.is_connected(el)
        };
        if !connected {
            return String::new();
        }
        self.computed_value(el, Pseudo::None, property)
    }

    fn ec_is_element(&self, n: NodeId) -> bool {
        let dom = self.dom();
        dom.is_alive(n) && dom.is_element(n)
    }

    fn ec_attr(&self, el: NodeId, name: &str) -> Option<String> {
        self.dom().get_attribute(el, name).map(str::to_owned)
    }

    // ----- definitions ------------------------------------------------------------------------------------

    /// "Effectively contained" in a range.
    pub(super) fn ec_effectively_contained(&self, n: NodeId, r: BoundaryPair) -> bool {
        if r.start == r.end {
            return false;
        }
        if self.ec_contained(n, r) {
            return true;
        }
        let (is_text, len, kids) = {
            let dom = self.dom();
            (
                dom.is_text(n),
                dom.length(n) as u32,
                dom.children(n).collect::<Vec<_>>(),
            )
        };
        if n == r.start.0 && is_text && len != r.start.1 {
            return true;
        }
        if n == r.end.0 && is_text && r.end.1 != 0 {
            return true;
        }
        if kids.is_empty() || !kids.iter().all(|&c| self.ec_effectively_contained(c, r)) {
            return false;
        }
        let descendant = |x: NodeId| x != n && self.dom().is_inclusive_ancestor_of(n, x);
        (!descendant(r.start.0) || !self.dom().is_text(r.start.0) || r.start.1 == 0)
            && (!descendant(r.end.0)
                || !self.dom().is_text(r.end.0)
                || r.end.1 == self.ec_len(r.end.0))
    }

    /// Every node effectively contained in `r` for which `cond` holds, in tree order.
    pub(super) fn ec_all_effectively_contained(
        &self,
        r: BoundaryPair,
        cond: impl Fn(&Engine, NodeId) -> bool,
    ) -> Vec<NodeId> {
        let mut node = r.start.0;
        while let Some(p) = self.ec_parent(node) {
            if self.ec_effectively_contained(p, r) {
                node = p;
            } else {
                break;
            }
        }
        let stop = self.dom().next_skipping_children(r.end.0, None);
        let mut out = Vec::new();
        let mut cur = Some(node);
        while let Some(n) = cur {
            if let Some(s) = stop {
                if !self.ec_before(n, s) {
                    break;
                }
            }
            if self.ec_effectively_contained(n, r) && cond(self, n) {
                out.push(n);
            }
            cur = self.dom().next_in_tree_order(n, None);
        }
        out
    }

    /// The first formattable node effectively contained in `r`.
    pub(super) fn ec_first_formattable(&self, r: BoundaryPair) -> Option<NodeId> {
        self.ec_all_effectively_contained(r, |e, n| e.ec_formattable(n))
            .into_iter()
            .next()
    }

    /// "A modifiable element".
    pub(super) fn ec_modifiable(&self, n: NodeId) -> bool {
        if !self.ec_html(n) {
            return false;
        }
        let dom = self.dom();
        let attrs = dom.attrs(n);
        let only = |allowed: &[&str]| {
            attrs
                .iter()
                .all(|a| a.namespace() == &ns!() && allowed.contains(&&**a.local_name()))
        };
        match &**dom.local_name(n) {
            "b" | "em" | "i" | "s" | "span" | "strike" | "strong" | "sub" | "sup" | "u" => {
                attrs.is_empty() || (attrs.len() == 1 && only(&["style"]))
            }
            "font" => only(&["style", "color", "face", "size"]),
            "a" => only(&["style", "href"]),
            _ => false,
        }
    }

    /// "A simple modifiable element".
    pub(super) fn ec_simple_modifiable(&self, n: NodeId) -> bool {
        const NAMES: &[&str] = &[
            "a", "b", "em", "font", "i", "s", "span", "strike", "strong", "sub", "sup", "u",
        ];
        if !self.ec_is(n, NAMES) {
            return false;
        }
        let (count, attr) = {
            let dom = self.dom();
            let attrs = dom.attrs(n);
            (
                attrs.len(),
                attrs
                    .first()
                    .filter(|a| a.namespace() == &ns!())
                    .map(|a| a.local_name().to_string()),
            )
        };
        if count == 0 {
            return true;
        }
        let Some(attr) = attr.filter(|_| count == 1) else {
            return false;
        };
        let name = self.ec_name(n).unwrap_or_default();
        match attr.as_str() {
            "href" => return name == "a",
            "color" | "face" | "size" => return name == "font",
            "style" => {}
            _ => return false,
        }
        if self.ec_style_longhands(n).is_empty() {
            return true;
        }
        let Some(prop) = self.ec_style_single_property(n) else {
            return false;
        };
        match name.as_str() {
            "b" | "strong" if prop == "font-weight" => return true,
            "i" | "em" if prop == "font-style" => return true,
            "a" | "font" | "span" if prop != "text-decoration" => return true,
            _ => {}
        }
        matches!(name.as_str(), "a" | "font" | "s" | "span" | "strike" | "u")
            && prop == "text-decoration"
            && matches!(
                self.ec_style_value(n, "text-decoration").as_str(),
                "line-through" | "underline" | "overline" | "none"
            )
    }

    /// "A formattable node": an editable visible Text node, `img` or `br`.
    pub(super) fn ec_formattable(&self, n: NodeId) -> bool {
        self.ec_editable(n)
            && self.ec_visible(n)
            && (self.dom().is_text(n) || self.ec_is(n, &["img", "br"]))
    }

    // ----- command values ---------------------------------------------------------------------------------

    /// "The effective command value" of a node for a command.
    pub(crate) fn ec_effective_value(&self, node: NodeId, cmd: &str) -> Val {
        let mut node = node;
        if !self.ec_is_element(node) {
            let p = self.ec_parent(node)?;
            if !self.ec_is_element(p) {
                return None;
            }
            node = p;
        }
        match cmd {
            "createlink" | "unlink" => {
                let mut cur = Some(node);
                while let Some(n) = cur {
                    if self.ec_is(n, &["a"]) && self.dom().has_attribute(n, "href") {
                        return self.ec_attr(n, "href");
                    }
                    cur = self.ec_parent(n);
                }
                None
            }
            "backcolor" | "hilitecolor" => loop {
                let v = self.ec_resolved(node, "background-color");
                let transparent = v.is_empty()
                    || v == "transparent"
                    || normalize_color(&v).is_some_and(|c| c.3 == 0);
                match self.ec_parent(node) {
                    Some(p) if transparent && self.ec_is_element(p) => node = p,
                    _ => return Some(v),
                }
            },
            "subscript" | "superscript" => {
                let (mut sub, mut sup) = (false, false);
                let mut cur = Some(node);
                while let Some(n) = cur.filter(|&n| self.ec_inline(n)) {
                    if self.ec_is(n, &["sub"]) {
                        sub = true;
                    } else if self.ec_is(n, &["sup"]) {
                        sup = true;
                    }
                    cur = self.ec_parent(n);
                }
                match (sub, sup) {
                    (true, true) => Some("mixed".to_owned()),
                    (true, false) => Some("subscript".to_owned()),
                    (false, true) => Some("superscript".to_owned()),
                    _ => None,
                }
            }
            "strikethrough" | "underline" => {
                let line = if cmd == "underline" {
                    "underline"
                } else {
                    "line-through"
                };
                let mut cur = Some(node);
                while let Some(n) = cur.filter(|&n| self.ec_is_element(n)) {
                    if self
                        .ec_resolved(n, "text-decoration-line")
                        .split_ascii_whitespace()
                        .any(|w| w == line)
                    {
                        return Some(line.to_owned());
                    }
                    cur = self.ec_parent(n);
                }
                None
            }
            _ => relevant_css_property(cmd).map(|p| self.ec_resolved(node, p)),
        }
    }

    /// "The specified command value" of an element for a command.
    pub(crate) fn ec_specified_value(&self, el: NodeId, cmd: &str) -> Val {
        if !self.ec_is_element(el) {
            return None;
        }
        if matches!(cmd, "backcolor" | "hilitecolor") && self.ec_display(el) != "inline" {
            return None;
        }
        match cmd {
            "createlink" | "unlink" => {
                return if self.ec_is(el, &["a"]) {
                    self.ec_attr(el, "href")
                } else {
                    None
                };
            }
            "subscript" | "superscript" => {
                return if self.ec_is(el, &["sup"]) {
                    Some("superscript".to_owned())
                } else if self.ec_is(el, &["sub"]) {
                    Some("subscript".to_owned())
                } else {
                    None
                };
            }
            "strikethrough" | "underline" => {
                let (line, names): (&str, &[&str]) = if cmd == "underline" {
                    ("underline", &["u"])
                } else {
                    ("line-through", &["s", "strike"])
                };
                let mut td = self.ec_style_value(el, "text-decoration");
                if td.is_empty() {
                    td = self.ec_style_value(el, "text-decoration-line");
                }
                if !td.is_empty() {
                    return td
                        .split_ascii_whitespace()
                        .any(|w| w == line)
                        .then(|| line.to_owned());
                }
                return self.ec_is(el, names).then(|| line.to_owned());
            }
            _ => {}
        }
        let property = relevant_css_property(cmd)?;
        let v = self.ec_style_value(el, property);
        if !v.is_empty() {
            return Some(v);
        }
        if self.ec_is(el, &["font"]) {
            match property {
                "color" => {
                    if let Some(c) = self.ec_attr(el, "color") {
                        return Some(c);
                    }
                }
                "font-family" => {
                    if let Some(f) = self.ec_attr(el, "face") {
                        return Some(f);
                    }
                }
                "font-size" => {
                    if let Some(n) = self
                        .ec_attr(el, "size")
                        .and_then(|s| parse_legacy_font_size(&s))
                    {
                        return Some(SIZE_KEYWORDS[n - 1].to_owned());
                    }
                }
                _ => {}
            }
        }
        match property {
            "font-weight" if self.ec_is(el, &["b", "strong"]) => Some("bold".to_owned()),
            "font-style" if self.ec_is(el, &["i", "em"]) => Some("italic".to_owned()),
            _ => None,
        }
    }

    // ----- the assorted algorithms ------------------------------------------------------------------------

    /// "Reorder modifiable descendants" of a node.
    fn ec_reorder_modifiable_descendants(
        &self,
        cx: &mut Cx,
        node: Option<NodeId>,
        cmd: &str,
        new: Option<&str>,
    ) {
        let Some(node) = node else { return };
        let mut candidate = node;
        loop {
            let kids = self.ec_children(candidate);
            if self.ec_modifiable(candidate)
                && kids.len() == 1
                && self.ec_modifiable(kids[0])
                && (!self.ec_simple_modifiable(candidate)
                    || !equivalent(cmd, self.ec_specified_value(candidate, cmd).as_deref(), new))
            {
                candidate = kids[0];
            } else {
                break;
            }
        }
        if candidate == node
            || !self.ec_simple_modifiable(candidate)
            || !equivalent(cmd, self.ec_specified_value(candidate, cmd).as_deref(), new)
            || !loosely_equivalent(cmd, self.ec_effective_value(candidate, cmd).as_deref(), new)
        {
            return;
        }
        while let Some(c) = self.ec_first_child(candidate) {
            let Some(p) = self.ec_parent(candidate) else {
                return;
            };
            let i = self.ec_index(candidate);
            self.ec_move(cx, c, p, i);
        }
        self.ec_insert_after(cx, node, candidate);
        let n = self.dom().child_count(candidate) as u32;
        self.ec_move(cx, node, candidate, n);
    }

    /// "Record the values" of a list of nodes.
    pub(super) fn ec_record_values(&self, nodes: &[NodeId]) -> Vec<(NodeId, &'static str, Val)> {
        let mut values = Vec::new();
        for &node in nodes {
            for &cmd in RECORDED {
                let mut ancestor = if self.ec_is_element(node) {
                    Some(node)
                } else {
                    self.ec_parent(node)
                };
                while let Some(a) = ancestor {
                    if self.ec_is_element(a) && self.ec_specified_value(a, cmd).is_none() {
                        ancestor = self.ec_parent(a);
                    } else {
                        break;
                    }
                }
                let v = ancestor
                    .filter(|&a| self.ec_is_element(a))
                    .and_then(|a| self.ec_specified_value(a, cmd));
                values.push((node, cmd, v));
            }
        }
        values
    }

    /// "Restore the values" recorded by [`Engine::ec_record_values`].
    pub(super) fn ec_restore_values(&self, cx: &mut Cx, values: &[(NodeId, &'static str, Val)]) {
        for (node, cmd, value) in values {
            let (node, cmd) = (*node, *cmd);
            if !self.dom().is_alive(node) {
                continue;
            }
            let mut ancestor = if self.ec_is_element(node) {
                Some(node)
            } else {
                self.ec_parent(node)
            };
            while let Some(a) = ancestor {
                if self.ec_is_element(a) && self.ec_specified_value(a, cmd).is_none() {
                    ancestor = self.ec_parent(a);
                } else {
                    break;
                }
            }
            let element = ancestor.filter(|&a| self.ec_is_element(a));
            match (value, element) {
                (None, Some(_)) => self.ec_push_down_values(cx, node, cmd, None),
                (v, Some(a))
                    if !equivalent(
                        cmd,
                        self.ec_specified_value(a, cmd).as_deref(),
                        v.as_deref(),
                    ) =>
                {
                    self.ec_force_value(cx, node, cmd, v.as_deref());
                }
                (Some(v), None) => self.ec_force_value(cx, node, cmd, Some(v)),
                _ => {}
            }
        }
    }

    /// "Clear the value" of an element; returns the nodes now in its place.
    fn ec_clear_value(&self, cx: &mut Cx, el: NodeId, cmd: &str) -> Vec<NodeId> {
        if !self.ec_editable(el) || self.ec_specified_value(el, cmd).is_none() {
            return Vec::new();
        }
        if self.ec_simple_modifiable(el) {
            let children = self.ec_children(el);
            for &c in &children {
                if let Some(p) = self.ec_parent(el) {
                    let i = self.ec_index(el);
                    self.ec_move(cx, c, p, i);
                }
            }
            self.ec_remove(cx, el);
            return children;
        }
        for (c, line) in [
            ("strikethrough", "line-through"),
            ("underline", "underline"),
        ] {
            if cmd != c {
                continue;
            }
            let td = self.ec_style_value(el, "text-decoration");
            if td.contains(line) {
                let v = if td == line {
                    String::new()
                } else {
                    td.replacen(line, "", 1)
                };
                self.ec_set_style(cx, el, "text-decoration", &v);
            }
        }
        if let Some(p) = relevant_css_property(cmd) {
            if !self.ec_style_value(el, p).is_empty() {
                self.ec_set_style(cx, el, p, "");
            }
        }
        if self.ec_is(el, &["font"]) {
            match cmd {
                "forecolor" => self.ec_remove_attribute(cx, el, "color"),
                "fontname" => self.ec_remove_attribute(cx, el, "face"),
                "fontsize" => self.ec_remove_attribute(cx, el, "size"),
                _ => {}
            }
        }
        if self.ec_is(el, &["a"]) && matches!(cmd, "createlink" | "unlink") {
            self.ec_remove_attribute(cx, el, "href");
        }
        if self.ec_specified_value(el, cmd).is_none() {
            return Vec::new();
        }
        vec![self.ec_set_tag_name(cx, el, "span")]
    }

    /// "Push down values" on a node.
    fn ec_push_down_values(&self, cx: &mut Cx, node: NodeId, cmd: &str, new: Option<&str>) {
        let Some(parent) = self.ec_parent(node).filter(|&p| self.ec_is_element(p)) else {
            return;
        };
        if loosely_equivalent(cmd, self.ec_effective_value(node, cmd).as_deref(), new) {
            return;
        }
        let mut current = Some(parent);
        let mut ancestors = Vec::new();
        while let Some(a) = current {
            if self.ec_editable(a)
                && self.ec_is_element(a)
                && !loosely_equivalent(cmd, self.ec_effective_value(a, cmd).as_deref(), new)
            {
                ancestors.push(a);
                current = self.ec_parent(a);
            } else {
                break;
            }
        }
        let Some(&last) = ancestors.last() else {
            return;
        };
        let mut propagated = self.ec_specified_value(last, cmd);
        if propagated.is_none() && new.is_some() {
            return;
        }
        if new.is_some()
            && !self.ec_parent(last).is_some_and(|p| {
                loosely_equivalent(cmd, self.ec_effective_value(p, cmd).as_deref(), new)
            })
        {
            return;
        }
        while let Some(current) = ancestors.pop() {
            let specified = self.ec_specified_value(current, cmd);
            if specified.is_some() {
                propagated = specified.clone();
            }
            let children = self.ec_children(current);
            if specified.is_some() {
                self.ec_clear_value(cx, current, cmd);
            }
            for child in children {
                if child == node {
                    continue;
                }
                if self.ec_is_element(child) {
                    let s = self.ec_specified_value(child, cmd);
                    if s.is_some() && !equivalent(cmd, propagated.as_deref(), s.as_deref()) {
                        continue;
                    }
                }
                if ancestors.last() == Some(&child) {
                    continue;
                }
                self.ec_force_value(cx, child, cmd, propagated.as_deref());
            }
        }
    }

    /// The children of `node` that are not elements whose specified command value is neither null
    /// nor equivalent to `new`.
    fn ec_children_to_force(&self, node: NodeId, cmd: &str, new: Option<&str>) -> Vec<NodeId> {
        self.ec_children(node)
            .into_iter()
            .filter(|&c| {
                !self.ec_is_element(c) || {
                    let s = self.ec_specified_value(c, cmd);
                    s.is_none() || equivalent(cmd, new, s.as_deref())
                }
            })
            .collect()
    }

    /// "Force the value" of a node.
    fn ec_force_value(&self, cx: &mut Cx, node: NodeId, cmd: &str, new: Option<&str>) {
        if !self.dom().is_alive(node) || self.ec_parent(node).is_none() {
            return;
        }
        let Some(new) = new else {
            return;
        };
        let span_child =
            |e: &Engine, n: NodeId| e.ec_allowed_child(Item::Node(n), Item::Name("span"));
        let doc = self.dom().node_document(node);
        let css = self.doc_state(doc, |s| s.css_styling);
        if span_child(self, node) {
            let prev = self.dom().previous_sibling(node);
            self.ec_reorder_modifiable_descendants(cx, prev, cmd, Some(new));
            let next = self.dom().next_sibling(node);
            self.ec_reorder_modifiable_descendants(cx, next, cmd, Some(new));
            // (as browsers do, the text joins a `b` or `i` beside it but not a `strong` or `em`)
            let criteria = |e: &Engine, n: NodeId| {
                !e.ec_is(n, &["strong", "em"])
                    && e.ec_simple_modifiable(n)
                    && equivalent(cmd, e.ec_specified_value(n, cmd).as_deref(), Some(new))
                    && loosely_equivalent(cmd, e.ec_effective_value(n, cmd).as_deref(), Some(new))
            };
            self.ec_wrap(cx, vec![node], &criteria, &|_, _| None);
        }
        if self.ec_invisible(node) {
            return;
        }
        if loosely_equivalent(
            cmd,
            self.ec_effective_value(node, cmd).as_deref(),
            Some(new),
        ) {
            return;
        }
        if !span_child(self, node) {
            for c in self.ec_children_to_force(node, cmd, Some(new)) {
                self.ec_force_value(cx, c, cmd, Some(new));
            }
            return;
        }
        let Some(parent) = self.ec_parent(node) else {
            return;
        };
        let mut new_parent = None;
        if !css {
            new_parent = match cmd {
                "bold" if new == "bold" || new == "700" => Some(self.ec_create(doc, "b")),
                "italic" if new == "italic" => Some(self.ec_create(doc, "i")),
                "strikethrough" if new == "line-through" => Some(self.ec_create(doc, "s")),
                "underline" if new == "underline" => Some(self.ec_create(doc, "u")),
                "forecolor" => simple_color(new).map(|c| {
                    let f = self.ec_create(doc, "font");
                    self.set_attr(cx, f, "color", &c);
                    f
                }),
                "fontname" => {
                    let f = self.ec_create(doc, "font");
                    self.set_attr(cx, f, "face", new);
                    Some(f)
                }
                _ => None,
            };
        }
        if matches!(cmd, "createlink" | "unlink") {
            let a = self.ec_create(doc, "a");
            self.set_attr(cx, a, "href", new);
            new_parent = Some(a);
            let mut ancestor = Some(parent);
            while let Some(an) = ancestor {
                let an = if self.ec_is(an, &["a"]) {
                    self.ec_set_tag_name(cx, an, "span")
                } else {
                    an
                };
                ancestor = self.ec_parent(an);
            }
        }
        if cmd == "fontsize" && (!css || new == "xxx-large") {
            if let Some(n) = css_size_to_legacy(new) {
                let f = self.ec_create(doc, "font");
                self.set_attr(cx, f, "size", &n.to_string());
                new_parent = Some(f);
            }
        }
        if matches!(cmd, "subscript" | "superscript") {
            if new == "subscript" {
                new_parent = Some(self.ec_create(doc, "sub"));
            } else if new == "superscript" {
                new_parent = Some(self.ec_create(doc, "sup"));
            }
        }
        let new_parent = new_parent.unwrap_or_else(|| self.ec_create(doc, "span"));
        // (the parent may have changed when an ancestor `a` became a `span`)
        let Some(parent) = self.ec_parent(node) else {
            return;
        };
        self.ec_insert(cx, parent, new_parent, Some(node));
        if let Some(p) = relevant_css_property(cmd) {
            if !loosely_equivalent(
                cmd,
                self.ec_effective_value(new_parent, cmd).as_deref(),
                Some(new),
            ) {
                self.ec_set_style(cx, new_parent, p, new);
            }
        }
        for (c, line) in [
            ("strikethrough", "line-through"),
            ("underline", "underline"),
        ] {
            if cmd == c
                && new == line
                && self.ec_effective_value(new_parent, c).as_deref() != Some(line)
            {
                self.ec_set_style(cx, new_parent, "text-decoration", line);
            }
        }
        let n = self.dom().child_count(new_parent) as u32;
        self.ec_move(cx, node, new_parent, n);
        if self.ec_is_element(node)
            && !equivalent(
                cmd,
                self.ec_effective_value(node, cmd).as_deref(),
                Some(new),
            )
        {
            if let Some(gp) = self.ec_parent(new_parent) {
                let i = self.ec_index(new_parent);
                self.ec_move(cx, node, gp, i);
            }
            self.ec_remove(cx, new_parent);
            for c in self.ec_children_to_force(node, cmd, Some(new)) {
                self.ec_force_value(cx, c, cmd, Some(new));
            }
        }
    }

    /// Sets the active range's start (its end and the selection's direction stay).
    fn ec_set_active(&self, doc: NodeId, r: BoundaryPair) {
        if let Some(h) = self.ec_range_handle(doc) {
            self.set_range(h, r);
        }
    }

    /// Splits the active range's start and end Text nodes (when editable and inside them) so the
    /// range's ends are node boundaries.
    fn ec_split_active_ends(&self, cx: &mut Cx, doc: NodeId) {
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let (s, so) = r.start;
        if self.ec_editable(s) && self.dom().is_text(s) && so != 0 && so != self.ec_len(s) {
            let new = self.dom_mut().split_text(s, so as usize);
            self.after_mutation(cx);
            if let (Ok(new), Some(r)) = (new, self.ec_active(doc)) {
                self.ec_set_active(
                    doc,
                    BoundaryPair {
                        start: (new, 0),
                        end: r.end,
                    },
                );
            }
        }
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let (e, eo) = r.end;
        if self.ec_editable(e) && self.dom().is_text(e) && eo != 0 && eo != self.ec_len(e) {
            let _ = self.dom_mut().split_text(e, eo as usize);
            self.after_mutation(cx);
            self.ec_set_active(doc, r);
        }
    }

    /// "Set the selection's value" of a command to `new`.
    fn ec_set_selection_value(&self, cx: &mut Cx, doc: NodeId, cmd: &str, new: Option<&str>) {
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        if self.ec_first_formattable(r).is_none() {
            if let Some(vals) = activated_values(cmd) {
                self.ec_set_state_override(doc, cmd, new.is_some_and(|v| vals.contains(&v)));
            }
            if cmd == "subscript" {
                self.ec_unset_state_override(doc, "superscript");
            }
            if cmd == "superscript" {
                self.ec_unset_state_override(doc, "subscript");
            }
            match new {
                None => self.ec_unset_value_override(doc, cmd),
                Some(v) if cmd == "createlink" || has_value(cmd) => {
                    self.ec_set_value_override(doc, cmd, v)
                }
                _ => {}
            }
            return;
        }
        self.ec_split_active_ends(cx, doc);
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let elements =
            self.ec_all_effectively_contained(r, |e, n| e.ec_editable(n) && e.ec_is_element(n));
        for el in elements {
            if self.dom().is_alive(el) && self.ec_parent(el).is_some() {
                self.ec_clear_value(cx, el, cmd);
            }
        }
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let nodes = self.ec_all_effectively_contained(r, |e, n| e.ec_editable(n));
        for n in nodes {
            if !self.dom().is_alive(n) || self.ec_parent(n).is_none() {
                continue;
            }
            self.ec_push_down_values(cx, n, cmd, new);
            // (an element that holds the whole range keeps its place: the contents the range
            // selects are styled inside it, as browsers do; the spec wraps the element. A link
            // still goes around it.)
            let holds_range = cmd != "createlink" && self.ec_is_element(n) && {
                let dom = self.dom();
                dom.is_inclusive_ancestor_of(n, r.start.0)
                    && dom.is_inclusive_ancestor_of(n, r.end.0)
            };
            if !holds_range && self.ec_allowed_child(Item::Node(n), Item::Name("span")) {
                self.ec_force_value(cx, n, cmd, new);
            }
        }
    }

    // ----- overrides --------------------------------------------------------------------------------------

    /// The document's overrides, unset first when the active range is not the one they were set
    /// at.
    fn ec_overrides<R>(&self, doc: NodeId, f: impl FnOnce(&mut Overrides) -> R) -> R {
        let active = self.ec_active(doc);
        self.doc_state(doc, |s| {
            let o = &mut s.overrides;
            let same = match (o.range, active) {
                (Some(a), Some(b)) => a.start == b.start && a.end == b.end,
                (None, None) => true,
                _ => false,
            };
            if !same {
                o.state.clear();
                o.value.clear();
                o.range = active;
            }
            f(o)
        })
    }

    pub(crate) fn ec_state_override(&self, doc: NodeId, cmd: &str) -> Option<bool> {
        self.ec_overrides(doc, |o| {
            o.state.iter().find(|(c, _)| *c == cmd).map(|(_, v)| *v)
        })
    }

    fn ec_set_state_override(&self, doc: NodeId, cmd: &str, v: bool) {
        let cmd = static_name(cmd);
        self.ec_overrides(doc, |o| {
            o.state.retain(|(c, _)| *c != cmd);
            o.state.push((cmd, v));
        });
    }

    fn ec_unset_state_override(&self, doc: NodeId, cmd: &str) {
        self.ec_overrides(doc, |o| o.state.retain(|(c, _)| *c != cmd));
    }

    pub(crate) fn ec_value_override(&self, doc: NodeId, cmd: &str) -> Option<String> {
        self.ec_overrides(doc, |o| {
            o.value
                .iter()
                .find(|(c, _)| *c == cmd)
                .map(|(_, v)| v.clone())
        })
    }

    /// Sets a value override (`backColor`'s and `hiliteColor`'s are one).
    fn ec_set_value_override(&self, doc: NodeId, cmd: &str, v: &str) {
        let names: &[&'static str] = match cmd {
            "backcolor" | "hilitecolor" => &["backcolor", "hilitecolor"],
            _ => &[static_name(cmd)],
        };
        self.ec_overrides(doc, |o| {
            for &c in names {
                o.value.retain(|(n, _)| *n != c);
                o.value.push((c, v.to_owned()));
            }
        });
    }

    fn ec_unset_value_override(&self, doc: NodeId, cmd: &str) {
        let names: &[&str] = match cmd {
            "backcolor" | "hilitecolor" => &["backcolor", "hilitecolor"],
            _ => &[cmd],
        };
        self.ec_overrides(doc, |o| o.value.retain(|(n, _)| !names.contains(n)));
    }

    /// "Record current overrides".
    pub(crate) fn ec_record_current_overrides(&self, doc: NodeId) -> Vec<(&'static str, Recorded)> {
        let mut out = Vec::new();
        if let Some(v) = self.ec_value_override(doc, "createlink") {
            out.push(("createlink", Recorded::Value(Some(v))));
        }
        for c in [
            "bold",
            "italic",
            "strikethrough",
            "subscript",
            "superscript",
            "underline",
        ] {
            if let Some(s) = self.ec_state_override(doc, c) {
                out.push((c, Recorded::State(s)));
            }
        }
        for c in ["fontname", "fontsize", "forecolor", "hilitecolor"] {
            if let Some(v) = self.ec_value_override(doc, c) {
                out.push((c, Recorded::Value(Some(v))));
            }
        }
        out
    }

    /// "Record current states and values", of `at` when given (else of the first formattable node
    /// effectively contained in the active range).
    pub(crate) fn ec_record_current_states_and_values(
        &self,
        doc: NodeId,
        at: Option<NodeId>,
    ) -> Vec<(&'static str, Recorded)> {
        let mut out = Vec::new();
        let Some(node) = at.or_else(|| {
            self.ec_active(doc)
                .and_then(|r| self.ec_first_formattable(r))
        }) else {
            return out;
        };
        out.push((
            "createlink",
            Recorded::Value(self.ec_effective_value(node, "createlink")),
        ));
        for c in [
            "bold",
            "italic",
            "strikethrough",
            "subscript",
            "superscript",
            "underline",
        ] {
            let on = self
                .ec_effective_value(node, c)
                .is_some_and(|v| activated_values(c).unwrap_or(&[]).contains(&v.as_str()));
            out.push((c, Recorded::State(on)));
        }
        for c in ["fontname", "forecolor", "hilitecolor"] {
            let v = match at {
                Some(n) => self.ec_effective_value(n, c).unwrap_or_default(),
                None => self.ec_inline_value(doc, c),
            };
            out.push((c, Recorded::Value(Some(v))));
        }
        out.push((
            "fontsize",
            Recorded::Value(self.ec_effective_value(node, "fontsize")),
        ));
        out
    }

    /// "Restore states and values".
    pub(crate) fn ec_restore_states_and_values(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        overrides: &[(&'static str, Recorded)],
    ) {
        let first = |e: &Engine| e.ec_active(doc).and_then(|r| e.ec_first_formattable(r));
        let Some(mut node) = first(self) else {
            for (cmd, o) in overrides {
                match o {
                    Recorded::State(s) => self.ec_set_state_override(doc, cmd, *s),
                    Recorded::Value(Some(v)) => self.ec_set_value_override(doc, cmd, v),
                    Recorded::Value(None) => {}
                }
            }
            return;
        };
        for (cmd, o) in overrides {
            let cmd = *cmd;
            match o {
                Recorded::State(s) => {
                    if self.ec_state(doc, cmd) == *s {
                        continue;
                    }
                    self.ec_inline_action(cx, doc, cmd, "");
                }
                Recorded::Value(None) => continue,
                Recorded::Value(Some(v)) => match cmd {
                    "createlink" => {
                        let differs = match self.ec_value_override(doc, cmd) {
                            Some(o) => o != *v,
                            None => self.ec_effective_value(node, cmd).as_deref() != Some(v),
                        };
                        if !differs {
                            continue;
                        }
                        self.ec_inline_action(cx, doc, cmd, v);
                    }
                    "fontsize" => {
                        let differs = match self.ec_value_override(doc, cmd) {
                            Some(o) => o != *v,
                            None => !loosely_equivalent(
                                cmd,
                                self.ec_effective_value(node, cmd).as_deref(),
                                Some(v),
                            ),
                        };
                        if !differs {
                            continue;
                        }
                        let size = legacy_font_size(v).unwrap_or_default();
                        self.ec_inline_action(cx, doc, cmd, &size);
                    }
                    _ => {
                        let current = self.ec_value(doc, cmd);
                        if equivalent(cmd, Some(&current), Some(v)) {
                            continue;
                        }
                        self.ec_inline_action(cx, doc, cmd, v);
                    }
                },
            }
            node = first(self).unwrap_or(node);
        }
    }

    // ----- states, values, indeterminacy ------------------------------------------------------------------

    /// Whether `cmd` (lowercased) is an inline formatting command.
    pub(crate) fn ec_is_inline_command(cmd: &str) -> bool {
        INLINE.contains(&cmd)
    }

    /// `queryCommandState` of an inline formatting command: its state override, else its state.
    pub(crate) fn ec_state(&self, doc: NodeId, cmd: &str) -> bool {
        if let Some(s) = self.ec_state_override(doc, cmd) {
            return s;
        }
        let Some(vals) = activated_values(cmd) else {
            return false;
        };
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        let nodes = self.ec_all_effectively_contained(r, |e, n| e.ec_formattable(n));
        let on = |n: NodeId| {
            self.ec_effective_value(n, cmd)
                .is_some_and(|v| vals.contains(&v.as_str()))
        };
        if nodes.is_empty() {
            on(r.start.0)
        } else {
            nodes.into_iter().all(on)
        }
    }

    /// A command's value without its override ("" for one without a value).
    fn ec_inline_value(&self, doc: NodeId, cmd: &str) -> String {
        let Some(r) = self.ec_active(doc) else {
            return String::new();
        };
        if !has_value(cmd) {
            return String::new();
        }
        let node = self.ec_first_formattable(r).unwrap_or(r.start.0);
        let v = self.ec_effective_value(node, cmd);
        if cmd == "fontsize" {
            return v.and_then(|v| legacy_font_size(&v)).unwrap_or_default();
        }
        v.unwrap_or_default()
    }

    /// `queryCommandValue` of an inline formatting command.
    pub(crate) fn ec_value(&self, doc: NodeId, cmd: &str) -> String {
        if !has_value(cmd) {
            return String::new();
        }
        if let Some(o) = self.ec_value_override(doc, cmd) {
            if cmd == "fontsize" {
                return legacy_font_size(&o).unwrap_or_default();
            }
            return o;
        }
        self.ec_inline_value(doc, cmd)
    }

    /// `queryCommandIndeterm` of an inline formatting command.
    pub(crate) fn ec_inline_indeterm(&self, doc: NodeId, cmd: &str) -> bool {
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        let values = || -> Vec<Val> {
            self.ec_all_effectively_contained(r, |e, n| e.ec_formattable(n))
                .into_iter()
                .map(|n| self.ec_effective_value(n, cmd))
                .collect()
        };
        match cmd {
            "fontsize" => {
                let v = values();
                v.iter().skip(1).any(|x| *x != v[0])
            }
            "subscript" | "superscript" => {
                let v = values();
                (v.iter().any(|x| x.as_deref() == Some(cmd))
                    && v.iter().any(|x| x.as_deref() != Some(cmd)))
                    || v.iter().any(|x| x.as_deref() == Some("mixed"))
            }
            _ if standard_inline_value(cmd) => {
                let v = values();
                v.windows(2).any(|w| w[0] != w[1])
            }
            _ => match activated_values(cmd) {
                Some(vals) => {
                    let v = values();
                    let on = v
                        .iter()
                        .filter(|x| x.as_deref().is_some_and(|x| vals.contains(&x)))
                        .count();
                    on >= 1 && v.len() - on >= 1
                }
                None => false,
            },
        }
    }

    // ----- the commands -----------------------------------------------------------------------------------

    /// A color command's value as a valid CSS color (a hex color without its `#` gets one), or
    /// `None` when it is not one or is `currentColor`.
    fn ec_color_value(value: &str) -> Option<String> {
        let hex =
            |s: &str| (s.len() == 3 || s.len() == 6) && s.bytes().all(|c| c.is_ascii_hexdigit());
        let v = if hex(value) {
            format!("#{value}")
        } else {
            value.to_owned()
        };
        normalize_color(&v).map(|_| v)
    }

    /// The action of an inline formatting command (`cmd` lowercased).
    pub(crate) fn ec_inline_action(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        cmd: &str,
        value: &str,
    ) -> bool {
        match cmd {
            "backcolor" | "forecolor" | "hilitecolor" => {
                let Some(v) = Self::ec_color_value(value) else {
                    return false;
                };
                self.ec_set_selection_value(cx, doc, cmd, Some(&v));
            }
            "bold" | "italic" => {
                let on = if cmd == "bold" { "bold" } else { "italic" };
                let v = if self.ec_state(doc, cmd) {
                    "normal"
                } else {
                    on
                };
                self.ec_set_selection_value(cx, doc, cmd, Some(v));
            }
            "strikethrough" | "underline" => {
                let line = if cmd == "underline" {
                    "underline"
                } else {
                    "line-through"
                };
                let v = (!self.ec_state(doc, cmd)).then_some(line);
                self.ec_set_selection_value(cx, doc, cmd, v);
            }
            "subscript" | "superscript" => {
                let state = self.ec_state(doc, cmd);
                self.ec_set_selection_value(cx, doc, cmd, None);
                if !state {
                    self.ec_set_selection_value(cx, doc, cmd, Some(cmd));
                }
            }
            "createlink" => {
                if value.is_empty() {
                    return false;
                }
                let Some(r) = self.ec_active(doc) else {
                    return false;
                };
                let mut links = Vec::new();
                for n in self.ec_all_effectively_contained(r, |_, _| true) {
                    let dom = self.dom();
                    for a in dom.ancestors(n) {
                        if !links.contains(&a) {
                            links.push(a);
                        }
                    }
                }
                for a in links {
                    if self.ec_editable(a)
                        && self.ec_is(a, &["a"])
                        && self.dom().has_attribute(a, "href")
                    {
                        self.set_attr(cx, a, "href", value);
                    }
                }
                self.ec_set_selection_value(cx, doc, cmd, Some(value));
            }
            "fontname" => self.ec_set_selection_value(cx, doc, cmd, Some(value)),
            "fontsize" => {
                let Some(v) = normalize_font_size(value) else {
                    return false;
                };
                self.ec_set_selection_value(cx, doc, cmd, Some(v));
            }
            "removeformat" => self.ec_remove_format(cx, doc),
            "unlink" => self.ec_unlink(cx, doc),
            _ => return false,
        }
        true
    }

    /// "A removeFormat candidate".
    fn ec_remove_format_candidate(&self, n: NodeId) -> bool {
        self.ec_editable(n)
            && self.ec_is(
                n,
                &[
                    "abbr", "acronym", "b", "bdi", "bdo", "big", "blink", "cite", "code", "dfn",
                    "em", "font", "i", "ins", "kbd", "mark", "nobr", "q", "s", "samp", "small",
                    "span", "strike", "strong", "sub", "sup", "tt", "u", "var",
                ],
            )
    }

    /// The `removeFormat` command's action.
    fn ec_remove_format(&self, cx: &mut Cx, doc: NodeId) {
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let elements = self.ec_all_effectively_contained(r, |e, n| e.ec_remove_format_candidate(n));
        for el in elements {
            while let Some(c) = self.ec_first_child(el) {
                let Some(p) = self.ec_parent(el) else { break };
                let i = self.ec_index(el);
                self.ec_move(cx, c, p, i);
            }
            self.ec_remove(cx, el);
        }
        self.ec_split_active_ends(cx, doc);
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        for n in self.ec_all_effectively_contained(r, |e, n| e.ec_editable(n)) {
            while let Some(p) = self.ec_parent(n) {
                if !(self.ec_remove_format_candidate(p) && self.ec_same_host(p, Some(n))) {
                    break;
                }
                self.ec_split_parent(cx, &[n]);
                // (a split that leaves the node where it was would not end)
                if self.ec_parent(n) == Some(p) {
                    break;
                }
            }
        }
        for cmd in RECORDED {
            self.ec_set_selection_value(cx, doc, cmd, None);
        }
    }

    /// The `unlink` command's action.
    fn ec_unlink(&self, cx: &mut Cx, doc: NodeId) {
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let link = |e: &Engine, n: NodeId| e.ec_is(n, &["a"]) && e.dom().has_attribute(n, "href");
        let mut hyperlinks: Vec<NodeId> = {
            let dom = self.dom();
            dom.inclusive_ancestors(r.start.0)
                .filter(|&n| link(self, n))
                .collect()
        };
        hyperlinks.reverse();
        let stop = self.dom().next_skipping_children(r.end.0, None);
        let mut cur = Some(r.start.0);
        while let Some(n) = cur {
            if Some(n) == stop {
                break;
            }
            if link(self, n)
                && (self.ec_contained(n, r) || self.dom().is_inclusive_ancestor_of(n, r.end.0))
                && !hyperlinks.contains(&n)
            {
                hyperlinks.push(n);
            }
            cur = self.dom().next_in_tree_order(n, None);
        }
        for a in hyperlinks {
            self.ec_clear_value(cx, a, "unlink");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_sizes_normalize_to_keywords() {
        assert_eq!(normalize_font_size("3"), Some("medium"));
        assert_eq!(normalize_font_size(" +1 "), Some("large"));
        assert_eq!(normalize_font_size("-2"), Some("x-small"));
        assert_eq!(normalize_font_size("9"), Some("xxx-large"));
        assert_eq!(normalize_font_size("2.5"), Some("small"));
        assert_eq!(normalize_font_size("1e1"), Some("x-small"));
        assert_eq!(normalize_font_size("abc"), None);
        assert_eq!(normalize_font_size("2px"), None);
    }

    #[test]
    fn legacy_sizes_of_lengths() {
        assert_eq!(legacy_font_size("16px").as_deref(), Some("3"));
        assert_eq!(legacy_font_size("13px").as_deref(), Some("2"));
        assert_eq!(legacy_font_size("48px").as_deref(), Some("7"));
        assert_eq!(legacy_font_size("xx-large").as_deref(), Some("6"));
        assert_eq!(legacy_font_size("2em"), None);
    }

    #[test]
    fn equivalent_values_of_colors_and_weights() {
        assert!(equivalent("bold", Some("bold"), Some("700")));
        assert!(!equivalent("bold", Some("bold"), None));
        assert!(equivalent("forecolor", Some("red"), Some("rgb(255, 0, 0)")));
        assert!(loosely_equivalent("fontsize", Some("18px"), Some("large")));
        assert!(!loosely_equivalent("fontsize", Some("17px"), Some("large")));
    }
}
