// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Character encodings: `encoding_rs` for the Encoding Standard's encodings, plus HTML §13.2.3
//! "determining the character encoding" (BOM, then the transport's charset, then a `<meta>` in the
//! document's head, then the parent's encoding, then the default), and the Encoding Standard's
//! decode helpers.

use std::borrow::Cow;

pub use encoding_rs::Encoding;
use encoding_rs::{UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252, X_USER_DEFINED};

use crate::headers::HeaderList;
use crate::mime::extract_mime_type;

/// HTML's confidence in an encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// From a BOM or the transport layer: a `<meta>` met later can't change it.
    Certain,
    /// From the prescan, the parent, or the default: the parser may still change it.
    Tentative,
}

/// Encoding Standard "get an encoding" from a label.
pub fn encoding_for_label(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label(label.as_bytes())
}

/// Encoding Standard "BOM sniff".
pub fn bom_sniff(bytes: &[u8]) -> Option<&'static Encoding> {
    Encoding::for_bom(bytes).map(|(e, _)| e)
}

/// Fetch "legacy extract an encoding": the `charset` of the response's MIME type, as an encoding.
pub fn legacy_extract_encoding(headers: &HeaderList) -> Option<&'static Encoding> {
    let mime = extract_mime_type(headers)?;
    encoding_for_label(mime.parameter("charset")?)
}

/// The inputs of HTML's encoding determination.
#[derive(Clone, Copy, Debug)]
pub struct SniffInput<'a> {
    /// The first bytes of the document (at least the first 1024 when available).
    pub bytes: &'a [u8],
    /// The `charset` parameter of the Content-Type, if any.
    pub transport_charset: Option<&'a str>,
    /// The encoding of a same-origin parent document (step 5), if any.
    pub parent: Option<&'static Encoding>,
    /// Step 7's autodetection: pick UTF-8 when the bytes are valid UTF-8 and not plain ASCII.
    pub detect_utf8: bool,
    /// The locale default (step 8): windows-1252 for English locales.
    pub default: &'static Encoding,
}

impl<'a> SniffInput<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        SniffInput {
            bytes,
            transport_charset: None,
            parent: None,
            detect_utf8: true,
            default: WINDOWS_1252,
        }
    }
}

/// HTML §13.2.3.1 "determining the character encoding", with the `<meta>` declaration found by a
/// [`MetaScanner`] of `input.bytes`.
pub fn sniff_html_encoding(input: &SniffInput<'_>) -> (&'static Encoding, Confidence) {
    sniff_html_encoding_with(input, &mut MetaScanner::default())
}

/// [`sniff_html_encoding`] that keeps its `<meta>` scan in `scan`: when the scan is still running
/// afterwards ([`MetaScanner::running`]), a declaration may come in the bytes after `input.bytes`,
/// which the caller gives it (HTML §13.2.3.4 "change the encoding").
pub fn sniff_html_encoding_with(
    input: &SniffInput<'_>,
    scan: &mut MetaScanner,
) -> (&'static Encoding, Confidence) {
    if let Some(e) = bom_sniff(input.bytes) {
        scan.stop();
        return (e, Confidence::Certain);
    }
    if let Some(e) = input.transport_charset.and_then(encoding_for_label) {
        scan.stop();
        return (e, Confidence::Certain);
    }
    if let MetaScan::Found(e, _) = scan.feed(input.bytes) {
        return (e, Confidence::Tentative);
    }
    if let Some(parent) = input.parent {
        if parent != UTF_16BE && parent != UTF_16LE && parent != encoding_rs::REPLACEMENT {
            return (parent, Confidence::Tentative);
        }
    }
    if input.detect_utf8 && !input.bytes.is_ascii() && std::str::from_utf8(input.bytes).is_ok() {
        return (UTF_8, Confidence::Tentative);
    }
    (input.default, Confidence::Tentative)
}

