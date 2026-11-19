// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MIME types (MIME Sniffing Standard §4) and the Fetch header-value helpers built on HTTP quoted strings.

use crate::headers::HeaderList;

/// A parsed MIME type record: lowercase type and subtype, and parameters in order with lowercase names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MimeType {
    pub type_: String,
    pub subtype: String,
    pub parameters: Vec<(String, String)>,
}

pub(crate) fn is_http_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | ' ')
}

pub(crate) fn is_http_tab_or_space(c: char) -> bool {
    matches!(c, '\t' | ' ')
}

/// HTTP token code point.
pub(crate) fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_token_char)
}

fn is_quoted_string_token_char(c: char) -> bool {
    c == '\t' || (' '..='~').contains(&c) || ('\u{80}'..='\u{FF}').contains(&c)
}

/// Fetch "collect an HTTP quoted string" starting at `chars[*pos]`, which must be '"'.
pub(crate) fn collect_http_quoted_string(
    chars: &[char],
    pos: &mut usize,
    extract_value: bool,
) -> String {
    let start = *pos;
    let mut value = String::new();
    debug_assert_eq!(chars.get(*pos), Some(&'"'));
    *pos += 1;
    loop {
        while *pos < chars.len() && chars[*pos] != '"' && chars[*pos] != '\\' {
            value.push(chars[*pos]);
            *pos += 1;
        }
        if *pos >= chars.len() {
            break;
        }
        let quote_or_backslash = chars[*pos];
        *pos += 1;
        if quote_or_backslash == '\\' {
            if *pos >= chars.len() {
                value.push('\\');
                break;
            }
            value.push(chars[*pos]);
            *pos += 1;
        } else {
            break;
        }
    }
    if extract_value {
        value
    } else {
        chars[start..*pos].iter().collect()
    }
}

impl MimeType {
    /// MIME Sniffing §4.4 "parse a MIME type".
    pub fn parse(input: &str) -> Option<MimeType> {
        let input = input.trim_matches(is_http_whitespace);
        let chars: Vec<char> = input.chars().collect();
        let mut pos = 0;
        let mut type_ = String::new();
        while pos < chars.len() && chars[pos] != '/' {
            type_.push(chars[pos]);
            pos += 1;
        }
        if !is_token(&type_) || pos >= chars.len() {
            return None;
        }
        pos += 1;
        let mut subtype = String::new();
        while pos < chars.len() && chars[pos] != ';' {
            subtype.push(chars[pos]);
            pos += 1;
        }
        let subtype = subtype.trim_end_matches(is_http_whitespace).to_string();
        if !is_token(&subtype) {
            return None;
        }
        let mut mime = MimeType {
            type_: type_.to_ascii_lowercase(),
            subtype: subtype.to_ascii_lowercase(),
            parameters: Vec::new(),
        };
        while pos < chars.len() {
            pos += 1; // (the ';')
            while pos < chars.len() && is_http_whitespace(chars[pos]) {
                pos += 1;
            }
            let mut name = String::new();
            while pos < chars.len() && chars[pos] != ';' && chars[pos] != '=' {
                name.push(chars[pos]);
                pos += 1;
            }
            let name = name.to_ascii_lowercase();
            if pos < chars.len() {
                if chars[pos] == ';' {
                    continue;
                }
                pos += 1; // (the '=')
            }
            if pos >= chars.len() {
                break;
            }
            let value;
            if chars[pos] == '"' {
                value = collect_http_quoted_string(&chars, &mut pos, true);
                while pos < chars.len() && chars[pos] != ';' {
                    pos += 1;
                }
            } else {
                let mut v = String::new();
                while pos < chars.len() && chars[pos] != ';' {
                    v.push(chars[pos]);
                    pos += 1;
                }
                value = v.trim_end_matches(is_http_whitespace).to_string();
                if value.is_empty() {
                    continue;
                }
            }
            if is_token(&name)
                && value.chars().all(is_quoted_string_token_char)
                && !mime.parameters.iter().any(|(n, _)| *n == name)
            {
                mime.parameters.push((name, value));
            }
        }
        Some(mime)
    }

    /// "type/subtype".
    pub fn essence(&self) -> String {
        format!("{}/{}", self.type_, self.subtype)
    }

    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn set_parameter(&mut self, name: &str, value: &str) {
        if let Some(entry) = self.parameters.iter_mut().find(|(n, _)| n == name) {
            entry.1 = value.to_string();
        } else {
            self.parameters.push((name.to_string(), value.to_string()));
        }
    }

    /// MIME Sniffing §4.5 "serialize a MIME type".
    pub fn serialize(&self) -> String {
        let mut out = self.essence();
        for (name, value) in &self.parameters {
            out.push(';');
            out.push_str(name);
            out.push('=');
            if value.is_empty() || !is_token(value) {
                out.push('"');
                for c in value.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            } else {
                out.push_str(value);
            }
        }
        out
    }

    /// MIME Sniffing "JavaScript MIME type essence match".
    pub fn is_javascript(&self) -> bool {
        is_javascript_essence(&self.essence())
    }
}

