// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rust names for IDL identifiers.

/// `getElementsByTagNameNS` → `get_elements_by_tag_name_ns`, `innerHTML` → `inner_html`, `URL` → `url`.
pub fn snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '-' || c == '_' {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            continue;
        }
        if c.is_ascii_uppercase() {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let next = chars.get(i + 1).copied();
            let boundary = match prev {
                Some(p) if p.is_ascii_lowercase() || p.is_ascii_digit() => true,
                Some(p) if p.is_ascii_uppercase() => next.is_some_and(|n| n.is_ascii_lowercase()),
                _ => false,
            };
            if boundary && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    escape(out)
}

/// `HTMLElement` → `HTML_ELEMENT` (constants of prototype ids).
pub fn upper(name: &str) -> String {
    snake(name).trim_end_matches('_').to_ascii_uppercase()
}

/// `no-referrer` → `NoReferrer`, `datetime-local` → `DatetimeLocal`, `""` → `Empty`, `2d` → `_2d`.
pub fn camel(value: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in value.chars() {
        if c.is_ascii_alphanumeric() {
            if up {
                out.push(c.to_ascii_uppercase());
                up = false;
            } else {
                out.push(c);
            }
        } else {
            up = true;
        }
    }
    if out.is_empty() {
        return "Empty".into();
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "box", "break", "const", "continue", "crate", "do", "dyn", "else",
    "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in", "let", "loop",
    "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return", "self",
    "static", "struct", "super", "trait", "true", "try", "type", "typeof", "unsafe", "unsized",
    "use", "virtual", "where", "while", "yield", "abstract", "become",
    // (not a keyword: the name of the generated code's `&mut Cx` parameter, which an IDL argument
    // such as `SVGTransform.setRotate`'s `cx` must not shadow)
    "cx",
];

/// Appends `_` to Rust keywords.
pub fn escape(s: String) -> String {
    if KEYWORDS.contains(&s.as_str()) {
        format!("{s}_")
    } else {
        s
    }
}

/// A Rust string literal.
pub fn lit(s: &str) -> String {
    format!("{s:?}")
}

/// An expression for a `&'static rt::Name` of `s`: a property name whose V8 string (or private
/// symbol) each isolate makes once (`rt::cached_name`, `rt::cached_private`).
pub fn name_key(s: &str) -> String {
    format!("{{ static K: rt::Name = rt::Name::new({}); &K }}", lit(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_cases() {
        assert_eq!(
            snake("getElementsByTagNameNS"),
            "get_elements_by_tag_name_ns"
        );
        assert_eq!(snake("innerHTML"), "inner_html");
        assert_eq!(snake("URL"), "url");
        assert_eq!(snake("HTMLElement"), "html_element");
        assert_eq!(snake("type"), "type_");
        assert_eq!(snake("ELEMENT_NODE"), "element_node");
        assert_eq!(snake("onDOMContentLoaded"), "on_dom_content_loaded");
        assert_eq!(snake("x2D"), "x2_d");
        assert_eq!(upper("HTMLElement"), "HTML_ELEMENT");
        assert_eq!(camel("datetime-local"), "DatetimeLocal");
        assert_eq!(camel(""), "Empty");
    }
}
