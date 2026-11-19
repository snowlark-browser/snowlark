// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fetching ahead of the parser: speculative HTML parsing (HTML §13.2.8) while the parser waits for
//! a script, and the link types that fetch or connect early (HTML §4.6.7: `preload`,
//! `modulepreload`, `preconnect`, `dns-prefetch`, also from `Link:` response headers).
//!
//! Every early fetch goes through snowlark-net's list of preloaded resources
//! ([`snowlark_net::NetClient::preload`]), keyed per the HTML spec by URL, destination, mode and
//! credentials mode within the document. The requests are built exactly as the elements that use
//! them later build theirs (the loader's scripts and stylesheets, the renderer's images), so the
//! element's own fetch takes the preloaded response, in flight or done, instead of fetching again.

use std::collections::HashSet;
use std::sync::Arc;

use snowlark_dom::speculative::{ScannedTag, TagScanner};
use snowlark_dom::{local_name, ns, NodeId};
use snowlark_net::{CredentialsMode, Destination, Mode, Request};

use crate::engine::Engine;
use crate::loader::{FetchJob, Purpose};

/// A speculative HTML parser over the input after the insertion point of a blocked parser.
pub(crate) struct Speculation {
    scanner: TagScanner,
    /// Bytes of the parser's pending input fed to the scanner.
    scanned: usize,
    /// The base URL for the URLs it finds (a `base` element it meets changes it, once).
    base: url::Url,
    base_frozen: bool,
    /// Inside a `picture`: the source chosen so far, or that one could not be decided.
    picture: Option<Picture>,
    template_depth: u32,
    /// A `meta` CSP was met: nothing past it is fetched until the parser itself has applied it.
    stopped: bool,
    /// The referrer policy of the last `meta name=referrer` met.
    referrer_policy: Option<snowlark_net::ReferrerPolicy>,
}

#[derive(Default)]
struct Picture {
    chosen: Option<String>,
    undecidable: bool,
}

/// One early fetch: what it is for, and its place in the order fetches start.
struct Early {
    url: url::Url,
    kind: EarlyKind,
    class: u8,
    /// The element's cryptographic nonce, for the document's CSP.
    nonce: String,
    /// A script element's `referrerpolicy` (as its own fetch has it).
    referrer_policy: snowlark_net::ReferrerPolicy,
}

/// What an early fetch is, which decides its request (as the consumer will make it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EarlyKind {
    /// HTML "fetch a classic script" from a `script` element (the loader's request).
    ClassicScript,
    /// "fetch a single module script" (the loader's request: cors, same-origin credentials).
    ModuleScript,
    Stylesheet,
    /// The renderer's request for an `img`.
    Image,
    /// `<link rel=preload>`: HTML "create a potential-CORS request" for a destination.
    Preload(Destination, Option<CredentialsMode>),
}

/// The counters of early fetches.
#[derive(Clone, Copy, Debug, Default)]
pub struct EarlyStats {
    /// Fetches the speculative parser started.
    pub speculative: u32,
    /// Fetches `<link rel=preload>`, `modulepreload` and `Link:` headers started.
    pub links: u32,
    /// Connections and lookups started ahead (`preconnect`, `dns-prefetch`).
    pub connects: u32,
}

/// Speculative fetching can be turned off (`SNOWLARK_PRELOAD_SCANNER=0`) for measurements.
fn scanner_enabled() -> bool {
    std::env::var("SNOWLARK_PRELOAD_SCANNER").map_or(true, |v| v != "0")
}

/// The link types that fetch or connect ahead can be turned off (`SNOWLARK_LINK_PRELOAD=0`) for
/// measurements.
fn links_enabled() -> bool {
    std::env::var("SNOWLARK_LINK_PRELOAD").map_or(true, |v| v != "0")
}

impl Engine {
    // ----- speculative HTML parsing ----------------------------------------------------------------

