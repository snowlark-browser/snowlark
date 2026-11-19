// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generated content (CSS Generated Content 3, CSS Lists 3 §4): resolving a `content` value's
//! counters and quotes for one pseudo-element, and the counter styles of CSS Counter Styles 3 §6–7 that
//! `counter()` formats with.
//!
//! Counters and quote depth depend on document order, so they come from one walk over the document
//! ([`CounterWalk`]) that records the resolved `content` of every `::before`, `::after` and `::marker`
//! that needs it. Content made of strings and images only needs no walk ([`resolve_simple`]).

use std::collections::HashMap;
use std::sync::Arc;

use servo_arc::Arc as ServoArc;
use snowlark_dom::{Dom, NodeData, NodeId};
use snowlark_layout::style::{self as ls, PseudoElement};
use style::counter_style::CounterStyle;
use style::properties::ComputedValues;
use style::values::computed as c;
use style::values::generics::counters::{GenericContent, GenericContentItem};

use crate::adapter::RenderStyle;

type Item = GenericContentItem<c::Image>;

/// Whether resolving `items` needs the counter walk.
pub(crate) fn needs_walk(values: &ComputedValues) -> bool {
    match &values.get_counters().content {
        GenericContent::Items(items) => items.items[..items.alt_start]
            .iter()
            .any(|i| !matches!(i, Item::String(_) | Item::Image(_))),
        _ => false,
    }
}

fn image_item(style: &RenderStyle, image: &c::Image) -> Option<ls::ContentItem> {
    match style_image(style, image) {
        ls::Image::None => None,
        i => Some(ls::ContentItem::Image(i)),
    }
}

fn style_image(style: &RenderStyle, image: &c::Image) -> ls::Image {
    // (url(), image-set(), gradients...: what a background layer would show)
    style.image(image)
}

/// The content of strings and images; counters and quotes produce nothing.
pub(crate) fn resolve_simple(style: &RenderStyle, items: &[Item]) -> ls::Content {
    let mut out = Vec::new();
    for item in items {
        match item {
            Item::String(s) => push_text(&mut out, s),
            Item::Image(i) => out.extend(image_item(style, i)),
            _ => {}
        }
    }
    ls::Content::Items(out)
}

fn push_text(out: &mut Vec<ls::ContentItem>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(ls::ContentItem::Text(prev)) = out.last_mut() {
        let mut joined = String::with_capacity(prev.len() + s.len());
        joined.push_str(prev);
        joined.push_str(s);
        *prev = Arc::from(joined);
        return;
    }
    out.push(ls::ContentItem::Text(Arc::from(s)));
}

/// A counter's representation in a counter style (CSS Counter Styles 3 §6), without the marker
/// suffix.
pub fn format_counter(value: i64, style: &CounterStyle) -> String {
    match style {
        CounterStyle::None => String::new(),
        CounterStyle::String(s) => s.to_string(),
        CounterStyle::Symbols { .. } => value.to_string(),
        CounterStyle::Name(name) => format_named(value, &name.0),
    }
}

fn format_named(value: i64, name: &str) -> String {
    match name {
        "disc" => "\u{2022}".into(),
        "circle" => "\u{25E6}".into(),
        "square" => "\u{25AA}".into(),
        "disclosure-open" => "\u{25BE}".into(),
        "disclosure-closed" => "\u{25B8}".into(),
        "decimal-leading-zero" => {
            if (0..10).contains(&value) {
                format!("0{value}")
            } else if (-9..0).contains(&value) {
                format!("-0{}", -value)
            } else {
                value.to_string()
            }
        }
        "lower-alpha" | "lower-latin" => alphabetic(value, b'a', 26),
        "upper-alpha" | "upper-latin" => alphabetic(value, b'A', 26),
        "lower-greek" => {
            // §6.4 alphabetic over α..ω without final sigma.
            const GREEK: [char; 24] = [
                'α', 'β', 'γ', 'δ', 'ε', 'ζ', 'η', 'θ', 'ι', 'κ', 'λ', 'μ', 'ν', 'ξ', 'ο', 'π',
                'ρ', 'σ', 'τ', 'υ', 'φ', 'χ', 'ψ', 'ω',
            ];
            alphabetic_chars(value, &GREEK)
        }
        "lower-roman" => roman(value).map_or_else(|| value.to_string(), |r| r.to_lowercase()),
        "upper-roman" => roman(value).unwrap_or_else(|| value.to_string()),
        _ => value.to_string(),
    }
}

/// §6.4 alphabetic: bijective base-n; values below 1 fall back to decimal.
fn alphabetic(value: i64, first: u8, n: u8) -> String {
    let chars: Vec<char> = (0..n).map(|i| (first + i) as char).collect();
    alphabetic_chars(value, &chars)
}

