// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headers (Fetch §2.2.2 header lists, §5.2 the `Headers` class with its guards, and the header-name
//! classifications CORS and no-CORS requests depend on).
//!
//! Header values are byte sequences. A [`HeaderList`] keeps them isomorphic-decoded: each byte is one
//! `char` U+0000..=U+00FF, which is also how JavaScript sees them (ByteString). Names keep the case they
//! were first appended with and compare ASCII case-insensitively.

use crate::mime::{split_header_value, MimeType};

/// Bytes to a string with one char per byte (Infra "isomorphic decode").
pub fn isomorphic_decode(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// A string whose chars are all ≤ U+00FF back to bytes (Infra "isomorphic encode"); other chars become '?'.
pub fn isomorphic_encode(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| if (c as u32) <= 0xFF { c as u8 } else { b'?' })
        .collect()
}

/// A header list: an ordered list of (name, value) pairs.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeaderList {
    entries: Vec<(String, String)>,
}

impl HeaderList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The raw entries, in order, without combining.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(name))
    }

    /// "get": the values of `name` combined with ", ", or `None`.
    pub fn get(&self, name: &str) -> Option<String> {
        let mut out: Option<String> = None;
        for (n, v) in &self.entries {
            if n.eq_ignore_ascii_case(name) {
                match &mut out {
                    Some(s) => {
                        s.push_str(", ");
                        s.push_str(v);
                    }
                    None => out = Some(v.clone()),
                }
            }
        }
        out
    }

    /// Every value of `name` separately (used for `Set-Cookie`).
    pub fn get_all(&self, name: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// "get, decode, and split".
    pub fn get_decode_split(&self, name: &str) -> Option<Vec<String>> {
        self.get(name).map(|v| split_header_value(&v))
    }

    /// "append": the name keeps the casing of an existing entry with the same name.
    pub fn append(&mut self, name: &str, value: &str) {
        let name = self
            .entries
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| name.to_string());
        self.entries.push((name, value.to_string()));
    }

    pub fn delete(&mut self, name: &str) {
        self.entries.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    }

    /// "set": replace the first entry named `name` and remove the others, or append.
    pub fn set(&mut self, name: &str, value: &str) {
        if let Some(i) = self
            .entries
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            self.entries[i].1 = value.to_string();
            let mut index = 0;
            self.entries.retain(|(n, _)| {
                let keep = index <= i || !n.eq_ignore_ascii_case(name);
                index += 1;
                keep
            });
        } else {
            self.entries.push((name.to_string(), value.to_string()));
        }
    }

    /// "combine": append `value` to the existing value with ", ", or append the header.
    pub fn combine(&mut self, name: &str, value: &str) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            entry.1.push_str(", ");
            entry.1.push_str(value);
        } else {
            self.entries.push((name.to_string(), value.to_string()));
        }
    }

    /// "sort and combine": lowercase names in order, values combined, `set-cookie` kept separate. This is
    /// what iterating a `Headers` object yields.
    pub fn sort_and_combine(&self) -> Vec<(String, String)> {
        let mut names: Vec<String> = self
            .entries
            .iter()
            .map(|(n, _)| n.to_ascii_lowercase())
            .collect();
        names.sort();
        names.dedup();
        let mut out = Vec::new();
        for name in names {
            if name == "set-cookie" {
                for v in self.get_all("set-cookie") {
                    out.push((name.clone(), v.to_string()));
                }
            } else if let Some(v) = self.get(&name) {
                out.push((name, v));
            }
        }
        out
    }

    /// The unique lowercase names in this list.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for (n, _) in &self.entries {
            let lower = n.to_ascii_lowercase();
            if !names.contains(&lower) {
                names.push(lower);
            }
        }
        names
    }
}

impl<'a> FromIterator<(&'a str, &'a str)> for HeaderList {
    fn from_iter<T: IntoIterator<Item = (&'a str, &'a str)>>(iter: T) -> Self {
        let mut list = HeaderList::new();
        for (n, v) in iter {
            list.append(n, v);
        }
        list
    }
}

