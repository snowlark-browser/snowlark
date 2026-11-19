// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tokenizer side of HTML's speculative HTML parser (HTML §13.2.8): the same html5ever tokenizer
//! the document's parser uses, over a copy of the input after the parser's insertion point, without
//! a tree builder. It reports the start and end tags that can lead to a fetch (the page layer
//! decides which); the elements whose contents are not markup (`script`, `style`, `textarea`,
//! `title`, `noscript` with scripting enabled, ...) switch the tokenizer to their text state, as the
//! tree builder does.

use std::cell::RefCell;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    states::RawKind, BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use html5ever::{local_name, LocalName};

/// A tag the speculative parser met.
#[derive(Clone, Debug, PartialEq)]
pub struct ScannedTag {
    pub name: LocalName,
    pub end: bool,
    /// The attributes (start tags), in source order.
    pub attrs: Vec<(LocalName, String)>,
}

impl ScannedTag {
    /// The value of attribute `name`.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| &**n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The tags whose start or end the scanner reports.
fn reported(name: &LocalName) -> bool {
    matches!(
        *name,
        local_name!("img")
            | local_name!("source")
            | local_name!("picture")
            | local_name!("link")
            | local_name!("script")
            | local_name!("video")
            | local_name!("audio")
            | local_name!("base")
            | local_name!("meta")
            | local_name!("template")
    )
}

struct ScanSink {
    tags: RefCell<Vec<ScannedTag>>,
    /// Inside `svg` or `math` (foreign content: no raw text elements there).
    foreign: RefCell<u32>,
}

impl TokenSink for ScanSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let Token::TagToken(tag) = token else {
            return TokenSinkResult::Continue;
        };
        let start = tag.kind == TagKind::StartTag;
        if matches!(tag.name, local_name!("svg") | local_name!("math")) {
            let mut f = self.foreign.borrow_mut();
            if start && !tag.self_closing {
                *f += 1;
            } else if !start {
                *f = f.saturating_sub(1);
            }
            return TokenSinkResult::Continue;
        }
        if *self.foreign.borrow() > 0 {
            return TokenSinkResult::Continue;
        }
        if reported(&tag.name) {
            self.tags.borrow_mut().push(ScannedTag {
                name: tag.name.clone(),
                end: !start,
                attrs: if start {
                    tag.attrs
                        .iter()
                        .map(|a| (a.name.local.clone(), a.value.to_string()))
                        .collect()
                } else {
                    Vec::new()
                },
            });
        }
        if !start || tag.self_closing {
            return TokenSinkResult::Continue;
        }
        // The tree builder's tokenizer state switches (HTML §13.2.6.4.4 "in head" and §13.2.6.4.7
        // "in body"), with scripting enabled.
        match tag.name {
            local_name!("script") => TokenSinkResult::RawData(RawKind::ScriptData),
            local_name!("style")
            | local_name!("xmp")
            | local_name!("iframe")
            | local_name!("noembed")
            | local_name!("noframes")
            | local_name!("noscript") => TokenSinkResult::RawData(RawKind::Rawtext),
            local_name!("title") | local_name!("textarea") => {
                TokenSinkResult::RawData(RawKind::Rcdata)
            }
            local_name!("plaintext") => TokenSinkResult::Plaintext,
            _ => TokenSinkResult::Continue,
        }
    }
}

/// A speculative tokenizer: push the input text as it comes, take the tags found.
pub struct TagScanner {
    tokenizer: Tokenizer<ScanSink>,
    input: BufferQueue,
}

impl Default for TagScanner {
    fn default() -> Self {
        TagScanner::new()
    }
}

impl TagScanner {
    pub fn new() -> TagScanner {
        TagScanner {
            tokenizer: Tokenizer::new(
                ScanSink {
                    tags: RefCell::default(),
                    foreign: RefCell::default(),
                },
                TokenizerOpts::default(),
            ),
            input: BufferQueue::default(),
        }
    }

    /// Tokenizes `text` (after what came before) and returns the tags it completed.
    pub fn push(&mut self, text: &str) -> Vec<ScannedTag> {
        if !text.is_empty() {
            self.input.push_back(StrTendril::from_slice(text));
        }
        let _ = self.tokenizer.feed(&self.input);
        std::mem::take(&mut *self.tokenizer.sink.tags.borrow_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tags: &[ScannedTag]) -> Vec<String> {
        tags.iter()
            .map(|t| format!("{}{}", if t.end { "/" } else { "" }, t.name))
            .collect()
    }

    #[test]
    fn reports_fetching_tags_and_skips_text_contents() {
        let mut s = TagScanner::new();
        let tags = s.push(
            "<p><img src=a.png><script>var x = '<img src=no.png>';</script>\
             <noscript><img src=no2.png></noscript><textarea><img src=no3></textarea>\
             <svg><script href=x></script><image href=s.png/></svg>\
             <picture><source srcset=b.webp><img src=c.png></picture><link rel=stylesheet href=s.css>",
        );
        assert_eq!(
            names(&tags),
            ["img", "script", "/script", "picture", "source", "img", "/picture", "link"]
        );
        assert_eq!(tags[0].attr("src"), Some("a.png"));
        assert_eq!(tags[7].attr("href"), Some("s.css"));
    }

    #[test]
    fn resumes_across_chunks() {
        let mut s = TagScanner::new();
        assert!(s.push("<img sr").is_empty());
        let tags = s.push("c=\"a.png\"><script src=b.js></scr");
        assert_eq!(names(&tags), ["img", "script"]);
        assert_eq!(tags[0].attr("src"), Some("a.png"));
        let tags = s.push("ipt><img src=c.png>");
        assert_eq!(names(&tags), ["/script", "img"]);
    }
}
