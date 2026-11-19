// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Content Security Policy Level 3 (<https://w3c.github.io/webappsec-csp/>): the policy model, parsing
//! (§2.2), source lists and their matching (§6.7), and the checks as pure functions over a [`CspList`]:
//! the request checks Fetch runs (§4.1), plus the inline, eval, navigation, `frame-ancestors` and
//! `base-uri` checks the document side calls (§4.2–4.5). Each check returns a [`Verdict`]: whether an
//! enforced policy blocks, and the violations (from enforced and report-only policies) to report. Turning
//! violations into events and reports belongs to the page layer.

use base64::Engine;
use sha2::{Digest, Sha256, Sha384, Sha512};
use url::{Origin, Url};

use crate::headers::HeaderList;
use crate::request::{Destination, Initiator, ParserMetadata, Request};

/// A policy's disposition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Disposition {
    Enforce,
    Report,
}

/// Where a policy came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PolicySource {
    Header,
    Meta,
}

/// A hash algorithm a hash-source may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HashAlgorithm {
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlgorithm {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "sha256" => Some(HashAlgorithm::Sha256),
            "sha384" => Some(HashAlgorithm::Sha384),
            "sha512" => Some(HashAlgorithm::Sha512),
            _ => None,
        }
    }

    /// The base64 digest of `bytes`.
    pub fn digest_base64(self, bytes: &[u8]) -> String {
        let engine = base64::engine::general_purpose::STANDARD;
        match self {
            HashAlgorithm::Sha256 => engine.encode(Sha256::digest(bytes)),
            HashAlgorithm::Sha384 => engine.encode(Sha384::digest(bytes)),
            HashAlgorithm::Sha512 => engine.encode(Sha512::digest(bytes)),
        }
    }
}

/// base64url digests compare as base64 (§6.7.3.5).
fn normalize_base64(value: &str) -> String {
    value.replace('-', "+").replace('_', "/")
}

/// base64-value = 1*( ALPHA / DIGIT / "+" / "/" / "-" / "_" )*2( "=" )
fn is_base64_value(value: &str) -> bool {
    let body = value.trim_end_matches('=');
    let pad = value.len() - body.len();
    !body.is_empty()
        && pad <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_'))
}

/// A host-source (§2.3.1): `[ scheme-part "://" ] host-part [ ":" port-part ] [ path-part ]`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostSource {
    pub scheme: Option<String>,
    /// Lowercase; may start with "*." or be "*".
    pub host: String,
    /// `None`, "*", or digits.
    pub port: Option<String>,
    /// "" when absent.
    pub path: String,
}

/// A source list (§2.3.1) with its keywords, nonces and hashes picked out once when parsed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceList {
    /// The list is exactly `'none'`.
    pub none: bool,
    pub self_: bool,
    pub star: bool,
    pub unsafe_inline: bool,
    pub unsafe_eval: bool,
    pub wasm_unsafe_eval: bool,
    pub strict_dynamic: bool,
    pub unsafe_hashes: bool,
    pub report_sample: bool,
    pub unsafe_allow_redirects: bool,
    pub inline_speculation_rules: bool,
    pub schemes: Vec<String>,
    pub hosts: Vec<HostSource>,
    pub nonces: Vec<String>,
    pub hashes: Vec<(HashAlgorithm, String)>,
}

