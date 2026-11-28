// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Fetch Standard's algorithms (§4): fetch, main fetch, scheme fetch, HTTP fetch, HTTP-redirect fetch,
//! HTTP-network-or-cache fetch, HTTP-network fetch and CORS-preflight fetch. They run on the network
//! thread; their results go to the page through a [`Sink`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use bytes::Bytes;
use http_body_util::BodyExt;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::blob::BlobStore;
use crate::client::FetchEvent;
use crate::config::NetConfig;
use crate::cookies::{CookieContext, CookieStore};
use crate::cors::{cors_check, evaluate_preflight, exposed_header_names, PreflightCache};
use crate::csp::RequestInfo;
use crate::data_url::process_data_url;
use crate::decode::ContentDecoder;
use crate::filter::{FilterAction, RequestFilter};
use crate::headers::{
    cors_unsafe_request_header_names, is_cors_safelisted_method, is_request_body_header_name,
    parse_single_range, HeaderList,
};
use crate::hsts::HstsStore;
use crate::metadata::{default_accept, fetch_metadata_headers, origin_header, ACCEPT_ENCODING};
use crate::mime::extract_mime_type;
use crate::origin::{
    is_http_scheme, is_local_scheme, is_url_potentially_trustworthy, origin_of, same_site,
};
use crate::policy::{
    corp_blocks, file_access_allowed, is_bad_port_request, is_blocked_mixed_content,
    upgrade_insecure_request, upgrade_mixed_content,
};
use crate::referrer::{determine_referrer, parse_referrer_policy_header, DEFAULT_REFERRER_POLICY};
use crate::request::{
    CacheMode, CredentialsMode, Destination, Mode, RedirectMode, Referrer, ReferrerPolicy, Request,
    RequestOrigin, ResponseTainting,
};
use crate::response::{BlockReason, NetError, NetErrorKind, ResponseHead, ResponseType};
use crate::transport::{H1Return, Transport, WireRequest};

/// Everything the network thread shares between fetches.
pub(crate) struct NetState {
    pub config: Arc<NetConfig>,
    /// The user agent now ([`crate::NetClient::set_user_agent`]; starts as the config's).
    pub user_agent: std::sync::RwLock<Arc<crate::ua::UserAgent>>,
    /// Starts a fetch nobody reads, on the network runtime (a violation report sent from a fetch):
    /// set by the owner once the state is shared, so a fetch spawning a fetch is no recursive
    /// future.
    pub(crate) detached: std::sync::OnceLock<Box<dyn Fn(Request, Sink) + Send + Sync>>,
    /// Parses scripts for opaque-response blocking ([`crate::orb::ScriptCheck`]); set by the
    /// owner, or absent (such responses are then blocked).
    pub(crate) script_check: std::sync::OnceLock<Arc<dyn crate::orb::ScriptCheck>>,
    /// The URLs whose `no-cors` response to a media element sniffed as audio or video, by the
    /// requesting client's origin: a later range request of the media element for the same URL
    /// (the proposal's "subsequent" no-cors media request state) is let through without sniffing
    /// what the middle of the file looks like. Only this layer's own verdicts go in, never the
    /// renderer's word.
    pub(crate) media_urls: std::sync::Mutex<std::collections::HashSet<(String, String)>>,
    pub transport: Arc<Transport>,
    pub cookies: Arc<CookieStore>,
    pub hsts: Arc<HstsStore>,
    pub blobs: Arc<BlobStore>,
    pub preflight: PreflightCache,
    pub preloads: crate::preload::PreloadList,
    /// The HTTP cache.
    pub http_cache: Arc<crate::http_cache::HttpCache>,
}