fn is_space(b: u8) -> bool {
    matches!(b, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

/// HTML "get an attribute" (prescan). `None` when there are no more attributes, or the input ran out.
fn get_attribute(bytes: &[u8], pos: &mut usize) -> Option<(Vec<u8>, Vec<u8>)> {
    while *pos < bytes.len() && (is_space(bytes[*pos]) || bytes[*pos] == b'/') {
        *pos += 1;
    }
    if *pos >= bytes.len() || bytes[*pos] == b'>' {
        return None;
    }
    let mut name = Vec::new();
    let mut value = Vec::new();
    // name
    loop {
        let b = *bytes.get(*pos)?;
        if b == b'=' && !name.is_empty() {
            *pos += 1;
            break;
        }
        if is_space(b) {
            // spaces, then maybe '='
            while *pos < bytes.len() && is_space(bytes[*pos]) {
                *pos += 1;
            }
            if bytes.get(*pos)? != &b'=' {
                return Some((name, value));
            }
            *pos += 1;
            break;
        }
        if b == b'/' || b == b'>' {
            return Some((name, value));
        }
        name.push(b.to_ascii_lowercase());
        *pos += 1;
    }
    // value
    while *pos < bytes.len() && is_space(bytes[*pos]) {
        *pos += 1;
    }
    let b = *bytes.get(*pos)?;
    if b == b'"' || b == b'\'' {
        let quote = b;
        loop {
            *pos += 1;
            let c = *bytes.get(*pos)?;
            if c == quote {
                *pos += 1;
                return Some((name, value));
            }
            value.push(c.to_ascii_lowercase());
        }
    }
    if b == b'>' {
        return Some((name, value));
    }
    loop {
        let c = *bytes.get(*pos)?;
        if is_space(c) || c == b'>' {
            return Some((name, value));
        }
        value.push(c.to_ascii_lowercase());
        *pos += 1;
    }
}

/// HTML "algorithm for extracting a character encoding from a meta element" (its `content` value).
pub fn encoding_from_meta_content(content: &[u8]) -> Option<&'static Encoding> {
    let mut pos = 0;
    loop {
        let rest = &content[pos..];
        let found = rest
            .windows(7)
            .position(|w| w.eq_ignore_ascii_case(b"charset"))?;
        pos += found + 7;
        while pos < content.len() && is_space(content[pos]) {
            pos += 1;
        }
        if content.get(pos) != Some(&b'=') {
            continue;
        }
        pos += 1;
        while pos < content.len() && is_space(content[pos]) {
            pos += 1;
        }
        let first = *content.get(pos)?;
        if first == b'"' || first == b'\'' {
            let start = pos + 1;
            let end = content[start..].iter().position(|&b| b == first)?;
            return Encoding::for_label(&content[start..start + end]);
        }
        let start = pos;
        let end = content[start..]
            .iter()
            .position(|&b| is_space(b) || b == b';')
            .map_or(content.len(), |e| start + e);
        return Encoding::for_label(&content[start..end]);
    }
}

/// The encoding a `<meta>` start tag declares (the prescan's `<meta` step, HTML §13.2.3.2), its
/// attributes read from `pos` (just after the tag name) to its '>', where `pos` is left: `None` when
/// the input ends first, `Some(None)` when the tag declares none.
fn meta_declaration(bytes: &[u8], pos: &mut usize) -> Option<Option<&'static Encoding>> {
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut got_pragma = false;
    let mut need_pragma: Option<bool> = None;
    let mut charset: Option<&'static Encoding> = None;
    loop {
        let Some((name, value)) = get_attribute(bytes, pos) else {
            if *pos >= bytes.len() {
                return None;
            }
            break;
        };
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        match name.as_slice() {
            b"http-equiv" => {
                if value == b"content-type" {
                    got_pragma = true;
                }
            }
            b"content" => {
                if charset.is_none() {
                    if let Some(e) = encoding_from_meta_content(&value) {
                        charset = Some(e);
                        need_pragma = Some(true);
                    }
                }
            }
            b"charset" => {
                charset = Encoding::for_label(&value);
                need_pragma = Some(false);
            }
            _ => {}
        }
    }
    let decided = match need_pragma {
        None => None,
        Some(true) if !got_pragma => None,
        _ => charset,
    };
    Some(decided.map(|e| {
        if e == UTF_16BE || e == UTF_16LE {
            UTF_8
        } else if e == X_USER_DEFINED {
            WINDOWS_1252
        } else {
            e
        }
    }))
}