/// A header name: an HTTP token.
pub fn is_header_name(name: &str) -> bool {
    crate::mime::is_token(name)
}

/// A header value: no leading or trailing HTTP tab or space, no NUL, CR or LF, all chars ≤ U+00FF.
pub fn is_header_value(value: &str) -> bool {
    if value.starts_with([' ', '\t']) || value.ends_with([' ', '\t']) {
        return false;
    }
    value
        .chars()
        .all(|c| (c as u32) <= 0xFF && c != '\0' && c != '\r' && c != '\n')
}

/// "normalize" a header value: strip leading and trailing HTTP whitespace bytes.
pub fn normalize_header_value(value: &str) -> &str {
    value.trim_matches(|c| matches!(c, '\t' | '\n' | '\r' | ' '))
}

const FORBIDDEN_REQUEST_HEADERS: &[&str] = &[
    "accept-charset",
    "accept-encoding",
    "access-control-request-headers",
    "access-control-request-method",
    "connection",
    "content-length",
    "cookie",
    "cookie2",
    "date",
    "dnt",
    "expect",
    "host",
    "keep-alive",
    "origin",
    "referer",
    "set-cookie",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "via",
];

/// Fetch "forbidden method": CONNECT, TRACE, TRACK.
pub fn is_forbidden_method(method: &str) -> bool {
    ["CONNECT", "TRACE", "TRACK"]
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method))
}

/// Fetch "forbidden request-header" (name, value).
pub fn is_forbidden_request_header(name: &str, value: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if FORBIDDEN_REQUEST_HEADERS.contains(&lower.as_str()) {
        return true;
    }
    if lower.starts_with("proxy-") || lower.starts_with("sec-") {
        return true;
    }
    if matches!(
        lower.as_str(),
        "x-http-method" | "x-http-method-override" | "x-method-override"
    ) {
        return split_header_value(value)
            .iter()
            .any(|m| is_forbidden_method(m));
    }
    false
}

/// Fetch "forbidden response-header name": `Set-Cookie`, `Set-Cookie2`.
pub fn is_forbidden_response_header_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("set-cookie") || name.eq_ignore_ascii_case("set-cookie2")
}

/// Fetch "request-body-header name": dropped when a redirect turns a request into a GET.
pub fn is_request_body_header_name(name: &str) -> bool {
    [
        "content-encoding",
        "content-language",
        "content-location",
        "content-type",
    ]
    .iter()
    .any(|n| n.eq_ignore_ascii_case(name))
}

/// Fetch "CORS-safelisted method".
pub fn is_cors_safelisted_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "POST")
}

fn is_cors_unsafe_request_header_byte(c: char) -> bool {
    let b = c as u32;
    (b < 0x20 && b != 0x09)
        || matches!(
            c,
            '"' | '(' | ')' | ':' | '<' | '>' | '?' | '@' | '[' | '\\' | ']' | '{' | '}' | '\u{7F}'
        )
}

/// Fetch "CORS-safelisted request-header" (name, value).
pub fn is_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    if value.chars().count() > 128 {
        return false;
    }
    match name.to_ascii_lowercase().as_str() {
        "accept" => !value.chars().any(is_cors_unsafe_request_header_byte),
        "accept-language" | "content-language" => value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " *,-.;=".contains(c)),
        "content-type" => {
            if value.chars().any(is_cors_unsafe_request_header_byte) {
                return false;
            }
            match MimeType::parse(value) {
                Some(m) => matches!(
                    m.essence().as_str(),
                    "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
                ),
                None => false,
            }
        }
        "range" => matches!(parse_single_range(value, false), Some((Some(_), _))),
        _ => false,
    }
}