    /// The parser of `doc` waits for a script: start the speculative HTML parser at the insertion
    /// point, or feed it the input that arrived since (HTML §13.2.8 "start the speculative HTML
    /// parser"; the bytes pushed into the parser's input are pushed into the speculative parser's).
    pub(crate) fn speculate(&self, doc: NodeId) {
        if !scanner_enabled() || self.hooks.borrow().net.is_none() {
            return;
        }
        let mut spec = {
            let mut l = self.loader.borrow_mut();
            let Some(d) = l.docs.get_mut(&doc) else {
                return;
            };
            if d.parser.as_ref().and_then(|p| p.parser.as_ref()).is_none() {
                return;
            }
            d.speculation.take()
        }
        .unwrap_or_else(|| {
            let base = {
                let dom = self.dom();
                self.document_base_url(&dom, doc)
            };
            let base_frozen = {
                let dom = self.dom();
                dom.descendants(doc).any(|n| {
                    dom.is_html_element_named(n, &local_name!("base"))
                        && dom.attr(n, &ns!(), &local_name!("href")).is_some()
                })
            };
            Speculation {
                scanner: TagScanner::new(),
                scanned: 0,
                base,
                base_frozen,
                picture: None,
                template_depth: 0,
                stopped: false,
                referrer_policy: None,
            }
        });
        let text = {
            let l = self.loader.borrow();
            l.docs
                .get(&doc)
                .and_then(|d| d.parser.as_ref())
                .and_then(|p| p.parser.as_ref())
                .map(|p| p.pending_input(spec.scanned))
                .unwrap_or_default()
        };
        spec.scanned += text.len();
        let mut early = Vec::new();
        if !spec.stopped && !text.is_empty() {
            let tags = spec.scanner.push(&text);
            let (width, dpr) = self.viewport_for_images();
            for tag in &tags {
                if spec.stopped {
                    break;
                }
                self.speculative_tag(&mut spec, tag, width, dpr, &mut early);
            }
        }
        self.doc_load(doc, |d| d.speculation = Some(spec));
        // Stylesheets and parser-blocking scripts first, then other scripts, fonts, images, each in
        // document order (`fetchpriority` moves one class up or down).
        early.sort_by_key(|e| e.class);
        for e in early {
            if self
                .start_early(doc, e.url, e.kind, &e.nonce, e.referrer_policy, None)
                .is_some()
            {
                self.loader.borrow_mut().early.speculative += 1;
            }
        }
    }

    /// The parser resumed (its script ran): HTML "stop the speculative HTML parser".
    pub(crate) fn stop_speculation(&self, doc: NodeId) {
        self.doc_load(doc, |d| d.speculation = None);
    }

    /// The viewport width and device pixel ratio images are selected for.
    fn viewport_for_images(&self) -> (f32, f32) {
        self.with_renderer(|r, _| {
            let c = r.device_config();
            (c.viewport.0, c.device_pixel_ratio)
        })
        .unwrap_or((1280.0, 1.0))
    }

    /// What the speculative parser does with one tag: the effects of `base` and CSP `meta` on later
    /// fetches, and for the other elements, the fetch they would make (HTML §13.2.8, "speculative
    /// mock element").
    fn speculative_tag(
        &self,
        spec: &mut Speculation,
        tag: &ScannedTag,
        width: f32,
        dpr: f32,
        out: &mut Vec<Early>,
    ) {
        let pushed = out.len();
        self.speculative_tag_fetches(spec, tag, width, dpr, out);
        if let Some(nonce) = tag.attr("nonce") {
            for e in &mut out[pushed..] {
                e.nonce = nonce.to_owned();
            }
        }
        // (a `meta name=referrer` met before is the document's policy by the time the parser
        // reaches this element: its fetch takes it, unless it has a policy of its own)
        if let Some(policy) = spec.referrer_policy {
            for e in &mut out[pushed..] {
                if e.referrer_policy == snowlark_net::ReferrerPolicy::Empty {
                    e.referrer_policy = policy;
                }
            }
        }
    }

