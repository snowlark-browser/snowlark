// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `data:` URLs (Fetch §4.? "data: URL processor") and Infra's forgiving-base64 decode.

use base64::engine::general_purpose::GeneralPurpose;
use base64::engine::{DecodePaddingMode, GeneralPurposeConfig};
use base64::Engine;
use url::Url;

use crate::mime::MimeType;

/// A processed `data:` URL: its MIME type and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataUrl {
    pub mime_type: MimeType,
    pub body: Vec<u8>,
}

/// URL Standard "percent-decode" of a byte sequence.
pub fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'%' && i + 2 < input.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(input[i + 1]), hex(input[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    out
}

const FORGIVING: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone),
);

/// Infra "forgiving-base64 decode".
pub fn forgiving_base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut data: String = input
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' '))
        .collect();
    if data.len().is_multiple_of(4) {
        if data.ends_with("==") {
            data.truncate(data.len() - 2);
        } else if data.ends_with('=') {
            data.truncate(data.len() - 1);
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }
    if !data
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
    {
        return None;
    }
    FORGIVING.decode(data.as_bytes()).ok()
}

/// The data: URL processor. `None` is failure (a network error to Fetch).
pub fn process_data_url(url: &Url) -> Option<DataUrl> {
    if url.scheme() != "data" {
        return None;
    }
    let mut serialized = url.as_str();
    if let Some(hash) = serialized.find('#') {
        serialized = &serialized[..hash];
    }
    let input = &serialized["data:".len()..];
    let comma = input.find(',')?;
    let mut mime_type = input[..comma]
        .trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0C')
        .to_string();
    let encoded_body = &input[comma + 1..];
    let mut body = percent_decode(encoded_body.as_bytes());
    if let Some(stripped) = strip_base64_suffix(&mime_type) {
        let string_body = crate::headers::isomorphic_decode(&body);
        body = forgiving_base64_decode(&string_body)?;
        mime_type = stripped;
    }
    if mime_type.starts_with(';') {
        mime_type.insert_str(0, "text/plain");
    }
    let mime_type = MimeType::parse(&mime_type)
        .unwrap_or_else(|| MimeType::parse("text/plain;charset=US-ASCII").unwrap());
    Some(DataUrl { mime_type, body })
}

/// If the MIME type ends in ';' + spaces + "base64" (ASCII case-insensitive), the type without that suffix.
fn strip_base64_suffix(mime: &str) -> Option<String> {
    let lower = mime.to_ascii_lowercase();
    let without = lower.strip_suffix("base64")?;
    let trimmed = without.trim_end_matches(' ');
    if !trimmed.ends_with(';') {
        return None;
    }
    let mut kept = mime[..mime.len() - 6].trim_end_matches(' ').to_string();
    kept.pop(); // the ';'
    Some(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &str) -> Option<(String, Vec<u8>)> {
        process_data_url(&Url::parse(s).unwrap()).map(|d| (d.mime_type.serialize(), d.body))
    }

    // Cases from wpt/fetch/data-urls/resources/data-urls.json.
    #[test]
    fn wpt_cases() {
        assert_eq!(
            run("data://test/,X"),
            Some(("text/plain;charset=US-ASCII".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:,X"),
            Some(("text/plain;charset=US-ASCII".into(), b"X".to_vec()))
        );
        assert_eq!(run("data:"), None);
        assert_eq!(run("data:text/html"), None);
        assert_eq!(
            run("data:text/html    ;charset=x   ,"),
            Some(("text/html;charset=x".into(), vec![]))
        );
        assert_eq!(
            run("data:,%FF"),
            Some(("text/plain;charset=US-ASCII".into(), vec![0xFF]))
        );
        assert_eq!(
            run("data:text/plain,X#frag"),
            Some(("text/plain".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:;base64,WA"),
            Some(("text/plain;charset=US-ASCII".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:;base64 ,WA"),
            Some(("text/plain;charset=US-ASCII".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:x/x;base64;base64,WA"),
            Some(("x/x".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:;base64,W%20A"),
            Some(("text/plain;charset=US-ASCII".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:;base64,W%0CA"),
            Some(("text/plain;charset=US-ASCII".into(), b"X".to_vec()))
        );
        assert_eq!(run("data:;base64,=WA"), None);
        assert_eq!(
            run("data:x;base64,WA"),
            Some(("text/plain;charset=US-ASCII".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:;charset=x,X"),
            Some(("text/plain;charset=x".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:;CHARSET=\"X\",X"),
            Some(("text/plain;charset=X".into(), b"X".to_vec()))
        );
        assert_eq!(
            run("data:;base64;,WA"),
            Some(("text/plain".into(), b"WA".to_vec()))
        );
    }

    #[test]
    fn forgiving_base64() {
        assert_eq!(forgiving_base64_decode("YQ=="), Some(b"a".to_vec()));
        assert_eq!(forgiving_base64_decode("YQ"), Some(b"a".to_vec()));
        assert_eq!(forgiving_base64_decode("YR"), Some(b"a".to_vec()));
        assert_eq!(forgiving_base64_decode("Y"), None);
        assert_eq!(forgiving_base64_decode("YQ="), None);
        assert_eq!(forgiving_base64_decode("Y Q = ="), Some(b"a".to_vec()));
        assert_eq!(forgiving_base64_decode("ab-c"), None);
        assert_eq!(forgiving_base64_decode(""), Some(vec![]));
    }
}
