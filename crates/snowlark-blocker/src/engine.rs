// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compiled filters: Brave's `adblock` engine over the enabled lists, with the request-type
//! mapping from Fetch's destinations and the verdicts snowlark-net's hook takes.

use std::collections::HashSet;

use adblock::lists::{FilterFormat, ParseOptions};
use adblock::resources::{PermissionMask, Resource};
use adblock::{Engine, FilterSet};
use base64::Engine as _;
use snowlark_net::{Destination, FilterAction, FilterVerdict, Mode, Request};

/// How a list's lines are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ListFormat {
    /// Adblock Plus / uBlock Origin filter syntax.
    Filters,
    /// A hosts file (`0.0.0.0 host` lines): each host becomes a `||host^` network filter.
    Hosts,
}

/// One list's text for [`FilterEngine::compile`].
pub struct ListText<'a> {
    pub text: &'a str,
    pub format: ListFormat,
    /// uBlock Origin's own lists and the user's filters: their trusted scriptlets
    /// (`trusted-*`) may run.
    pub trusted: bool,
}

/// The permission bit uBlock Origin's `requiresTrust` scriptlets need.
pub(crate) const TRUSTED: u8 = 0b1;

/// What a lookup gives for one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkMatch {
    Allow,
    Block,
    /// A `redirect=` resource, as a `data:` URL.
    Redirect(String),
    Rewrite(String),
}

/// What the engine keeps besides adblock-rust's own data: what the lists need that adblock-rust
/// does not parse (see [`preprocess`]).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Extras {
    /// Hosts (with their subdomains) whose site-specific cosmetic filters an `$specifichide`
    /// (or `$elemhide`) exception turns off.
    pub specifichide: Vec<String>,
    /// Filter lines the lists gave.
    pub filter_count: usize,
}

/// A compiled engine.
pub struct FilterEngine {
    engine: Engine,
    pub extras: Extras,
}

/// The start of a serialized engine (then the length of the extras, the extras as JSON, and
/// adblock-rust's bytes).
const MAGIC: &[u8; 8] = b"OSMBLK1\n";