    fn speculative_tag_fetches(
        &self,
        spec: &mut Speculation,
        tag: &ScannedTag,
        width: f32,
        dpr: f32,
        out: &mut Vec<Early>,
    ) {
        let name = &tag.name;
        if *name == local_name!("template") {
            // Template contents are inert: their URLs are not fetched, except a declarative shadow
            // root's.
            if tag.end {
                spec.template_depth = spec.template_depth.saturating_sub(1);
            } else if tag.attr("shadowrootmode").is_none() || spec.template_depth > 0 {
                spec.template_depth += 1;
            }
            return;
        }
        if spec.template_depth > 0 {
            return;
        }
        if *name == local_name!("picture") {
            spec.picture = (!tag.end).then(Picture::default);
            return;
        }
        if tag.end {
            return;
        }
        let resolve = |v: &str| spec.base.join(v.trim()).ok();
        // `fetchpriority` moves a fetch half a class up or down.
        let fp = |class: u8| match tag.attr("fetchpriority").map(str::to_ascii_lowercase) {
            Some(p) if p == "high" => class * 10 + 5,
            Some(p) if p == "low" => class * 10 + 15,
            _ => class * 10 + 10,
        };
        match *name {
            local_name!("base") => {
                if let (false, Some(href)) = (spec.base_frozen, tag.attr("href")) {
                    if let Ok(u) = spec.base.join(href.trim()) {
                        spec.base = u;
                    }
                    spec.base_frozen = true;
                }
            }
            local_name!("meta") => {
                let csp = tag
                    .attr("http-equiv")
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("content-security-policy"));
                if csp {
                    spec.stopped = true;
                }
                // (a referrer policy applies to the fetches after it)
                if tag
                    .attr("name")
                    .is_some_and(|v| v.eq_ignore_ascii_case("referrer"))
                {
                    if let Some(p) = tag
                        .attr("content")
                        .and_then(crate::csp::meta_referrer_policy)
                    {
                        spec.referrer_policy = Some(p);
                    }
                }
            }
            local_name!("script") => {
                let Some(src) = tag.attr("src").filter(|s| !s.trim().is_empty()) else {
                    return;
                };
                // (as "prepare the script element" decides: "module" unstripped, a JavaScript MIME type
                // either way)
                let kind = match tag.attr("type") {
                    None | Some("") => EarlyKind::ClassicScript,
                    Some(t) if t.eq_ignore_ascii_case("module") => EarlyKind::ModuleScript,
                    Some(t)
                        if snowlark_net::mime::is_javascript_essence(
                            &t.trim_matches(|c: char| c.is_ascii_whitespace())
                                .to_ascii_lowercase(),
                        ) =>
                    {
                        EarlyKind::ClassicScript
                    }
                    _ => return,
                };
                if kind == EarlyKind::ClassicScript && tag.attr("nomodule").is_some() {
                    return;
                }
                let blocking = kind == EarlyKind::ClassicScript
                    && tag.attr("async").is_none()
                    && tag.attr("defer").is_none();
                let referrer_policy = referrer_policy_attr(tag.attr("referrerpolicy"));
                if let Some(url) = resolve(src) {
                    out.push(Early {
                        nonce: String::new(),
                        referrer_policy,
                        url,
                        kind,
                        class: fp(if blocking { 0 } else { 1 }),
                    });
                }
            }
            local_name!("link") => {
                let rel = tag.attr("rel").unwrap_or("").to_ascii_lowercase();
                let rels: Vec<&str> = rel.split_ascii_whitespace().collect();
                let Some(url) = tag
                    .attr("href")
                    .filter(|h| !h.trim().is_empty())
                    .and_then(resolve)
                else {
                    return;
                };
                if rels.contains(&"stylesheet") && !rels.contains(&"alternate") {
                    out.push(Early {
                        nonce: String::new(),
                        referrer_policy: referrer_policy_attr(tag.attr("referrerpolicy")),
                        url,
                        kind: EarlyKind::Stylesheet,
                        class: fp(0),
                    });
                } else if rels.contains(&"modulepreload") {
                    out.push(Early {
                        nonce: String::new(),
                        referrer_policy: referrer_policy_attr(tag.attr("referrerpolicy")),
                        url,
                        kind: EarlyKind::ModuleScript,
                        class: fp(1),
                    });
                } else if rels.contains(&"preload") {
                    let Some(dest) = preload_destination(tag.attr("as").unwrap_or("")) else {
                        return;
                    };
                    if tag
                        .attr("type")
                        .is_some_and(|t| !preload_type_supported(dest, t))
                    {
                        return;
                    }
                    if !media_matches(tag.attr("media").unwrap_or(""), width) {
                        return;
                    }
                    let class = match dest {
                        Destination::Style | Destination::Script => 1,
                        Destination::Font => 2,
                        _ => 3,
                    };
                    out.push(Early {
                        nonce: String::new(),
                        referrer_policy: referrer_policy_attr(tag.attr("referrerpolicy")),
                        url,
                        kind: EarlyKind::Preload(dest, cors_credentials(tag.attr("crossorigin"))),
                        class: fp(class),
                    });
                }
            }
            local_name!("source") => {
                let Some(p) = spec.picture.as_mut() else {
                    return;
                };
                if p.chosen.is_some() || p.undecidable {
                    return;
                }
                let Some(srcset) = tag.attr("srcset") else {
                    return;
                };
                let media = match evaluate_media(tag.attr("media").unwrap_or(""), width) {
                    Some(m) => m,
                    None => {
                        p.undecidable = true;
                        return;
                    }
                };
                if !media
                    || !snowlark_render::srcset::supported_type(tag.attr("type").unwrap_or(""))
                {
                    return;
                }
                match choose_image(Some(srcset), tag.attr("sizes"), None, width, dpr) {
                    Some(Some(u)) => p.chosen = Some(u),
                    Some(None) => {}
                    None => p.undecidable = true,
                }
            }
            local_name!("img") => {
                // A lazy image is fetched when it nears the viewport, which the speculative
                // parser does not know.
                if tag
                    .attr("loading")
                    .is_some_and(|l| l.trim().eq_ignore_ascii_case("lazy"))
                {
                    return;
                }
                let chosen = match spec.picture.as_ref() {
                    Some(p) if p.undecidable => return,
                    Some(p) if p.chosen.is_some() => p.chosen.clone(),
                    _ => match choose_image(
                        tag.attr("srcset"),
                        tag.attr("sizes"),
                        tag.attr("src"),
                        width,
                        dpr,
                    ) {
                        Some(u) => u,
                        None => return,
                    },
                };
                if let Some(url) = chosen.as_deref().and_then(resolve) {
                    // (the image's own fetch, the renderer's, does not apply the element's
                    // `referrerpolicy` yet, and the early one must make the same request)
                    out.push(Early {
                        nonce: String::new(),
                        referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                        url,
                        kind: EarlyKind::Image,
                        class: fp(3),
                    });
                }
            }
            _ => {}
        }
    }

    /// The request an early fetch of `kind` makes: the one its consumer will make, so that the
    /// consumer's fetch takes the preloaded response.
    fn early_request(&self, doc: NodeId, url: url::Url, kind: EarlyKind) -> Option<Request> {
        url::Url::parse(self.dom().document_url(doc)).ok()?;
        // The document's client, with its CSP list (from its response headers, and the `meta`
        // policies the parser applied): a preload that it blocks is not fetched (a blocked preload is
        // not consumed: the element's own fetch decides).
        let client = self.request_client(doc);
        let mut r = match kind {
            EarlyKind::ClassicScript => Request::subresource(url, Destination::Script, client),
            EarlyKind::ModuleScript => {
                let mut r = Request::subresource(url, Destination::Script, client);
                r.mode = Mode::Cors;
                r.credentials = CredentialsMode::SameOrigin;
                r
            }
            EarlyKind::Stylesheet => {
                let mut r = Request::subresource(url, Destination::Style, client);
                r.initiator_type = "link".into();
                r
            }
            EarlyKind::Image => {
                let mut r = Request::subresource(url, Destination::Image, client);
                r.initiator_type = "img".into();
                r
            }
            EarlyKind::Preload(dest, cors) => {
                // HTML "create a potential-CORS request".
                let mut r = Request::subresource(url, dest, client);
                if let Some(credentials) = cors {
                    r.mode = Mode::Cors;
                    r.credentials = credentials;
                }
                if dest == Destination::Empty {
                    // `as=fetch`: what `fetch()` sends.
                    r.mode = Mode::Cors;
                    r.credentials = cors.unwrap_or(CredentialsMode::SameOrigin);
                }
                r.initiator_type = "link".into();
                r
            }
        };
        r.initiator_type = if r.initiator_type.is_empty() {
            "other".into()
        } else {
            std::mem::take(&mut r.initiator_type)
        };
        Some(r)
    }

    /// Starts an early fetch for `doc` (once per URL and kind: the document's list of speculative
    /// fetch URLs); the stream of its progress, for a `link` element's events.
    fn start_early(
        &self,
        doc: NodeId,
        url: url::Url,
        kind: EarlyKind,
        nonce: &str,
        referrer_policy: snowlark_net::ReferrerPolicy,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<snowlark_net::ResponseStream> {
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let net = self.hooks.borrow().net.clone()?;
        let key = format!("{kind:?} {url}");
        let new = self.doc_load(doc, |d| d.speculative_urls.insert(key));
        let mut request = self.early_request(doc, url, kind)?;
        request.nonce = nonce.to_owned();
        request.referrer_policy = referrer_policy;
        if !new && notify.is_none() {
            return None;
        }
        net.preload(request, notify)
    }

    // ----- link types ------------------------------------------------------------------------------

    /// The `link` types that fetch or connect ahead: `preload`, `modulepreload`, `preconnect` and
    /// `dns-prefetch` (HTML §4.6.7), when a `link` is inserted or its `rel`/`href`/`as` change.
    pub(crate) fn link_fetches_ahead(&self, el: NodeId) {
        if !links_enabled() {
            return;
        }
        let referrer_policy = self.referrer_policy_attr(el);
        let (rel, href, as_, crossorigin, media, ty, integrity) = {
            let dom = self.dom();
            let a = |n| dom.attr(el, &ns!(), &n).map(str::to_owned);
            (
                a(local_name!("rel"))
                    .unwrap_or_default()
                    .to_ascii_lowercase(),
                a(local_name!("href")),
                a(local_name!("as")).unwrap_or_default(),
                a(local_name!("crossorigin")),
                a(local_name!("media")).unwrap_or_default(),
                a(local_name!("type")),
                a(local_name!("integrity")).unwrap_or_default(),
            )
        };
        // (the element's nonce, which may be hidden from the attribute)
        let nonce = self.nonce_of(el);
        let rels: HashSet<&str> = rel.split_ascii_whitespace().collect();
        let Some(href) = href.filter(|h| !h.trim().is_empty()) else {
            return;
        };
        let Ok(url) = self.base_url_of(el).join(href.trim()) else {
            return;
        };
        let doc = self.dom().node_document(el);
        if rels.contains("preconnect") || rels.contains("dns-prefetch") {
            self.connect_ahead(&url, rels.contains("preconnect"));
        }
        if rels.contains("modulepreload") {
            let options = LinkFetchOptions {
                credentials: cors_credentials(crossorigin.as_deref()),
                referrer_policy,
                integrity,
                nonce,
            };
            self.modulepreload(Some(el), doc, url, &as_, options);
        } else if rels.contains("preload") {
            // HTML "fetch and process the linked resource" for `preload`: no fetch for an unknown
            // `as`, an unsupported `type`, or a `media` that does not match.
            let Some(dest) = preload_destination(&as_) else {
                return;
            };
            if ty
                .as_deref()
                .is_some_and(|t| !preload_type_supported(dest, t))
            {
                return;
            }
            let (width, _) = self.viewport_for_images();
            if !media_matches(&media, width) {
                return;
            }
            // An image the document has in its list of available images (HTML §4.8.4.3.2), or is
            // fetching for one of its elements, is that one resource: a no-CORS image preload of
            // its URL takes it rather than fetching it again (a response the HTTP cache may not
            // keep would otherwise go to the server twice), as image requests of `img` elements
            // do (`update_the_image_data`).
            if dest == Destination::Image && crossorigin.is_none() {
                let available = self
                    .with_node_renderer(el, |r, _| r.available_image(url.as_str()))
                    .flatten();
                match available {
                    Some(snowlark_render::ImageStatus::Loaded { .. }) => {
                        return self.queue_element_task_fire(el, "load", false, false);
                    }
                    Some(snowlark_render::ImageStatus::Fetching) => {
                        self.images
                            .borrow_mut()
                            .preload_waiters
                            .push((el, url.to_string()));
                        return;
                    }
                    Some(snowlark_render::ImageStatus::Failed) | None => {}
                }
            }
            let waker = self.waker();
            let kind = EarlyKind::Preload(dest, cors_credentials(crossorigin.as_deref()));
            let Some(mut request) = self.early_request(doc, url.clone(), kind) else {
                return;
            };
            request.integrity = integrity;
            request.nonce = nonce;
            request.referrer_policy = referrer_policy;
            let net = self.hooks.borrow().net.clone();
            let Some(net) = net else { return };
            self.doc_load(doc, |d| {
                d.speculative_urls.insert(format!("{kind:?} {url}"));
            });
            // The preload reports timing for its response, as a `link` (HTML "preload"); the fetch
            // that takes the preloaded response is the same resource (see `mark_resource_timing`).
            self.timing
                .borrow_mut()
                .link_preloads
                .insert((doc, url.to_string()), false);
            match net.preload_observed(request, Some(Arc::new(move || waker.wake()))) {
                Some(stream) => {
                    self.loader.borrow_mut().early.links += 1;
                    self.loader()
                        .jobs
                        .push(FetchJob::new(Purpose::Preload(el), Some(stream), url));
                }
                None => self.queue_element_task_fire(el, "error", false, false),
            }
        }
    }

    /// `preconnect` / `dns-prefetch` to `url`'s origin.
    fn connect_ahead(&self, url: &url::Url, connect: bool) {
        let net = self.hooks.borrow().net.clone();
        let Some(net) = net else { return };
        if !matches!(url.scheme(), "http" | "https") {
            return;
        }
        self.loader.borrow_mut().early.connects += 1;
        if connect {
            net.preconnect(url);
        } else if let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) {
            net.dns_prefetch(host, port);
        }
    }

    /// HTML "fetch a modulepreload module script graph" for a `link` (or a `Link:` header): the
    /// single module script, not its descendants (which the spec leaves to the user agent, and
    /// which Chromium does not fetch either). A module already in the document's module map
    /// fires `load` (or `error`) at once, or when its fetch ends. Otherwise its bytes are preloaded
    /// with the request the module loader makes for it, and compiled only when a `script` or an
    /// `import` asks for the module (a page lists many modules it may not run during load:
    /// GitHub's front page modulepreloads 85); `load` fires when the response is in with a
    /// JavaScript (or JSON) MIME type, `error` otherwise. A syntax error surfaces when the module
    /// is used, as the spec's module script with a parse error does. The request carries the
    /// link's options (its `crossorigin` credentials mode, `referrerpolicy`, `integrity` and
    /// `nonce`), as the spec's fetch options for the graph.
    pub(crate) fn modulepreload(
        &self,
        el: Option<NodeId>,
        doc: NodeId,
        url: url::Url,
        as_: &str,
        options: LinkFetchOptions,
    ) {
        let fire = |ev: &'static str| {
            if let Some(el) = el {
                self.queue_element_task_fire(el, ev, false, false);
            }
        };
        let (ty, dest) = match as_.trim().to_ascii_lowercase().as_str() {
            "" | "script" | "audioworklet" | "paintworklet" | "serviceworker" | "sharedworker"
            | "worker" => ("javascript", Destination::Script),
            "json" => ("json", Destination::Json),
            _ => return fire("error"),
        };
        let key = crate::script::module_key(doc.to_bits(), url.as_str(), ty);
        {
            let mut modules = self.modules.borrow_mut();
            match modules.by_url.get(&key) {
                Some(
                    crate::script::ModuleState::Ready(_) | crate::script::ModuleState::Errored(_),
                ) => return fire("load"),
                Some(crate::script::ModuleState::Failed) => return fire("error"),
                Some(crate::script::ModuleState::Fetching) => {
                    if let Some(el) = el {
                        modules.preload_waiters.push((key, el));
                    }
                    return;
                }
                None => {}
            }
        }
        let net = self.hooks.borrow().net.clone();
        let Some(net) = net else { return };
        if url::Url::parse(self.dom().document_url(doc)).is_err() {
            return;
        }
        let client = self.request_client(doc);
        // The module loader's request (`fetch_resource` for `Purpose::Module`).
        let mut request = Request::subresource(url.clone(), dest, client);
        request.mode = Mode::Cors;
        request.credentials = options.credentials.unwrap_or(CredentialsMode::SameOrigin);
        request.referrer_policy = options.referrer_policy;
        request.integrity = options.integrity;
        request.nonce = options.nonce;
        let waker = self.waker();
        self.doc_load(doc, |d| {
            d.speculative_urls
                .insert(format!("{:?} {url}", EarlyKind::ModuleScript));
        });
        match net.preload(request, Some(Arc::new(move || waker.wake()))) {
            Some(stream) => {
                self.loader.borrow_mut().early.links += 1;
                if let Some(el) = el {
                    self.loader().jobs.push(FetchJob::new(
                        Purpose::ModulePreload(el, ty == "json"),
                        Some(stream),
                        url,
                    ));
                }
            }
            None => fire("error"),
        }
    }

    /// A `Link:` header of a document's response (HTML "process link headers"): the same early
    /// fetches and connections as the `link` elements, for the document about to be parsed.
    pub(crate) fn process_link_headers(&self, doc: NodeId, head: &snowlark_net::ResponseHead) {
        if !links_enabled() {
            return;
        }
        let Some(base) = head.url().cloned() else {
            return;
        };
        let values: Vec<String> = head
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("link"))
            .map(|(_, v)| v.to_string())
            .collect();
        for value in values {
            for link in snowlark_net::headers::parse_link_header(&value) {
                let param = |n: &str| {
                    link.params
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(n))
                        .map(|(_, v)| v.as_str())
                };
                let rel = param("rel").unwrap_or("").to_ascii_lowercase();
                let rels: HashSet<&str> = rel.split_ascii_whitespace().collect();
                let Ok(url) = base.join(link.target.trim()) else {
                    continue;
                };
                if rels.contains("preconnect") || rels.contains("dns-prefetch") {
                    self.connect_ahead(&url, rels.contains("preconnect"));
                }
                if rels.contains("modulepreload") {
                    let options = LinkFetchOptions {
                        credentials: cors_credentials(param("crossorigin")),
                        referrer_policy: referrer_policy_attr(param("referrerpolicy")),
                        integrity: param("integrity").unwrap_or("").to_owned(),
                        nonce: param("nonce").unwrap_or("").to_owned(),
                    };
                    self.modulepreload(None, doc, url, param("as").unwrap_or(""), options);
                } else if rels.contains("preload") {
                    let Some(dest) = preload_destination(param("as").unwrap_or("")) else {
                        continue;
                    };
                    let (width, _) = self.viewport_for_images();
                    if !media_matches(param("media").unwrap_or(""), width) {
                        continue;
                    }
                    let kind = EarlyKind::Preload(dest, cors_credentials(param("crossorigin")));
                    let nonce = param("nonce").unwrap_or("");
                    if self
                        .start_early(
                            doc,
                            url,
                            kind,
                            nonce,
                            referrer_policy_attr(param("referrerpolicy")),
                            None,
                        )
                        .is_some()
                    {
                        self.loader.borrow_mut().early.links += 1;
                    }
                }
            }
        }
    }
}

