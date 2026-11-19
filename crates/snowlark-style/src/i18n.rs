// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The language and directionality of elements (HTML §3.2.6.2, §3.2.6.4) and `:lang()` range
//! matching (Selectors 4 §8.2, RFC 4647 §3.3.2), on the DOM alone: what `:dir()` and `:lang()`
//! match, for style and for the DOM's own selector matching.

use snowlark_dom::{local_name, ns, Dom, LocalName, NodeData, NodeId};

/// The `dir` attribute's states.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DirState {
    Ltr,
    Rtl,
    Auto,
}

fn is_html(dom: &Dom, id: NodeId, local: &LocalName) -> bool {
    dom.is_html_element_named(id, local)
}

/// An HTML element's `dir` attribute state, when it has a valid one.
fn dir_state(dom: &Dom, id: NodeId) -> Option<DirState> {
    if dom.element(id).is_none_or(|e| e.name().ns != ns!(html)) {
        return None;
    }
    let v = dom.attr(id, &ns!(), &local_name!("dir"))?;
    [
        ("ltr", DirState::Ltr),
        ("rtl", DirState::Rtl),
        ("auto", DirState::Auto),
    ]
    .into_iter()
    .find_map(|(name, state)| v.eq_ignore_ascii_case(name).then_some(state))
}

/// An `input` element's type state keyword (lowercase), or `None` for other elements.
fn input_type(dom: &Dom, id: NodeId) -> Option<String> {
    if !is_html(dom, id, &local_name!("input")) {
        return None;
    }
    Some(
        dom.attr(id, &ns!(), &local_name!("type"))
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "text".to_owned()),
    )
}

/// HTML's "auto directionality form-associated elements": the controls whose value decides
/// their directionality in the auto state (input in the Hidden, Text, Search, Telephone, URL,
/// Email, Password, Submit Button, Reset Button or Button state, and textarea).
fn uses_value_for_auto(dom: &Dom, id: NodeId) -> bool {
    if is_html(dom, id, &local_name!("textarea")) {
        return true;
    }
    // (an unknown type is the Text state)
    input_type(dom, id).is_some_and(|t| {
        matches!(
            t.as_str(),
            "hidden"
                | "text"
                | "search"
                | "tel"
                | "url"
                | "email"
                | "password"
                | "submit"
                | "reset"
                | "button"
        ) || !KNOWN_INPUT_TYPES.contains(&t.as_str())
    })
}