fn is_scheme_part(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn is_host_part(host: &str) -> bool {
    if host == "*" {
        return true;
    }
    let host = host.strip_prefix("*.").unwrap_or(host);
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

fn parse_host_source(token: &str) -> Option<HostSource> {
    let (scheme, rest) = match token.find("://") {
        Some(i) => {
            if !is_scheme_part(&token[..i]) {
                return None;
            }
            (Some(token[..i].to_ascii_lowercase()), &token[i + 3..])
        }
        None => (None, token),
    };
    let host_end = rest.find([':', '/']).unwrap_or(rest.len());
    let host = rest[..host_end].to_ascii_lowercase();
    if !is_host_part(&host) {
        return None;
    }
    let mut rest = &rest[host_end..];
    let mut port = None;
    if let Some(after) = rest.strip_prefix(':') {
        let end = after.find('/').unwrap_or(after.len());
        let p = &after[..end];
        if p != "*" && (p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
            return None;
        }
        port = Some(p.to_string());
        rest = &after[end..];
    }
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(HostSource {
        scheme,
        host,
        port,
        path: rest.to_string(),
    })
}

impl SourceList {
    /// Parse a directive value as a source list.
    pub fn parse(tokens: &[String]) -> SourceList {
        let mut list = SourceList {
            none: tokens.len() == 1 && tokens[0].eq_ignore_ascii_case("'none'"),
            ..SourceList::default()
        };
        for token in tokens {
            let lower = token.to_ascii_lowercase();
            match lower.as_str() {
                "'none'" => continue,
                "'self'" => list.self_ = true,
                "*" => list.star = true,
                "'unsafe-inline'" => list.unsafe_inline = true,
                "'unsafe-eval'" => list.unsafe_eval = true,
                "'wasm-unsafe-eval'" => list.wasm_unsafe_eval = true,
                "'strict-dynamic'" => list.strict_dynamic = true,
                "'unsafe-hashes'" => list.unsafe_hashes = true,
                "'report-sample'" => list.report_sample = true,
                "'unsafe-allow-redirects'" => list.unsafe_allow_redirects = true,
                "'inline-speculation-rules'" => list.inline_speculation_rules = true,
                _ => {
                    if token.len() > 2 && token.starts_with('\'') && token.ends_with('\'') {
                        let inner = &token[1..token.len() - 1];
                        if lower.starts_with("'nonce-") {
                            let nonce = &inner[6..];
                            if is_base64_value(nonce) {
                                list.nonces.push(nonce.to_string());
                            }
                        } else if let Some(dash) = inner.find('-') {
                            if let Some(alg) = HashAlgorithm::parse(&inner[..dash]) {
                                let value = &inner[dash + 1..];
                                if is_base64_value(value) {
                                    list.hashes.push((alg, normalize_base64(value)));
                                }
                            }
                        }
                        // (other quoted tokens are unknown keywords: they match nothing)
                    } else if lower.len() >= 2
                        && lower.ends_with(':')
                        && is_scheme_part(&lower[..lower.len() - 1])
                    {
                        list.schemes.push(lower[..lower.len() - 1].to_string());
                    } else if let Some(host) = parse_host_source(token) {
                        list.hosts.push(host);
                    }
                }
            }
        }
        list
    }

    pub fn has_nonce_or_hash(&self) -> bool {
        !self.nonces.is_empty() || !self.hashes.is_empty()
    }

    /// §6.7.2.5 for each expression: "Does url match source list in origin with redirect count?".
    pub fn matches_url(&self, url: &Url, self_origin: &Origin, redirect_count: u32) -> bool {
        if self.none {
            return false;
        }
        let scheme = url.scheme();
        if self.star {
            let self_scheme = match self_origin {
                Origin::Tuple(s, _, _) => s.as_str(),
                Origin::Opaque(_) => "",
            };
            if matches!(scheme, "http" | "https" | "ws" | "wss") || scheme == self_scheme {
                return true;
            }
        }
        if self.schemes.iter().any(|s| scheme_part_match(s, scheme)) {
            return true;
        }
        if url.host_str().is_some()
            && self
                .hosts
                .iter()
                .any(|h| host_source_matches(h, url, self_origin, redirect_count))
        {
            return true;
        }
        self.self_ && self_matches(url, self_origin)
    }

    /// §6.7.3.2 "Does a source list allow all inline behavior for type?".
    pub fn allows_all_inline(&self, script_like: bool) -> bool {
        !self.has_nonce_or_hash() && !(script_like && self.strict_dynamic) && self.unsafe_inline
    }

    /// Whether a nonce-source equals `nonce` (§6.7.2.2).
    pub fn matches_nonce(&self, nonce: &str) -> bool {
        !nonce.is_empty() && self.nonces.iter().any(|n| n == nonce)
    }

    /// Whether a hash-source matches `source`'s UTF-8 bytes (§6.7.3.3 step 5).
    pub fn matches_hash(&self, source: &str) -> bool {
        let mut digests: [Option<String>; 3] = [None, None, None];
        self.hashes.iter().any(|(alg, value)| {
            let slot = &mut digests[*alg as usize];
            let digest = slot.get_or_insert_with(|| alg.digest_base64(source.as_bytes()));
            digest == value
        })
    }

    /// The "integrity" bypass of the script pre-request check: every SRI hash of the request is listed.
    fn matches_integrity(&self, integrity: &str) -> bool {
        if self.hashes.is_empty() {
            return false;
        }
        let sources: Vec<(HashAlgorithm, String)> = integrity
            .split_ascii_whitespace()
            .filter_map(|token| {
                let expr = token.split('?').next()?;
                let dash = expr.find('-')?;
                let alg = HashAlgorithm::parse(&expr[..dash])?;
                Some((alg, normalize_base64(&expr[dash + 1..])))
            })
            .collect();
        !sources.is_empty() && sources.iter().all(|s| self.hashes.contains(s))
    }
}

/// §6.7.2.8 scheme-part match.
fn scheme_part_match(a: &str, b: &str) -> bool {
    a == b
        || (a == "http" && b == "https")
        || (a == "ws" && matches!(b, "wss" | "http" | "https"))
        || (a == "wss" && b == "https")
}

/// §6.7.2.6 host-part match.
fn host_part_match(pattern: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if pattern == "*" {
        return true;
    }
    if let Some(rest) = pattern.strip_prefix('*') {
        return host.ends_with(rest);
    }
    pattern == host || pattern.trim_end_matches('.') == host.trim_end_matches('.')
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    }
}

/// §6.7.2.7 port-part match.
fn port_part_match(pattern: Option<&str>, url: &Url) -> bool {
    let url_default = default_port(url.scheme());
    match pattern {
        None => url.port().is_none() || url.port() == url_default,
        Some("*") => true,
        Some(p) => {
            let Ok(n) = p.parse::<u32>() else {
                return false;
            };
            match url.port() {
                Some(port) => n == port as u32,
                None => {
                    Some(n) == url_default.map(u32::from)
                        // (an http source upgraded to https: "example.com:80" allows "https://example.com")
                        || (n == 80 && matches!(url.scheme(), "https" | "wss"))
                }
            }
        }
    }
}

fn percent_decoded(s: &str) -> Vec<u8> {
    crate::data_url::percent_decode(s.as_bytes())
}

/// §6.7.2.9 path-part match.
fn path_part_match(a: &str, b: &str) -> bool {
    if a.is_empty() || (a == "/" && b.is_empty()) {
        return true;
    }
    let exact = !a.ends_with('/');
    let mut list_a: Vec<&str> = a.split('/').collect();
    let list_b: Vec<&str> = b.split('/').collect();
    if list_a.len() > list_b.len() || (exact && list_a.len() != list_b.len()) {
        return false;
    }
    if !exact {
        list_a.pop();
    }
    list_a
        .iter()
        .zip(list_b.iter())
        .all(|(x, y)| percent_decoded(x) == percent_decoded(y))
}

fn host_source_matches(
    source: &HostSource,
    url: &Url,
    self_origin: &Origin,
    redirect_count: u32,
) -> bool {
    let scheme_ok = match &source.scheme {
        Some(s) => scheme_part_match(s, url.scheme()),
        None => match self_origin {
            Origin::Tuple(s, _, _) => scheme_part_match(s, url.scheme()),
            Origin::Opaque(_) => false,
        },
    };
    if !scheme_ok {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if !host_part_match(&source.host, host) {
        return false;
    }
    if !port_part_match(source.port.as_deref(), url) {
        return false;
    }
    if !source.path.is_empty() && redirect_count == 0 && !path_part_match(&source.path, url.path())
    {
        return false;
    }
    true
}

/// 'self' (§6.7.2.5 step 4): the same origin, or the same host upgraded to a secure scheme.
fn self_matches(url: &Url, self_origin: &Origin) -> bool {
    let Origin::Tuple(self_scheme, self_host, self_port) = self_origin else {
        return false;
    };
    // (a `blob:` or `filesystem:` URL has its creator's origin, but 'self' names the scheme-host-port
    // of network URLs: those schemes must be listed explicitly)
    if matches!(url.scheme(), "blob" | "filesystem" | "data" | "about") {
        return false;
    }
    let url_origin = url.origin();
    if url_origin == *self_origin {
        return true;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if crate::origin::host_string(self_host) != host {
        return false;
    }
    let url_port = url.port_or_known_default();
    let ports_ok = url_port == Some(*self_port)
        || (default_port(self_scheme) == Some(*self_port) && url.port().is_none());
    ports_ok
        && (matches!(url.scheme(), "https" | "wss")
            || (self_scheme == "http" && matches!(url.scheme(), "http" | "ws")))
}

/// A directive: its name and value, the value read as a source list.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Directive {
    pub name: String,
    pub value: Vec<String>,
    pub sources: SourceList,
}

impl std::fmt::Display for Directive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)?;
        for v in &self.value {
            write!(f, " {v}")?;
        }
        Ok(())
    }
}

/// A policy (§2.2).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    pub directives: Vec<Directive>,
    pub disposition: Disposition,
    pub source: PolicySource,
    /// What 'self' means.
    #[serde(with = "crate::wire::origin")]
    pub self_origin: Origin,
    /// The serialized policy as delivered (originalPolicy in reports).
    pub text: String,
}

fn is_ascii_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ')
}

/// Directives a `<meta>` policy can't carry (§3.3).
const NOT_IN_META: &[&str] = &["report-uri", "frame-ancestors", "sandbox"];