/// HTML §13.2.3.2 "prescan a byte stream to determine its encoding".
pub fn prescan(bytes: &[u8]) -> Option<&'static Encoding> {
    let mut pos = 0;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        if rest.starts_with(b"<!--") {
            // the first '>' preceded by "--" after the '<' (the dashes may be those of "<!--")
            let mut i = pos + 2;
            loop {
                i += 1;
                if i >= bytes.len() {
                    return None;
                }
                if bytes[i] == b'>' && bytes[i - 1] == b'-' && bytes[i - 2] == b'-' {
                    break;
                }
            }
            pos = i;
        } else if rest.len() >= 6
            && rest[..5].eq_ignore_ascii_case(b"<meta")
            && (is_space(rest[5]) || rest[5] == b'/')
        {
            pos += 5;
            if let Some(e) = meta_declaration(bytes, &mut pos)? {
                return Some(e);
            }
        } else if rest.len() >= 2
            && rest[0] == b'<'
            && (rest[1].is_ascii_alphabetic()
                || (rest[1] == b'/' && rest.get(2).is_some_and(u8::is_ascii_alphabetic)))
        {
            while pos < bytes.len() && !is_space(bytes[pos]) && bytes[pos] != b'>' {
                pos += 1;
            }
            while get_attribute(bytes, &mut pos).is_some() {}
            if pos >= bytes.len() {
                return None;
            }
        } else if rest.starts_with(b"<!") || rest.starts_with(b"</") || rest.starts_with(b"<?") {
            pos += 1 + bytes[pos + 1..].iter().position(|&b| b == b'>')?;
        }
        pos += 1;
    }
    None
}

/// Where a [`MetaScanner`] stands after the bytes it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaScan {
    /// A `<meta>` declares this encoding; its tag ends at this offset in the bytes of the call
    /// that found it.
    Found(&'static Encoding, usize),
    /// The scan ended without one.
    NotFound,
    /// The scan needs the bytes that follow.
    NeedMore,
}

/// The `<meta>` element's encoding declaration in a document's head, found by a scan of its bytes
/// as they arrive. It replaces the prescan of the first 1024 bytes (HTML §13.2.3.2) with what the
/// engines in use do and the web relies on (whatwg/html#6962; the cases are WPT's
/// `html/syntax/charset`): the scan reads tags as the tokenizer does, so a `<meta>` in a comment or
/// in the text of `script`, `style`, `title` or `textarea` does not count; it goes on past the first
/// 1024 bytes while only elements that belong in a head have been seen, so a declaration after a
/// long head still counts (HTML §13.2.3.4 "change the encoding"), and it ends at the first of the
/// first 1024 bytes that comes after another element.
#[derive(Debug, Default)]
pub struct MetaScanner {
    /// The bytes from the first token not scanned yet.
    buf: Vec<u8>,
    /// The bytes given before `buf`.
    consumed: usize,
    /// An element that does not belong in a head was seen.
    left_head: bool,
    /// Inside the text of this raw text element: the end tag's name.
    raw: Option<&'static [u8]>,
    done: bool,
}

/// Elements whose text the tokenizer does not read as markup (HTML §13.2.6.4.7 and the states it
/// switches to for them); `noscript` is not one, as for a parser with scripting disabled.
const RAW_TEXT_ELEMENTS: [&[u8]; 8] = [
    b"script",
    b"style",
    b"title",
    b"textarea",
    b"xmp",
    b"iframe",
    b"noembed",
    b"noframes",
];