const KNOWN_INPUT_TYPES: &[&str] = &[
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

/// The direction of the first strong character of `s` (bidi types L, or R and AL): `Some(true)`
/// for right-to-left.
fn first_strong_is_rtl(s: &str) -> Option<bool> {
    match unicode_bidi::get_base_direction(s) {
        unicode_bidi::Direction::Ltr => Some(false),
        unicode_bidi::Direction::Rtl => Some(true),
        unicode_bidi::Direction::Mixed => None,
    }
}

/// HTML "contained text auto directionality": the first strong character of the text in `id`'s
/// subtree, leaving out `bdi`, `script`, `style` and `textarea` elements, those with a valid `dir`
/// of their own and auto directionality form-associated elements, with what they contain.
fn contained_text_is_rtl(dom: &Dom, id: NodeId) -> Option<bool> {
    for c in dom.children(id) {
        match dom.data(c) {
            NodeData::Text(t) | NodeData::CDataSection(t) => {
                if let Some(rtl) = first_strong_is_rtl(&t[..]) {
                    return Some(rtl);
                }
            }
            NodeData::Element(_) => {
                let skipped = [
                    local_name!("bdi"),
                    local_name!("script"),
                    local_name!("style"),
                    local_name!("textarea"),
                ]
                .iter()
                .any(|n| is_html(dom, c, n))
                    || dir_state(dom, c).is_some()
                    || uses_value_for_auto(dom, c);
                if !skipped {
                    if let Some(rtl) = contained_text_is_rtl(dom, c) {
                        return Some(rtl);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// HTML §3.2.6.4: whether `el`'s directionality is right-to-left. `value` gives a text field's
/// value (the dirty value the page holds, which the DOM does not), for the auto state.
pub fn directionality_is_rtl(dom: &Dom, el: NodeId, value: &dyn Fn(NodeId) -> String) -> bool {
    let mut current = el;
    loop {
        match dir_state(dom, current) {
            Some(DirState::Ltr) => return false,
            Some(DirState::Rtl) => return true,
            Some(DirState::Auto) => {
                if uses_value_for_auto(dom, current) {
                    return first_strong_is_rtl(&value(current)).unwrap_or(false);
                }
                return contained_text_is_rtl(dom, current).unwrap_or(false);
            }
            None => {
                // (a telephone field is left-to-right unless its own `dir` says otherwise)
                if input_type(dom, current).as_deref() == Some("tel") {
                    return false;
                }
                if is_html(dom, current, &local_name!("bdi")) {
                    return contained_text_is_rtl(dom, current).unwrap_or(false);
                }
            }
        }
        // (the parent element, or the host of the shadow root it is a child of)
        let Some(parent) = dom.parent(current) else {
            return false;
        };
        current = match dom.data(parent) {
            NodeData::Element(_) => parent,
            NodeData::ShadowRoot(s) => s.host(),
            _ => return false,
        };
    }
}

/// A text field's value as the DOM alone has it (the `value` attribute, a textarea's text), for
/// [`directionality_is_rtl`] where the page's dirty value is not at hand.
pub fn default_value(dom: &Dom, id: NodeId) -> String {
    if is_html(dom, id, &local_name!("textarea")) {
        return dom.child_text_content(id);
    }
    dom.attr(id, &ns!(), &local_name!("value"))
        .unwrap_or("")
        .to_owned()
}

/// HTML §3.2.6.2: the language of `el`, from its nearest inclusive ancestor (across shadow roots,
/// to their hosts) with `xml:lang` or `lang`, else the document's pragma-set default language;
/// empty when none says (unknown).
pub fn language_of(dom: &Dom, el: NodeId) -> &str {
    let mut current = el;
    loop {
        if let Some(l) = dom
            .attr(current, &ns!(xml), &local_name!("lang"))
            .or_else(|| dom.attr(current, &ns!(), &local_name!("lang")))
        {
            return l;
        }
        let Some(parent) = dom.parent(current) else {
            return "";
        };
        current = match dom.data(parent) {
            NodeData::Element(_) => parent,
            NodeData::ShadowRoot(s) => s.host(),
            NodeData::Document(_) => return dom.pragma_language(parent).unwrap_or(""),
            _ => return "",
        };
    }
}

/// Selectors 4 §8.2 `:lang()`: whether the language tag `tag` matches the language range `range`
/// by RFC 4647 §3.3.2 extended filtering (a `*` subtag matches any, a range subtag may skip tag
/// subtags but not a singleton). A tag with a subtag longer than eight characters is not a
/// well-formed language tag and matches no range.
pub fn lang_matches(tag: &str, range: &str) -> bool {
    let mut tags = tag.split('-');
    let mut ranges = range.split('-');
    if tag.split('-').any(|s| s.len() > 8) {
        return false;
    }
    // 2. The first subtags match, or the range's is a wildcard.
    match (ranges.next(), tags.next()) {
        (Some(r), Some(t)) if r == "*" || r.eq_ignore_ascii_case(t) => {}
        _ => return false,
    }
    let mut t = tags.next();
    // 3.
    for r in ranges {
        // 3a. A wildcard matches any (or no) subtags.
        if r == "*" {
            continue;
        }
        loop {
            // 3b. No tag subtag left.
            let Some(ts) = t else {
                return false;
            };
            // 3c. They match: on to the next of each.
            if r.eq_ignore_ascii_case(ts) {
                t = tags.next();
                break;
            }
            // 3d. A singleton cannot be skipped.
            if ts.len() == 1 {
                return false;
            }
            // 3e. Skip the tag subtag and try the range subtag against the next.
            t = tags.next();
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::lang_matches;

    #[test]
    fn extended_filtering() {
        assert!(lang_matches("de-DE", "de-DE"));
        assert!(lang_matches("de-Latn-DE", "de-DE"));
        assert!(lang_matches("de-Latf-DE-1996", "de-*-DE"));
        assert!(lang_matches("de-DE", "*-DE"));
        assert!(!lang_matches("de-x-DE", "de-DE"));
        assert!(!lang_matches("de", "de-DE"));
        assert!(lang_matches("fr-x-standard", "fr-x"));
        assert!(!lang_matches("fr-x-standard", "fr-standard"));
        assert!(lang_matches("cocoa-1-bar", "cocoa-1"));
        assert!(!lang_matches("cocoa-1-bar", "cocoa-bar"));
        assert!(!lang_matches("fr-ninechars", "fr"));
    }
}