impl Policy {
    /// §2.2.1 "parse a serialized CSP". Console warnings (repeated directives, directives a `<meta>`
    /// can't carry, invalid characters) are returned alongside.
    pub fn parse(
        serialized: &str,
        source: PolicySource,
        disposition: Disposition,
        self_origin: Origin,
    ) -> (Policy, Vec<String>) {
        let mut warnings = Vec::new();
        let mut policy = Policy {
            directives: Vec::new(),
            disposition,
            source,
            self_origin,
            text: serialized.trim_matches(is_ascii_whitespace).to_string(),
        };
        for token in serialized.split(';') {
            let token = token.trim_matches(is_ascii_whitespace);
            if token.is_empty() {
                continue;
            }
            let end = token.find(is_ascii_whitespace).unwrap_or(token.len());
            let name = token[..end].to_ascii_lowercase();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                continue;
            }
            if policy.directives.iter().any(|d| d.name == name) {
                warnings.push(format!(
                    "The Content Security Policy directive '{name}' is repeated. The second and later occurrences are ignored."
                ));
                continue;
            }
            if source == PolicySource::Meta && NOT_IN_META.contains(&name.as_str()) {
                warnings.push(format!(
                    "The Content Security Policy directive '{name}' is ignored when delivered via a <meta> element."
                ));
                continue;
            }
            let rest = &token[end..];
            // (directive-value = *( required-ascii-whitespace / %x21-%x2B / %x2D-%x3A / %x3C-%x7E ): a
            // value with other characters, a non-breaking space or a vertical tab say, is dropped whole)
            if rest
                .chars()
                .any(|c| !is_ascii_whitespace(c) && !('\x21'..='\x7E').contains(&c))
            {
                warnings.push(format!(
                    "The Content Security Policy directive '{name}' contains invalid characters and is ignored."
                ));
                continue;
            }
            let value: Vec<String> = rest
                .split(is_ascii_whitespace)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            let sources = SourceList::parse(&value);
            policy.directives.push(Directive {
                name,
                value,
                sources,
            });
        }
        (policy, warnings)
    }

    pub fn directive(&self, name: &str) -> Option<&Directive> {
        self.directives.iter().find(|d| d.name == name)
    }

    pub fn is_report_only(&self) -> bool {
        self.disposition == Disposition::Report
    }

    /// The `report-to` group, if any.
    pub fn report_to(&self) -> Option<&str> {
        self.directive("report-to")
            .and_then(|d| d.value.first())
            .map(String::as_str)
    }

    /// The `report-uri` URLs (ignored by the reporter when `report-to` is present, §5.5).
    pub fn report_uris(&self) -> Vec<&str> {
        self.directive("report-uri")
            .map(|d| d.value.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

/// §6.8.3 "get the fallback list" for an effective directive (itself first).
pub fn fallback_list(effective: &str) -> &'static [&'static str] {
    match effective {
        "script-src-elem" => &["script-src-elem", "script-src", "default-src"],
        "script-src-attr" => &["script-src-attr", "script-src", "default-src"],
        "style-src-elem" => &["style-src-elem", "style-src", "default-src"],
        "style-src-attr" => &["style-src-attr", "style-src", "default-src"],
        "worker-src" => &["worker-src", "child-src", "script-src", "default-src"],
        "connect-src" => &["connect-src", "default-src"],
        "manifest-src" => &["manifest-src", "default-src"],
        "object-src" => &["object-src", "default-src"],
        "frame-src" => &["frame-src", "child-src", "default-src"],
        "media-src" => &["media-src", "default-src"],
        "font-src" => &["font-src", "default-src"],
        "img-src" => &["img-src", "default-src"],
        "child-src" => &["child-src", "default-src"],
        "default-src" => &["default-src"],
        _ => &[],
    }
}

/// §6.8.1 "get the effective directive for request".
pub fn effective_directive_for(
    destination: Destination,
    initiator: Initiator,
) -> Option<&'static str> {
    if matches!(initiator, Initiator::Prefetch | Initiator::Prerender) {
        return Some("default-src");
    }
    Some(match destination {
        Destination::Manifest => "manifest-src",
        Destination::Object | Destination::Embed => "object-src",
        Destination::Frame | Destination::Iframe => "frame-src",
        Destination::Audio | Destination::Track | Destination::Video => "media-src",
        Destination::Font => "font-src",
        Destination::Image => "img-src",
        Destination::Style => "style-src-elem",
        Destination::Script
        | Destination::Xslt
        | Destination::AudioWorklet
        | Destination::PaintWorklet => "script-src-elem",
        Destination::ServiceWorker | Destination::SharedWorker | Destination::Worker => {
            "worker-src"
        }
        Destination::Json | Destination::WebIdentity | Destination::Empty => "connect-src",
        Destination::Report | Destination::Document => return None,
    })
}

/// The directive of `policy` that governs `effective` (the first of its fallback list present).
fn governing<'p>(policy: &'p Policy, effective: &str) -> Option<&'p Directive> {
    fallback_list(effective)
        .iter()
        .find_map(|name| policy.directive(name))
}

/// What a violation is about.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ViolationResource {
    Url(Url),
    /// The URL of a navigation into a frame cross-origin to the policy's document: reports name its
    /// origin only (the path of a cross-origin navigation is not the embedder's to learn).
    NavigationOrigin(Url),
    Inline,
    Eval,
    WasmEval,
    /// Trusted Types: a string reached a sink that `require-trusted-types-for` guards.
    TrustedTypesSink,
    /// Trusted Types: a policy name the `trusted-types` directive does not allow.
    TrustedTypesPolicy,
}

/// A violation (§2.4), ready for the `securitypolicyviolation` event, the console and a report.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Violation {
    pub resource: ViolationResource,
    pub effective_directive: String,
    /// The directive that did the blocking, as written (for console messages).
    pub directive: String,
    pub disposition: Disposition,
    /// The policy's index in its list.
    pub policy_index: usize,
    pub original_policy: String,
    /// The first 40 characters of the blocked code, when the directive has 'report-sample'.
    pub sample: String,
}

impl Violation {
    /// The blocked URI as reports show it (§5.4 "strip URL for use in reports"): no fragment or
    /// credentials; only the scheme of a non-HTTP(S) URL; "inline", "eval" or "wasm-eval" otherwise.
    pub fn blocked_uri(&self) -> String {
        match &self.resource {
            ViolationResource::Url(url) => strip_url_for_report(url),
            ViolationResource::NavigationOrigin(url) => url.origin().ascii_serialization(),
            ViolationResource::Inline => "inline".into(),
            ViolationResource::Eval => "eval".into(),
            ViolationResource::WasmEval => "wasm-eval".into(),
            ViolationResource::TrustedTypesSink => "trusted-types-sink".into(),
            ViolationResource::TrustedTypesPolicy => "trusted-types-policy".into(),
        }
    }

    /// A console message in the shape browsers print.
    pub fn console_message(&self) -> String {
        let prefix = if self.disposition == Disposition::Report {
            "[Report Only] "
        } else {
            ""
        };
        let what = match &self.resource {
            ViolationResource::Url(_) | ViolationResource::NavigationOrigin(_) => {
                format!("load '{}'", self.blocked_uri())
            }
            ViolationResource::Inline => "execute inline code".into(),
            ViolationResource::Eval => "evaluate a string as JavaScript".into(),
            ViolationResource::WasmEval => "compile WebAssembly".into(),
            ViolationResource::TrustedTypesSink => {
                "assign a string to a sink that requires a Trusted Type".into()
            }
            ViolationResource::TrustedTypesPolicy => {
                format!("create the Trusted Types policy '{}'", self.sample)
            }
        };
        let fallback =
            if self.directive.split(' ').next() != Some(self.effective_directive.as_str()) {
                format!(
                    " Note that '{}' was not explicitly set, so '{}' is used as a fallback.",
                    self.effective_directive,
                    self.directive.split(' ').next().unwrap_or("")
                )
            } else {
                String::new()
            };
        format!(
            "{prefix}Refused to {what} because it violates the following Content Security Policy directive: \"{}\".{fallback}",
            self.directive
        )
    }
}