fn alphabetic_chars(value: i64, chars: &[char]) -> String {
    if value < 1 {
        return value.to_string();
    }
    let n = chars.len() as i64;
    let mut v = value;
    let mut out = Vec::new();
    while v > 0 {
        v -= 1;
        out.push(chars[(v % n) as usize]);
        v /= n;
    }
    out.iter().rev().collect()
}

/// §7.1 roman (additive), for 1..=3999.
fn roman(value: i64) -> Option<String> {
    if !(1..=3999).contains(&value) {
        return None;
    }
    const TABLE: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut v = value;
    let mut out = String::new();
    for (n, s) in TABLE {
        while v >= n {
            out.push_str(s);
            v -= n;
        }
    }
    Some(out)
}

/// One counter instance: its value and the node whose children are its scope (the parent of the
/// element that created it; for a pseudo-element, its originating element).
struct Instance {
    value: i64,
    scope: NodeId,
}

/// Which pseudo-element a recorded content belongs to.
pub(crate) fn pseudo_key(p: PseudoElement) -> u8 {
    match p {
        PseudoElement::Before => 0,
        PseudoElement::After => 1,
        PseudoElement::Marker => 2,
        PseudoElement::FirstLine => 3,
        PseudoElement::FirstLetter => 4,
        PseudoElement::Backdrop => 5,
        PseudoElement::DetailsContent => 6,
        PseudoElement::DetailsSummary => 7,
    }
}

/// The document-order walk that resolves counters (CSS Lists 3 §4) and quote depth (CSS Generated
/// Content 3 §1.3) for generated content.
pub(crate) struct CounterWalk<'a> {
    dom: &'a Dom,
    /// The primary style of an element (`None`: not rendered, so it takes no part in counters).
    element_style: &'a dyn Fn(NodeId) -> Option<ServoArc<ComputedValues>>,
    /// A pseudo-element's style, when it exists.
    pseudo_style: &'a dyn Fn(NodeId, PseudoElement) -> Option<ServoArc<ComputedValues>>,
    make_style: &'a dyn Fn(ServoArc<ComputedValues>) -> RenderStyle,
    counters: HashMap<Arc<str>, Vec<Instance>>,
    /// Counter names in creation order with their scope, to drop them when a scope closes.
    created: Vec<(NodeId, Arc<str>)>,
    quote_depth: i32,
    pub(crate) out: HashMap<(NodeId, u8), Arc<ls::Content>>,
}

impl<'a> CounterWalk<'a> {
    pub(crate) fn new(
        dom: &'a Dom,
        element_style: &'a dyn Fn(NodeId) -> Option<ServoArc<ComputedValues>>,
        pseudo_style: &'a dyn Fn(NodeId, PseudoElement) -> Option<ServoArc<ComputedValues>>,
        make_style: &'a dyn Fn(ServoArc<ComputedValues>) -> RenderStyle,
    ) -> Self {
        CounterWalk {
            dom,
            element_style,
            pseudo_style,
            make_style,
            counters: HashMap::new(),
            created: Vec::new(),
            quote_depth: 0,
            out: HashMap::new(),
        }
    }

    pub(crate) fn run(&mut self, root: NodeId) {
        self.element(root, None);
    }

    fn instantiate(&mut self, name: &Arc<str>, value: i64, scope: NodeId) {
        let list = self.counters.entry(name.clone()).or_default();
        // A sibling (same scope) that created a counter of this name is replaced (CSS Lists 3 §4.4
        // "instantiate a counter").
        if list.last().is_some_and(|i| i.scope == scope) {
            list.last_mut().unwrap().value = value;
            return;
        }
        list.push(Instance { value, scope });
        self.created.push((scope, name.clone()));
    }

    fn close_scope(&mut self, scope: NodeId) {
        while let Some((s, _)) = self.created.last() {
            if *s != scope {
                break;
            }
            let (_, name) = self.created.pop().unwrap();
            if let Some(list) = self.counters.get_mut(&name) {
                list.pop();
            }
        }
    }

    /// Applies an element's (or pseudo-element's) `counter-reset`, and `counter-increment`
    /// (and `counter-set` after them, which stylo's servo build lacks), in that order (CSS Lists 3 §4.5), plus the implicit `list-item` increment.
    fn apply(&mut self, values: &ComputedValues, scope: NodeId, list_item: bool) {
        let counters = values.get_counters();
        for pair in counters.counter_reset.iter() {
            let name: Arc<str> = Arc::from(&*pair.name.0);
            // `reversed()` counts down from the number of items; without a count of the scope's items
            // here, it starts at the given value.
            self.instantiate(&name, pair.value as i64, scope);
        }
        let mut increments_list_item = false;
        for pair in counters.counter_increment.iter() {
            let name: Arc<str> = Arc::from(&*pair.name.0);
            if &*name == "list-item" {
                increments_list_item = true;
            }
            self.increment(&name, pair.value as i64, scope);
        }
        if list_item && !increments_list_item {
            let name: Arc<str> = Arc::from("list-item");
            self.increment(&name, 1, scope);
        }
        // `counter-set` is not in stylo's servo build.
    }