/// Elements that keep the scan in the head.
const HEAD_ELEMENTS: [&[u8]; 8] = [
    b"script",
    b"noscript",
    b"style",
    b"link",
    b"meta",
    b"object",
    b"title",
    b"base",
];

impl MetaScanner {
    /// Whether the scan goes on (it has neither found a declaration nor ended).
    pub fn running(&self) -> bool {
        !self.done
    }

    /// Ends the scan (the encoding came from elsewhere, or the input ended).
    pub fn stop(&mut self) {
        self.done = true;
        self.buf = Vec::new();
    }

    /// Scans `bytes`, which follow those of earlier calls.
    pub fn feed(&mut self, bytes: &[u8]) -> MetaScan {
        if self.done {
            return MetaScan::NotFound;
        }
        let before = self.consumed + self.buf.len();
        self.buf.extend_from_slice(bytes);
        let result = self.scan();
        match result {
            MetaScan::Found(e, end) => {
                self.stop();
                MetaScan::Found(e, end.saturating_sub(before))
            }
            MetaScan::NotFound => {
                self.stop();
                MetaScan::NotFound
            }
            MetaScan::NeedMore => MetaScan::NeedMore,
        }
    }

    /// Scans `buf`; a found tag's end is given as an offset in all the bytes scanned.
    fn scan(&mut self) -> MetaScan {
        let mut pos = 0;
        let result = loop {
            let bytes = &self.buf[..];
            if let Some(name) = self.raw {
                // the end tag of the raw text element
                let end = (pos..bytes.len()).find(|&i| {
                    bytes[i] == b'<'
                        && bytes.get(i + 1) == Some(&b'/')
                        && bytes
                            .get(i + 2..i + 2 + name.len())
                            .is_some_and(|n| n.eq_ignore_ascii_case(name))
                        && bytes
                            .get(i + 2 + name.len())
                            .is_some_and(|&b| is_space(b) || b == b'/' || b == b'>')
                });
                match end {
                    Some(i) => {
                        self.raw = None;
                        pos = i;
                    }
                    None => {
                        // keep what could be the start of the end tag
                        pos = pos.max(bytes.len().saturating_sub(name.len() + 2));
                        break MetaScan::NeedMore;
                    }
                }
                continue;
            }
            if self.left_head && self.consumed + pos >= 1024 {
                break MetaScan::NotFound;
            }
            if pos >= bytes.len() {
                break MetaScan::NeedMore;
            }
            let rest = &bytes[pos..];
            if rest[0] != b'<' {
                pos += rest.iter().position(|&b| b == b'<').unwrap_or(rest.len());
                continue;
            }
            if rest.len() < 4 {
                break MetaScan::NeedMore;
            }
            if rest.starts_with(b"<!--") {
                // the first '>' preceded by "--" after the '<' (the dashes may be those of "<!--")
                let Some(i) = (pos + 3..bytes.len())
                    .find(|&i| bytes[i] == b'>' && bytes[i - 1] == b'-' && bytes[i - 2] == b'-')
                else {
                    break MetaScan::NeedMore;
                };
                pos = i + 1;
            } else if rest[1].is_ascii_alphabetic()
                || (rest[1] == b'/' && rest[2].is_ascii_alphabetic())
            {
                let end_tag = rest[1] == b'/';
                let start = pos + if end_tag { 2 } else { 1 };
                let mut i = start;
                while i < bytes.len() && !is_space(bytes[i]) && bytes[i] != b'/' && bytes[i] != b'>'
                {
                    i += 1;
                }
                let name = bytes[start..i].to_ascii_lowercase();
                let decided = if !end_tag && name == b"meta" {
                    match meta_declaration(bytes, &mut i) {
                        Some(d) => d,
                        None => break MetaScan::NeedMore,
                    }
                } else {
                    while get_attribute(bytes, &mut i).is_some() {}
                    if i >= bytes.len() {
                        break MetaScan::NeedMore;
                    }
                    None
                };
                pos = i + 1;
                if let Some(e) = decided {
                    break MetaScan::Found(e, self.consumed + pos);
                }
                let in_head = HEAD_ELEMENTS.contains(&name.as_slice())
                    || (!end_tag && (name == b"html" || name == b"head"));
                if !in_head {
                    self.left_head = true;
                }
                if !end_tag {
                    self.raw = RAW_TEXT_ELEMENTS
                        .iter()
                        .find(|&&r| r == name.as_slice())
                        .copied();
                }
            } else if rest[1] == b'!' || rest[1] == b'/' || rest[1] == b'?' {
                // a bogus comment or a DOCTYPE, to the first '>'
                let Some(i) = rest.iter().position(|&b| b == b'>') else {
                    break MetaScan::NeedMore;
                };
                pos += i + 1;
            } else {
                pos += 1;
            }
        };
        let scanned = pos.min(self.buf.len());
        self.buf.drain(..scanned);
        self.consumed += scanned;
        result
    }
}