/// HTML "translate a preload destination": only `fetch`, `font`, `image`, `script`, `style` and
/// `track` preload; any other value (a destination of another kind, or none) fetches nothing.
fn preload_destination(as_: &str) -> Option<Destination> {
    Some(match as_.trim().to_ascii_lowercase().as_str() {
        "fetch" => Destination::Empty,
        "font" => Destination::Font,
        "image" => Destination::Image,
        "script" => Destination::Script,
        "style" => Destination::Style,
        "track" => Destination::Track,
        _ => return None,
    })
}

/// Whether a preload's `type` names something the destination's consumer supports.
fn preload_type_supported(dest: Destination, ty: &str) -> bool {
    let t = ty.trim().to_ascii_lowercase();
    if t.is_empty() {
        return true;
    }
    let essence = t.split(';').next().unwrap_or("").trim();
    match dest {
        Destination::Script => {
            snowlark_net::mime::is_javascript_essence(essence) || essence == "module"
        }
        Destination::Style => essence == "text/css",
        Destination::Image => snowlark_render::srcset::supported_type(essence),
        Destination::Font => matches!(
            essence,
            "font/woff" | "font/woff2" | "font/ttf" | "font/otf" | "font/sfnt" | "font/collection"
        ),
        _ => true,
    }
}