    fn increment(&mut self, name: &Arc<str>, by: i64, scope: NodeId) {
        match self.counters.get_mut(name).and_then(|l| l.last_mut()) {
            Some(i) => i.value = i.value.saturating_add(by),
            None => self.instantiate(name, by, scope),
        }
    }

    fn quote(&self, values: &ComputedValues, open: bool, depth: i32) -> String {
        use style::values::specified::list::Quotes;
        let index = depth.max(0) as usize;
        match &values.get_list().quotes {
            Quotes::QuoteList(list) => {
                let pairs = &list.0;
                if pairs.is_empty() {
                    return String::new();
                }
                let pair = &pairs[index.min(pairs.len() - 1)];
                if open {
                    pair.opening.to_string()
                } else {
                    pair.closing.to_string()
                }
            }
            // `auto`: the quotes of the content language; English ones for every language here.
            Quotes::Auto => match (index, open) {
                (0, true) => "\u{201C}".into(),
                (0, false) => "\u{201D}".into(),
                (_, true) => "\u{2018}".into(),
                (_, false) => "\u{2019}".into(),
            },
        }
    }

    fn resolve(
        &mut self,
        element: NodeId,
        pseudo: PseudoElement,
        values: ServoArc<ComputedValues>,
    ) {
        let GenericContent::Items(items) = &values.get_counters().content else {
            return;
        };
        let style = (self.make_style)(values.clone());
        let mut out = Vec::new();
        for item in &items.items[..items.alt_start] {
            match item {
                Item::String(s) => push_text(&mut out, s),
                Item::Image(i) => out.extend(image_item(&style, i)),
                Item::Counter(name, cs) => {
                    let v = self
                        .counters
                        .get(&*name.0)
                        .and_then(|l| l.last())
                        .map_or(0, |i| i.value);
                    push_text(&mut out, &format_counter(v, cs));
                }
                Item::Counters(name, sep, cs) => {
                    let text = match self.counters.get(&*name.0) {
                        Some(l) if !l.is_empty() => l
                            .iter()
                            .map(|i| format_counter(i.value, cs))
                            .collect::<Vec<_>>()
                            .join(sep),
                        _ => format_counter(0, cs),
                    };
                    push_text(&mut out, &text);
                }
                Item::OpenQuote => {
                    let q = self.quote(&values, true, self.quote_depth);
                    self.quote_depth += 1;
                    push_text(&mut out, &q);
                }
                Item::CloseQuote => {
                    self.quote_depth -= 1;
                    let q = self.quote(&values, false, self.quote_depth);
                    push_text(&mut out, &q);
                }
                Item::NoOpenQuote => self.quote_depth += 1,
                Item::NoCloseQuote => self.quote_depth -= 1,
            }
        }
        self.out.insert(
            (element, pseudo_key(pseudo)),
            Arc::new(ls::Content::Items(out)),
        );
    }

    fn pseudo(&mut self, element: NodeId, which: PseudoElement) {
        let Some(values) = (self.pseudo_style)(element, which) else {
            return;
        };
        if values.get_box().display.is_none() {
            return;
        }
        // A pseudo-element is the first (or last) child of its element: its counters are scoped to
        // the element.
        self.apply(&values, element, false);
        self.resolve(element, which, values);
    }

    fn element(&mut self, el: NodeId, parent: Option<NodeId>) {
        let Some(values) = (self.element_style)(el) else {
            return;
        };
        if values.get_box().display.is_none() {
            return;
        }
        let list_item = values.get_box().display.is_list_item();
        let scope = parent.unwrap_or(el);
        self.apply(&values, scope, list_item);
        if list_item {
            self.pseudo(el, PseudoElement::Marker);
        }
        self.pseudo(el, PseudoElement::Before);
        let mut child = self.dom.first_child(el);
        while let Some(c) = child {
            if matches!(self.dom.data(c), NodeData::Element(_)) {
                self.element(c, Some(el));
            }
            child = self.dom.next_sibling(c);
        }
        self.pseudo(el, PseudoElement::After);
        self.close_scope(el);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_styles() {
        assert_eq!(format_named(3, "decimal"), "3");
        assert_eq!(format_named(7, "decimal-leading-zero"), "07");
        assert_eq!(format_named(1, "lower-alpha"), "a");
        assert_eq!(format_named(27, "upper-alpha"), "AA");
        assert_eq!(format_named(1994, "upper-roman"), "MCMXCIV");
        assert_eq!(format_named(4, "lower-roman"), "iv");
        assert_eq!(format_named(2, "lower-greek"), "β");
        assert_eq!(format_named(0, "lower-alpha"), "0");
        assert_eq!(format_named(5, "disc"), "\u{2022}");
        assert_eq!(format_named(5, "no-such-style"), "5");
    }
}