/// Rewrites what uBlock Origin accepts and adblock-rust does not, the way uBlock Origin itself
/// reads it: `denyallow=a|b` is `to=~a|~b` (which adblock-rust 0.13 parses, then leaves out); `$specifichide`/`$shide` exceptions are kept aside
/// (`specifichide`), and `$elemhide`/`$ehide` is both those and `$generichide`.
pub fn preprocess(text: &str, specifichide: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    // uBlock Origin's `!#if <expression>` / `!#else` / `!#endif`: lines in a false branch are left
    // out. A stack of (branch taken, enclosing branches all taken).
    let mut conditions: Vec<(bool, bool)> = Vec::new();
    let mut active = true;
    for line in text.lines() {
        if let Some(expr) = line.strip_prefix("!#if ") {
            let value = eval_condition(expr);
            conditions.push((value, active));
            active = active && value;
            continue;
        }
        if line.starts_with("!#else") {
            if let Some((value, outer)) = conditions.last_mut() {
                *value = !*value;
                active = *outer && *value;
            }
            continue;
        }
        if line.starts_with("!#endif") {
            if let Some((_, outer)) = conditions.pop() {
                active = outer;
            }
            continue;
        }
        if !active {
            continue;
        }
        match rewrite_line(line, specifichide) {
            Some(l) => out.push_str(&l),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// The value of an `!#if` expression for this browser, as uBlock Origin evaluates its tokens: we
/// are a Chromium-class desktop browser running uBlock Origin's syntax (`env_chromium`,
/// `ext_ublock`, `cap_user_stylesheet`); every other token (`env_firefox`, `env_mobile`,
/// `env_safari`, `ext_ubol`, `adguard`, `cap_html_filtering`...) is false. `!`, `&&`, `||` and
/// parentheses as in JavaScript.
pub fn eval_condition(expr: &str) -> bool {
    fn token_value(t: &str) -> bool {
        matches!(t, "env_chromium" | "ext_ublock" | "cap_user_stylesheet")
    }
    struct P<'a> {
        s: &'a [u8],
        i: usize,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
        }
        fn or(&mut self) -> bool {
            let mut v = self.and();
            loop {
                self.ws();
                if self.s[self.i..].starts_with(b"||") {
                    self.i += 2;
                    let r = self.and();
                    v = v || r;
                } else {
                    return v;
                }
            }
        }
        fn and(&mut self) -> bool {
            let mut v = self.unary();
            loop {
                self.ws();
                if self.s[self.i..].starts_with(b"&&") {
                    self.i += 2;
                    let r = self.unary();
                    v = v && r;
                } else {
                    return v;
                }
            }
        }
        fn unary(&mut self) -> bool {
            self.ws();
            match self.s.get(self.i) {
                Some(b'!') => {
                    self.i += 1;
                    !self.unary()
                }
                Some(b'(') => {
                    self.i += 1;
                    let v = self.or();
                    self.ws();
                    if self.s.get(self.i) == Some(&b')') {
                        self.i += 1;
                    }
                    v
                }
                _ => {
                    let start = self.i;
                    while self.i < self.s.len()
                        && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_')
                    {
                        self.i += 1;
                    }
                    let t = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
                    if self.i == start {
                        // Not an expression: skip a byte so parsing ends.
                        self.i = self.s.len();
                    }
                    token_value(t)
                }
            }
        }
    }
    let mut p = P {
        s: expr.trim().as_bytes(),
        i: 0,
    };
    p.or()
}

fn rewrite_line(line: &str, specifichide: &mut Vec<String>) -> Option<String> {
    if line.starts_with('!') || line.contains('#') {
        return None;
    }
    let dollar = line.rfind('$')?;
    let (pattern, options) = (&line[..dollar], &line[dollar + 1..]);
    if !(options.contains("denyallow=")
        || options.contains("hide")
        || options.split(',').any(|o| o == "shide" || o == "ehide"))
    {
        return None;
    }
    let mut kept = Vec::new();
    let mut specific = false;
    let mut generic = false;
    for o in options.split(',') {
        if let Some(list) = o.strip_prefix("denyallow=") {
            let to: Vec<String> = list.split('|').map(|d| format!("~{d}")).collect();
            kept.push(format!("to={}", to.join("|")));
        } else if o == "specifichide" || o == "shide" {
            specific = true;
        } else if o == "elemhide" || o == "ehide" {
            specific = true;
            generic = true;
        } else {
            kept.push(o.to_owned());
        }
    }
    if specific {
        // Only exceptions of the form `@@||host^$specifichide` are meaningful.
        if let Some(host) = pattern
            .strip_prefix("@@||")
            .map(|h| h.trim_end_matches('^').trim_end_matches('/'))
            .filter(|h| !h.is_empty() && !h.contains(['/', '*']))
        {
            specifichide.push(host.to_ascii_lowercase());
        }
        if generic {
            kept.push("generichide".into());
        } else if kept.is_empty() {
            return Some(String::new());
        }
    }
    Some(format!("{pattern}${}", kept.join(",")))
}

impl FilterEngine {
    /// Parses and compiles `lists` (and loads `resources` for `redirect=` and `+js()`).
    pub fn compile(lists: &[ListText], resources: Vec<Resource>) -> FilterEngine {
        let mut set = FilterSet::new(false);
        let mut extras = Extras::default();
        for list in lists {
            let options = ParseOptions {
                format: match list.format {
                    ListFormat::Filters => FilterFormat::Standard,
                    ListFormat::Hosts => FilterFormat::Hosts,
                },
                permissions: if list.trusted {
                    PermissionMask::from_bits(TRUSTED)
                } else {
                    PermissionMask::default()
                },
                ..ParseOptions::default()
            };
            let text = match list.format {
                ListFormat::Filters => preprocess(list.text, &mut extras.specifichide),
                ListFormat::Hosts => list.text.to_owned(),
            };
            extras.filter_count += text
                .lines()
                .filter(|l| !l.is_empty() && !l.starts_with('!') && !l.starts_with('#'))
                .count();
            set.add_filter_list(text, options);
        }
        let mut engine = Engine::new_with_filter_set(set);
        engine.use_resources(resources);
        FilterEngine { engine, extras }
    }

    /// The engine as bytes to keep in the profile: ours, then adblock-rust's binary format.
    pub fn serialize(&self) -> Vec<u8> {
        let extras = serde_json::to_vec(&self.extras).unwrap_or_default();
        let mut out = Vec::with_capacity(MAGIC.len() + 4 + extras.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(extras.len() as u32).to_le_bytes());
        out.extend_from_slice(&extras);
        out.extend_from_slice(&self.engine.serialize());
        out
    }

    /// An engine from [`FilterEngine::serialize`]'s bytes (`None` when they are not one: another
    /// version's, or damaged: adblock-rust checks its part's checksum).
    pub fn deserialize(bytes: &[u8], resources: Vec<Resource>) -> Option<FilterEngine> {
        let rest = bytes.strip_prefix(MAGIC.as_slice())?;
        let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let extras = serde_json::from_slice(rest.get(4..4 + len)?).ok()?;
        let mut engine = Engine::default();
        engine.deserialize(rest.get(4 + len..)?).ok()?;
        engine.use_resources(resources);
        Some(FilterEngine { engine, extras })
    }

    /// Whether a `$specifichide` exception covers documents on `host`.
    pub fn specific_hide_off(&self, host: &str) -> bool {
        self.extras.specifichide.iter().any(|h| {
            host == h
                || (host.len() > h.len()
                    && host.ends_with(h.as_str())
                    && host.as_bytes()[host.len() - h.len() - 1] == b'.')
        })
    }

    /// The network filters' verdict on a request of `request_type` (adblock-rust's names) for
    /// `url`, made by the document at `source`.
    pub fn match_request(&self, url: &str, source: &str, request_type: &str) -> NetworkMatch {
        self.match_request_with_method(url, source, request_type, "GET")
    }

    /// [`FilterEngine::match_request`] for a request with `method` (for `$method=` filters).
    pub fn match_request_with_method(
        &self,
        url: &str,
        source: &str,
        request_type: &str,
        method: &str,
    ) -> NetworkMatch {
        let Ok(request) = adblock::request::Request::new(url, source, request_type, method) else {
            return NetworkMatch::Allow;
        };
        self.check(&request)
    }

    fn check(&self, request: &adblock::request::Request) -> NetworkMatch {
        let result = self.engine.check_network_request(request);
        if result.should_block() {
            match result.redirect {
                Some(data) => NetworkMatch::Redirect(data),
                None => NetworkMatch::Block,
            }
        } else if let Some(url) = result.rewritten_url {
            NetworkMatch::Rewrite(url)
        } else {
            NetworkMatch::Allow
        }
    }

    /// The `$csp` policies for a document or subdocument request.
    pub fn csp(&self, url: &str, source: &str, request_type: &str) -> Option<String> {
        let request = adblock::request::Request::new(url, source, request_type, "GET").ok()?;
        self.engine.get_csp_directives(&request)
    }

    /// The cosmetic filters for a document at `url`.
    pub fn cosmetic(&self, url: &str) -> adblock::cosmetic_filter_cache::UrlSpecificResources {
        self.engine.url_cosmetic_resources(url)
    }

    /// Generic hiding selectors for classes and ids seen in a document.
    pub fn class_id_selectors(
        &self,
        classes: &[String],
        ids: &[String],
        exceptions: &HashSet<String>,
    ) -> Vec<String> {
        self.engine
            .hidden_class_id_selectors(classes, ids, exceptions)
    }

    /// Network matching for one of snowlark-net's requests: its verdict for the fetch hook. The URLs
    /// are already parsed: their hosts and sites are taken from them rather than parsed again.
    pub fn verdict(&self, request: &Request) -> FilterVerdict {
        let url = request.url().as_str();
        let source_url = request.client.as_ref().map(|c| &c.referrer_source);
        let source = source_url.map_or(url, |u| u.as_str());
        let kind = request_type(request);
        let host = request.url().host_str().unwrap_or("");
        let source_host = source_url.map_or(host, |u| u.host_str().unwrap_or(""));
        let third_party =
            crate::policy::site_of_host(host) != crate::policy::site_of_host(source_host);
        let preparsed = adblock::request::Request::preparsed(
            url,
            host,
            source_host,
            kind,
            third_party,
            &request.method,
        );
        let action = match self.check(&preparsed) {
            NetworkMatch::Allow => FilterAction::Allow,
            NetworkMatch::Block => FilterAction::Block("a network filter".into()),
            NetworkMatch::Redirect(data) => match decode_data_url(&data) {
                Some((content_type, body)) => FilterAction::Resource {
                    content_type,
                    body: body.into(),
                },
                None => FilterAction::Block("a redirect filter".into()),
            },
            NetworkMatch::Rewrite(to) => match url::Url::parse(&to) {
                Ok(u) => FilterAction::Rewrite(u),
                Err(_) => FilterAction::Allow,
            },
        };
        let csp = if matches!(kind, "document" | "subdocument")
            && !matches!(action, FilterAction::Block(_))
        {
            self.csp(url, source, kind)
        } else {
            None
        };
        FilterVerdict { action, csp }
    }
}

/// The document a request is made for: its client's URL (for a frame's navigation, the parent's),
/// `None` for the browser's own requests (a top-level navigation: the URL is its own source).
pub fn source_url(request: &Request) -> Option<&str> {
    request.client.as_ref().map(|c| c.referrer_source.as_str())
}

/// adblock-rust's request type for a Fetch request (its `cpt` names, as uBlock Origin's): by
/// destination, and for the destination "" by what made the request.
pub fn request_type(request: &Request) -> &'static str {
    if request.mode == Mode::WebSocket {
        return "websocket";
    }
    match request.destination {
        Destination::Document => "document",
        Destination::Iframe | Destination::Frame => "subdocument",
        Destination::Object | Destination::Embed => "object",
        Destination::Script
        | Destination::AudioWorklet
        | Destination::PaintWorklet
        | Destination::ServiceWorker
        | Destination::SharedWorker
        | Destination::Worker => "script",
        Destination::Image => "image",
        Destination::Style => "stylesheet",
        Destination::Font => "font",
        Destination::Audio | Destination::Video | Destination::Track => "media",
        Destination::Json => "xmlhttprequest",
        Destination::Report => "csp_report",
        Destination::Empty => match request.initiator_type.as_str() {
            "fetch" | "xmlhttprequest" | "eventsource" => "xmlhttprequest",
            "beacon" | "ping" => "ping",
            _ => "other",
        },
        Destination::Manifest | Destination::WebIdentity | Destination::Xslt => "other",
    }
}