/// HTML's fetch options of a `link` (or a `Link:` header) that modulepreloads.
pub(crate) struct LinkFetchOptions {
    /// Its CORS settings attribute's credentials mode (`None`: no attribute, "same-origin").
    credentials: Option<CredentialsMode>,
    referrer_policy: snowlark_net::ReferrerPolicy,
    integrity: String,
    nonce: String,
}

/// A `referrerpolicy` attribute's (or `Link:` header parameter's) state: missing and unknown values
/// are the empty string's.
fn referrer_policy_attr(attr: Option<&str>) -> snowlark_net::ReferrerPolicy {
    attr.and_then(|v| snowlark_net::ReferrerPolicy::parse(&v.trim().to_ascii_lowercase()))
        .unwrap_or(snowlark_net::ReferrerPolicy::Empty)
}

/// The credentials mode a CORS settings attribute asks for (`None`: no CORS, the attribute is
/// absent).
fn cors_credentials(attr: Option<&str>) -> Option<CredentialsMode> {
    let v = attr?;
    Some(if v.trim().eq_ignore_ascii_case("use-credentials") {
        CredentialsMode::Include
    } else {
        CredentialsMode::SameOrigin
    })
}

/// Whether a `media` attribute matches; an empty one does, one this evaluator cannot decide does
/// not (nothing is fetched early that the page might not fetch).
fn media_matches(media: &str, width: f32) -> bool {
    evaluate_media(media, width) == Some(true)
}