/// Fetch "parse a single range header value": `bytes=start-end` with either side optional (not both).
pub fn parse_single_range(
    value: &str,
    allow_whitespace: bool,
) -> Option<(Option<u64>, Option<u64>)> {
    let chars: Vec<char> = value.chars().collect();
    let mut pos = 0;
    let skip_ws = |pos: &mut usize| {
        if allow_whitespace {
            while *pos < chars.len() && matches!(chars[*pos], ' ' | '\t') {
                *pos += 1;
            }
        }
    };
    let unit: String = chars.iter().take(5).collect();
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    pos += 5;
    skip_ws(&mut pos);
    if chars.get(pos) != Some(&'=') {
        return None;
    }
    pos += 1;
    skip_ws(&mut pos);
    let digits = |pos: &mut usize| -> Option<Option<u64>> {
        let start = *pos;
        while *pos < chars.len() && chars[*pos].is_ascii_digit() {
            *pos += 1;
        }
        if *pos == start {
            return Some(None);
        }
        let s: String = chars[start..*pos].iter().collect();
        s.parse().ok().map(Some)
    };
    let start = digits(&mut pos)?;
    skip_ws(&mut pos);
    if chars.get(pos) != Some(&'-') {
        return None;
    }
    pos += 1;
    skip_ws(&mut pos);
    let end = digits(&mut pos)?;
    if pos != chars.len() {
        return None;
    }
    match (start, end) {
        (None, None) => None,
        (Some(s), Some(e)) if s > e => None,
        r => Some(r),
    }
}

/// Fetch "CORS-unsafe request-header names": sorted, lowercase.
pub fn cors_unsafe_request_header_names(headers: &HeaderList) -> Vec<String> {
    let mut unsafe_names = Vec::new();
    let mut potentially_unsafe = Vec::new();
    let mut safelist_value_size = 0;
    for (name, value) in headers.iter() {
        if !is_cors_safelisted_request_header(name, value) {
            unsafe_names.push(name.to_ascii_lowercase());
        } else {
            potentially_unsafe.push(name.to_ascii_lowercase());
            safelist_value_size += value.chars().count();
        }
    }
    if safelist_value_size > 1024 {
        unsafe_names.extend(potentially_unsafe);
    }
    unsafe_names.sort();
    unsafe_names.dedup();
    unsafe_names
}

/// Fetch "CORS non-wildcard request-header name": `Authorization`.
pub fn is_cors_non_wildcard_request_header_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
}

/// Fetch "privileged no-CORS request-header name": `Range`.
pub fn is_privileged_no_cors_request_header_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("range")
}

/// Fetch "CORS-safelisted response-header name", given the response's CORS-exposed header-name list.
pub fn is_cors_safelisted_response_header_name(name: &str, exposed: &[String]) -> bool {
    [
        "cache-control",
        "content-language",
        "content-length",
        "content-type",
        "expires",
        "last-modified",
        "pragma",
    ]
    .iter()
    .any(|n| n.eq_ignore_ascii_case(name))
        || (!is_forbidden_response_header_name(name)
            && exposed.iter().any(|e| e.eq_ignore_ascii_case(name)))
}

/// Fetch "no-CORS-safelisted request-header name".
pub fn is_no_cors_safelisted_request_header_name(name: &str) -> bool {
    [
        "accept",
        "accept-language",
        "content-language",
        "content-type",
    ]
    .iter()
    .any(|n| n.eq_ignore_ascii_case(name))
}

/// Fetch "no-CORS-safelisted request-header" (name, value).
pub fn is_no_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    is_no_cors_safelisted_request_header_name(name)
        && is_cors_safelisted_request_header(name, value)
}

/// A `Headers` object's guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadersGuard {
    Immutable,
    Request,
    RequestNoCors,
    Response,
    None,
}

/// Why a `Headers` operation threw (a JS `TypeError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadersError {
    InvalidName(String),
    InvalidValue(String),
    Immutable,
}