/// A `data:` URL's type and bytes (adblock-rust gives redirect resources as base64 `data:` URLs).
fn decode_data_url(data: &str) -> Option<(String, Vec<u8>)> {
    let rest = data.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    Some((mime.to_owned(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(list: &str) -> FilterEngine {
        FilterEngine::compile(
            &[ListText {
                text: list,
                format: ListFormat::Filters,
                trusted: false,
            }],
            crate::resources::fallback_resources(),
        )
    }

    const PAGE: &str = "https://news.example/article";

    #[test]
    fn block_exception_and_important() {
        let e = engine(
            "||ads.example^\n@@||ads.example/ok.js\n||track.example^$important\n@@||track.example^",
        );
        assert_eq!(
            e.match_request("https://ads.example/x.js", PAGE, "script"),
            NetworkMatch::Block
        );
        assert_eq!(
            e.match_request("https://ads.example/ok.js", PAGE, "script"),
            NetworkMatch::Allow
        );
        assert_eq!(
            e.match_request("https://track.example/p.gif", PAGE, "image"),
            NetworkMatch::Block
        );
        assert_eq!(
            e.match_request("https://cdn.example/app.js", PAGE, "script"),
            NetworkMatch::Allow
        );
    }

    #[test]
    fn party_domain_denyallow_badfilter_all() {
        let e = engine(
            "||widget.example^$third-party\n\
             ||cdn.example/promo$domain=news.example|~sub.news.example\n\
             *$script,domain=strict.example,denyallow=good.example|fine.example\n\
             ||gone.example^\n||gone.example^$badfilter\n\
             ||everything.example^$all",
        );
        assert_eq!(
            e.match_request("https://widget.example/w.js", PAGE, "script"),
            NetworkMatch::Block
        );
        assert_eq!(
            e.match_request(
                "https://widget.example/w.js",
                "https://widget.example/",
                "script"
            ),
            NetworkMatch::Allow
        );
        assert_eq!(
            e.match_request("https://cdn.example/promo.png", PAGE, "image"),
            NetworkMatch::Block
        );
        assert_eq!(
            e.match_request(
                "https://cdn.example/promo.png",
                "https://sub.news.example/",
                "image"
            ),
            NetworkMatch::Allow
        );
        // `denyallow=` (uBlock Origin's `to=~`): adblock-rust 0.13 parses `to=` but leaves such
        // filters out of the engine, so the filter blocks nothing (as in Brave).
        assert_eq!(
            e.match_request(
                "https://any.example/a.js",
                "https://strict.example/",
                "script"
            ),
            NetworkMatch::Allow
        );
        assert_eq!(
            e.match_request(
                "https://good.example/a.js",
                "https://strict.example/",
                "script"
            ),
            NetworkMatch::Allow
        );
        assert_eq!(
            e.match_request("https://gone.example/a.js", PAGE, "script"),
            NetworkMatch::Allow
        );
        assert_eq!(
            e.match_request(
                "https://everything.example/",
                "https://everything.example/",
                "document"
            ),
            NetworkMatch::Block
        );
    }

    #[test]
    fn specifichide_and_elemhide_exceptions() {
        let mut hosts = Vec::new();
        let out = preprocess(
            "@@||quiet.example^$specifichide\n@@||calm.example^$ehide\n||x.example^$denyallow=a.example|b.example,script\n##.ad",
            &mut hosts,
        );
        assert_eq!(hosts, ["quiet.example", "calm.example"]);
        assert_eq!(
            out,
            "\n@@||calm.example^$generichide\n||x.example^$to=~a.example|~b.example,script\n##.ad\n"
        );
        let e = engine("@@||quiet.example^$specifichide\nquiet.example##.promo");
        assert!(e.specific_hide_off("www.quiet.example"));
        assert!(!e.specific_hide_off("notquiet.example"));
        let back = FilterEngine::deserialize(&e.serialize(), Vec::new()).unwrap();
        assert_eq!(back.extras, e.extras);
    }

    #[test]
    fn if_directives_select_lines_as_ublock_origin_does() {
        assert!(eval_condition("env_chromium"));
        assert!(eval_condition("!env_mobile"));
        assert!(!eval_condition("env_firefox || env_safari"));
        assert!(eval_condition("(ext_ublock || adguard) && !ext_ubol"));
        assert!(!eval_condition("cap_html_filtering"));
        let mut hosts = Vec::new();
        let out = preprocess(
            "a\n!#if env_mobile\nb\n!#if ext_ublock\nc\n!#endif\n!#else\nd\n!#endif\n!#if !cap_html_filtering\ne\n!#else\nf\n!#endif\ng",
            &mut hosts,
        );
        assert_eq!(out, "a\nd\ne\ng\n");
    }

    #[test]
    fn redirect_removeparam_and_csp() {
        let e = engine(
            "||ads.example/lib.js$script,redirect=noop.js\n\
             $removeparam=utm_source\n\
             ||strict.example^$csp=script-src 'self'",
        );
        match e.match_request("https://ads.example/lib.js", PAGE, "script") {
            NetworkMatch::Redirect(data) => {
                let (mime, body) = decode_data_url(&data).unwrap();
                assert_eq!(mime, "application/javascript");
                assert!(String::from_utf8(body).unwrap().contains("function"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            e.match_request("https://shop.example/?utm_source=x&id=2", PAGE, "document"),
            NetworkMatch::Rewrite("https://shop.example/?id=2".into())
        );
        assert_eq!(
            e.csp(
                "https://strict.example/",
                "https://strict.example/",
                "document"
            )
            .as_deref(),
            Some("script-src 'self'")
        );
    }

    #[test]
    fn hosts_files_become_network_filters() {
        let e = FilterEngine::compile(
            &[ListText {
                text: "# comment\n127.0.0.1 tracker.example\n0.0.0.0 ads.example\n",
                format: ListFormat::Hosts,
                trusted: false,
            }],
            Vec::new(),
        );
        assert_eq!(
            e.match_request("https://sub.tracker.example/p", PAGE, "image"),
            NetworkMatch::Block
        );
        assert_eq!(
            e.match_request("https://ads.example/x", PAGE, "script"),
            NetworkMatch::Block
        );
        assert_eq!(
            e.match_request("https://fine.example/x", PAGE, "script"),
            NetworkMatch::Allow
        );
    }

    #[test]
    fn serialized_engines_load_back() {
        let e = engine("||ads.example^\nexample.com##.banner");
        let bytes = e.serialize();
        let back = FilterEngine::deserialize(&bytes, Vec::new()).unwrap();
        assert_eq!(
            back.match_request("https://ads.example/x.js", PAGE, "script"),
            NetworkMatch::Block
        );
        assert!(back
            .cosmetic("https://example.com/")
            .hide_selectors
            .contains(".banner"));
        assert!(FilterEngine::deserialize(&bytes[..bytes.len() / 2], Vec::new()).is_none());
    }

    #[test]
    fn request_types_follow_destinations() {
        let u = url::Url::parse("https://a.example/").unwrap();
        let mut r = Request::new(u);
        for (d, t) in [
            (Destination::Script, "script"),
            (Destination::Image, "image"),
            (Destination::Style, "stylesheet"),
            (Destination::Font, "font"),
            (Destination::Video, "media"),
            (Destination::Iframe, "subdocument"),
            (Destination::Document, "document"),
            (Destination::Object, "object"),
            (Destination::Manifest, "other"),
        ] {
            r.destination = d;
            assert_eq!(request_type(&r), t);
        }
        r.destination = Destination::Empty;
        r.initiator_type = "fetch".into();
        assert_eq!(request_type(&r), "xmlhttprequest");
        r.initiator_type = "beacon".into();
        assert_eq!(request_type(&r), "ping");
        r.mode = Mode::WebSocket;
        assert_eq!(request_type(&r), "websocket");
    }
}