/// §5.4 "strip URL for use in reports".
pub fn strip_url_for_report(url: &Url) -> String {
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        return url.scheme().to_string();
    }
    let mut url = url.clone();
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

/// The outcome of a check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// An enforced policy blocks.
    pub blocked: bool,
    pub violations: Vec<Violation>,
}

impl Verdict {
    fn add(&mut self, violation: Violation) {
        if violation.disposition == Disposition::Enforce {
            self.blocked = true;
        }
        self.violations.push(violation);
    }
}

/// What a request carries for CSP: Fetch's request fields the checks read.
#[derive(Clone, Debug)]
pub struct RequestInfo<'a> {
    pub url: &'a Url,
    pub original_url: &'a Url,
    pub destination: Destination,
    pub initiator: Initiator,
    pub nonce: &'a str,
    pub integrity: &'a str,
    pub parser_metadata: ParserMetadata,
    pub redirect_count: u32,
}

impl<'a> RequestInfo<'a> {
    pub fn from_request(request: &'a Request) -> Self {
        RequestInfo {
            url: request.url(),
            original_url: request.original_url(),
            destination: request.destination,
            initiator: request.initiator,
            nonce: &request.nonce,
            integrity: &request.integrity,
            parser_metadata: request.parser_metadata,
            redirect_count: request.redirect_count,
        }
    }
}

/// The kinds of inline behaviour (§4.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineType {
    Script,
    ScriptAttribute,
    Style,
    StyleAttribute,
    /// A `javascript:` URL navigation.
    Navigation,
}

impl InlineType {
    fn effective_directive(self) -> &'static str {
        match self {
            InlineType::Script | InlineType::Navigation => "script-src-elem",
            InlineType::ScriptAttribute => "script-src-attr",
            InlineType::Style => "style-src-elem",
            InlineType::StyleAttribute => "style-src-attr",
        }
    }

    fn script_like(self) -> bool {
        matches!(
            self,
            InlineType::Script | InlineType::ScriptAttribute | InlineType::Navigation
        )
    }
}

/// §6.7.3.1 "Is element nonceable?": not when an attribute name or value contains "<script" or "<style",
/// or the element had a duplicate attribute when it was parsed.
pub fn is_nonceable<'a>(
    attributes: impl IntoIterator<Item = (&'a str, &'a str)>,
    had_duplicate_attribute: bool,
) -> bool {
    if had_duplicate_attribute {
        return false;
    }
    attributes.into_iter().all(|(name, value)| {
        let n = name.to_ascii_lowercase();
        let v = value.to_ascii_lowercase();
        !(n.contains("<script")
            || n.contains("<style")
            || v.contains("<script")
            || v.contains("<style"))
    })
}

fn sample_of(source: &str) -> String {
    source.chars().take(40).collect()
}

/// A document's (or worker's) CSP list.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CspList {
    pub policies: Vec<Policy>,
}

impl CspList {
    pub fn new() -> Self {
        CspList::default()
    }

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// Add a policy that has at least one directive.
    pub fn push(&mut self, policy: Policy) {
        if !policy.directives.is_empty() {
            self.policies.push(policy);
        }
    }

    /// §2.2.3 "parse a response's Content Security Policies": `Content-Security-Policy` and
    /// `Content-Security-Policy-Report-Only`, each a comma-separated list of policies.
    pub fn from_headers(headers: &HeaderList, self_origin: &Origin) -> (CspList, Vec<String>) {
        let mut list = CspList::new();
        let mut warnings = Vec::new();
        for (name, disposition) in [
            ("content-security-policy", Disposition::Enforce),
            ("content-security-policy-report-only", Disposition::Report),
        ] {
            for value in headers.get_all(name) {
                for serialized in value.split(',') {
                    let (policy, w) = Policy::parse(
                        serialized,
                        PolicySource::Header,
                        disposition,
                        self_origin.clone(),
                    );
                    warnings.extend(w);
                    list.push(policy);
                }
            }
        }
        (list, warnings)
    }

    /// Whether any policy came from a header (HTML nonce hiding applies then).
    pub fn has_header_policy(&self) -> bool {
        self.policies
            .iter()
            .any(|p| p.source == PolicySource::Header)
    }

    /// Whether an enforced policy has `upgrade-insecure-requests`.
    pub fn upgrade_insecure_requests(&self) -> bool {
        self.policies.iter().any(|p| {
            p.disposition == Disposition::Enforce
                && p.directive("upgrade-insecure-requests").is_some()
        })
    }

    /// Whether an enforced policy has the (obsolete) `block-all-mixed-content`.
    pub fn block_all_mixed_content(&self) -> bool {
        self.policies.iter().any(|p| {
            p.disposition == Disposition::Enforce
                && p.directive("block-all-mixed-content").is_some()
        })
    }

    /// The enforced `sandbox` directives' tokens, combined; `None` when there is no such directive.
    pub fn sandbox_tokens(&self) -> Option<Vec<String>> {
        let mut out: Option<Vec<String>> = None;
        for p in &self.policies {
            if p.disposition == Disposition::Enforce {
                if let Some(d) = p.directive("sandbox") {
                    out.get_or_insert_with(Vec::new)
                        .extend(d.value.iter().cloned());
                }
            }
        }
        out
    }

    fn url_violation(
        &self,
        index: usize,
        url: &Url,
        effective: &str,
        directive: &Directive,
    ) -> Violation {
        let policy = &self.policies[index];
        Violation {
            resource: ViolationResource::Url(url.clone()),
            effective_directive: effective.to_string(),
            directive: directive.to_string(),
            disposition: policy.disposition,
            policy_index: index,
            original_policy: policy.text.clone(),
            sample: String::new(),
        }
    }

    /// The pre-request check of the directive governing `effective` (§6.7.1).
    fn pre_request_allows(
        policy: &Policy,
        directive: &Directive,
        effective: &str,
        request: &RequestInfo<'_>,
    ) -> bool {
        let list = &directive.sources;
        let script_check = matches!(directive.name.as_str(), "script-src" | "script-src-elem")
            || (directive.name == "default-src" && effective == "script-src-elem")
            || (effective == "worker-src"
                && matches!(directive.name.as_str(), "script-src" | "default-src"));
        if script_check && request.destination.is_script_like() {
            // §6.7.1.1 script directives pre-request check
            if list.matches_nonce(request.nonce) {
                return true;
            }
            if list.matches_integrity(request.integrity) {
                return true;
            }
            if list.strict_dynamic {
                return request.parser_metadata != ParserMetadata::ParserInserted;
            }
        } else if effective == "style-src-elem" && list.matches_nonce(request.nonce) {
            return true;
        }
        list.matches_url(request.url, &policy.self_origin, request.redirect_count)
    }