/// Encoding Standard "decode": a BOM overrides `fallback`; errors become U+FFFD.
pub fn decode<'a>(
    bytes: &'a [u8],
    fallback: &'static Encoding,
) -> (Cow<'a, str>, &'static Encoding) {
    let (text, used, _) = fallback.decode(bytes);
    (text, used)
}

/// Encoding Standard "UTF-8 decode": a UTF-8 BOM is stripped, errors become U+FFFD.
pub fn utf8_decode(bytes: &[u8]) -> Cow<'_, str> {
    UTF_8.decode_with_bom_removal(bytes).0
}

/// Encoding Standard "UTF-8 decode without BOM".
pub fn utf8_decode_without_bom(bytes: &[u8]) -> Cow<'_, str> {
    UTF_8.decode_without_bom_handling(bytes).0
}

/// Encoding Standard "UTF-8 decode without BOM or fail".
pub fn utf8_decode_without_bom_or_fail(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

/// Decode a response body as text the way `Response.text()` and XHR's text do for a given type: the
/// body's charset when it names one, UTF-8 otherwise, a BOM overriding both.
pub fn decode_body<'a>(bytes: &'a [u8], headers: &HeaderList) -> Cow<'a, str> {
    let fallback = legacy_extract_encoding(headers).unwrap_or(UTF_8);
    decode(bytes, fallback).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sniff(bytes: &[u8], charset: Option<&str>) -> (&'static str, Confidence) {
        let mut input = SniffInput::new(bytes);
        input.transport_charset = charset;
        let (e, c) = sniff_html_encoding(&input);
        (e.name(), c)
    }

    #[test]
    fn order() {
        use Confidence::*;
        assert_eq!(
            sniff(b"\xEF\xBB\xBF<meta charset=gbk>", Some("big5")),
            ("UTF-8", Certain)
        );
        assert_eq!(sniff(b"\xFF\xFEa\0", None), ("UTF-16LE", Certain));
        assert_eq!(
            sniff(b"<meta charset=gbk>", Some("Shift_JIS")),
            ("Shift_JIS", Certain)
        );
        assert_eq!(
            sniff(b"<meta charset=gbk>", Some("bogus")),
            ("GBK", Tentative)
        );
        assert_eq!(sniff(b"<p>plain ascii", None), ("windows-1252", Tentative));
        assert_eq!(sniff("<p>caf\u{e9}".as_bytes(), None), ("UTF-8", Tentative));
        assert_eq!(sniff(b"<p>caf\xE9", None), ("windows-1252", Tentative));
        // latin1 is windows-1252
        assert_eq!(sniff(b"", Some("ISO-8859-1")), ("windows-1252", Certain));
        let mut input = SniffInput::new(b"<p>x");
        input.parent = Some(encoding_rs::SHIFT_JIS);
        assert_eq!(sniff_html_encoding(&input).0.name(), "Shift_JIS");
        input.parent = Some(UTF_16LE);
        assert_eq!(sniff_html_encoding(&input).0.name(), "windows-1252");
    }

    #[test]
    fn prescan_cases() {
        let p = |s: &[u8]| prescan(s).map(|e| e.name());
        assert_eq!(p(b"<meta charset=\"koi8-r\">"), Some("KOI8-R"));
        assert_eq!(p(b"<META CHARSET='ISO-8859-2'>"), Some("ISO-8859-2"));
        assert_eq!(
            p(b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=euc-kr\">"),
            Some("EUC-KR")
        );
        // content without the pragma doesn't count
        assert_eq!(p(b"<meta content=\"text/html; charset=euc-kr\">"), None);
        // pragma after content works too
        assert_eq!(
            p(b"<meta content='text/html; charset=gb2312' http-equiv=content-type>"),
            Some("GBK")
        );
        // comments are skipped, including "<!-->"
        assert_eq!(
            p(b"<!-- <meta charset=big5> --><meta charset=utf-8>"),
            Some("UTF-8")
        );
        assert_eq!(p(b"<!--><meta charset=big5>"), Some("Big5"));
        // attributes of other tags are skipped
        assert_eq!(
            p(b"<div title='<meta charset=big5>'><meta charset=windows-1251>"),
            Some("windows-1251")
        );
        // utf-16 in a meta means utf-8; x-user-defined means windows-1252
        assert_eq!(p(b"<meta charset=utf-16>"), Some("UTF-8"));
        assert_eq!(p(b"<meta charset=x-user-defined>"), Some("windows-1252"));
        // the first charset attribute wins
        assert_eq!(p(b"<meta charset=big5 charset=gbk>"), Some("Big5"));
        assert_eq!(p(b"<meta/charset=gbk>"), Some("GBK"));
        assert_eq!(p(b"<metacharset=gbk>"), None);
        assert_eq!(p(b"<meta charset=bogus><meta charset=gbk>"), Some("GBK"));
        assert_eq!(
            p(b"<?xml version='1.0' encoding='bogus'?><meta charset=gbk>"),
            Some("GBK")
        );
        assert_eq!(p(b"<meta charset=\"unterminated"), None);
    }

    /// The cases of WPT `html/syntax/charset`: where a `<meta>` declaration counts.
    #[test]
    fn meta_scan_cases() {
        let pad = " ".repeat(1100);
        let found = |doc: String| {
            let mut s = MetaScanner::default();
            match s.feed(doc.as_bytes()) {
                MetaScan::Found(e, _) => Some(e.name()),
                _ => None,
            }
        };
        let m = "<meta charset=windows-1251>";
        let cases: [(String, Option<&str>); 17] = [
            (format!("<!doctype html><head>{m}"), Some("windows-1251")),
            // in the head after the first kilobyte
            (
                format!("<head><link rel=x>{pad}{m}</head>"),
                Some("windows-1251"),
            ),
            (
                format!("<head><script></script>{pad}{m}"),
                Some("windows-1251"),
            ),
            // after another element: within the first kilobyte only
            (format!("<head><bogus>{m}"), Some("windows-1251")),
            (format!("<head>{pad}<bogus>{m}"), None),
            (format!("<head></head>{m}"), Some("windows-1251")),
            (format!("<head>{pad}</head>{m}"), None),
            (format!("<head>{pad}<template>{m}</template>"), None),
            (
                format!("<head>{pad}<noscript><template></template>{m}"),
                None,
            ),
            (format!("<head><object>{m}</object>"), Some("windows-1251")),
            (format!("<head></head><body><svg>{m}"), Some("windows-1251")),
            // markup that is not a tag
            (format!("<head><!--{m}-->"), None),
            (format!("<head><script>{m}</script>"), None),
            (format!("<head><style>{m}</style>"), None),
            (format!("<head><title>{m}</title>"), None),
            (format!("<svg><![CDATA[{m}]]></svg>"), None),
            (
                format!("<head><title>\u{e9}</title>{m}"),
                Some("windows-1251"),
            ),
        ];
        for (doc, want) in cases {
            assert_eq!(found(doc.clone()), want, "{}", doc.trim());
        }
        // the end of a raw text element ends at its own end tag only
        assert_eq!(
            found(format!("<script>'</style>{m}'</SCRIPT >{m}")),
            Some("windows-1251")
        );
    }

    /// Bytes arriving in pieces: the scan waits for the rest of a tag, and says where in the last
    /// piece the declaring tag ends.
    #[test]
    fn meta_scan_in_pieces() {
        let doc = format!(
            "<!doctype html><head><script>{}</script><meta charset=\"utf-8\"><title>x</title>",
            "/* a long inline script */".repeat(60)
        );
        let bytes = doc.as_bytes();
        let end = doc.find("<title>").unwrap();
        for cut in [1, 1024, end - 20, end - 1] {
            let mut s = MetaScanner::default();
            let mut got = None;
            for (i, piece) in [&bytes[..cut], &bytes[cut..]].into_iter().enumerate() {
                match s.feed(piece) {
                    MetaScan::NeedMore => {}
                    MetaScan::Found(e, at) => got = Some((i, e.name(), at)),
                    MetaScan::NotFound => panic!("not found, cut at {cut}"),
                }
            }
            assert_eq!(got, Some((1, "UTF-8", end - cut)), "cut at {cut}");
            assert!(!s.running());
        }
        // the scan ends at the first of the first kilobyte after the head
        let mut s = MetaScanner::default();
        assert_eq!(s.feed(b"<head></head><body>"), MetaScan::NeedMore);
        assert_eq!(s.feed(" ".repeat(1100).as_bytes()), MetaScan::NotFound);
        assert!(!s.running());
        // a document's sniffing keeps the scan running for the bytes after its first ones
        let mut s = MetaScanner::default();
        let head = format!("<head><link rel=x>{}", " ".repeat(1100));
        let (e, _) = sniff_html_encoding_with(&SniffInput::new(head.as_bytes()), &mut s);
        assert_eq!(e.name(), "windows-1252");
        assert!(s.running());
        assert_eq!(s.feed(b"<meta charset=utf-8>"), MetaScan::Found(UTF_8, 20));
    }

    #[test]
    fn meta_content() {
        let e = |s: &[u8]| encoding_from_meta_content(s).map(|e| e.name());
        assert_eq!(e(b"text/html; charset=utf-8"), Some("UTF-8"));
        assert_eq!(e(b"text/html; CHARSET = \"shift_jis\""), Some("Shift_JIS"));
        assert_eq!(e(b"charset='unterminated"), None);
        assert_eq!(e(b"charsetx charset=gbk"), Some("GBK"));
        assert_eq!(e(b"text/html"), None);
    }

    #[test]
    fn decode_helpers() {
        assert_eq!(utf8_decode(b"\xEF\xBB\xBFhi"), "hi");
        assert_eq!(utf8_decode_without_bom(b"\xEF\xBB\xBFhi"), "\u{FEFF}hi");
        assert_eq!(utf8_decode(b"a\xFFb"), "a\u{FFFD}b");
        assert!(utf8_decode_without_bom_or_fail(b"a\xFF").is_none());
        let (text, used) = decode(b"caf\xE9", WINDOWS_1252);
        assert_eq!((text.as_ref(), used.name()), ("caf\u{e9}", "windows-1252"));
        let (text, used) = decode(b"\xFE\xFF\0a", WINDOWS_1252);
        assert_eq!((text.as_ref(), used.name()), ("a", "UTF-16BE"));
        let mut h = HeaderList::new();
        h.append("Content-Type", "text/plain; charset=iso-8859-1");
        assert_eq!(decode_body(b"\xE9", &h), "\u{e9}");
        assert_eq!(
            decode_body("\u{e9}".as_bytes(), &HeaderList::new()),
            "\u{e9}"
        );
    }
}