impl std::fmt::Display for HeadersError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeadersError::InvalidName(n) => write!(f, "Invalid header name: {n:?}"),
            HeadersError::InvalidValue(v) => write!(f, "Invalid header value: {v:?}"),
            HeadersError::Immutable => write!(f, "Headers are immutable"),
        }
    }
}

impl std::error::Error for HeadersError {}

/// Fetch §5.2 the `Headers` class: a header list behind a guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Headers {
    pub list: HeaderList,
    pub guard: HeadersGuard,
}

impl Headers {
    pub fn new(guard: HeadersGuard) -> Self {
        Headers {
            list: HeaderList::new(),
            guard,
        }
    }

    /// "validate": errors throw; `Ok(false)` means the operation silently does nothing.
    fn validate(&self, name: &str, value: &str) -> Result<bool, HeadersError> {
        if !is_header_name(name) {
            return Err(HeadersError::InvalidName(name.to_string()));
        }
        if !is_header_value(value) {
            return Err(HeadersError::InvalidValue(value.to_string()));
        }
        match self.guard {
            HeadersGuard::Immutable => Err(HeadersError::Immutable),
            HeadersGuard::Request if is_forbidden_request_header(name, value) => Ok(false),
            HeadersGuard::Response if is_forbidden_response_header_name(name) => Ok(false),
            _ => Ok(true),
        }
    }

    fn remove_privileged_no_cors_headers(&mut self) {
        self.list.delete("range");
    }

