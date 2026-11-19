// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Name validity (DOM §1.4 "namespaces", HTML §4.13.3 "valid custom element name", XML 1.0 `Name`).

use html5ever::{ns, LocalName, Namespace, Prefix, QualName};

use crate::error::{DomError, DomResult};

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

fn is_ascii_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ')
}

/// DOM: "a valid namespace prefix": non-empty, no ASCII whitespace, NULL, `/` or `>`.
pub fn is_valid_namespace_prefix(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| is_ascii_ws(c) || matches!(c, '\0' | '/' | '>'))
}

/// DOM: "a valid attribute local name": non-empty, no ASCII whitespace, NULL, `/`, `=` or `>`.
pub fn is_valid_attribute_local_name(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| is_ascii_ws(c) || matches!(c, '\0' | '/' | '=' | '>'))
}

/// DOM: "a valid element local name".
pub fn is_valid_element_local_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first.is_ascii_alphabetic() {
        return !s
            .chars()
            .any(|c| is_ascii_ws(c) || matches!(c, '\0' | '/' | '>'));
    }
    if !(matches!(first, ':' | '_') || first >= '\u{80}') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_') || c >= '\u{80}')
}

/// DOM: "a valid doctype name": no ASCII whitespace, NULL or `>`.
pub fn is_valid_doctype_name(s: &str) -> bool {
    !s.chars().any(|c| is_ascii_ws(c) || matches!(c, '\0' | '>'))
}

fn is_name_start_char(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

fn is_name_char(c: char) -> bool {
    is_name_start_char(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// XML 1.0 (5th edition) `Name` production.
pub fn is_xml_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(is_name_start_char) && chars.all(is_name_char)
}

/// HTML §4.13.3 "valid custom element name".
pub fn is_valid_custom_element_name(s: &str) -> bool {
    // A valid element local name that starts with an ASCII lower alpha, has no ASCII upper alpha
    // and contains a hyphen, and is not one of the reserved names.
    if !s.starts_with(|c: char| c.is_ascii_lowercase()) || !s.contains('-') {
        return false;
    }
    if s.chars().any(|c| c.is_ascii_uppercase()) || !is_valid_element_local_name(s) {
        return false;
    }
    !matches!(
        s,
        "annotation-xml"
            | "color-profile"
            | "font-face"
            | "font-face-src"
            | "font-face-uri"
            | "font-face-format"
            | "font-face-name"
            | "missing-glyph"
    )
}

/// DOM §4.8 "valid shadow host name".
pub fn is_valid_shadow_host_name(s: &str) -> bool {
    matches!(
        s,
        "article"
            | "aside"
            | "blockquote"
            | "body"
            | "div"
            | "footer"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "main"
            | "nav"
            | "p"
            | "section"
            | "span"
    ) || is_valid_custom_element_name(s)
}

/// What "validate and extract" validates a local name as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameContext {
    Element,
    Attribute,
}

/// DOM §1.4 "validate and extract" a `namespace` and `qualifiedName`.
pub fn validate_and_extract(
    namespace: Option<&str>,
    qualified_name: &str,
    context: NameContext,
) -> DomResult<QualName> {
    // 1.
    let namespace = namespace.filter(|n| !n.is_empty());
    // 2-4.
    let (prefix, local) = match qualified_name.split_once(':') {
        Some((p, l)) => {
            if !is_valid_namespace_prefix(p) {
                return Err(DomError::InvalidCharacter);
            }
            (Some(p), l)
        }
        None => (None, qualified_name),
    };
    // 5-7.
    let valid = match context {
        NameContext::Attribute => is_valid_attribute_local_name(local),
        NameContext::Element => is_valid_element_local_name(local),
    };
    if !valid {
        return Err(DomError::InvalidCharacter);
    }
    // 8.
    if prefix.is_some() && namespace.is_none() {
        return Err(DomError::Namespace);
    }
    // 9.
    if prefix == Some("xml") && namespace != Some(XML_NS) {
        return Err(DomError::Namespace);
    }
    // 10.
    if (qualified_name == "xmlns" || prefix == Some("xmlns")) && namespace != Some(XMLNS_NS) {
        return Err(DomError::Namespace);
    }
    // 11.
    if namespace == Some(XMLNS_NS) && qualified_name != "xmlns" && prefix != Some("xmlns") {
        return Err(DomError::Namespace);
    }
    Ok(QualName::new(
        prefix.map(Prefix::from),
        namespace.map_or(ns!(), Namespace::from),
        LocalName::from(local),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_local_names() {
        assert!(is_valid_element_local_name("div"));
        assert!(is_valid_element_local_name("a:b"));
        assert!(is_valid_element_local_name("_x"));
        assert!(is_valid_element_local_name("\u{e9}t\u{e9}"));
        assert!(is_valid_element_local_name("a\u{1}b"));
        assert!(!is_valid_element_local_name(""));
        assert!(!is_valid_element_local_name("a b"));
        assert!(!is_valid_element_local_name("a>"));
        assert!(!is_valid_element_local_name("1a"));
        assert!(!is_valid_element_local_name("-a"));
        assert!(!is_valid_element_local_name("_a b"));
    }

    #[test]
    fn custom_element_names() {
        assert!(is_valid_custom_element_name("my-element"));
        assert!(is_valid_custom_element_name("a-\u{e9}"));
        assert!(!is_valid_custom_element_name("myelement"));
        assert!(!is_valid_custom_element_name("My-element"));
        assert!(!is_valid_custom_element_name("font-face"));
        assert!(!is_valid_custom_element_name("a-B"));
    }

    #[test]
    fn validate_and_extract_errors() {
        use NameContext::*;
        let q = validate_and_extract(Some("urn:x"), "p:l", Element).unwrap();
        assert_eq!(&*q.prefix.unwrap(), "p");
        assert_eq!(&*q.local, "l");
        assert_eq!(
            validate_and_extract(None, "p:l", Element),
            Err(DomError::Namespace)
        );
        assert_eq!(
            validate_and_extract(Some("urn:x"), "xml:l", Element),
            Err(DomError::Namespace)
        );
        assert_eq!(
            validate_and_extract(Some("urn:x"), "xmlns", Attribute),
            Err(DomError::Namespace)
        );
        assert_eq!(
            validate_and_extract(Some(XMLNS_NS), "a", Attribute),
            Err(DomError::Namespace)
        );
        assert!(validate_and_extract(Some(XMLNS_NS), "xmlns:a", Attribute).is_ok());
        assert_eq!(
            validate_and_extract(Some("urn:x"), ":a", Element),
            Err(DomError::InvalidCharacter)
        );
        assert_eq!(
            validate_and_extract(None, "a=b", Attribute),
            Err(DomError::InvalidCharacter)
        );
        assert_eq!(
            validate_and_extract(None, "", Element),
            Err(DomError::InvalidCharacter)
        );
        // An empty namespace is null.
        assert_eq!(
            validate_and_extract(Some(""), "p:l", Element),
            Err(DomError::Namespace)
        );
        // Only the first colon splits.
        let q = validate_and_extract(Some("urn:x"), "a:b:c", Element).unwrap();
        assert_eq!(&*q.local, "b:c");
    }

    #[test]
    fn xml_names() {
        assert!(is_xml_name("x"));
        assert!(is_xml_name("x-1.y"));
        assert!(!is_xml_name("1x"));
        assert!(!is_xml_name(""));
    }
}