/// Where a fetch's events go: the page's [`crate::ResponseStream`], plus an optional wake-up callback.
pub(crate) struct Sink {
    /// The page's stream (none for a preload, whose events go to its entry through `observer`).
    pub tx: Option<tokio::sync::mpsc::Sender<FetchEvent>>,
    pub notify: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Sees every event on the network thread before it is queued (see
    /// [`crate::NetClient::fetch_observed`]).
    pub observer: Option<crate::client::Observer>,
    /// The client's content filter ([`crate::filter`]).
    pub filter: Option<std::sync::Arc<dyn RequestFilter>>,
    /// Done or Error was sent.
    pub ended: std::sync::atomic::AtomicBool,
}

impl Sink {
    /// False when the page dropped its stream (the fetch should stop).
    async fn send(&self, event: FetchEvent) -> bool {
        if matches!(event, FetchEvent::Done | FetchEvent::Error(_)) {
            self.ended.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(observe) = &self.observer {
            observe(&event);
        }
        let ok = match &self.tx {
            Some(tx) => tx.send(event).await.is_ok(),
            None => true,
        };
        if let Some(notify) = &self.notify {
            notify();
        }
        ok
    }
}

impl Drop for Sink {
    /// A fetch cancelled midway (its stream dropped, the network thread stopping): the observer
    /// learns the body will not complete.
    fn drop(&mut self) {
        if !self.ended.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(observe) = &self.observer {
                observe(&FetchEvent::Error(NetError::new(
                    NetErrorKind::Aborted,
                    "the fetch was cancelled",
                )));
            }
        }
    }
}

/// A response body still being read.
pub(crate) struct NetworkBody {
    body: hyper::body::Incoming,
    decoder: Option<ContentDecoder>,
    release: Option<H1Return>,
    read_timeout: Duration,
    done: bool,
}

impl NetworkBody {
    async fn next(&mut self) -> Result<Option<Bytes>, NetError> {
        loop {
            if self.done {
                return Ok(None);
            }
            let frame = tokio::time::timeout(self.read_timeout, self.body.frame())
                .await
                .map_err(|_| {
                    NetError::new(NetErrorKind::Timeout, "the server stopped sending the body")
                })?;
            match frame {
                None => {
                    self.done = true;
                    if let Some(release) = self.release.take() {
                        release.release();
                    }
                    if let Some(decoder) = &mut self.decoder {
                        let tail = decoder.finish().map_err(decoding_error)?;
                        if !tail.is_empty() {
                            return Ok(Some(Bytes::from(tail)));
                        }
                    }
                    return Ok(None);
                }
                Some(Err(e)) => {
                    return Err(NetError::new(
                        if e.is_timeout() {
                            NetErrorKind::Timeout
                        } else {
                            NetErrorKind::Io
                        },
                        format!("reading the body failed: {e}"),
                    ))
                }
                Some(Ok(frame)) => {
                    let Ok(data) = frame.into_data() else {
                        continue; // (trailers)
                    };
                    match &mut self.decoder {
                        None => return Ok(Some(data)),
                        Some(decoder) => {
                            let out = decoder.push(&data).map_err(decoding_error)?;
                            if !out.is_empty() {
                                return Ok(Some(Bytes::from(out)));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn decoding_error(e: std::io::Error) -> NetError {
    NetError::new(
        NetErrorKind::Protocol,
        format!("content decoding failed: {e}"),
    )
}

pub(crate) enum BodySource {
    None,
    Bytes(Bytes),
    Network(Box<NetworkBody>),
    /// A network body kept for the HTTP cache as it is read.
    Caching(Box<CachingBody>),
    File(tokio::fs::File),
    /// Bytes already read from a body (sniffed), then the rest of it.
    Chained(Bytes, Box<BodySource>),
}

/// A response body read from the network and copied as it goes, stored in the HTTP cache once read
/// to its end (not when it is dropped early or turns out too large).
pub(crate) struct CachingBody {
    inner: NetworkBody,
    copy: Option<Vec<u8>>,
    cache: Arc<crate::http_cache::HttpCache>,
    key: crate::http_cache::Key,
    request_headers: HeaderList,
    status: u16,
    reason: String,
    headers: HeaderList,
    request_time: i64,
    response_time: i64,
}

impl CachingBody {
    async fn next(&mut self) -> Result<Option<Bytes>, NetError> {
        match self.inner.next().await {
            Ok(Some(chunk)) => {
                if let Some(copy) = &mut self.copy {
                    if copy.len() + chunk.len() > crate::http_cache::MAX_ENTRY {
                        self.copy = None;
                    } else {
                        copy.extend_from_slice(&chunk);
                    }
                }
                Ok(Some(chunk))
            }
            Ok(None) => {
                if let Some(copy) = self.copy.take() {
                    self.cache.store(
                        self.key.clone(),
                        &self.request_headers,
                        crate::http_cache::Response {
                            status: self.status,
                            reason: std::mem::take(&mut self.reason),
                            headers: std::mem::take(&mut self.headers),
                            body: Bytes::from(copy),
                            request_time: self.request_time,
                            response_time: self.response_time,
                        },
                    );
                }
                Ok(None)
            }
            Err(e) => {
                self.copy = None;
                Err(e)
            }
        }
    }
}

impl BodySource {
    pub async fn next(&mut self) -> Result<Option<Bytes>, NetError> {
        // (a chained body's rest is never chained itself, so this unwraps at most once)
        if let BodySource::Chained(first, rest) = self {
            if !first.is_empty() {
                return Ok(Some(std::mem::take(first)));
            }
            *self = std::mem::replace(&mut **rest, BodySource::None);
        }
        match self {
            BodySource::None | BodySource::Chained(..) => Ok(None),
            BodySource::Bytes(b) => {
                let b = std::mem::take(b);
                *self = BodySource::None;
                Ok((!b.is_empty()).then_some(b))
            }
            BodySource::Network(n) => n.next().await,
            BodySource::Caching(c) => c.next().await,
            BodySource::File(f) => {
                let mut buf = Vec::with_capacity(64 * 1024);
                let n = f.read_buf(&mut buf).await.map_err(|e| {
                    NetError::new(NetErrorKind::Io, format!("reading the file failed: {e}"))
                })?;
                if n == 0 {
                    *self = BodySource::None;
                    return Ok(None);
                }
                Ok(Some(Bytes::from(buf)))
            }
        }
    }

    /// Read the rest and drop it (so an HTTP/1.1 connection can go back to the pool). Large leftovers are
    /// not worth the wait: the connection is closed instead.
    pub async fn discard(&mut self) {
        if let BodySource::Chained(_, rest) = self {
            *self = std::mem::replace(&mut **rest, BodySource::None);
        }
        let mut budget: usize = 256 * 1024;
        let network = match self {
            BodySource::Network(n) => Some(&mut **n),
            BodySource::Caching(c) => Some(&mut c.inner),
            _ => None,
        };
        if let Some(n) = network {
            while let Ok(Some(chunk)) = n.next().await {
                if chunk.len() >= budget {
                    break;
                }
                budget -= chunk.len();
            }
        }
        *self = BodySource::None;
    }
}

/// What one pass through main fetch ended with: a response, or a redirect that HTTP-redirect fetch has
/// already applied to the request (the spec's recursive main fetch, run as a loop).
// (a transient value: boxing the response would cost an allocation per hop for nothing)
#[allow(clippy::large_enum_variant)]
pub(crate) enum Hop {
    Response(Fetched),
    Redirect,
}

/// A response with its body to come.
pub(crate) struct Fetched {
    pub head: ResponseHead,
    pub body: BodySource,
}

fn simple_response(status: u16, text: &str, headers: HeaderList, body: Bytes) -> Fetched {
    let mut head = ResponseHead::new(status, text);
    head.headers = headers;
    Fetched {
        head,
        body: BodySource::Bytes(body),
    }
}

/// The largest body opaque-response blocking parses as a script; a larger one is blocked.
pub(crate) const MAX_SCRIPT_PARSE: usize = 32 * 1024 * 1024;

/// Fetch "null body status".
fn is_null_body_status(status: u16) -> bool {
    matches!(status, 101 | 103 | 204 | 205 | 304)
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Fetch "determine nosniff" + "should response to request be blocked due to nosniff?" + "... due to its
/// MIME type?".
fn mime_blocks(request: &Request, headers: &HeaderList) -> Option<String> {
    let mime = extract_mime_type(headers);
    let essence = mime.as_ref().map(|m| m.essence());
    if request.destination.is_script_like() {
        if let Some(e) = &essence {
            if e.starts_with("audio/")
                || e.starts_with("image/")
                || e.starts_with("video/")
                || e == "text/csv"
            {
                return Some(format!("a script can't be of type {e}"));
            }
        }
    }
    if !crate::orb::determine_nosniff(headers) {
        return None;
    }
    if request.destination.is_script_like() && !mime.as_ref().is_some_and(|m| m.is_javascript()) {
        return Some(format!(
            "nosniff: '{}' is not a JavaScript MIME type",
            essence.unwrap_or_default()
        ));
    }
    if request.destination == Destination::Style && essence.as_deref() != Some("text/css") {
        return Some(format!(
            "nosniff: '{}' is not text/css",
            essence.unwrap_or_default()
        ));
    }
    None
}

/// The extension's type, for file: responses (and files dropped on a page).
pub fn file_mime_type(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "xhtml" | "xht" => "application/xhtml+xml",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "txt" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

impl NetState {
    /// The user agent in force.
    pub(crate) fn user_agent(&self) -> Arc<crate::ua::UserAgent> {
        self.user_agent
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn trace(&self, line: impl FnOnce() -> String) {
        if self.config.trace {
            eprintln!("{}", line());
        }
    }

    /// Fetch §4.1 "fetch": the entry point. Sends every event to `sink`.
    pub async fn fetch(self: Arc<Self>, request: Request, sink: Sink) {
        self.fetch_with(request, sink, true).await
    }

    /// "fetch"; `consume_preloads` is false for a preload's own fetch.
    pub async fn fetch_with(
        self: Arc<Self>,
        mut request: Request,
        sink: Sink,
        consume_preloads: bool,
    ) {
        // The step that consumes a preloaded resource: a request from a document whose preload key
        // matches a preload takes its response, in flight or done.
        if consume_preloads {
            if let Some((events, mut rest)) = self.preloads.consume(&request) {
                self.trace(|| format!("[net] preloaded   {}", request.url()));
                let mut ended = false;
                for e in events {
                    ended |= matches!(e, FetchEvent::Done | FetchEvent::Error(_));
                    if !sink.send(e).await {
                        return;
                    }
                }
                while !ended {
                    let Some(e) = rest.recv().await else {
                        sink.send(FetchEvent::Error(NetError::new(
                            NetErrorKind::Aborted,
                            "the preload stopped",
                        )))
                        .await;
                        return;
                    };
                    ended = matches!(e, FetchEvent::Done | FetchEvent::Error(_));
                    if !sink.send(e).await {
                        return;
                    }
                }
                return;
            }
        }
        let started = Instant::now();
        if request.origin == RequestOrigin::Client {
            request.origin = RequestOrigin::Origin(request.resolved_origin());
        }
        if !request.headers.contains("accept") {
            let accept = default_accept(&request);
            request.headers.append("Accept", accept);
        }
        if !request.headers.contains("accept-language") {
            request
                .headers
                .append("Accept-Language", &self.config.accept_language);
        }
        let result = Box::pin(self.main_fetch(&mut request, &sink)).await;
        let mut fetched = match result {
            Ok(f) => f,
            Err(e) => {
                self.trace(|| {
                    format!(
                        "[net] ERR {:>5} ms {} {}: {}",
                        started.elapsed().as_millis(),
                        request.method,
                        request.url(),
                        e.message
                    )
                });
                sink.send(FetchEvent::Error(e)).await;
                return;
            }
        };
        if !request.integrity.is_empty() {
            // (SRI needs the whole body before any of it may be used)
            let mut all = Vec::new();
            loop {
                match fetched.body.next().await {
                    Ok(Some(chunk)) => all.extend_from_slice(&chunk),
                    Ok(None) => break,
                    Err(e) => {
                        sink.send(FetchEvent::Error(e)).await;
                        return;
                    }
                }
            }
            if !crate::integrity::bytes_match(&fetched.head, &all, &request.integrity) {
                let e = NetError::new(
                    NetErrorKind::Integrity,
                    format!(
                        "the resource {} does not match its integrity metadata",
                        request.url()
                    ),
                );
                sink.send(FetchEvent::Error(e)).await;
                return;
            }
            fetched.body = BodySource::Bytes(Bytes::from(all));
        }
        let url = request.url().clone();
        fetched.head.timing.start = Some(started);
        if !sink.send(FetchEvent::Head(fetched.head)).await {
            return;
        }
        let mut total = 0usize;
        loop {
            match fetched.body.next().await {
                Ok(Some(chunk)) => {
                    total += chunk.len();
                    if !sink.send(FetchEvent::Body(chunk)).await {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    self.trace(|| format!("[net] ERR body {url}: {}", e.message));
                    sink.send(FetchEvent::Error(e)).await;
                    return;
                }
            }
        }
        self.trace(|| {
            format!(
                "[net] done {:>5} ms {total:>9} B {url}",
                started.elapsed().as_millis()
            )
        });
        sink.send(FetchEvent::Done).await;
    }

    /// Fetch §4.2 "main fetch": the first pass and, for each redirect, the recursive ones; then the
    /// response filtering and checks the non-recursive call does.
    async fn main_fetch(
        self: &Arc<Self>,
        request: &mut Request,
        sink: &Sink,
    ) -> Result<Fetched, NetError> {
        // A policy the content filter adds to the final response (`$csp`).
        let mut filter_csp = None;
        // Fetch timing info: the end of the redirects (the start of the final request).
        let mut redirect_end = None;
        let mut fetched = loop {
            match self.main_fetch_pass(request, sink, &mut filter_csp).await? {
                Hop::Response(f) => break f,
                Hop::Redirect => redirect_end = Some(Instant::now()),
            }
        };
        fetched.head.timing.redirect_end = redirect_end;
        if let Some(policy) = filter_csp {
            fetched
                .head
                .headers
                .append("Content-Security-Policy", &policy);
        }
        if fetched.head.response_type == ResponseType::Default {
            fetched.head.response_type = match request.response_tainting {
                ResponseTainting::Basic => ResponseType::Basic,
                ResponseTainting::Cors => {
                    fetched.head.cors_exposed_header_names =
                        exposed_header_names(request, &fetched.head.headers);
                    ResponseType::Cors
                }
                ResponseTainting::Opaque => ResponseType::Opaque,
            };
        }
        if fetched.head.url_list.is_empty() {
            fetched.head.url_list = request.url_list.clone();
        }
        if request.mode == Mode::Navigate {
            fetched.head.redirects = request.navigation_redirects.clone().into_boxed_slice();
            // The referrer main fetch determined last (each redirect determines it again).
            fetched.head.referrer = match &request.referrer {
                Referrer::Url(url) => Some(url.clone()),
                _ => None,
            };
        }
        if fetched.head.response_type != ResponseType::OpaqueRedirect {
            if let Some(ancestors) = request.frame_ancestors.clone() {
                self.check_framing(request, &mut fetched, &ancestors, sink)
                    .await?;
            }
            if let Some(reason) = mime_blocks(request, &fetched.head.headers) {
                fetched.body.discard().await;
                return Err(NetError::blocked(
                    BlockReason::Nosniff,
                    format!("{}: {reason}", request.url()),
                ));
            }
            // (a response whose final URL is the client's own origin, reached through a
            // cross-origin redirect, is opaque to script but no other site's data: unchecked,
            // as the reference implementations have it)
            let own_origin = request
                .client
                .as_ref()
                .is_some_and(|c| c.origin == crate::origin::origin_of(request.url(), None));
            if request.mode == Mode::NoCors
                && request.response_tainting == ResponseTainting::Opaque
                && request.client.is_some()
                && !own_origin
            {
                self.check_opaque_response(request, &mut fetched).await?;
            }
        }
        if request.method == "HEAD"
            || request.method == "CONNECT"
            || is_null_body_status(fetched.head.status)
        {
            fetched.body.discard().await;
        }
        Ok(fetched)
    }

    /// Fetch "opaque-response-safelist check" (opaque-response blocking, [`crate::orb`]) on a
    /// cross-origin `no-cors` response, here, before any of it reaches the requester: a response
    /// that is neither labelled nor sniffed as a script, style sheet, image or media, and does
    /// not parse as a script, is a network error ([`BlockReason::OpaqueResponse`]) with its body
    /// discarded. Where this departs from the Fetch text: audio and video types are safelisted
    /// without sniffing, and a body that sniffs as media is let through whatever requested it
    /// (the text admits only a media element's own fetch), both as the reference implementations
    /// have it; a body past [`MAX_SCRIPT_PARSE`] is blocked instead of parsed; a response whose
    /// final URL is the client's own origin is not checked at all.
    async fn check_opaque_response(
        &self,
        request: &Request,
        fetched: &mut Fetched,
    ) -> Result<(), NetError> {
        use crate::orb;
        let mime = extract_mime_type(&fetched.head.headers);
        let essence = mime.as_ref().map(|m| m.essence());
        let nosniff = orb::determine_nosniff(&fetched.head.headers);
        let status = fetched.head.status;
        let blocked = |why: &str| -> NetError {
            NetError::blocked(
                BlockReason::OpaqueResponse,
                format!(
                    "Blocked the cross-origin response from '{}' (type '{}'): {why}; a no-cors \
                     request only gets scripts, style sheets, images and media \
                     (opaque-response blocking).",
                    request.url(),
                    essence.as_deref().unwrap_or("")
                ),
            )
        };
        if let Some(m) = &mime {
            if orb::is_safelisted(m) {
                return Ok(());
            }
            let essence = m.essence();
            if orb::is_never_sniffed(&essence) {
                fetched.body.discard().await;
                return Err(blocked("a type never read without CORS"));
            }
            let blocklisted = orb::is_blocklisted(m);
            if status == 206 && blocklisted {
                fetched.body.discard().await;
                return Err(blocked("a partial HTML, JSON or XML response"));
            }
            if nosniff && (blocklisted || essence == "text/plain") {
                fetched.body.discard().await;
                return Err(blocked("the type is final (nosniff)"));
            }
        }
        // The media element's own later range requests ("subsequent"): the first response at this
        // URL, for this client, sniffed as media here.
        let media_element = matches!(request.destination, Destination::Audio | Destination::Video);
        let media_key = request.client.as_ref().map(|c| {
            (
                c.origin.ascii_serialization(),
                request.url().as_str().to_owned(),
            )
        });
        if media_element {
            let known = media_key.as_ref().is_some_and(|k| {
                self.media_urls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(k)
            });
            if known {
                return Ok(());
            }
        }
        if status == 206 && !orb::partial_response_starts_at_zero(&fetched.head.headers) {
            fetched.body.discard().await;
            return Err(blocked("a partial response not from the start"));
        }
        // The first 1024 bytes (or the whole body when shorter).
        let mut first = Vec::new();
        let mut ended = false;
        while first.len() < 1024 {
            match fetched.body.next().await {
                Ok(Some(chunk)) => first.extend_from_slice(&chunk),
                Ok(None) => {
                    ended = true;
                    break;
                }
                Err(_) => {
                    fetched.body.discard().await;
                    return Err(blocked("its body could not be read"));
                }
            }
        }
        let rest = std::mem::replace(&mut fetched.body, BodySource::None);
        fetched.body = if ended {
            BodySource::Bytes(Bytes::from(std::mem::take(&mut first)))
        } else {
            let first = Bytes::from(first.clone());
            BodySource::Chained(first, Box::new(rest))
        };
        let bytes: &[u8] = match &fetched.body {
            BodySource::Bytes(b) => b,
            BodySource::Chained(b, _) => b,
            _ => &[],
        };
        let bytes = &bytes[..bytes.len().min(1024)];
        if orb::audio_or_video_type_pattern(bytes).is_some() {
            if status != 200 && status != 206 {
                fetched.body.discard().await;
                return Err(blocked("media with a status other than 200 or 206"));
            }
            if let Some(key) = media_key {
                let mut urls = self.media_urls.lock().unwrap_or_else(|e| e.into_inner());
                if urls.len() >= 4096 {
                    urls.clear();
                }
                urls.insert(key);
            }
            return Ok(());
        }
        if media_element {
            fetched.body.discard().await;
            return Err(blocked("not media"));
        }
        if orb::image_type_pattern(bytes).is_some() {
            return Ok(());
        }
        if nosniff {
            fetched.body.discard().await;
            return Err(blocked(
                "neither an image nor media, and the type is final (nosniff)",
            ));
        }
        if !(200..=299).contains(&status) {
            fetched.body.discard().await;
            return Err(blocked("an error status"));
        }
        let Some(m) = &mime else {
            return Ok(());
        };
        if matches!(m.type_.as_str(), "audio" | "image" | "video") {
            fetched.body.discard().await;
            return Err(blocked("labelled as an image or media, but not one"));
        }
        // "Determine if response is JavaScript and not JSON": the whole body.
        let mut all = match std::mem::replace(&mut fetched.body, BodySource::None) {
            BodySource::Bytes(b) => b.to_vec(),
            BodySource::Chained(b, rest) => {
                fetched.body = *rest;
                b.to_vec()
            }
            other => {
                fetched.body = other;
                Vec::new()
            }
        };
        loop {
            match fetched.body.next().await {
                Ok(Some(chunk)) => {
                    all.extend_from_slice(&chunk);
                    if all.len() > MAX_SCRIPT_PARSE {
                        fetched.body.discard().await;
                        return Err(blocked("too large to parse as a script"));
                    }
                }
                Ok(None) => break,
                Err(_) => return Err(blocked("its body could not be read")),
            }
        }
        if orb::is_json(&all) {
            return Err(blocked("JSON"));
        }
        let encoding = m
            .parameter("charset")
            .and_then(crate::encoding::encoding_for_label)
            .or_else(|| {
                request
                    .js_fallback_encoding
                    .as_deref()
                    .and_then(crate::encoding::encoding_for_label)
            })
            .unwrap_or(encoding_rs::UTF_8);
        let (source, _, _) = encoding.decode(&all);
        let source = source.into_owned();
        let parses = match self.script_check.get() {
            Some(check) => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                check.parses_as_script(
                    source,
                    Box::new(move |ok| {
                        let _ = tx.send(ok);
                    }),
                );
                rx.await.unwrap_or(false)
            }
            None => false,
        };
        if !parses {
            return Err(blocked("not a script"));
        }
        fetched.body = BodySource::Bytes(Bytes::from(all));
        Ok(())
    }

    /// A child navigable's response is checked for `frame-ancestors` (CSP §6.4.1) and
    /// `X-Frame-Options` (HTML "check a navigation response's adherence to X-Frame-Options") here,
    /// outside the renderer (`frame-ancestors` first, then `X-Frame-Options`, which an enforced
    /// `frame-ancestors` overrides): a refused response is a network error
    /// ([`BlockReason::Framing`]) and its body never reaches the renderer. A violation is
    /// reported to the response's policy's endpoints from here, as the embedding document's
    /// request, and sent to the requester for its console ([`FetchEvent::Violation`]).
    async fn check_framing(
        self: &Arc<Self>,
        request: &Request,
        fetched: &mut Fetched,
        ancestors: &[url::Origin],
        sink: &Sink,
    ) -> Result<(), NetError> {
        let url = fetched
            .head
            .url()
            .cloned()
            .unwrap_or_else(|| request.url().clone());
        let destination = crate::origin::origin_of(&url, None);
        let (csp, _) = crate::csp::CspList::from_headers(&fetched.head.headers, &destination);
        let has_xfo = fetched.head.headers.contains("x-frame-options");
        if csp.is_empty() && !has_xfo {
            return Ok(());
        }
        let verdict = csp.should_block_framing(ancestors);
        if !verdict.violations.is_empty() {
            let endpoints = crate::report::reporting_endpoints(&fetched.head.headers, &url);
            let ua = self.user_agent();
            for v in &verdict.violations {
                let report =
                    crate::report::ViolationReport::new(v, &url, "", fetched.head.status, None);
                for (to, reporting_api) in
                    crate::report::report_destinations(&csp, v, &endpoints, &url)
                {
                    let (body, content_type) = if reporting_api {
                        (report.reports_json(&ua.value), "application/reports+json")
                    } else {
                        (report.csp_report_json(), "application/csp-report")
                    };
                    if let Some(client) = request.client.clone() {
                        let report = crate::report::report_request(to, body, content_type, client);
                        let sink = Sink {
                            tx: None,
                            notify: None,
                            observer: None,
                            filter: sink.filter.clone(),
                            ended: Default::default(),
                        };
                        if let Some(start) = self.detached.get() {
                            start(report, sink);
                        }
                    }
                }
                sink.send(FetchEvent::Violation(v.clone())).await;
            }
        }
        let allowed = !verdict.blocked
            && crate::report::x_frame_options_allows(
                &fetched.head.headers,
                &csp,
                ancestors,
                &destination,
            );
        if allowed {
            return Ok(());
        }
        fetched.body.discard().await;
        let what = if verdict.blocked {
            "its Content Security Policy's frame-ancestors"
        } else {
            "'X-Frame-Options'"
        };
        Err(NetError::blocked(
            BlockReason::Framing,
            format!(
                "Refused to display '{}' in a frame because it set {what}.",
                crate::csp::strip_url_for_report(&url)
            ),
        ))
    }

    /// Main fetch steps 1-11: the request checks, then scheme fetch or HTTP fetch.
    async fn main_fetch_pass(
        self: &Arc<Self>,
        request: &mut Request,
        sink: &Sink,
        csp_out: &mut Option<String>,
    ) -> Result<Hop, NetError> {
        if request.local_urls_only && !is_local_scheme(request.url().scheme()) {
            return Err(NetError::blocked(
                BlockReason::LocalUrlsOnly,
                "only local URLs may be fetched",
            ));
        }
        let csp = request.client.as_ref().and_then(|c| c.csp.clone());
        if let Some(csp) = &csp {
            for v in csp.report_only_violations(&RequestInfo::from_request(request)) {
                sink.send(FetchEvent::Violation(v)).await;
            }
        }
        upgrade_insecure_request(request);
        upgrade_mixed_content(request);
        if is_bad_port_request(request.url()) {
            return Err(NetError::blocked(
                BlockReason::BadPort,
                format!("{} uses a blocked port", request.url()),
            ));
        }
        let block_all = csp.as_ref().is_some_and(|c| c.block_all_mixed_content())
            && request
                .client
                .as_ref()
                .is_some_and(|c| c.prohibits_mixed_security_contexts)
            && matches!(request.url().scheme(), "http" | "ws");
        if is_blocked_mixed_content(request) || block_all {
            return Err(NetError::blocked(
                BlockReason::MixedContent,
                format!("mixed content: {} on a secure page", request.url()),
            ));
        }
        if !file_access_allowed(request) {
            return Err(NetError::blocked(
                BlockReason::FileAccess,
                format!("a web page can't load local files ({})", request.url()),
            ));
        }
        if let Some(csp) = &csp {
            let verdict = csp.should_block_request(&RequestInfo::from_request(request));
            for v in verdict.violations {
                sink.send(FetchEvent::Violation(v)).await;
            }
            if verdict.blocked {
                return Err(NetError::blocked(
                    BlockReason::Csp,
                    format!("blocked by Content Security Policy: {}", request.url()),
                ));
            }
            // CSP §6.4.1 form-action: the pre-navigation check of a form submission, again at each
            // redirect (HTML runs "should navigation request be blocked by CSP" per hop).
            if request.form_submission {
                let verdict = csp.should_block_form_action(request.url(), request.redirect_count);
                for v in verdict.violations {
                    sink.send(FetchEvent::Violation(v)).await;
                }
                if verdict.blocked {
                    return Err(NetError::blocked(
                        BlockReason::Csp,
                        format!(
                            "form submission blocked by Content Security Policy: {}",
                            request.url()
                        ),
                    ));
                }
            }
        }
        if request.referrer_policy == ReferrerPolicy::Empty {
            if let Some(client) = &request.client {
                request.referrer_policy = client.referrer_policy;
            }
        }
        let policy = match request.referrer_policy {
            ReferrerPolicy::Empty => DEFAULT_REFERRER_POLICY,
            p => p,
        };
        if request.referrer != Referrer::NoReferrer {
            request.referrer = match determine_referrer(request, policy) {
                Some(url) => Referrer::Url(url),
                None => Referrer::NoReferrer,
            };
        }
        if let Some(upgraded) = self.hsts.upgrade(request.url()) {
            *request.url_mut() = upgraded;
        }
        if let Some(filter) = &sink.filter {
            if let Some(hop) = self.apply_filter(filter.as_ref(), request, csp_out)? {
                return Ok(hop);
            }
        }

        let current_origin = match self.held_blob(request) {
            Some(entry) => entry.origin,
            None => origin_of(request.url(), Some(&self.blobs)),
        };
        let request_origin = request.resolved_origin();
        let scheme = request.url().scheme().to_string();
        let fetched = if (current_origin == request_origin
            && request.response_tainting == ResponseTainting::Basic)
            || scheme == "data"
            || matches!(request.mode, Mode::Navigate | Mode::WebSocket)
        {
            request.response_tainting = ResponseTainting::Basic;
            self.scheme_fetch(request).await
        } else if request.mode == Mode::SameOrigin {
            Err(NetError::new(
                NetErrorKind::Cors,
                format!(
                    "{} is cross-origin and the request's mode is same-origin",
                    request.url()
                ),
            ))
        } else if request.mode == Mode::NoCors {
            if request.redirect != RedirectMode::Follow {
                Err(NetError::new(
                    NetErrorKind::Redirect,
                    "a no-cors request must follow redirects",
                ))
            } else {
                request.response_tainting = ResponseTainting::Opaque;
                self.scheme_fetch(request).await
            }
        } else if !is_http_scheme(&scheme) {
            Err(NetError::new(
                NetErrorKind::Cors,
                format!("cross-origin requests are only supported for HTTP(S), not {scheme}:"),
            ))
        } else {
            let preflight = request.use_cors_preflight
                || (request.unsafe_request
                    && (!is_cors_safelisted_method(&request.method)
                        || !cors_unsafe_request_header_names(&request.headers).is_empty()));
            request.response_tainting = ResponseTainting::Cors;
            self.http_fetch(request, preflight).await
        };
        fetched
    }

    /// The content filter's verdict on a request about to go out (see [`crate::filter`]): `Err` to
    /// block it, a response to answer it with, or nothing to go on (with its URL maybe rewritten).
    fn apply_filter(
        &self,
        filter: &dyn RequestFilter,
        request: &mut Request,
        csp_out: &mut Option<String>,
    ) -> Result<Option<Hop>, NetError> {
        if !matches!(request.url().scheme(), "http" | "https" | "ws" | "wss") {
            return Ok(None);
        }
        let verdict = filter.check(request);
        *csp_out = verdict.csp;
        match verdict.action {
            FilterAction::Allow => Ok(None),
            FilterAction::Block(rule) => {
                self.trace(|| format!("[net] filtered  {} ({rule})", request.url()));
                Err(NetError::blocked(
                    BlockReason::ContentFilter,
                    format!("{} was blocked by a content filter: {rule}", request.url()),
                ))
            }
            FilterAction::Resource { content_type, body } => {
                self.trace(|| format!("[net] neutered  {} ({content_type})", request.url()));
                let mut headers = HeaderList::new();
                headers.append("Content-Type", &content_type);
                let mut fetched = simple_response(200, "OK", headers, body);
                // Not from the URL's server: the page sees an opaque response (a navigation, a
                // document it can't read either way).
                fetched.head.response_type = if request.mode == Mode::Navigate {
                    ResponseType::Basic
                } else {
                    ResponseType::Opaque
                };
                fetched.head.url_list = request.url_list.clone();
                Ok(Some(Hop::Response(fetched)))
            }
            FilterAction::Rewrite(url) => {
                self.trace(|| format!("[net] rewritten {} -> {url}", request.url()));
                *request.url_mut() = url;
                Ok(None)
            }
        }
    }

    /// Fetch §4.3 "scheme fetch".
    async fn scheme_fetch(self: &Arc<Self>, request: &mut Request) -> Result<Hop, NetError> {
        let url = request.url().clone();
        match url.scheme() {
            "about" => {
                if url.path() == "blank" {
                    let mut headers = HeaderList::new();
                    headers.append("Content-Type", "text/html;charset=utf-8");
                    Ok(Hop::Response(simple_response(
                        200,
                        "OK",
                        headers,
                        Bytes::new(),
                    )))
                } else {
                    Err(NetError::new(
                        NetErrorKind::NotFound,
                        format!("{url} doesn't exist"),
                    ))
                }
            }
            "blob" => self.blob_fetch(request).map(Hop::Response),
            "data" => {
                let data = process_data_url(&url)
                    .ok_or_else(|| NetError::new(NetErrorKind::InvalidUrl, "invalid data: URL"))?;
                let mut headers = HeaderList::new();
                headers.append("Content-Type", &data.mime_type.serialize());
                Ok(Hop::Response(simple_response(
                    200,
                    "OK",
                    headers,
                    Bytes::from(data.body),
                )))
            }
            "file" => {
                let path = url.to_file_path().map_err(|_| {
                    NetError::new(
                        NetErrorKind::InvalidUrl,
                        format!("{url} is not a local path"),
                    )
                })?;
                let file = tokio::fs::File::open(&path).await.map_err(|e| {
                    NetError::new(
                        NetErrorKind::NotFound,
                        format!("can't open {}: {e}", path.display()),
                    )
                })?;
                let meta = file
                    .metadata()
                    .await
                    .map_err(|e| NetError::new(NetErrorKind::Io, e.to_string()))?;
                if meta.is_dir() {
                    return Err(NetError::new(
                        NetErrorKind::NotFound,
                        format!("{} is a folder", path.display()),
                    ));
                }
                let mut headers = HeaderList::new();
                headers.append("Content-Type", file_mime_type(&path));
                headers.append("Content-Length", &meta.len().to_string());
                let mut head = ResponseHead::new(200, "OK");
                head.headers = headers;
                Ok(Hop::Response(Fetched {
                    head,
                    body: BodySource::File(file),
                }))
            }
            "http" | "https" => self.http_fetch(request, false).await,
            other => Err(NetError::new(
                NetErrorKind::UnsupportedScheme,
                format!("can't fetch {other}: URLs"),
            )),
        }
    }

    /// The request's blob URL entry ([`Request::blob_url_entry`]), when it carries a hold.
    fn held_blob(&self, request: &Request) -> Option<crate::blob::BlobEntry> {
        request
            .blob_url_entry
            .and_then(|token| self.blobs.held(token, request.url()))
    }

    /// The blob case of scheme fetch, with single-range support: the URL's blob URL entry when the
    /// request carries one, else what the URL names now.
    fn blob_fetch(&self, request: &Request) -> Result<Fetched, NetError> {
        let url = request.url();
        let entry = self.held_blob(request).or_else(|| self.blobs.resolve(url));
        let entry = match entry {
            Some(e) if request.method == "GET" => e,
            _ => {
                return Err(NetError::new(
                    NetErrorKind::NotFound,
                    format!("{url} is not a registered blob"),
                ))
            }
        };
        let bytes = entry.blob.bytes.clone();
        let mut headers = HeaderList::new();
        let Some(range) = request.headers.get("range") else {
            // The blob's type even when empty (Fetch's scheme fetch, "blob"): an empty value
            // extracts no MIME type, as an absent header does.
            headers.append("Content-Length", &bytes.len().to_string());
            headers.append("Content-Type", &entry.blob.mime_type);
            return Ok(simple_response(200, "OK", headers, bytes));
        };
        let len = bytes.len() as u64;
        let (start, end) = match parse_single_range(&range, true) {
            Some((Some(s), e)) => (
                s,
                e.unwrap_or(len.saturating_sub(1))
                    .min(len.saturating_sub(1)),
            ),
            Some((None, Some(suffix))) => (len.saturating_sub(suffix), len.saturating_sub(1)),
            _ => {
                return Err(NetError::new(
                    NetErrorKind::Protocol,
                    "invalid Range header for a blob",
                ))
            }
        };
        if start >= len || start > end {
            return Err(NetError::new(
                NetErrorKind::Protocol,
                "unsatisfiable Range for a blob",
            ));
        }
        let slice = bytes.slice(start as usize..=end as usize);
        headers.append("Content-Length", &slice.len().to_string());
        headers.append("Content-Type", &entry.blob.mime_type);
        headers.append("Content-Range", &format!("bytes {start}-{end}/{len}"));
        Ok(simple_response(206, "Partial Content", headers, slice))
    }

    /// Fetch §4.4 "HTTP fetch".
    async fn http_fetch(
        self: &Arc<Self>,
        request: &mut Request,
        make_preflight: bool,
    ) -> Result<Hop, NetError> {
        if make_preflight && self.preflight.needs_preflight(request) {
            self.cors_preflight_fetch(request).await?;
        }
        let mut fetched = self.http_network_or_cache_fetch(request).await?;
        if request.response_tainting == ResponseTainting::Cors {
            if let Err(message) = cors_check(request, &fetched.head.headers) {
                fetched.body.discard().await;
                // "clear cache entries": a preflight result cached for this origin and URL is not
                // trusted once the request it allowed fails its CORS check.
                self.preflight.clear_for(request);
                return Err(NetError::new(
                    NetErrorKind::Cors,
                    format!("{}: {message}", request.url()),
                ));
            }
        }
        if request.response_tainting == ResponseTainting::Opaque
            && corp_blocks(
                request,
                fetched
                    .head
                    .headers
                    .get("cross-origin-resource-policy")
                    .as_deref(),
            )
        {
            fetched.body.discard().await;
            return Err(NetError::blocked(
                BlockReason::CrossOriginResourcePolicy,
                format!(
                    "{} is blocked by its Cross-Origin-Resource-Policy",
                    request.url()
                ),
            ));
        }
        if is_redirect_status(fetched.head.status) {
            match request.redirect {
                RedirectMode::Error => {
                    fetched.body.discard().await;
                    return Err(NetError::new(
                        NetErrorKind::Redirect,
                        format!(
                            "{} redirected and the request's redirect mode is error",
                            request.url()
                        ),
                    ));
                }
                RedirectMode::Manual => {
                    if request.mode != Mode::Navigate {
                        fetched.body.discard().await;
                        fetched.head.response_type = ResponseType::OpaqueRedirect;
                        fetched.head.url_list = request.url_list.clone();
                    }
                    return Ok(Hop::Response(fetched));
                }
                RedirectMode::Follow => return self.http_redirect_fetch(request, fetched).await,
            }
        }
        Ok(Hop::Response(fetched))
    }

    /// Fetch §4.5 "HTTP-redirect fetch".
    async fn http_redirect_fetch(
        self: &Arc<Self>,
        request: &mut Request,
        mut fetched: Fetched,
    ) -> Result<Hop, NetError> {
        let Some(location) = fetched.head.headers.get("location") else {
            return Ok(Hop::Response(fetched));
        };
        let current = request.url().clone();
        let mut location_url = current.join(&location).map_err(|e| {
            NetError::new(
                NetErrorKind::Redirect,
                format!("invalid Location '{location}' from {current}: {e}"),
            )
        })?;
        if location_url.fragment().is_none() {
            if let Some(f) = current.fragment() {
                location_url.set_fragment(Some(f));
            }
        }
        fetched.body.discard().await;
        if !is_http_scheme(location_url.scheme()) {
            return Err(NetError::new(
                NetErrorKind::Redirect,
                format!("{current} redirected to a non-HTTP(S) URL ({location_url})"),
            ));
        }
        if request.redirect_count >= 20 {
            return Err(NetError::new(
                NetErrorKind::TooManyRedirects,
                format!("too many redirects ({current})"),
            ));
        }
        request.redirect_count += 1;
        let includes_credentials =
            !location_url.username().is_empty() || location_url.password().is_some();
        let request_origin = request.resolved_origin();
        let location_origin = location_url.origin();
        if request.mode == Mode::Cors && includes_credentials && request_origin != location_origin {
            return Err(NetError::new(
                NetErrorKind::Cors,
                "a CORS redirect may not carry credentials",
            ));
        }
        if request.response_tainting == ResponseTainting::Cors && includes_credentials {
            return Err(NetError::new(
                NetErrorKind::Cors,
                "a CORS redirect may not carry credentials",
            ));
        }
        let status = fetched.head.status;
        if ((status == 301 || status == 302) && request.method == "POST")
            || (status == 303 && request.method != "GET" && request.method != "HEAD")
        {
            request.method = "GET".into();
            request.body = None;
            let names: Vec<String> = request
                .headers
                .names()
                .into_iter()
                .filter(|n| is_request_body_header_name(n))
                .collect();
            for n in names {
                request.headers.delete(&n);
            }
        }
        let current_origin = current.origin();
        if current_origin != location_origin {
            request.headers.delete("authorization");
        }
        if location_origin != current_origin && request_origin != current_origin {
            request.tainted_origin = true;
        }
        self.trace(|| format!("[net] {status} redirect {current} -> {location_url}"));
        if request.mode == Mode::Navigate {
            let mut headers = HeaderList::new();
            for (name, value) in fetched.head.headers.iter() {
                if name.eq_ignore_ascii_case("cross-origin-opener-policy")
                    || name.eq_ignore_ascii_case("cross-origin-opener-policy-report-only")
                {
                    headers.append(name, value);
                }
            }
            request
                .navigation_redirects
                .push(crate::response::RedirectHop {
                    url: current.clone(),
                    headers,
                });
        }
        request.url_list.push(location_url);
        let policy = parse_referrer_policy_header(&fetched.head.headers);
        if policy != ReferrerPolicy::Empty {
            request.referrer_policy = policy;
        }
        Ok(Hop::Redirect)
    }

    /// RFC 6265bis §5.2 "same-site" for this request, and the rest of the cookie context.
    fn cookie_context(&self, request: &Request) -> CookieContext {
        let same = match &request.client {
            None => true,
            Some(client) => {
                if client.has_cross_site_ancestor {
                    false
                } else {
                    let site_for_cookies = if request.top_level_navigation {
                        &client.origin
                    } else {
                        client.top_level_origin()
                    };
                    request
                        .url_list
                        .iter()
                        .all(|u| same_site(site_for_cookies, &origin_of(u, None)))
                }
            }
        };
        CookieContext {
            url: request.url().clone(),
            http_api: true,
            same_site: same,
            top_level_navigation: request.top_level_navigation,
            safe_method: matches!(
                request.method.as_str(),
                "GET" | "HEAD" | "OPTIONS" | "TRACE"
            ),
        }
    }

    fn cookies_allowed(&self, request: &Request, ctx: &CookieContext) -> bool {
        !self.config.block_third_party_cookies || ctx.same_site || request.top_level_navigation
    }

    /// Fetch §4.6 "HTTP-network-or-cache fetch" (the HTTP cache is not implemented: every request goes to
    /// the network) and §4.7 "HTTP-network fetch".
    async fn http_network_or_cache_fetch(
        self: &Arc<Self>,
        request: &mut Request,
    ) -> Result<Fetched, NetError> {
        let include_credentials = match request.credentials {
            CredentialsMode::Include => true,
            CredentialsMode::SameOrigin => request.response_tainting == ResponseTainting::Basic,
            CredentialsMode::Omit => false,
        };
        let mut headers = request.headers.clone();
        let content_length = match &request.body {
            Some(b) => Some(b.len()),
            None if request.method == "POST" || request.method == "PUT" => Some(0),
            None => None,
        };
        if let Some(n) = content_length {
            headers.set("Content-Length", &n.to_string());
        }
        if let Referrer::Url(r) = &request.referrer {
            headers.set("Referer", r.as_str());
        }
        if let Some(origin) = origin_header(request) {
            headers.set("Origin", &origin);
        }
        for (name, value) in fetch_metadata_headers(request) {
            headers.set(name, &value);
        }
        if !headers.contains("user-agent") {
            let ua = self.user_agent();
            let ua = ua.for_url(request.url());
            headers.append("User-Agent", &ua.header_value());
            // A Chromium user agent sends its client hints to every potentially trustworthy origin,
            // except with a service worker's script fetch (`Service-Worker: script`, which the
            // registration makes outside any page).
            if is_url_potentially_trustworthy(request.url()) && !headers.contains("service-worker")
            {
                for (name, value) in ua.profile().client_hint_headers() {
                    if !headers.contains(name) {
                        headers.append(name, &value);
                    }
                }
            }
        }
        let mut cache = request.cache;
        if cache == CacheMode::Default
            && [
                "if-modified-since",
                "if-none-match",
                "if-unmodified-since",
                "if-match",
                "if-range",
            ]
            .iter()
            .any(|h| headers.contains(h))
        {
            cache = CacheMode::NoStore;
        }
        if cache == CacheMode::NoCache && !headers.contains("cache-control") {
            headers.append("Cache-Control", "max-age=0");
        }
        if matches!(cache, CacheMode::NoStore | CacheMode::Reload) {
            if !headers.contains("pragma") {
                headers.append("Pragma", "no-cache");
            }
            if !headers.contains("cache-control") {
                headers.append("Cache-Control", "no-cache");
            }
        }
        if headers.contains("range") {
            headers.set("Accept-Encoding", "identity");
        } else if !headers.contains("accept-encoding") {
            headers.append("Accept-Encoding", ACCEPT_ENCODING);
        }
        let ctx = self.cookie_context(request);
        let cookies_ok = include_credentials && self.cookies_allowed(request, &ctx);
        if cookies_ok {
            let cookie = self.cookies.cookie_string(&ctx);
            if !cookie.is_empty() {
                headers.set("Cookie", &cookie);
            }
        }
        let mut url = request.url().clone();
        if !url.username().is_empty() || url.password().is_some() {
            if include_credentials
                && request.use_url_credentials
                && !headers.contains("authorization")
            {
                let user = crate::data_url::percent_decode(url.username().as_bytes());
                let pass = crate::data_url::percent_decode(url.password().unwrap_or("").as_bytes());
                let mut plain = user;
                plain.push(b':');
                plain.extend(pass);
                let encoded = base64::engine::general_purpose::STANDARD.encode(plain);
                headers.set("Authorization", &format!("Basic {encoded}"));
            }
            let _ = url.set_username("");
            let _ = url.set_password(None);
        }
        url.set_fragment(None);

        // The HTTP cache (RFC 9111): a fresh stored response is the answer; a stale one with
        // validators makes this a conditional request.
        let cache_key = crate::http_cache::Key {
            partition: network_partition(request),
            url: url.to_string(),
        };
        let range = headers.get("range");
        let use_cache = self.config.http_cache && request.method == "GET" && range.is_none();
        // A range of a fresh stored complete response is answered from it (RFC 9111 §3.4: a
        // cache may serve a range of a complete response it holds).
        if let Some(range) = &range {
            if self.config.http_cache
                && request.method == "GET"
                && !matches!(cache, CacheMode::NoStore | CacheMode::Reload)
            {
                let any_age = matches!(cache, CacheMode::ForceCache | CacheMode::OnlyIfCached);
                if let crate::http_cache::Lookup::Fresh(entry) =
                    self.http_cache.lookup(&cache_key, &headers, any_age)
                {
                    if let Some(f) =
                        ranged_cached_response(request, entry, range, include_credentials)
                    {
                        self.trace(|| format!("[net] cache range  GET {url}"));
                        return Ok(f);
                    }
                }
            }
        }
        let mut stored = None;
        if use_cache && !matches!(cache, CacheMode::NoStore | CacheMode::Reload) {
            use crate::http_cache::Lookup;
            let any_age = matches!(cache, CacheMode::ForceCache | CacheMode::OnlyIfCached);
            match self.http_cache.lookup(&cache_key, &headers, any_age) {
                Lookup::Fresh(entry) => {
                    self.trace(|| format!("[net] cache        GET {url}"));
                    return Ok(cached_response(request, entry, include_credentials));
                }
                Lookup::Stale(entry, validators) => {
                    for (name, value) in validators {
                        headers.set(name, &value);
                    }
                    stored = Some(entry);
                }
                Lookup::Miss => {}
            }
        }
        if cache == CacheMode::OnlyIfCached && stored.is_none() {
            return Err(NetError::new(
                NetErrorKind::NotFound,
                format!("{url} is not in the HTTP cache (only-if-cached)"),
            ));
        }
        let request_headers = headers.clone();
        let request_time = crate::http_cache::now_ms();

        let started = Instant::now();
        let wire = WireRequest {
            method: request.method.clone(),
            url: url.clone(),
            headers,
            body: request.body.as_ref().map(|b| b.bytes.clone()),
            upgrade: request.upgrade.clone(),
        };
        let response = match self.transport.send(&wire).await {
            Ok(r) => r,
            Err(e) => {
                self.trace(|| {
                    format!(
                        "[net] ERR {:>5} ms {} {url}: {}",
                        started.elapsed().as_millis(),
                        request.method,
                        e.message
                    )
                });
                return Err(e);
            }
        };
        self.trace(|| {
            format!(
                "[net] {} {:>5} ms {} {}{} {url}",
                response.status,
                started.elapsed().as_millis(),
                request.method,
                response.connection.protocol,
                if response.connection.reused {
                    " reused"
                } else {
                    ""
                },
            )
        });
        if cookies_ok {
            for value in response.headers.get_all("set-cookie") {
                self.cookies.set_cookie(value, &ctx);
            }
        }
        self.hsts.observe(&url, &response.headers);
        let response_time = crate::http_cache::now_ms();
        // RFC 9111 §4.4: an unsafe method's success invalidates what is stored for the URL.
        if !matches!(
            request.method.as_str(),
            "GET" | "HEAD" | "OPTIONS" | "TRACE"
        ) && (200..400).contains(&response.status)
        {
            self.http_cache.invalidate(url.as_str());
        }
        // §4.3.3-4: a 304 validates the stored response, which it refreshes.
        if let Some(entry) = stored {
            if response.status == 304 {
                let mut rest = BodySource::Network(Box::new(NetworkBody {
                    body: response.body,
                    decoder: None,
                    release: response.release,
                    read_timeout: self.config.read_timeout,
                    done: false,
                }));
                rest.discard().await;
                let entry = self.http_cache.refresh(
                    &cache_key,
                    entry,
                    &response.headers,
                    request_time,
                    response_time,
                );
                return Ok(cached_response(request, entry, include_credentials));
            }
        }
        let store = use_cache
            && cache != CacheMode::NoStore
            && crate::http_cache::storable(
                &request.method,
                &request_headers,
                response.status,
                &response.headers,
            );

        let mut head = ResponseHead::new(response.status, &response.reason);
        head.url_list = request.url_list.clone();
        head.request_includes_credentials = include_credentials;
        head.connection = response.connection;
        head.timing = response.timing;
        let codings = if request.method != "HEAD" && !is_null_body_status(response.status) {
            response
                .headers
                .get_decode_split("content-encoding")
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let decoder = ContentDecoder::for_codings(&codings).map_err(decoding_error)?;
        let network = NetworkBody {
            body: response.body,
            decoder,
            release: response.release,
            read_timeout: self.config.read_timeout,
            done: false,
        };
        let body = if store {
            BodySource::Caching(Box::new(CachingBody {
                inner: network,
                copy: Some(Vec::new()),
                cache: self.http_cache.clone(),
                key: cache_key,
                request_headers,
                status: response.status,
                reason: response.reason.clone(),
                headers: response.headers.clone(),
                request_time,
                response_time,
            }))
        } else {
            BodySource::Network(Box::new(network))
        };
        head.headers = response.headers;
        Ok(Fetched { head, body })
    }

    /// Fetch §4.8 "CORS-preflight fetch".
    async fn cors_preflight_fetch(self: &Arc<Self>, request: &Request) -> Result<(), NetError> {
        let mut preflight = Request::new(request.url().clone());
        preflight.url_list = request.url_list.clone();
        preflight.method = "OPTIONS".into();
        preflight.initiator = request.initiator;
        preflight.destination = request.destination;
        preflight.origin = request.origin.clone();
        preflight.referrer = request.referrer.clone();
        preflight.referrer_policy = request.referrer_policy;
        preflight.mode = Mode::Cors;
        preflight.response_tainting = ResponseTainting::Cors;
        preflight.client = request.client.clone();
        preflight.tainted_origin = request.tainted_origin;
        preflight.headers.append("Accept", "*/*");
        preflight
            .headers
            .append("Access-Control-Request-Method", &request.method);
        let unsafe_names = cors_unsafe_request_header_names(&request.headers);
        if !unsafe_names.is_empty() {
            preflight
                .headers
                .append("Access-Control-Request-Headers", &unsafe_names.join(","));
        }
        let mut fetched = self.http_network_or_cache_fetch(&mut preflight).await?;
        fetched.body.discard().await;
        let fail = |message: String| {
            self.preflight.clear_for(request);
            Err(NetError::new(
                NetErrorKind::Cors,
                format!("preflight for {} failed: {message}", request.url()),
            ))
        };
        if let Err(message) = cors_check(request, &fetched.head.headers) {
            return fail(message);
        }
        if !fetched.head.ok() {
            return fail(format!("status {}", fetched.head.status));
        }
        match evaluate_preflight(request, &fetched.head.headers) {
            Ok(result) => {
                self.preflight.store(request, &result);
                Ok(())
            }
            Err(message) => fail(message),
        }
    }
}

/// The HTTP cache's network partition for a request: the top-level origin of its client (none for a
/// request without one), so that one site cannot learn what another site's pages loaded.
fn network_partition(request: &Request) -> String {
    match &request.client {
        Some(c) => {
            crate::origin::serialize_origin(c.top_level_origin.as_ref().unwrap_or(&c.origin))
        }
        None => String::new(),
    }
}

/// A response from the HTTP cache.
fn cached_response(
    request: &Request,
    entry: crate::http_cache::Entry,
    include_credentials: bool,
) -> Fetched {
    let mut head = ResponseHead::new(entry.status, &entry.reason);
    head.url_list = request.url_list.clone();
    head.request_includes_credentials = include_credentials;
    head.headers = entry.headers;
    Fetched {
        head,
        body: BodySource::Bytes(entry.body),
    }
}

/// A 206 of one satisfiable byte range of a stored complete response (a 200 without a content
/// coding, whose stored bytes are the representation's); `None` otherwise, and the request goes to
/// the network.
fn ranged_cached_response(
    request: &Request,
    entry: crate::http_cache::Entry,
    range: &str,
    include_credentials: bool,
) -> Option<Fetched> {
    if entry.status != 200 || entry.headers.contains("content-encoding") {
        return None;
    }
    let len = entry.body.len() as u64;
    let (start, end) = match parse_single_range(range, false)? {
        (Some(s), e) => (s, e.unwrap_or(u64::MAX).min(len.checked_sub(1)?)),
        (None, Some(suffix)) if suffix > 0 => (len.saturating_sub(suffix), len.checked_sub(1)?),
        _ => return None,
    };
    if start > end || start >= len {
        return None;
    }
    let mut head = ResponseHead::new(206, "Partial Content");
    head.url_list = request.url_list.clone();
    head.request_includes_credentials = include_credentials;
    head.headers = entry.headers;
    head.headers
        .set("Content-Range", &format!("bytes {start}-{end}/{len}"));
    head.headers
        .set("Content-Length", &(end - start + 1).to_string());
    Some(Fetched {
        head,
        body: BodySource::Bytes(entry.body.slice(start as usize..=end as usize)),
    })
}

/// Whether `url` is a URL the network layer can fetch at all (for callers that want to check up front).
pub fn is_fetchable(url: &Url) -> bool {
    crate::origin::is_fetch_scheme(url.scheme())
}