    pub fn append(&mut self, name: &str, value: &str) -> Result<(), HeadersError> {
        let value = normalize_header_value(value);
        if !self.validate(name, value)? {
            return Ok(());
        }
        if self.guard == HeadersGuard::RequestNoCors {
            let temporary = match self.list.get(name) {
                Some(existing) => format!("{existing}, {value}"),
                None => value.to_string(),
            };
            if !is_no_cors_safelisted_request_header(name, &temporary) {
                return Ok(());
            }
        }
        self.list.append(name, value);
        if self.guard == HeadersGuard::RequestNoCors {
            self.remove_privileged_no_cors_headers();
        }
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), HeadersError> {
        if !self.validate(name, "")? {
            return Ok(());
        }
        if self.guard == HeadersGuard::RequestNoCors
            && !is_no_cors_safelisted_request_header_name(name)
            && !is_privileged_no_cors_request_header_name(name)
        {
            return Ok(());
        }
        if !self.list.contains(name) {
            return Ok(());
        }
        self.list.delete(name);
        if self.guard == HeadersGuard::RequestNoCors {
            self.remove_privileged_no_cors_headers();
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Option<String>, HeadersError> {
        if !is_header_name(name) {
            return Err(HeadersError::InvalidName(name.to_string()));
        }
        Ok(self.list.get(name))
    }

    pub fn get_set_cookie(&self) -> Vec<String> {
        self.list
            .get_all("set-cookie")
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    pub fn has(&self, name: &str) -> Result<bool, HeadersError> {
        if !is_header_name(name) {
            return Err(HeadersError::InvalidName(name.to_string()));
        }
        Ok(self.list.contains(name))
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), HeadersError> {
        let value = normalize_header_value(value);
        if !self.validate(name, value)? {
            return Ok(());
        }
        if self.guard == HeadersGuard::RequestNoCors
            && !is_no_cors_safelisted_request_header(name, value)
        {
            return Ok(());
        }
        self.list.set(name, value);
        if self.guard == HeadersGuard::RequestNoCors {
            self.remove_privileged_no_cors_headers();
        }
        Ok(())
    }

    /// The iteration order (sort and combine).
    pub fn entries(&self) -> Vec<(String, String)> {
        self.list.sort_and_combine()
    }
}

/// One link of a `Link` header (RFC 8288 §3): its target URI reference and its parameters (names
/// lowercased, values unquoted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkValue {
    pub target: String,
    pub params: Vec<(String, String)>,
}

/// Parses a `Link` header value (RFC 8288 §3, with HTML's "process link headers" leniency: links
/// that do not parse are skipped).
pub fn parse_link_header(value: &str) -> Vec<LinkValue> {
    let mut out = Vec::new();
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;
    loop {
        while i < chars.len() && (chars[i] == ',' || chars[i].is_whitespace()) {
            i += 1;
        }
        if i >= chars.len() {
            return out;
        }
        if chars[i] != '<' {
            // Not a link: skip to the next comma outside quotes.
            let mut quoted = false;
            while i < chars.len() && (quoted || chars[i] != ',') {
                if chars[i] == '"' {
                    quoted = !quoted;
                }
                i += 1;
            }
            continue;
        }
        let start = i + 1;
        while i < chars.len() && chars[i] != '>' {
            i += 1;
        }
        if i >= chars.len() {
            return out;
        }
        let target: String = chars[start..i].iter().collect();
        i += 1;
        let mut params = Vec::new();
        loop {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i >= chars.len() || chars[i] == ',' {
                break;
            }
            if chars[i] != ';' {
                i += 1;
                continue;
            }
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let ns = i;
            while i < chars.len()
                && !matches!(chars[i], '=' | ';' | ',')
                && !chars[i].is_whitespace()
            {
                i += 1;
            }
            let name: String = chars[ns..i].iter().collect::<String>().to_ascii_lowercase();
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let mut val = String::new();
            if i < chars.len() && chars[i] == '=' {
                i += 1;
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                if i < chars.len() && chars[i] == '"' {
                    i += 1;
                    while i < chars.len() && chars[i] != '"' {
                        if chars[i] == '\\' && i + 1 < chars.len() {
                            i += 1;
                        }
                        val.push(chars[i]);
                        i += 1;
                    }
                    i += 1;
                } else {
                    while i < chars.len() && !matches!(chars[i], ';' | ',') {
                        val.push(chars[i]);
                        i += 1;
                    }
                    val = val.trim_end().to_owned();
                }
            }
            if !name.is_empty() && !params.iter().any(|(n, _): &(String, String)| *n == name) {
                params.push((name, val));
            }
        }
        out.push(LinkValue { target, params });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_headers() {
        let links = super::parse_link_header(
            r#"</s.css>; rel=preload; as=style, <https://cdn.test/f.woff2>; rel="preload"; as=font; crossorigin, <https://a.test>; rel=preconnect"#,
        );
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].target, "/s.css");
        assert_eq!(
            links[0].params,
            vec![
                ("rel".into(), "preload".into()),
                ("as".into(), "style".into())
            ]
        );
        assert_eq!(links[1].params[2], ("crossorigin".into(), String::new()));
        assert_eq!(links[2].params[0].1, "preconnect");
        assert!(super::parse_link_header("garbage, <x>; rel=a").len() == 1);
    }

    use super::*;

    #[test]
    fn list_operations() {
        let mut h = HeaderList::new();
        h.append("X-A", "1");
        h.append("x-a", "2");
        h.append("B", "3");
        assert_eq!(h.get("x-A").as_deref(), Some("1, 2"));
        h.set("x-a", "9");
        assert_eq!(h.get("X-A").as_deref(), Some("9"));
        assert_eq!(h.len(), 2);
        h.append("Set-Cookie", "a=1");
        h.append("Set-Cookie", "b=2");
        assert_eq!(
            h.sort_and_combine(),
            vec![
                ("b".to_string(), "3".to_string()),
                ("set-cookie".to_string(), "a=1".to_string()),
                ("set-cookie".to_string(), "b=2".to_string()),
                ("x-a".to_string(), "9".to_string()),
            ]
        );
        h.delete("SET-COOKIE");
        assert!(!h.contains("set-cookie"));
    }

    #[test]
    fn guards() {
        let mut request = Headers::new(HeadersGuard::Request);
        request.append("Cookie", "a=b").unwrap();
        request.append("Sec-Fetch-Dest", "x").unwrap();
        request.append("Proxy-Foo", "x").unwrap();
        request.append("X-HTTP-Method-Override", "TRACE").unwrap();
        request.append("X-Fine", "  padded  ").unwrap();
        assert_eq!(request.list.len(), 1);
        assert_eq!(request.get("x-fine").unwrap().as_deref(), Some("padded"));
        assert!(request.append("Bad Name", "x").is_err());
        assert!(request.append("X", "a\nb").is_err());

        let mut immutable = Headers::new(HeadersGuard::Immutable);
        assert_eq!(immutable.append("X", "y"), Err(HeadersError::Immutable));

        let mut response = Headers::new(HeadersGuard::Response);
        response.append("Set-Cookie", "a=b").unwrap();
        assert!(response.list.is_empty());

        let mut no_cors = Headers::new(HeadersGuard::RequestNoCors);
        no_cors.append("Accept", "text/html").unwrap();
        no_cors.append("X-Custom", "1").unwrap();
        no_cors.append("Content-Type", "application/json").unwrap();
        no_cors.append("Content-Type", "text/plain").unwrap();
        no_cors.set("Range", "bytes=0-").unwrap();
        assert_eq!(no_cors.list.names(), vec!["accept", "content-type"]);
    }

    #[test]
    fn safelisting() {
        assert!(is_cors_safelisted_request_header("Accept", "*/*"));
        assert!(!is_cors_safelisted_request_header("Accept", "a(b"));
        assert!(is_cors_safelisted_request_header(
            "Content-Type",
            "text/plain;charset=utf-8"
        ));
        assert!(!is_cors_safelisted_request_header(
            "Content-Type",
            "application/json"
        ));
        assert!(is_cors_safelisted_request_header(
            "Accept-Language",
            "en-US,en;q=0.9"
        ));
        assert!(!is_cors_safelisted_request_header(
            "Accept-Language",
            "en_US"
        ));
        assert!(is_cors_safelisted_request_header("Range", "bytes=0-"));
        assert!(is_cors_safelisted_request_header("Range", "bytes=5-10"));
        assert!(!is_cors_safelisted_request_header("Range", "bytes=-5"));
        assert!(!is_cors_safelisted_request_header("Range", "bytes=10-5"));
        assert!(!is_cors_safelisted_request_header("X-Foo", "bar"));
        let long = "a".repeat(129);
        assert!(!is_cors_safelisted_request_header("Accept", &long));

        let mut h = HeaderList::new();
        h.append("X-B", "1");
        h.append("Accept", "*/*");
        h.append("x-a", "1");
        assert_eq!(cors_unsafe_request_header_names(&h), vec!["x-a", "x-b"]);
        let mut big = HeaderList::new();
        for _ in 0..9 {
            big.append("Accept", &"a".repeat(128));
        }
        assert_eq!(cors_unsafe_request_header_names(&big), vec!["accept"]);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_single_range("bytes=0-", false), Some((Some(0), None)));
        assert_eq!(
            parse_single_range("bytes = 1 - 2", true),
            Some((Some(1), Some(2)))
        );
        assert_eq!(parse_single_range("bytes = 1 - 2", false), None);
        assert_eq!(parse_single_range("bytes=-", false), None);
        assert_eq!(parse_single_range("items=1-2", false), None);
    }

    #[test]
    fn response_exposure() {
        let exposed = vec!["x-visible".to_string(), "set-cookie".to_string()];
        assert!(is_cors_safelisted_response_header_name("Content-Type", &[]));
        assert!(is_cors_safelisted_response_header_name(
            "X-Visible",
            &exposed
        ));
        assert!(!is_cors_safelisted_response_header_name(
            "Set-Cookie",
            &exposed
        ));
        assert!(!is_cors_safelisted_response_header_name(
            "X-Hidden", &exposed
        ));
    }
}
