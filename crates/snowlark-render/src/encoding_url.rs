// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §2.4.2 "encoding-parsing a URL": the URL parser with a document's character encoding for
//! the query (the page's element attributes and the renderer's image sources share it).

use url::Url;

/// "Encoding-parse a URL" `input` against `base`, with `encoding` (a document's character
/// encoding) for the query; `None` when it does not parse.
pub fn encoding_parse_url(
    base: &Url,
    encoding: &'static encoding_rs::Encoding,
    input: &str,
) -> Option<Url> {
    let encoding = encoding.output_encoding();
    if encoding == encoding_rs::UTF_8 {
        return base.join(input).ok();
    }
    let encode: &dyn Fn(&str) -> std::borrow::Cow<'_, [u8]> =
        &|s| encode_or_percent_escape(encoding, s).into();
    Url::options()
        .base_url(Some(base))
        .encoding_override(Some(encode))
        .parse(input)
        .ok()
}

/// URL Standard "percent-encode after encoding", for a query: `input` in `encoding`, with each code
/// point it cannot represent written as `%26%23`, the code point in decimal, and `%3B`. Those bytes
/// are outside the query percent-encode sets, so the URL parser keeps them as they are.
fn encode_or_percent_escape(encoding: &'static encoding_rs::Encoding, input: &str) -> Vec<u8> {
    use encoding_rs::EncoderResult;
    let mut encoder = encoding.new_encoder();
    let mut out = Vec::with_capacity(input.len());
    let mut buffer = [0u8; 256];
    let mut rest = input;
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(rest, &mut buffer, true);
        out.extend_from_slice(&buffer[..written]);
        rest = &rest[read..];
        match result {
            EncoderResult::InputEmpty => return out,
            EncoderResult::OutputFull => {}
            EncoderResult::Unmappable(c) => {
                out.extend_from_slice(format!("%26%23{}%3B", c as u32).as_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_takes_the_documents_encoding() {
        let base = Url::parse("http://a.test/dir/").unwrap();
        let parse = |e, s| encoding_parse_url(&base, e, s).unwrap().to_string();
        assert_eq!(
            parse(encoding_rs::WINDOWS_1252, "i.png?x=\u{df}"),
            "http://a.test/dir/i.png?x=%DF"
        );
        assert_eq!(
            parse(encoding_rs::UTF_8, "i.png?x=\u{df}"),
            "http://a.test/dir/i.png?x=%C3%9F"
        );
        // (the path is UTF-8 whatever the encoding; a code point it lacks is a character reference)
        assert_eq!(
            parse(encoding_rs::WINDOWS_1252, "\u{df}?\u{3b1}"),
            "http://a.test/dir/%C3%9F?%26%23945%3B"
        );
    }
}
