// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.2.5.3 the `refresh` pragma (`<meta http-equiv=refresh>`) and §7.5.1 the `Refresh`
//! header: the shared declarative refresh steps, and the navigation they make once the refresh
//! comes due.

use snowlark_bindings::rt::Cx;
use snowlark_dom::{local_name, ns, NodeId};

use crate::engine::frames::HistoryHandling;
use crate::engine::Engine;
use crate::event_loop::TimerHandler;

/// A document's declarative refresh, once the shared declarative refresh steps parsed one (HTML
/// "will declaratively refresh" is true from then on).
#[derive(Clone, Debug)]
pub struct DeclarativeRefresh {
    /// The delay, in seconds.
    seconds: u64,
    url: url::Url,
    /// The steps were given a `meta` element (not the `Refresh` header).
    meta: bool,
    /// Its timer is started: the document has completely loaded.
    started: bool,
}

/// HTML §ASCII whitespace.
fn is_ascii_whitespace(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// The parse of the shared declarative refresh steps (steps 2-11): the delay in seconds and the
/// URL string, `None` for the document's own URL; `None` altogether when `input` is not a refresh.
pub(crate) fn parse_declarative_refresh(input: &str) -> Option<(u64, Option<&str>)> {
    let b = input.as_bytes();
    let mut pos = 0;
    let skip_whitespace = |pos: &mut usize| {
        while *pos < b.len() && is_ascii_whitespace(b[*pos]) {
            *pos += 1;
        }
    };
    // 3-7: the time, from the leading ASCII digits ("rules for parsing non-negative integers").
    skip_whitespace(&mut pos);
    let start = pos;
    while pos < b.len() && b[pos].is_ascii_digit() {
        pos += 1;
    }
    let time = if pos == start {
        if b.get(pos) != Some(&b'.') {
            return None;
        }
        0
    } else {
        b[start..pos].iter().fold(0u64, |n, d| {
            n.saturating_mul(10).saturating_add(u64::from(d - b'0'))
        })
    };
    // 8: digits and full stops after it are ignored.
    while pos < b.len() && (b[pos].is_ascii_digit() || b[pos] == b'.') {
        pos += 1;
    }
    // 10: the separator.
    if pos < b.len() {
        if !matches!(b[pos], b';' | b',') && !is_ascii_whitespace(b[pos]) {
            return None;
        }
        skip_whitespace(&mut pos);
        if pos < b.len() && matches!(b[pos], b';' | b',') {
            pos += 1;
        }
        skip_whitespace(&mut pos);
    }
    if pos >= b.len() {
        return Some((time, None));
    }
    // 11: the URL, after an optional `url=` and an optional opening quote.
    let remainder = &input[pos..];
    let at = |pos: usize, c: u8| b.get(pos).is_some_and(|x| x.eq_ignore_ascii_case(&c));
    let quotes_at = if at(pos, b'u') {
        // ("url", whitespace, "=", whitespace; anything else and the remainder is the URL)
        if !at(pos + 1, b'r') || !at(pos + 2, b'l') {
            return Some((time, Some(remainder)));
        }
        let mut p = pos + 3;
        skip_whitespace(&mut p);
        if b.get(p) != Some(&b'=') {
            return Some((time, Some(remainder)));
        }
        p += 1;
        skip_whitespace(&mut p);
        p
    } else {
        pos
    };
    // "Skip quotes".
    let (quote, from) = match b.get(quotes_at) {
        Some(&q @ (b'\'' | b'"')) => (Some(q as char), quotes_at + 1),
        _ => (None, quotes_at),
    };
    let mut url = &input[from..];
    if let Some(i) = quote.and_then(|q| url.find(q)) {
        url = &url[..i];
    }
    Some((time, Some(url)))
}

impl Engine {
    /// HTML §4.2.5.3 the refresh state of `http-equiv`: a `meta` inserted into a document (in its
    /// document tree, not a shadow tree) with a non-empty `content` runs the shared declarative
    /// refresh steps with its node document.
    pub(crate) fn meta_refresh_inserted(&self, el: NodeId) {
        let (doc, input) = {
            let dom = self.dom();
            let refresh = dom
                .attr(el, &ns!(), &local_name!("http-equiv"))
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("refresh"));
            let doc = dom.node_document(el);
            if !refresh || dom.root(el) != doc {
                return;
            }
            match dom.attr(el, &ns!(), &local_name!("content")) {
                Some(c) if !c.is_empty() => (doc, c.to_owned()),
                _ => return,
            }
        };
        self.shared_declarative_refresh(doc, &input, true);
    }

    /// HTML "shared declarative refresh steps": the first refresh a document gets is the one it
    /// does, once it has completely loaded and the delay has passed (from then, or from the
    /// insertion of the `meta`, whichever is later).
    pub(crate) fn shared_declarative_refresh(&self, doc: NodeId, input: &str, meta: bool) {
        if self.doc_state(doc, |s| s.declarative_refresh.is_some()) {
            return;
        }
        let Some((seconds, url)) = parse_declarative_refresh(input) else {
            return;
        };
        let url = match url {
            Some(s) => match self.encoding_parse_url(doc, s) {
                Some(u) => u,
                None => return,
            },
            None => match url::Url::parse(self.dom().document_url(doc)) {
                Ok(u) => u,
                Err(_) => return,
            },
        };
        self.doc_state(doc, |s| {
            s.declarative_refresh = Some(DeclarativeRefresh {
                seconds,
                url,
                meta,
                started: false,
            })
        });
        if self.is_completely_loaded(doc) {
            self.start_declarative_refresh(doc);
        }
    }

    /// The document has completely loaded (or was already, when the steps ran): the refresh comes
    /// due after its delay.
    pub(crate) fn start_declarative_refresh(&self, doc: NodeId) {
        let Some(refresh) = self.doc_state(doc, |s| match &mut s.declarative_refresh {
            Some(r) if !r.started => {
                r.started = true;
                Some(r.clone())
            }
            _ => None,
        }) else {
            return;
        };
        // (a `meta` refresh in a document whose active sandboxing flag set has the sandboxed
        // automatic features browsing context flag does not navigate: `allow-scripts` lifts it)
        if refresh.meta && self.automatic_features_sandboxed(doc) {
            return;
        }
        let delay = refresh.seconds as f64 * 1000.0;
        self.start_timer(
            TimerHandler::Internal(Some(Box::new(move |e: &Engine, cx: &mut Cx| {
                e.declarative_refresh_due(cx, doc, refresh.url)
            }))),
            delay,
            false,
            None,
        );
    }

    /// The refresh came due: "navigate document's node navigable to urlRecord using document,
    /// with historyHandling set to `replace`", if the document is still its navigable's active
    /// document.
    fn declarative_refresh_due(&self, _cx: &mut Cx, doc: NodeId, url: url::Url) {
        if !self.dom().is_alive(doc) {
            return;
        }
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        if let Some(id) = self.context_of_document(doc) {
            self.stop_lazy_loading(id);
        }
        if !self.navigate_to_fragment(window, &url, true) {
            self.request_navigation(window, url, Some(doc), HistoryHandling::Replace);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_declarative_refresh as p;

    #[test]
    fn the_shared_declarative_refresh_parse() {
        assert_eq!(p(""), None);
        assert_eq!(p("1"), Some((1, None)));
        assert_eq!(p("1\x0c"), Some((1, None)));
        assert_eq!(p("1,"), Some((1, None)));
        assert_eq!(p("1; url=foo"), Some((1, Some("foo"))));
        assert_eq!(p("1\turl=foo"), Some((1, Some("foo"))));
        assert_eq!(p("1url=foo"), None);
        assert_eq!(p("1x;url=foo"), None);
        assert_eq!(p("1 x;url=foo"), Some((1, Some("x;url=foo"))));
        assert_eq!(p("1;;url=foo"), Some((1, Some(";url=foo"))));
        assert_eq!(p("  1  ;  url  =  foo"), Some((1, Some("foo"))));
        assert_eq!(p("  1  url  =  foo"), Some((1, Some("foo"))));
        assert_eq!(p("1; url=\"foo\"bar"), Some((1, Some("foo"))));
        assert_eq!(p("1; url=\"foo'bar"), Some((1, Some("foo'bar"))));
        assert_eq!(p("1; url foo"), Some((1, Some("url foo"))));
        assert_eq!(p("1; urfoo"), Some((1, Some("urfoo"))));
        assert_eq!(p("1; 'foo'bar"), Some((1, Some("foo"))));
        assert_eq!(p("; foo"), None);
        assert_eq!(p("+1"), None);
        assert_eq!(p("-0; url=foo"), None);
        assert_eq!(p("1.9..5.; url=foo"), Some((1, Some("foo"))));
        assert_eq!(p(".9; url=foo"), Some((0, Some("foo"))));
        assert_eq!(p("0...; url=foo"), Some((0, Some("foo"))));
        assert_eq!(p("1e0; url=foo"), None);
        assert_eq!(p("99999999999999999999999"), Some((u64::MAX, None)));
    }
}