pub fn is_javascript_essence(essence: &str) -> bool {
    matches!(
        essence.to_ascii_lowercase().as_str(),
        "application/ecmascript"
            | "application/javascript"
            | "application/x-ecmascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "text/javascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
            | "text/jscript"
            | "text/livescript"
            | "text/x-ecmascript"
            | "text/x-javascript"
    )
}

/// Fetch §2.2.2 "get, decode, and split" a header value: split on commas outside quoted strings, trimming
/// tabs and spaces.
pub fn split_header_value(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let mut pos = 0;
    let mut values = Vec::new();
    let mut temp = String::new();
    loop {
        while pos < chars.len() && chars[pos] != '"' && chars[pos] != ',' {
            temp.push(chars[pos]);
            pos += 1;
        }
        if pos < chars.len() {
            if chars[pos] == '"' {
                temp.push_str(&collect_http_quoted_string(&chars, &mut pos, false));
                if pos < chars.len() {
                    continue;
                }
            } else {
                pos += 1;
            }
        }
        values.push(temp.trim_matches(is_http_tab_or_space).to_string());
        temp.clear();
        if pos >= chars.len() {
            return values;
        }
    }
}

/// Fetch §3.? "extract a MIME type" from a header list's `Content-Type` values.
pub fn extract_mime_type(headers: &HeaderList) -> Option<MimeType> {
    let values = headers.get_decode_split("content-type")?;
    let mut charset: Option<String> = None;
    let mut essence: Option<String> = None;
    let mut mime: Option<MimeType> = None;
    for value in values {
        let Some(mut temp) = MimeType::parse(&value) else {
            continue;
        };
        if temp.essence() == "*/*" {
            continue;
        }
        let temp_essence = temp.essence();
        if essence.as_deref() != Some(temp_essence.as_str()) {
            charset = temp.parameter("charset").map(str::to_owned);
            essence = Some(temp_essence);
        } else if temp.parameter("charset").is_none() {
            if let Some(c) = &charset {
                temp.set_parameter("charset", c);
            }
        }
        mime = Some(temp);
    }
    mime
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let m = MimeType::parse(" Text/HTML ; Charset=\"utf-8\" ; foo=bar").unwrap();
        assert_eq!(m.essence(), "text/html");
        assert_eq!(m.parameter("charset"), Some("utf-8"));
        assert_eq!(m.parameter("foo"), Some("bar"));
        assert_eq!(m.serialize(), "text/html;charset=utf-8;foo=bar");
    }

    #[test]
    fn parse_failures() {
        assert!(MimeType::parse("text").is_none());
        assert!(MimeType::parse("/html").is_none());
        assert!(MimeType::parse("text/").is_none());
        assert!(MimeType::parse("te xt/html").is_none());
    }

    #[test]
    fn parse_edge_parameters() {
        // first parameter wins; empty unquoted values drop; quoted values keep escapes resolved
        let m = MimeType::parse("a/b;x=1;x=2;y=;z=\"q\\\"w\"").unwrap();
        assert_eq!(m.parameters.len(), 2);
        assert_eq!(m.parameter("x"), Some("1"));
        assert_eq!(m.parameter("z"), Some("q\"w"));
        assert_eq!(m.serialize(), "a/b;x=1;z=\"q\\\"w\"");
        let m = MimeType::parse("text/plain;charset=\"\"").unwrap();
        assert_eq!(m.serialize(), "text/plain;charset=\"\"");
    }

    #[test]
    fn split_values() {
        assert_eq!(
            split_header_value("text/html;\", text/plain\""),
            vec!["text/html;\", text/plain\""]
        );
        assert_eq!(split_header_value("a, b ,c"), vec!["a", "b", "c"]);
        assert_eq!(split_header_value(""), vec![""]);
        assert_eq!(split_header_value("a,"), vec!["a"]);
        assert_eq!(split_header_value(",a"), vec!["", "a"]);
    }

    #[test]
    fn extract() {
        let mut h = HeaderList::new();
        h.append("Content-Type", "text/plain;charset=gbk, text/plain");
        assert_eq!(
            extract_mime_type(&h).unwrap().serialize(),
            "text/plain;charset=gbk"
        );
        let mut h = HeaderList::new();
        h.append("Content-Type", "text/html;charset=gbk");
        h.append("Content-Type", "*/*");
        assert_eq!(
            extract_mime_type(&h).unwrap().serialize(),
            "text/html;charset=gbk"
        );
        let mut h = HeaderList::new();
        h.append("Content-Type", "text/html;charset=gbk, text/plain");
        assert_eq!(extract_mime_type(&h).unwrap().serialize(), "text/plain");
        // A quoted string spans the commas; a parameter value keeps its case.
        let mut h = HeaderList::new();
        for v in ["text/html;\"", "\\\"", "text/plain", "\";charset=GBK"] {
            h.append("Content-Type", v);
        }
        assert_eq!(
            extract_mime_type(&h).unwrap().serialize(),
            "text/html;charset=GBK"
        );
    }
}