/// A small media query list evaluator for the speculative parser: media types `all`, `screen`,
/// `print`, and `min-width`/`max-width` in `px` or `em`, joined by `and`, listed with commas.
/// `None` for anything else.
fn evaluate_media(media: &str, width: f32) -> Option<bool> {
    let media = media.trim().to_ascii_lowercase();
    if media.is_empty() {
        return Some(true);
    }
    let mut any = false;
    for query in media.split(',') {
        let mut ok = true;
        for part in query.split(" and ") {
            let part = part.trim();
            let part = part.strip_prefix("only ").unwrap_or(part).trim();
            match part {
                "" | "all" | "screen" => {}
                "print" => ok = false,
                _ => {
                    let inner = part.strip_prefix('(')?.strip_suffix(')')?;
                    let (feature, value) = inner.split_once(':')?;
                    let value = value.trim();
                    let px = match value.strip_suffix("px") {
                        Some(n) => n.trim().parse::<f32>().ok()?,
                        None => value.strip_suffix("em")?.trim().parse::<f32>().ok()? * 16.0,
                    };
                    match feature.trim() {
                        "min-width" => ok &= width >= px,
                        "max-width" => ok &= width <= px,
                        _ => return None,
                    }
                }
            }
        }
        any |= ok;
    }
    Some(any)
}