    /// The URL a violation names after redirects: the first URL, unless that is same-origin with where the
    /// request ended up (the redirect target isn't the page's to know).
    fn violation_url<'u>(request: &RequestInfo<'u>) -> &'u Url {
        if request.redirect_count > 0 && request.original_url.origin() != request.url.origin() {
            request.original_url
        } else {
            request.url
        }
    }

    fn check_request_with(
        &self,
        request: &RequestInfo<'_>,
        disposition: Option<Disposition>,
    ) -> Verdict {
        let mut verdict = Verdict::default();
        let Some(effective) = effective_directive_for(request.destination, request.initiator)
        else {
            return verdict;
        };
        for (index, policy) in self.policies.iter().enumerate() {
            if disposition.is_some_and(|d| d != policy.disposition) {
                continue;
            }
            let Some(directive) = governing(policy, effective) else {
                continue;
            };
            if !Self::pre_request_allows(policy, directive, effective, request) {
                let url = Self::violation_url(request);
                let mut v = self.url_violation(index, url, effective, directive);
                if request.destination.is_navigation() && url.origin() != policy.self_origin {
                    v.resource = ViolationResource::NavigationOrigin(url.clone());
                }
                verdict.add(v);
            }
        }
        verdict
    }

    /// §4.1.1 + §4.1.2: report-only violations, and whether an enforced policy blocks the request.
    pub fn check_request(&self, request: &RequestInfo<'_>) -> Verdict {
        self.check_request_with(request, None)
    }

    /// §4.1.1 "report Content Security Policy violations for request" (report-only policies only).
    pub fn report_only_violations(&self, request: &RequestInfo<'_>) -> Vec<Violation> {
        self.check_request_with(request, Some(Disposition::Report))
            .violations
    }

    /// §4.1.2 "should request be blocked by Content Security Policy?" (enforced policies only).
    pub fn should_block_request(&self, request: &RequestInfo<'_>) -> Verdict {
        self.check_request_with(request, Some(Disposition::Enforce))
    }

    /// §4.1.3 "should response to request be blocked?": the post-request check against the response's URL.
    pub fn should_block_response(&self, request: &RequestInfo<'_>, response_url: &Url) -> Verdict {
        let info = RequestInfo {
            url: response_url,
            ..request.clone()
        };
        self.check_request_with(&info, None)
    }

    /// §4.2.3 "should element's inline type behavior be blocked?". `nonce` is the element's nonce,
    /// `nonceable` [`is_nonceable`]'s answer, `source` the script or style text (or attribute value).
    pub fn should_block_inline(
        &self,
        kind: InlineType,
        nonce: &str,
        nonceable: bool,
        source: &str,
    ) -> Verdict {
        let mut verdict = Verdict::default();
        let effective = kind.effective_directive();
        for (index, policy) in self.policies.iter().enumerate() {
            let Some(directive) = governing(policy, effective) else {
                continue;
            };
            let list = &directive.sources;
            let allowed = list.allows_all_inline(kind.script_like())
                || (matches!(kind, InlineType::Script | InlineType::Style)
                    && nonceable
                    && list.matches_nonce(nonce))
                || ((matches!(kind, InlineType::Script | InlineType::Style) || list.unsafe_hashes)
                    && list.matches_hash(source));
            if allowed {
                continue;
            }
            verdict.add(Violation {
                resource: ViolationResource::Inline,
                effective_directive: effective.into(),
                directive: directive.to_string(),
                disposition: policy.disposition,
                policy_index: index,
                original_policy: policy.text.clone(),
                sample: if list.report_sample {
                    sample_of(source)
                } else {
                    String::new()
                },
            });
        }
        verdict
    }

    /// [`CspList::should_block_inline`] for a `<script>` element's text, with whether the parser
    /// inserted it: under a policy whose `script-src` has 'strict-dynamic', a script that is not
    /// parser-inserted runs (§8.2: 'strict-dynamic' trusts what an allowed script inserts, inline or
    /// not).
    pub fn should_block_inline_script(
        &self,
        nonce: &str,
        nonceable: bool,
        source: &str,
        parser_inserted: bool,
    ) -> Verdict {
        let mut verdict = self.should_block_inline(InlineType::Script, nonce, nonceable, source);
        if !parser_inserted && !verdict.violations.is_empty() {
            verdict.violations.retain(|v| {
                !governing(&self.policies[v.policy_index], "script-src-elem")
                    .is_some_and(|d| d.sources.strict_dynamic)
            });
            verdict.blocked = verdict
                .violations
                .iter()
                .any(|v| v.disposition == Disposition::Enforce);
        }
        verdict
    }

    fn eval_check(&self, wasm: bool, source: &str) -> Verdict {
        let mut verdict = Verdict::default();
        for (index, policy) in self.policies.iter().enumerate() {
            let Some(directive) = policy
                .directive("script-src")
                .or_else(|| policy.directive("default-src"))
            else {
                continue;
            };
            let list = &directive.sources;
            if list.unsafe_eval || (wasm && list.wasm_unsafe_eval) {
                continue;
            }
            verdict.add(Violation {
                resource: if wasm {
                    ViolationResource::WasmEval
                } else {
                    ViolationResource::Eval
                },
                effective_directive: "script-src".into(),
                directive: directive.to_string(),
                disposition: policy.disposition,
                policy_index: index,
                original_policy: policy.text.clone(),
                sample: if list.report_sample && !wasm {
                    sample_of(source)
                } else {
                    String::new()
                },
            });
        }
        verdict
    }

    /// Trusted Types §4.1.1 "Should Trusted Type policy creation be blocked by Content Security
    /// Policy?": `name` against each `trusted-types` directive, given the policy names already made.
    pub fn should_block_trusted_type_policy(&self, name: &str, created: &[String]) -> Verdict {
        let mut verdict = Verdict::default();
        for (index, policy) in self.policies.iter().enumerate() {
            let Some(directive) = policy.directive("trusted-types") else {
                continue;
            };
            let v = &directive.value;
            let has = |k: &str| v.iter().any(|t| t.eq_ignore_ascii_case(k));
            let none = v.len() == 1 && has("'none'");
            let duplicate = created.iter().any(|c| c == name) && !has("'allow-duplicates'");
            let unlisted = !v.iter().any(|t| t == name || t == "*");
            if !(none || duplicate || unlisted) {
                continue;
            }
            verdict.add(Violation {
                resource: ViolationResource::TrustedTypesPolicy,
                effective_directive: "trusted-types".into(),
                directive: directive.to_string(),
                disposition: policy.disposition,
                policy_index: index,
                original_policy: policy.text.clone(),
                sample: sample_of(name),
            });
        }
        verdict
    }

    /// Trusted Types §4.1.2 "Does sink type require trusted types?" for the `'script'` sink group:
    /// (an enforced policy requires them, a report-only one does).
    pub fn requires_trusted_types_for_script(&self) -> (bool, bool) {
        let mut out = (false, false);
        for policy in &self.policies {
            let Some(d) = policy.directive("require-trusted-types-for") else {
                continue;
            };
            if !d.value.iter().any(|t| t == "'script'") {
                continue;
            }
            match policy.disposition {
                Disposition::Enforce => out.0 = true,
                Disposition::Report => out.1 = true,
            }
        }
        out
    }

    /// Trusted Types §4.1.3 "Should sink type mismatch violation be blocked by Content Security
    /// Policy?": a string `source` reached `sink` (e.g. "Element innerHTML").
    pub fn trusted_types_sink_violation(&self, sink: &str, source: &str) -> Verdict {
        let mut verdict = Verdict::default();
        let trimmed: String = source.chars().take(40).collect();
        for (index, policy) in self.policies.iter().enumerate() {
            let Some(directive) = policy.directive("require-trusted-types-for") else {
                continue;
            };
            if !directive.value.iter().any(|t| t == "'script'") {
                continue;
            }
            verdict.add(Violation {
                resource: ViolationResource::TrustedTypesSink,
                effective_directive: "require-trusted-types-for".into(),
                directive: directive.to_string(),
                disposition: policy.disposition,
                policy_index: index,
                original_policy: policy.text.clone(),
                sample: format!("{sink}|{trimmed}"),
            });
        }
        verdict
    }

    /// §4.4 EnsureCSPDoesNotBlockStringCompilation: `eval()`, `new Function()`, string timers.
    pub fn should_block_eval(&self, source: &str) -> Verdict {
        self.eval_check(false, source)
    }

    /// §4.5 WebAssembly compilation: needs 'wasm-unsafe-eval' or 'unsafe-eval'.
    pub fn should_block_wasm(&self) -> Verdict {
        self.eval_check(true, "")
    }

    fn url_directive_check(
        &self,
        name: &str,
        url: &Url,
        redirect_count: u32,
        header_only: bool,
    ) -> Verdict {
        let mut verdict = Verdict::default();
        for (index, policy) in self.policies.iter().enumerate() {
            if header_only && policy.source != PolicySource::Header {
                continue;
            }
            let Some(directive) = policy.directive(name) else {
                continue;
            };
            if directive
                .sources
                .matches_url(url, &policy.self_origin, redirect_count)
            {
                continue;
            }
            verdict.add(self.url_violation(index, url, name, directive));
        }
        verdict
    }

    /// §6.4.1 form-action (the pre-navigation check of a form submission, again after each redirect).
    pub fn should_block_form_action(&self, url: &Url, redirect_count: u32) -> Verdict {
        self.url_directive_check("form-action", url, redirect_count, false)
    }

    /// §6.3.1 base-uri: whether `<base href>` may set the document's base URL to `url`.
    pub fn should_block_base_uri(&self, url: &Url) -> Verdict {
        self.url_directive_check("base-uri", url, 0, false)
    }

    /// §6.4.2 frame-ancestors, on a framed response's own CSP list: every ancestor's origin (nearest
    /// first) must match. Meta policies don't count.
    pub fn should_block_framing(&self, ancestors: &[Origin]) -> Verdict {
        let mut verdict = Verdict::default();
        for (index, policy) in self.policies.iter().enumerate() {
            if policy.source != PolicySource::Header {
                continue;
            }
            let Some(directive) = policy.directive("frame-ancestors") else {
                continue;
            };
            for ancestor in ancestors {
                let url = match Url::parse(&format!("{}/", ancestor.ascii_serialization())) {
                    Ok(u) if ancestor.is_tuple() => u,
                    _ => Url::parse("about:blank").unwrap(),
                };
                if !directive.sources.matches_url(&url, &policy.self_origin, 0) {
                    verdict.add(self.url_violation(index, &url, "frame-ancestors", directive));
                    break;
                }
            }
        }
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(s: &str) -> Origin {
        Url::parse(s).unwrap().origin()
    }

    fn list(policy: &str, self_url: &str) -> CspList {
        let mut l = CspList::new();
        l.push(
            Policy::parse(
                policy,
                PolicySource::Header,
                Disposition::Enforce,
                origin(self_url),
            )
            .0,
        );
        l
    }

    fn allows(policy: &str, self_url: &str, destination: Destination, url: &str) -> bool {
        let l = list(policy, self_url);
        let u = Url::parse(url).unwrap();
        let info = RequestInfo {
            url: &u,
            original_url: &u,
            destination,
            initiator: Initiator::None,
            nonce: "",
            integrity: "",
            parser_metadata: ParserMetadata::None,
            redirect_count: 0,
        };
        !l.check_request(&info).blocked
    }

    const SELF: &str = "http://web-platform.test:8000/page.html";

    #[test]
    fn parsing() {
        let (p, warnings) = Policy::parse(
            "  SCRIPT-SRC 'self' https://a.example ; script-src 'none'; img-src *; ;bad_name x",
            PolicySource::Header,
            Disposition::Enforce,
            origin(SELF),
        );
        assert_eq!(p.directives.len(), 2);
        assert_eq!(p.directives[0].name, "script-src");
        assert_eq!(
            p.directives[0].to_string(),
            "script-src 'self' https://a.example"
        );
        assert_eq!(warnings.len(), 1);
        let (meta, w) = Policy::parse(
            "frame-ancestors 'none'; sandbox; report-uri /r; img-src 'none'",
            PolicySource::Meta,
            Disposition::Enforce,
            origin(SELF),
        );
        assert_eq!(meta.directives.len(), 1);
        assert_eq!(w.len(), 3);
        let mut headers = HeaderList::new();
        headers.append(
            "Content-Security-Policy",
            "img-src 'none', script-src 'self'",
        );
        headers.append("Content-Security-Policy-Report-Only", "default-src 'none'");
        headers.append("Content-Security-Policy", " , ");
        let (l, _) = CspList::from_headers(&headers, &origin(SELF));
        assert_eq!(l.policies.len(), 3);
        assert!(l.policies[2].is_report_only());
    }

    // wpt/content-security-policy/generic/only-valid-whitespaces-are-allowed.html
    #[test]
    fn whitespace_rules() {
        let img = Destination::Image;
        let target = "http://web-platform.test:8000/pass.png";
        for ws in ['\t', '\x0C', '\n', '\r', ' '] {
            assert!(!allows(&format!("img-src{ws}'none';"), SELF, img, target));
            assert!(!allows(
                &format!("img-src http://example.com{ws}http://example2.com;"),
                SELF,
                img,
                target
            ));
        }
        for not_ws in ['\u{A0}', '\x0B'] {
            assert!(allows(
                &format!("img-src{not_ws}'none';"),
                SELF,
                img,
                target
            ));
            assert!(allows(
                &format!("img-src http://example.com{not_ws}http://example2.com;"),
                SELF,
                img,
                target
            ));
        }
    }

    // wpt/content-security-policy/generic/wildcard-host-part.sub.window.js
    #[test]
    fn wildcard_host() {
        let p = "img-src http://*:8000";
        let img = Destination::Image;
        assert!(!allows(
            p,
            SELF,
            img,
            "data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw=="
        ));
        assert!(allows(
            p,
            SELF,
            img,
            "http://web-platform.test:8000/pass.png"
        ));
        assert!(allows(
            p,
            SELF,
            img,
            "http://www1.web-platform.test:8000/pass.png"
        ));
        assert!(!allows(
            p,
            SELF,
            img,
            "http://web-platform.test:8001/fail.png"
        ));
        // "*.example.com" needs a subdomain
        assert!(allows(
            "img-src *.example.com",
            SELF,
            img,
            "http://a.b.example.com/x"
        ));
        assert!(!allows(
            "img-src *.example.com",
            SELF,
            img,
            "http://example.com/x"
        ));
    }

    // wpt/content-security-policy/generic/source-list-wildcard-host-and-port.https.sub.html
    #[test]
    fn wildcard_host_and_port_with_path() {
        let p = "connect-src *:*/content-security-policy/support/resource.py";
        let s = "https://web-platform.test:8443/page";
        let e = Destination::Empty;
        assert!(allows(
            p,
            s,
            e,
            "https://web-platform.test:8443/content-security-policy/support/resource.py?x"
        ));
        assert!(allows(
            p,
            s,
            e,
            "https://web-platform.test:8444/content-security-policy/support/resource.py"
        ));
        assert!(allows(
            p,
            s,
            e,
            "https://not-web-platform.test:8443/content-security-policy/support/resource.py"
        ));
        assert!(!allows(
            p,
            s,
            e,
            "https://web-platform.test:8443/content-security-policy/support/other.py"
        ));
    }

    // wpt/content-security-policy/generic/src-trailing-dot.sub.any.js
    #[test]
    fn trailing_dot() {
        assert!(allows(
            "connect-src http://web-platform.test.:8000",
            SELF,
            Destination::Empty,
            "http://web-platform.test.:8000/content-security-policy/support/resource.py"
        ));
    }

    #[test]
    fn self_and_schemes() {
        let s = "http://a.example/";
        let e = Destination::Empty;
        assert!(allows("connect-src 'self'", s, e, "http://a.example/x"));
        assert!(allows("connect-src 'self'", s, e, "https://a.example/x"));
        assert!(allows("connect-src 'self'", s, e, "ws://a.example/x"));
        assert!(allows("connect-src 'self'", s, e, "wss://a.example/x"));
        assert!(!allows("connect-src 'self'", s, e, "http://b.example/x"));
        assert!(!allows(
            "connect-src 'self'",
            "https://a.example/",
            e,
            "http://a.example/x"
        ));
        assert!(!allows("connect-src 'self'", s, e, "http://a.example:81/x"));
        assert!(allows(
            "connect-src https:",
            s,
            e,
            "https://anything.example/"
        ));
        assert!(!allows(
            "connect-src https:",
            s,
            e,
            "http://anything.example/"
        ));
        assert!(allows(
            "connect-src http:",
            s,
            e,
            "https://anything.example/"
        ));
        assert!(allows(
            "img-src data:",
            s,
            Destination::Image,
            "data:image/png,x"
        ));
        assert!(!allows(
            "img-src *",
            s,
            Destination::Image,
            "data:image/png,x"
        ));
        assert!(allows(
            "img-src *",
            s,
            Destination::Image,
            "https://x.example/"
        ));
        assert!(!allows(
            "img-src 'none'",
            s,
            Destination::Image,
            "http://a.example/"
        ));
        // host-source without a scheme follows the page's scheme (upgrades allowed)
        assert!(allows("connect-src b.example", s, e, "https://b.example/"));
        assert!(!allows(
            "connect-src b.example",
            "https://a.example/",
            e,
            "http://b.example/"
        ));
        assert!(allows(
            "connect-src b.example:80",
            s,
            e,
            "https://b.example/"
        ));
    }

    #[test]
    fn paths_and_redirects() {
        let s = "https://a.example/";
        let e = Destination::Empty;
        assert!(allows(
            "connect-src https://b.example/js/",
            s,
            e,
            "https://b.example/js/app.js"
        ));
        assert!(!allows(
            "connect-src https://b.example/js/",
            s,
            e,
            "https://b.example/css/app.css"
        ));
        assert!(allows(
            "connect-src https://b.example/js/app.js",
            s,
            e,
            "https://b.example/js/app.js"
        ));
        assert!(!allows(
            "connect-src https://b.example/js/app.js",
            s,
            e,
            "https://b.example/js/app.jsx"
        ));
        assert!(allows(
            "connect-src https://b.example/a%20b",
            s,
            e,
            "https://b.example/a b"
        ));
        // after a redirect, paths are ignored (§6.7.2.5 step 3.4)
        let l = list("connect-src https://b.example/js/", s);
        let first = Url::parse("https://a.example/start").unwrap();
        let last = Url::parse("https://b.example/elsewhere").unwrap();
        let info = RequestInfo {
            url: &last,
            original_url: &first,
            destination: e,
            initiator: Initiator::None,
            nonce: "",
            integrity: "",
            parser_metadata: ParserMetadata::None,
            redirect_count: 1,
        };
        assert!(!l.check_request(&info).blocked);
    }

    #[test]
    fn fallbacks_and_effective_directives() {
        let s = "https://a.example/";
        assert!(!allows(
            "default-src 'none'",
            s,
            Destination::Image,
            "https://a.example/i.png"
        ));
        assert!(allows(
            "default-src 'none'; img-src 'self'",
            s,
            Destination::Image,
            "https://a.example/i.png"
        ));
        assert!(!allows(
            "child-src 'none'",
            s,
            Destination::Iframe,
            "https://a.example/f"
        ));
        assert!(allows(
            "child-src 'none'; frame-src *",
            s,
            Destination::Iframe,
            "https://b.example/f"
        ));
        assert!(!allows(
            "script-src 'none'",
            s,
            Destination::Worker,
            "https://a.example/w.js"
        ));
        assert!(allows(
            "script-src 'none'; worker-src 'self'",
            s,
            Destination::Worker,
            "https://a.example/w.js"
        ));
        // document navigations aren't governed by fetch directives
        assert!(allows(
            "default-src 'none'",
            s,
            Destination::Document,
            "https://b.example/"
        ));
        assert!(!allows(
            "connect-src 'none'",
            s,
            Destination::Empty,
            "https://a.example/api"
        ));
    }

    fn script_request(policy: &str, nonce: &str, integrity: &str, parser: ParserMetadata) -> bool {
        let l = list(policy, "https://a.example/");
        let u = Url::parse("https://cdn.example/s.js").unwrap();
        let info = RequestInfo {
            url: &u,
            original_url: &u,
            destination: Destination::Script,
            initiator: Initiator::None,
            nonce,
            integrity,
            parser_metadata: parser,
            redirect_count: 0,
        };
        !l.check_request(&info).blocked
    }

    #[test]
    fn nonces_hashes_strict_dynamic() {
        use ParserMetadata::*;
        assert!(script_request(
            "script-src 'nonce-abc'",
            "abc",
            "",
            ParserInserted
        ));
        assert!(!script_request(
            "script-src 'nonce-abc'",
            "abd",
            "",
            ParserInserted
        ));
        assert!(!script_request(
            "script-src 'nonce-abc'",
            "",
            "",
            ParserInserted
        ));
        let h = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        assert!(script_request(
            &format!("script-src '{h}'"),
            "",
            h,
            ParserInserted
        ));
        assert!(script_request(
            &format!("script-src '{}'", h.replace('+', "-").replace('/', "_")),
            "",
            h,
            ParserInserted
        ));
        assert!(!script_request(
            &format!("script-src '{h}'"),
            "",
            "sha256-other",
            ParserInserted
        ));
        assert!(script_request(
            "script-src 'strict-dynamic' 'nonce-x'",
            "",
            "",
            NotParserInserted
        ));
        assert!(!script_request(
            "script-src 'strict-dynamic' 'nonce-x'",
            "",
            "",
            ParserInserted
        ));
        assert!(script_request(
            "script-src 'strict-dynamic' 'nonce-x'",
            "x",
            "",
            ParserInserted
        ));
        // 'strict-dynamic' ignores host sources
        assert!(!script_request(
            "script-src 'strict-dynamic' https://cdn.example",
            "",
            "",
            ParserInserted
        ));
    }

    #[test]
    fn inline_checks() {
        let l = list(
            "script-src 'nonce-abc' 'sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU='",
            "https://a.example/",
        );
        assert!(
            !l.should_block_inline(InlineType::Script, "abc", true, "x()")
                .blocked
        );
        assert!(
            l.should_block_inline(InlineType::Script, "abc", false, "x()")
                .blocked
        );
        // the empty string's SHA-256
        assert!(
            !l.should_block_inline(InlineType::Script, "", true, "")
                .blocked
        );
        assert!(
            l.should_block_inline(InlineType::ScriptAttribute, "", true, "")
                .blocked
        );
        let hashes = list(
            "script-src 'unsafe-hashes' 'sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU='",
            "https://a.example/",
        );
        assert!(
            !hashes
                .should_block_inline(InlineType::ScriptAttribute, "", true, "")
                .blocked
        );
        // 'unsafe-inline' is ignored next to a nonce
        let both = list("script-src 'unsafe-inline' 'nonce-a'", "https://a.example/");
        assert!(
            both.should_block_inline(InlineType::Script, "", true, "x")
                .blocked
        );
        let inline = list(
            "style-src 'unsafe-inline' 'report-sample'; script-src 'report-sample'",
            "https://a.example/",
        );
        assert!(
            !inline
                .should_block_inline(InlineType::StyleAttribute, "", true, "color:red")
                .blocked
        );
        let v = inline.should_block_inline(InlineType::Script, "", true, &"a".repeat(50));
        assert!(v.blocked);
        assert_eq!(v.violations[0].sample.len(), 40);
        assert_eq!(v.violations[0].blocked_uri(), "inline");
        assert!(is_nonceable([("src", "a.js")], false));
        assert!(!is_nonceable([("src", "<script")], false));
        assert!(!is_nonceable([("src", "a")], true));
    }

    #[test]
    fn eval_and_wasm() {
        let l = list("script-src 'self'", "https://a.example/");
        assert!(l.should_block_eval("1+1").blocked);
        assert!(l.should_block_wasm().blocked);
        let l = list("default-src 'wasm-unsafe-eval'", "https://a.example/");
        assert!(l.should_block_eval("1").blocked);
        assert!(!l.should_block_wasm().blocked);
        let l = list("script-src 'unsafe-eval'", "https://a.example/");
        assert!(!l.should_block_eval("1").blocked);
        let l = list("img-src 'none'", "https://a.example/");
        assert!(!l.should_block_eval("1").blocked);
    }

    #[test]
    fn report_only_reports_without_blocking() {
        let mut l = CspList::new();
        l.push(
            Policy::parse(
                "img-src 'none'",
                PolicySource::Header,
                Disposition::Report,
                origin(SELF),
            )
            .0,
        );
        let u = Url::parse("http://web-platform.test:8000/a#frag").unwrap();
        let info = RequestInfo {
            url: &u,
            original_url: &u,
            destination: Destination::Image,
            initiator: Initiator::None,
            nonce: "",
            integrity: "",
            parser_metadata: ParserMetadata::None,
            redirect_count: 0,
        };
        let v = l.check_request(&info);
        assert!(!v.blocked);
        assert_eq!(v.violations.len(), 1);
        assert_eq!(
            v.violations[0].blocked_uri(),
            "http://web-platform.test:8000/a"
        );
        assert!(v.violations[0]
            .console_message()
            .starts_with("[Report Only] Refused to load"));
        assert_eq!(l.report_only_violations(&info).len(), 1);
        assert!(l.should_block_request(&info).violations.is_empty());
    }

    #[test]
    fn navigation_directives() {
        let l = list(
            "form-action 'self'; base-uri 'none'; frame-ancestors https://top.example",
            "https://a.example/",
        );
        assert!(
            !l.should_block_form_action(&Url::parse("https://a.example/submit").unwrap(), 0)
                .blocked
        );
        assert!(
            l.should_block_form_action(&Url::parse("https://b.example/submit").unwrap(), 0)
                .blocked
        );
        assert!(
            l.should_block_base_uri(&Url::parse("https://a.example/").unwrap())
                .blocked
        );
        assert!(
            !l.should_block_framing(&[origin("https://top.example/")])
                .blocked
        );
        assert!(
            l.should_block_framing(&[
                origin("https://top.example/"),
                origin("https://evil.example/")
            ])
            .blocked
        );
        assert!(l.should_block_framing(&[Origin::new_opaque()]).blocked);
        let flags = list(
            "upgrade-insecure-requests; sandbox allow-scripts",
            "https://a.example/",
        );
        assert!(flags.upgrade_insecure_requests());
        assert_eq!(
            flags.sandbox_tokens(),
            Some(vec!["allow-scripts".to_string()])
        );
    }

    #[test]
    fn strict_dynamic_trusts_inserted_inline_scripts() {
        let l = list("script-src 'nonce-a' 'strict-dynamic'", SELF);
        assert!(l.should_block_inline_script("", true, "x()", true).blocked);
        assert!(!l.should_block_inline_script("", true, "x()", false).blocked);
        assert!(!l.should_block_inline_script("a", true, "x()", true).blocked);
        let l = list("script-src 'nonce-a'", SELF);
        assert!(l.should_block_inline_script("", true, "x()", false).blocked);
    }

    #[test]
    fn self_does_not_match_local_schemes() {
        let blob = "blob:http://web-platform.test:8000/0f5d4a6e-0000-4000-8000-000000000000";
        assert!(!allows(
            "script-src 'self'",
            SELF,
            Destination::Script,
            blob
        ));
        assert!(allows(
            "script-src 'self' blob:",
            SELF,
            Destination::Script,
            blob
        ));
        assert!(allows(
            "script-src 'self'",
            SELF,
            Destination::Script,
            "http://web-platform.test:8000/s.js"
        ));
    }

    #[test]
    fn cross_origin_frame_violations_name_the_origin_only() {
        let l = list("frame-src 'self'", SELF);
        let frame = Url::parse("http://www1.web-platform.test:8000/path/page.html?q").unwrap();
        let info = RequestInfo {
            url: &frame,
            original_url: &frame,
            destination: Destination::Iframe,
            initiator: Initiator::None,
            nonce: "",
            integrity: "",
            parser_metadata: ParserMetadata::None,
            redirect_count: 0,
        };
        let v = l.check_request(&info);
        assert!(v.blocked);
        assert_eq!(
            v.violations[0].blocked_uri(),
            "http://www1.web-platform.test:8000"
        );
        // An image keeps its whole URL.
        let info = RequestInfo {
            destination: Destination::Image,
            ..info
        };
        let l = list("img-src 'none'", SELF);
        assert_eq!(
            l.check_request(&info).violations[0].blocked_uri(),
            "http://www1.web-platform.test:8000/path/page.html?q"
        );
    }
}