/// HTML "select an image source" for an `img` (or a `source`) as the renderer does it: `None`
/// when it cannot be decided here (a `sizes` this evaluator does not handle), `Some(None)` when
/// there is no image.
fn choose_image(
    srcset: Option<&str>,
    sizes: Option<&str>,
    src: Option<&str>,
    width: f32,
    dpr: f32,
) -> Option<Option<String>> {
    use snowlark_render::srcset::{parse_srcset, select, Candidate};
    let mut candidates = srcset.map(parse_srcset).unwrap_or_default();
    if let Some(src) = src.filter(|s| !s.trim().is_empty()) {
        if !candidates
            .iter()
            .any(|c| c.density == Some(1.0) || c.width.is_some())
        {
            candidates.push(Candidate {
                url: src.to_owned(),
                density: Some(1.0),
                width: None,
            });
        }
    }
    if candidates.is_empty() {
        return Some(None);
    }
    let size = if candidates.iter().any(|c| c.width.is_some()) {
        match sizes {
            None => width,
            Some(s) => source_size(s, width)?,
        }
    } else {
        width
    };
    Some(select(&candidates, size, dpr).map(|(u, _)| u))
}

/// HTML "parse a sizes attribute" for the simple cases: entries of an optional media condition
/// (as [`evaluate_media`] takes them) and a length in `px` or `vw`; the last one has no condition.
fn source_size(sizes: &str, width: f32) -> Option<f32> {
    for entry in sizes.split(',') {
        let entry = entry.trim();
        let (cond, len) = match entry.rfind(char::is_whitespace) {
            Some(i) if entry[..i].trim_end().ends_with(')') => (&entry[..i], entry[i..].trim()),
            _ => ("", entry),
        };
        if !evaluate_media(cond, width)? {
            continue;
        }
        return if let Some(n) = len.strip_suffix("px") {
            n.parse().ok()
        } else if let Some(n) = len.strip_suffix("vw") {
            n.parse::<f32>().ok().map(|v| v * width / 100.0)
        } else {
            None
        };
    }
    Some(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_and_sizes() {
        assert_eq!(evaluate_media("", 800.0), Some(true));
        assert_eq!(evaluate_media("(min-width: 600px)", 800.0), Some(true));
        assert_eq!(
            evaluate_media("screen and (max-width: 40em)", 800.0),
            Some(false)
        );
        assert_eq!(
            evaluate_media("print, (max-width: 900px)", 800.0),
            Some(true)
        );
        assert_eq!(evaluate_media("(orientation: portrait)", 800.0), None);
        assert_eq!(
            source_size("(max-width: 600px) 100vw, 50vw", 800.0),
            Some(400.0)
        );
        assert_eq!(source_size("300px", 800.0), Some(300.0));
        assert_eq!(source_size("calc(100vw - 10px)", 800.0), None);
    }

    #[test]
    fn image_selection_follows_the_renderer() {
        assert_eq!(
            choose_image(Some("a.png 1x, b.png 2x"), None, Some("c.png"), 800.0, 2.0),
            Some(Some("b.png".into()))
        );
        assert_eq!(
            choose_image(None, None, Some("c.png"), 800.0, 1.0),
            Some(Some("c.png".into()))
        );
        assert_eq!(
            choose_image(
                Some("s.png 400w, l.png 1600w"),
                Some("50vw"),
                None,
                800.0,
                1.0
            ),
            Some(Some("s.png".into()))
        );
        assert_eq!(choose_image(None, None, None, 800.0, 1.0), Some(None));
        assert_eq!(
            choose_image(
                Some("s.png 400w"),
                Some("calc(1px + 2px)"),
                None,
                800.0,
                1.0
            ),
            None
        );
    }
}
