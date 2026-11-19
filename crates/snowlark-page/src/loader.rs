// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Loading (HTML §7.4, §13.2): fetching through `snowlark-net`, each document's incremental parser with
//! the script element processing model (HTML §4.12.1.1: prepare, the parsing-blocking script, defer,
//! async, in-order lists, execution), `document.write`/`open`/`close` (HTML §8.4), stylesheets handed to
//! the style hook, and "the end" (HTML §13.2.7) with `DOMContentLoaded` and `load`. Every browsing
//! context (the top-level one and each iframe's) loads its documents through here; the per-document
//! state is a [`DocLoad`].

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use snowlark_bindings::generated::DocumentReadyState;
use snowlark_bindings::rt::Cx;
use snowlark_dom::{
    element_flags, local_name, ns, DocumentKind, Dom, HtmlParser, NodeId, ParseStep,
};
use snowlark_js::code_cache::{
    CacheConsumer, CacheKind, CodeCache, CodeCacheStore, Entry, ResponseHasher,
};
use snowlark_js::streaming::{Feed, ScriptStreamer, StreamEncoding, StreamKind};
use snowlark_net::{FetchEvent, ResponseHead, ResponseStream};

use crate::engine::frames::ContextId;
use crate::engine::{node_handle, Engine};
use crate::event_loop::TaskSource;

/// What a fetch is for.
#[derive(Clone, Debug)]
pub enum Purpose {
    /// A navigation of a browsing context.
    Document(ContextId),
    Script(NodeId),
    Module(String),
    Stylesheet(NodeId),
    /// A `<link rel=preload>`'s preload (fetched by the network layer's list of preloaded
    /// resources; this job only follows it, for the element's `load` or `error`).
    Preload(NodeId),
    /// A `<link rel=modulepreload>`'s, likewise (whether it is a JSON module).
    ModulePreload(NodeId, bool),
    /// An SVG `image` element's resource, for its `load` or `error` (the renderer has its own fetch
    /// of it, from the HTTP cache, to paint).
    SvgImage(NodeId),
}

/// Who started a navigation, for its request's client: the initiating document (none: the user),
/// and whether it is a form submission (CSP form-action).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavInfo {
    pub source: Option<NodeId>,
    pub form: bool,
    /// HTML "reload": the document state's "reload pending".
    pub reload: bool,
    /// The navigation's referrer policy (`Empty`: the client's).
    pub referrer_policy: snowlark_net::ReferrerPolicy,
}

impl Default for NavInfo {
    fn default() -> Self {
        NavInfo {
            source: None,
            form: false,
            reload: false,
            referrer_policy: snowlark_net::ReferrerPolicy::Empty,
        }
    }
}

/// A top-level navigation the browser began and hands to the renderer to commit
/// ([`Engine::commit_navigation`]; `docs/process-isolation.md`, M5).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CommitParams {
    /// The URL the navigation was to (an error page shows it; a response's own URL is in its head).
    pub url: url::Url,
    /// The page's own request this commits ([`crate::embedder::BeginNavigation::seq`]), or none
    /// for the user's.
    pub begun: Option<u64>,
    pub reload: bool,
    pub response: CommitResponse,
    /// The CSP violations the request met (report-only or enforced), for the initiator to report.
    pub violations: Vec<snowlark_net::csp::Violation>,
    /// When the navigation began (Navigation Timing's `navigationStart`).
    #[serde(with = "snowlark_net::wire::instant")]
    pub started: Option<std::time::Instant>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum CommitResponse {
    /// The response the browser fetched: its head, and (beside the message) the stream with the
    /// rest of its body.
    Fetched { head: Box<ResponseHead> },
    /// The fetch failed before a response: an error page.
    Failed(snowlark_net::NetError),
    /// Nothing was fetched: the engine makes the document itself (`about:` URLs).
    Direct,
}

/// A body that arrived without the network: bytes, MIME type, URL.
pub type Immediate = (Vec<u8>, String, url::Url);

/// A fetch in flight.
pub struct FetchJob {
    pub purpose: Purpose,
    pub stream: Option<ResponseStream>,
    pub head: Option<ResponseHead>,
    pub body: Vec<u8>,
    /// A body that arrived without the network (an override, a file read).
    pub immediate: Option<Result<Immediate, ()>>,
    pub url: url::Url,
    /// For a navigation the browser fetched: its head is in `head` already and has not been
    /// handled ([`Engine::commit_navigation`]).
    pub head_pending: bool,
    /// For a navigation, the document its response is parsed into (made when the head arrives).
    pub document: Option<NodeId>,
    /// For a navigation, the body bytes that arrived.
    pub received: usize,
    /// For a script, what the network thread does with its body as it arrives.
    pub tap: Option<Arc<ScriptTap>>,
    /// For a script with a code cache entry, the entry being read.
    pub cache_read: Option<std::sync::mpsc::Receiver<Option<Entry>>>,
    /// That entry, being deserialized on a thread of its own, with the hash of the response it was
    /// made from and its kind.
    pub cache_consumer: Option<(CacheConsumer, [u8; 32], CacheKind)>,
    /// The entry was a timestamp of a load less than [`snowlark_js::code_cache::HOT_FOR`] ago: the
    /// hash of that load's response (the script is hot if this one is the same).
    pub cache_hot: Option<[u8; 32]>,
    /// For a script V8 parses while it downloads.
    pub streamer: Option<ScriptStreamer>,
    /// The body is complete; the script waits for V8's background part of the streaming
    /// compilation before it is done (the page thread does other work meanwhile).
    pub finishing: bool,
}

/// A script body as the network thread sees it: fed to V8's streaming compiler, and hashed for the
/// code cache, without the page thread.
pub struct ScriptTap {
    /// The streaming compiler's input, until the response head decides against streaming.
    feed: Arc<Feed>,
    hasher: std::sync::Mutex<Option<ResponseHasher>>,
    /// SHA-256 of the whole body, once it is complete.
    hash: std::sync::Mutex<Option<[u8; 32]>>,
}

impl ScriptTap {
    fn new(hash: bool) -> ScriptTap {
        ScriptTap {
            feed: Feed::new(),
            hasher: std::sync::Mutex::new(hash.then(ResponseHasher::new)),
            hash: std::sync::Mutex::new(None),
        }
    }

    /// One event of the fetch, on the network thread.
    fn observe(&self, e: &FetchEvent) {
        match e {
            FetchEvent::Body(b) => {
                self.feed.push(b);
                if let Some(h) = self.hasher.lock().unwrap().as_mut() {
                    h.update(b);
                }
            }
            FetchEvent::Done => {
                self.feed.finish();
                if let Some(h) = self.hasher.lock().unwrap().take() {
                    *self.hash.lock().unwrap() = Some(h.finish());
                }
            }
            FetchEvent::Error(_) => self.feed.abort(),
            _ => {}
        }
    }

    fn hash(&self) -> Option<[u8; 32]> {
        *self.hash.lock().unwrap()
    }
}

/// An external script's code cache: its key, the hash of the response it was compiled from, and the
/// entry that was on disk for it (deserialized off the page thread).
pub struct CacheInfo {
    pub cache: Arc<dyn CodeCacheStore>,
    pub key: String,
    pub hash: [u8; 32],
    pub consumer: Option<(CacheConsumer, [u8; 32], CacheKind)>,
    /// This response was loaded less than [`snowlark_js::code_cache::HOT_FOR`] ago: its code cache is
    /// made once it has run.
    pub hot: bool,
    /// The entry there was code made from another response.
    pub stale: bool,
}

impl CacheInfo {
    /// The entry made from this very response, if there is one.
    pub(crate) fn take_consumer(&mut self, kind: CacheKind) -> Option<CacheConsumer> {
        let (consumer, hash, k) = self.consumer.take()?;
        (hash == self.hash && k == kind).then_some(consumer)
    }
}

/// A compiled script whose code cache is made once it has run.
pub(crate) enum CacheSource {
    Classic(v8::Global<v8::UnboundScript>),
    Module(v8::Global<v8::Module>),
}

impl FetchJob {
    /// A job following `stream`.
    pub(crate) fn new(purpose: Purpose, stream: Option<ResponseStream>, url: url::Url) -> FetchJob {
        FetchJob {
            purpose,
            stream,
            head: None,
            body: Vec::new(),
            immediate: None,
            url,
            head_pending: false,
            document: None,
            received: 0,
            tap: None,
            cache_read: None,
            cache_consumer: None,
            cache_hot: None,
            streamer: None,
            finishing: false,
        }
    }
}

/// The script element's processing state (HTML §4.12.1.1).
#[derive(Default)]
pub struct ScriptState {
    pub parser_document: Option<NodeId>,
    pub seen: bool,
    pub force_async: bool,
    pub module: bool,
    pub preparation_document: Option<NodeId>,
    pub from_external: bool,
    pub ready_to_be_parser_executed: bool,
    pub result: ScriptResult,
    pub on_ready: OnReady,
    pub delaying_load: bool,
    /// The script fetch options "prepare the script element" chose (a classic script's go to
    /// the script it creates).
    pub options: crate::script::ModuleOptions,
    /// For a script the HTML parser created, the source line (1-based) its text starts on: an
    /// inline script's line numbers count from there. 0: not known (line 1).
    pub start_line: u32,
}

#[derive(Default)]
pub enum ScriptResult {
    #[default]
    Uninitialized,
    Null,
    Classic {
        source: String,
        url: String,
        line: i32,
        /// Its base URL and fetch options, as a [`crate::script::ReferrerScript`] id.
        referrer: u32,
        /// "Muted errors": its response was CORS-cross-origin.
        muted: bool,
        /// An external script V8 parsed while it downloaded.
        streamer: Option<ScriptStreamer>,
        /// An external script's code cache.
        cache: Option<Box<CacheInfo>>,
    },
    Module(v8::Global<v8::Module>),
    /// A module script whose error to rethrow is set (a specifier that failed to resolve).
    ModuleError(v8::Global<v8::Value>),
    /// An import map parse result (HTML §8.1.5.3): an import map, or its error to rethrow.
    ImportMap(Result<crate::import_map::ImportMap, v8::Global<v8::Value>>),
}

impl ScriptResult {
    fn is_uninitialized(&self) -> bool {
        matches!(self, ScriptResult::Uninitialized)
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum OnReady {
    #[default]
    Nothing,
    Asap,
    InOrder,
    ParserExecute,
}

/// URL §1.3 "percent-decode", then UTF-8 decode without BOM or failure.
fn percent_decoded(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The PDF viewer renders pages at this many device pixels per CSS px (sharp at 200% zoom or display
/// scale).
const PDF_SCALE: f32 = 2.0;

/// The PDF viewer shows at most this many pages of a file.
const PDF_MAX_PAGES: usize = 200;

/// How a navigation response becomes a document (HTML §7.4.2.3 "loading a document").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    Html,
    /// An XML MIME type: the XML parser, once the whole body is there.
    Xml,
    /// Text shown as a `pre` element (text/plain and other text types).
    Plain,
    /// A media file: a document holding an `img`/`video` element.
    Media,
    /// A PDF file with a PDF viewer (HTML "PDF viewer supported"): a document showing its pages.
    Pdf,
}

/// The kind of document a response's MIME type essence makes.
pub fn body_kind(mime: &str) -> BodyKind {
    let m = mime.to_ascii_lowercase();
    if m.is_empty() || m == "text/html" {
        BodyKind::Html
    } else if m == "text/xml" || m == "application/xml" || m.ends_with("+xml") {
        BodyKind::Xml
    } else if m.starts_with("text/") || m == "application/json" || m == "application/javascript" {
        BodyKind::Plain
    } else if m.starts_with("image/") || m.starts_with("video/") || m.starts_with("audio/") {
        BodyKind::Media
    } else {
        BodyKind::Plain
    }
}

/// A document's parser (HTML §13.2), or the XML parser and the body it waits for (HTML §13.4).
pub struct ParserState {
    pub parser: Option<HtmlParser<Rc<RefCell<Dom>>>>,
    pub xml: Option<snowlark_dom::XmlParser>,
    pub kind: BodyKind,
    /// The body of an XML or plain text document, until it is complete.
    pub buffered: Vec<u8>,
    pub nesting: u32,
    pub paused: bool,
    pub insertion_point: bool,
    pub script_created: bool,
    pub eof: bool,
    pub encoding_decided: bool,
    pub pending_bytes: Vec<u8>,
    pub transport_charset: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EndStage {
    Defer,
    WaitAsap,
    Done,
}

/// The loading state of one document: its parser, its script lists, "the end".
#[derive(Default)]
pub struct DocLoad {
    pub parser: Option<ParserState>,
    pub pending_parsing_blocking: Option<NodeId>,
    pub defer_scripts: VecDeque<NodeId>,
    pub in_order_scripts: VecDeque<NodeId>,
    pub asap_scripts: Vec<NodeId>,
    end: Option<EndStage>,
    pub load_delayers: usize,
    /// HTML "script-blocking style sheet set": `link` elements whose style sheet and its `@import`s
    /// are still being fetched; each leaves it after its `load` or `error` event
    /// ([`Engine::queue_sheet_event`]). A parser-inserted script does not run while it is not empty.
    pub(crate) script_blocking_sheets: rustc_hash::FxHashSet<NodeId>,
    /// The speculative HTML parser while the parser waits for a script.
    pub(crate) speculation: Option<crate::speculative::Speculation>,
    /// HTML "list of speculative fetch URLs" (with what they were fetched as).
    pub(crate) speculative_urls: std::collections::HashSet<String>,
    /// When "the end" started waiting for the document's images: the renderer's rendering update
    /// count then, the time, and whether the document had any `<img>` then.
    images_wait: Option<(u64, std::time::Instant, bool)>,
}

impl DocLoad {
    fn busy(&self) -> bool {
        self.parser.is_some() || matches!(self.end, Some(EndStage::Defer | EndStage::WaitAsap))
    }
}

/// The loader's state.
#[derive(Default)]
pub struct Loader {
    pub jobs: Vec<FetchJob>,
    pub scripts: rustc_hash::FxHashMap<NodeId, ScriptState>,
    pub docs: rustc_hash::FxHashMap<NodeId, DocLoad>,
    /// What loading cost, for `--render --trace` and the tests.
    pub stats: LoadStats,
    /// Early fetches and connections.
    pub(crate) early: crate::speculative::EarlyStats,
    streaming: Option<bool>,
    code_cache: Option<Option<Arc<dyn CodeCacheStore>>>,
    /// Scripts that ran whose code cache is to be made, with their cache key and response hash.
    pub(crate) pending_caches: Vec<(CacheSource, String, [u8; 32])>,
    /// A task making them is queued.
    pub(crate) caches_queued: bool,
}

/// Counters of the loading work: the top-level navigation's milestones, script compilation on the
/// page thread, streaming.
#[derive(Clone, Debug, Default)]
pub struct LoadStats {
    /// When the top-level navigation started ([`Engine::navigate`]).
    pub navigation_start: Option<std::time::Instant>,
    /// Milliseconds from the navigation's start to its response head, to `DOMContentLoaded` and to
    /// `load` of the top-level document (0 until reached).
    pub response_ms: f64,
    pub dom_content_loaded_ms: f64,
    pub load_ms: f64,
    /// Scripts (classic and module, inline and external) compiled on the page thread, and the time
    /// spent there compiling them (including finishing streamed compilations).
    pub compiled: u32,
    pub compile_ms: f64,
    /// External scripts V8 parsed and compiled on a background thread while they downloaded.
    pub streamed: u32,
    /// The page-thread part of those (finalization), included in `compile_ms`.
    pub streamed_finish_ms: f64,
    /// Bytes of the streamed scripts.
    pub streamed_bytes: usize,
    /// Streams whose bytes were not the script (a byte order mark of another encoding, an abort).
    pub stream_fallbacks: u32,
    /// Page-thread time in the scripts of the document: compiling and running each script
    /// element's script, compiling module scripts (their later work, event handlers and timers
    /// are not counted).
    pub script_ms: f64,
    /// Scripts compiled from their code cache entry, and the bytes of those entries.
    pub cache_hits: u32,
    pub cache_bytes: usize,
    /// Entries V8 rejected (another V8, corrupted).
    pub cache_rejected: u32,
    /// Entries made from another response than the one that arrived (the script changed).
    pub cache_stale: u32,
    /// Entries made, and the page-thread time making them.
    pub cache_produced: u32,
    pub cache_produce_ms: f64,
    /// Scripts not loaded recently, which left a timestamp rather than code.
    pub cache_timestamps: u32,
}

impl LoadStats {
    fn since_navigation(&self) -> f64 {
        self.navigation_start
            .map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0)
    }
}

/// Scripts below this size are compiled on the page thread when they arrive: a streaming thread
/// costs more than parsing them (Chromium's threshold for streaming is 30 KiB of source).
const STREAMING_MIN_BYTES: usize = 16 * 1024;
/// At most this many scripts stream at once (each has a thread of its own while it downloads).
const MAX_STREAMS: usize = 32;

/// The longest the load event waits for a document's images (a browser waits as long as they take;
/// this bounds what a stuck image or a document that never gets a rendering update costs).
const IMAGES_LOAD_DELAY_MAX: std::time::Duration = std::time::Duration::from_secs(10);

/// Scripts below this size get no code cache (Chromium's threshold too).
pub(crate) const CODE_CACHE_MIN_BYTES: usize = 1024;

impl Loader {
    /// Whether external scripts are compiled while they download (`SNOWLARK_SCRIPT_STREAMING=0` turns
    /// it off, for measurements).
    fn streaming_enabled(&mut self) -> bool {
        *self.streaming.get_or_insert_with(|| {
            std::env::var("SNOWLARK_SCRIPT_STREAMING").map_or(true, |v| v != "0")
        })
    }
}

/// What a script element's external script is: HTML's `src`; SVG's `href`, else `xlink:href`
/// (SVG 2 §15.6).
fn script_source(dom: &snowlark_dom::Dom, el: NodeId) -> Option<&str> {
    if dom.is_html_element_named(el, &local_name!("script")) {
        return dom.attr(el, &ns!(), &local_name!("src"));
    }
    dom.attr(el, &ns!(), &local_name!("href"))
        .or_else(|| dom.attr(el, &ns!(xlink), &local_name!("href")))
}

impl Engine {
    /// The code cache: the `code-cache` folder of the network's profile folder, or
    /// `SNOWLARK_CODE_CACHE_DIR` (for headless runs); none without either, or with
    /// `SNOWLARK_CODE_CACHE=0`.
    pub(crate) fn code_cache(&self) -> Option<Arc<dyn CodeCacheStore>> {
        if let Some(c) = &self.loader.borrow().code_cache {
            return c.clone();
        }
        let enabled = std::env::var("SNOWLARK_CODE_CACHE").map_or(true, |v| v != "0");
        let dir = self
            .hooks
            .borrow()
            .net
            .as_ref()
            .and_then(|n| n.profile_dir())
            .map(|d| d.join("code-cache"))
            .or_else(|| std::env::var_os("SNOWLARK_CODE_CACHE_DIR").map(std::path::PathBuf::from));
        let cache = dir.filter(|_| enabled).map(|d| {
            CodeCache::open(d, snowlark_js::code_cache::DEFAULT_BUDGET) as Arc<dyn CodeCacheStore>
        });
        self.loader().code_cache = Some(cache.clone());
        cache
    }

    /// The code cache key of a script at `url`: the URL within the top-level origin (the cache is
    /// partitioned like Chromium's, so a site cannot learn from load times what another site ran).
    pub(crate) fn code_cache_key(&self, url: &str) -> String {
        let top = self
            .document()
            .map(|d| self.origin_of_document(d))
            .unwrap_or_default();
        format!("{top} {url}")
    }
}

impl Engine {
    pub(crate) fn loader(&self) -> std::cell::RefMut<'_, Loader> {
        self.loader.borrow_mut()
    }

    /// The loading state of `doc` (made on first use).
    pub(crate) fn doc_load<R>(&self, doc: NodeId, f: impl FnOnce(&mut DocLoad) -> R) -> R {
        let mut l = self.loader.borrow_mut();
        f(l.docs.entry(doc).or_default())
    }

    /// The parser of `doc`, if it has one.
    fn parser_of<R>(&self, doc: NodeId, f: impl FnOnce(Option<&mut ParserState>) -> R) -> R {
        let mut l = self.loader.borrow_mut();
        f(l.docs.get_mut(&doc).and_then(|d| d.parser.as_mut()))
    }

    fn script<R>(&self, el: NodeId, f: impl FnOnce(&mut ScriptState) -> R) -> R {
        let mut l = self.loader.borrow_mut();
        let st = l.scripts.entry(el).or_default();
        if !st.seen {
            st.seen = true;
            let dom = self.dom();
            // (force async is initially true: the HTML and XML parsers set it to false on the
            // scripts they insert, not the fragment parser)
            if dom.element_flags(el) & element_flags::PARSER_INSERTED != 0 {
                st.parser_document = Some(dom.node_document(el));
            } else {
                st.force_async = true;
            }
        }
        f(st)
    }

    /// Whether the script element was inserted by the parser (its fetch's parser metadata).
    pub(crate) fn script_parser_inserted(&self, el: NodeId) -> bool {
        self.script(el, |s| s.parser_document.is_some())
    }

    pub(crate) fn script_force_async(&self, el: NodeId) -> bool {
        self.script(el, |s| s.force_async)
    }

    pub(crate) fn set_script_force_async(&self, el: NodeId, v: bool) {
        self.script(el, |s| s.force_async = v);
    }

    /// Whether fetches, parsing or "the end" are still under way.
    pub(crate) fn loader_busy(&self) -> bool {
        let l = self.loader.borrow();
        !l.jobs.is_empty()
            || l.docs.values().any(DocLoad::busy)
            || !self.api_jobs.borrow().is_empty()
            || !self.workers.borrow().is_empty()
    }

    /// What the loader is waiting for, a line per fetch in flight (its purpose, URL, response
    /// status, bytes so far, and whether its script waits for V8's background compile), then the
    /// documents still loading: for a harness to say why a run stalled.
    pub fn loading_report(&self) -> Vec<String> {
        let l = self.loader.borrow();
        let mut lines = Vec::new();
        for j in &l.jobs {
            let purpose = match &j.purpose {
                Purpose::Document(_) => "document",
                Purpose::Script(_) => "script",
                Purpose::Module(_) => "module",
                Purpose::Stylesheet(_) => "stylesheet",
                Purpose::Preload(_) => "preload",
                Purpose::ModulePreload(..) => "modulepreload",
                Purpose::SvgImage(_) => "svg image",
            };
            let status = j.head.as_ref().map_or("no response yet".to_owned(), |h| {
                format!("status {}", h.status)
            });
            let mut line = format!(
                "fetch {purpose} {}: {status}, {} bytes",
                j.url,
                j.received + j.body.len()
            );
            if j.stream.is_none() {
                line.push_str(", stream ended");
            }
            if j.finishing {
                line.push_str(", waiting for the background compile");
            }
            lines.push(line);
        }
        let busy = l.docs.values().filter(|d| d.busy()).count();
        if busy > 0 {
            lines.push(format!("{busy} documents still loading"));
        }
        let api = self.api_jobs.borrow().len();
        if api > 0 {
            lines.push(format!(
                "{api} script fetches (fetch(), XMLHttpRequest) in flight"
            ));
        }
        lines
    }

    /// Stops the top-level window's loading (the embedder's stop button).
    pub fn stop_loading_top(&self) {
        if let Some(w) = self.window() {
            self.stop_loading(w);
        }
    }

    /// `window.stop()`: abort the window's navigation fetches and its document's parser.
    pub(crate) fn stop_loading(&self, window: snowlark_bindings::rt::Handle) {
        let Some(id) = self.context_of_window(window) else {
            return;
        };
        self.cancel_document_fetches(id);
        if let Some(doc) = self.document_of_window(window) {
            let active = self.parser_of(doc, |p| {
                p.map(|p| {
                    p.eof = true;
                })
                .is_some()
            });
            // HTML "abort a document" step 4.1
            if active {
                self.doc_state(doc, |s| s.active_parser_aborted = true);
            }
        }
    }

    /// Drops the navigation fetches of a browsing context.
    pub(crate) fn cancel_document_fetches(&self, id: ContextId) {
        self.loader()
            .jobs
            .retain(|j| !matches!(j.purpose, Purpose::Document(c) if c == id));
    }

    /// HTML §4.8.7: the response to an `object`'s fetch arrived. A load that failed (an HTTP
    /// error status) fires `error` and shows the fallback content, as does a type the element
    /// shows neither as a document nor as an image (without an event). True when it fell back.
    fn object_falls_back(&self, id: ContextId, head: &ResponseHead) -> bool {
        let ok = head.ok() || head.status == 0;
        let mime = snowlark_net::mime::extract_mime_type(&head.headers)
            .map(|m| m.essence())
            .unwrap_or_default();
        self.object_result_falls_back(id, if ok { Ok(&mime) } else { Err(()) })
    }

    /// [`Engine::object_falls_back`] with the response's type, or `Err` for a failed load.
    fn object_result_falls_back(&self, id: ContextId, result: Result<&str, ()>) -> bool {
        let Some(el) = self.object_fetching(id) else {
            return false;
        };
        match result {
            Err(()) => self.object_fallback(id, el, true, true),
            Ok(mime) if !self.object_shows_type(el, mime) => {
                self.object_fallback(id, el, false, true)
            }
            Ok(_) => return false,
        }
        true
    }

    /// A navigation the page performs after the current task (`location.href = …`).
    pub(crate) fn request_navigation(
        &self,
        window: snowlark_bindings::rt::Handle,
        url: url::Url,
        source: Option<NodeId>,
        history: crate::engine::frames::HistoryHandling,
    ) {
        if let Some(id) = self.context_of_window(window) {
            let request = crate::engine::frames::NavigationRequest {
                url,
                post: None,
                source,
                form: false,
                reload: false,
                referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                history,
            };
            // (HTML "navigate" step 13: a `javascript:` URL queues its task now, so it runs before
            // the tasks queued after this one, such as the load event's)
            if request.url.scheme() == "javascript" {
                self.navigate_request(id, request);
                return;
            }
            self.frames.borrow_mut().list[id].navigation_request = Some(request);
        }
    }

    /// `location.reload()`: the page reloads after the current task.
    pub(crate) fn request_reload(
        &self,
        window: snowlark_bindings::rt::Handle,
        url: url::Url,
        source: Option<NodeId>,
    ) {
        if let Some(id) = self.context_of_window(window) {
            self.frames.borrow_mut().list[id].navigation_request =
                Some(crate::engine::frames::NavigationRequest {
                    url,
                    post: None,
                    source,
                    form: false,
                    reload: true,
                    referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                    history: Default::default(),
                });
        }
    }

    // ----- fetching -------------------------------------------------------------------------------------

    /// Starts a fetch for `purpose`.
    pub(crate) fn fetch_resource(&self, url: url::Url, purpose: Purpose) {
        self.fetch_resource_with(url, purpose, None, NavInfo::default());
    }

    /// Starts a fetch for `purpose`; a document fetch with `post` sends that body and content type,
    /// and `nav` says who started the navigation.
    pub(crate) fn fetch_resource_with(
        &self,
        url: url::Url,
        purpose: Purpose,
        post: Option<(Vec<u8>, String)>,
        nav: NavInfo,
    ) {
        let overridden = {
            let hooks = self.hooks.borrow();
            hooks.resource_override.as_ref().and_then(|f| f(&url))
        };
        let job = |stream, immediate| FetchJob {
            purpose: purpose.clone(),
            stream,
            head: None,
            body: Vec::new(),
            immediate,
            url: url.clone(),
            head_pending: false,
            document: None,
            received: 0,
            tap: None,
            cache_read: None,
            cache_consumer: None,
            cache_hot: None,
            streamer: None,
            finishing: false,
        };
        if let Some((body, mime)) = overridden {
            let j = job(None, Some(Ok((body, mime, url.clone()))));
            self.loader().jobs.push(j);
            return;
        }
        if url.scheme() == "about" {
            // about:blank and friends: an empty HTML document.
            let j = job(
                None,
                Some(Ok((Vec::new(), "text/html".into(), url.clone()))),
            );
            self.loader().jobs.push(j);
            return;
        }
        let net = self.hooks.borrow().net.clone();
        let Some(net) = net else {
            // No network: file: URLs are read directly, everything else fails.
            let immediate = if url.scheme() == "file" {
                url.to_file_path()
                    .ok()
                    .and_then(|p| std::fs::read(p).ok())
                    .map(|b| (b, mime_for_path(url.path()).to_owned(), url.clone()))
                    .ok_or(())
            } else {
                Err(())
            };
            let j = job(None, Some(immediate));
            self.loader().jobs.push(j);
            return;
        };
        // The request and its client: the document of the element (or of the module map, or the
        // navigation's initiator) the fetch is for.
        let (mut request, client_doc) = match &purpose {
            Purpose::Document(id) => self.navigation_request(*id, url.clone(), nav),
            p => {
                let client_doc = match p {
                    Purpose::Script(el)
                    | Purpose::Stylesheet(el)
                    | Purpose::SvgImage(el)
                    | Purpose::Preload(el)
                    | Purpose::ModulePreload(el, _) => Some(self.dom().node_document(*el)),
                    Purpose::Module(key) => {
                        // (a worker's map has no document of its own: the engine's only one is it)
                        match NodeId::from_bits(crate::script::split_module_key(key).0) {
                            Some(d) => self.dom().is_alive(d).then_some(d),
                            None => self.document(),
                        }
                    }
                    Purpose::Document(_) => None,
                };
                let dest = match p {
                    Purpose::Stylesheet(_) => snowlark_net::Destination::Style,
                    Purpose::SvgImage(_) => snowlark_net::Destination::Image,
                    // A module script's destination follows its module type.
                    Purpose::Module(key) => match crate::script::split_module_key(key).1 {
                        "json" => snowlark_net::Destination::Json,
                        "css" => snowlark_net::Destination::Style,
                        "text" => snowlark_net::Destination::Empty,
                        _ => snowlark_net::Destination::Script,
                    },
                    _ => snowlark_net::Destination::Script,
                };
                let Some(d) = client_doc else {
                    let j = job(None, Some(Err(())));
                    self.loader().jobs.push(j);
                    return;
                };
                let mut r =
                    snowlark_net::Request::subresource(url.clone(), dest, self.request_client(d));
                match p {
                    Purpose::Module(key) => {
                        r.mode = snowlark_net::Mode::Cors;
                        r.credentials = snowlark_net::CredentialsMode::SameOrigin;
                        // The module script's fetch options (its root's, or its importer's).
                        if let Some(o) = self.modules.borrow().options.get(key) {
                            r.nonce = o.nonce.clone();
                            r.parser_metadata = o.parser_metadata;
                            r.integrity = o.integrity.clone();
                            r.render_blocking = o.render_blocking;
                            r.credentials = o.credentials;
                            r.referrer_policy = o.referrer_policy;
                            // ("fetch a single imported module script": the importing script's
                            // base URL is the referrer)
                            if let Ok(u) = url::Url::parse(&o.referrer) {
                                r.referrer = snowlark_net::Referrer::Url(u);
                            }
                            // A module worker's scripts (HTML "fetch a single module script"): its
                            // credentials mode and destination; its main script same-origin, the
                            // creator's fetch (as a classic worker's main script is).
                            if let Some(w) = o.worker {
                                r.credentials = w.credentials;
                                r.destination = snowlark_net::Destination::Worker;
                                if w.top_level {
                                    r.mode = snowlark_net::Mode::SameOrigin;
                                    let ws = self.worker_scope.borrow();
                                    if let Some(ws) = ws.as_ref() {
                                        if let Some(c) = &ws.creator {
                                            r.client = Some(c.clone());
                                        }
                                        r.blob_url_entry = ws.blob_url_entry;
                                    }
                                }
                            }
                        }
                    }
                    Purpose::Script(el) => {
                        // HTML "prepare the script element": the fetch options' cryptographic
                        // nonce, integrity metadata and parser metadata.
                        r.nonce = self.nonce_of(*el);
                        // Fetch's "no-cors JavaScript fallback encoding": the element's charset
                        // or the document's encoding, for the network layer's script check.
                        r.js_fallback_encoding = Some(self.script_encoding(*el).name().to_owned());
                        r.integrity = self
                            .dom()
                            .attr(*el, &ns!(), &local_name!("integrity"))
                            .unwrap_or("")
                            .to_owned();
                        // Step 21 and "create a potential-CORS request": the crossorigin
                        // attribute's CORS setting is the request's mode and credentials mode.
                        if let Some(credentials) = self.cors_settings(*el) {
                            r.mode = snowlark_net::Mode::Cors;
                            r.credentials = credentials;
                        }
                        // Step 25: the referrerpolicy attribute's state.
                        r.referrer_policy = self.script(*el, |s| s.options.referrer_policy);
                        let parser_inserted = self.script(*el, |s| s.parser_document.is_some());
                        r.parser_metadata = if parser_inserted {
                            snowlark_net::ParserMetadata::ParserInserted
                        } else {
                            snowlark_net::ParserMetadata::NotParserInserted
                        };
                        // HTML: a parser-inserted classic script without async or defer is
                        // implicitly potentially render-blocking.
                        let implicit = parser_inserted && {
                            let dom = self.dom();
                            dom.attr(*el, &ns!(), &local_name!("async")).is_none()
                                && dom.attr(*el, &ns!(), &local_name!("defer")).is_none()
                        };
                        r.render_blocking = self.render_blocking_element(*el, implicit);
                    }
                    Purpose::Stylesheet(el) => {
                        r.nonce = self.nonce_of(*el);
                        r.initiator_type = "link".into();
                        // HTML "create a link request": the link's integrity metadata, and a
                        // potential-CORS request of its crossorigin attribute's CORS setting.
                        r.integrity = self
                            .dom()
                            .attr(*el, &ns!(), &local_name!("integrity"))
                            .unwrap_or("")
                            .to_owned();
                        if let Some(credentials) = self.cors_settings(*el) {
                            r.mode = snowlark_net::Mode::Cors;
                            r.credentials = credentials;
                        }
                        // (and its referrer policy, the referrerpolicy attribute's state)
                        r.referrer_policy = self.referrer_policy_attr(*el);
                        // HTML: a stylesheet link its document's parser made is implicitly
                        // potentially render-blocking.
                        let implicit = self.dom().element_flags(*el)
                            & snowlark_dom::element_flags::PARSER_INSERTED
                            != 0;
                        // (one whose media do not match blocks nothing)
                        let media = self
                            .dom()
                            .attr(*el, &ns!(), &local_name!("media"))
                            .map(str::to_owned);
                        r.render_blocking = self.render_blocking_element(*el, implicit)
                            && media.is_none_or(|m| self.media_query_matches(*el, &m));
                    }
                    _ => {}
                }
                (r, Some(d))
            }
        };
        if let Some((body, content_type)) = post {
            request.method = "POST".into();
            request.headers.append("Content-Type", &content_type);
            request.body = Some(snowlark_net::Body::new(body));
        }
        let waker = self.waker();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || waker.wake());
        // A script's bytes also go to V8's streaming compiler, from the network thread (see
        // `start_streaming`).
        let script = match &purpose {
            Purpose::Script(_) => true,
            Purpose::Module(key) => crate::script::split_module_key(key).1 == "javascript",
            _ => false,
        } && matches!(url.scheme(), "http" | "https");
        let hash = script && self.code_cache().is_some();
        let tapped = hash || (script && self.loader().streaming_enabled());
        let (observer, tap) = if tapped {
            let tap = Arc::new(ScriptTap::new(hash));
            let t = tap.clone();
            let observer: snowlark_net::Observer = Arc::new(move |e: &FetchEvent| t.observe(e));
            (Some(observer), Some(tap))
        } else {
            (None, None)
        };
        // Every request but the user's own navigations goes through the request policy.
        let stream = match client_doc {
            Some(d) => self.policy_fetch(d, request, Some(notify), observer),
            None => Some(match observer {
                Some(o) => net.fetch_observed(request, notify, o),
                None => net.fetch_with_notify(request, notify),
            }),
        };
        let mut j = job(stream, None);
        j.tap = tap;
        self.loader().jobs.push(j);
    }

    /// The request of a navigation of the browsing context `id` (HTML "navigate" and "create
    /// navigation params by fetching"): the user's own top-level navigations have no client; the
    /// others have their initiator's (for a frame's own, its container's document), so that its
    /// policy applies (frame-src, form-action, mixed content, upgrade-insecure-requests) and the
    /// referrer is sent. Returns the request and its client's document.
    fn navigation_request(
        &self,
        id: ContextId,
        url: url::Url,
        nav: NavInfo,
    ) -> (snowlark_net::Request, Option<NodeId>) {
        // (the URL was parsed by whatever navigates: its blob URL entry is what is fetched)
        let blob_url_entry = self.hold_blob_url(&url);
        let mut r = snowlark_net::Request::navigation(url);
        r.blob_url_entry = blob_url_entry;
        // A child navigable's response is checked for framing by the network layer (M5 step 3).
        r.frame_ancestors = self.frame_ancestor_origins(id);
        if nav.reload {
            // HTML "create navigation params by fetching" sets the reload-navigation flag, and
            // neither it nor Fetch says what that asks of the HTTP cache. The user asked for the
            // page as it is now: "no-cache" sends `Cache-Control: max-age=0`, and a stored
            // response is validated (RFC 9111 §5.2.1.1, §4.3), not used because it is fresh.
            // Subresources keep their own freshness.
            r.reload_navigation = true;
            r.cache = snowlark_net::CacheMode::NoCache;
        }
        let container = self.frames.borrow().list.get(id).and_then(|b| b.container);
        let source = nav
            .source
            .or_else(|| container.map(|c| self.dom().node_document(c)))
            .filter(|&d| self.dom().is_alive(d));
        let Some(source) = source else {
            return (r, None);
        };
        r.user_initiated = false;
        r.user_activation = false;
        r.referrer = snowlark_net::Referrer::Client;
        r.referrer_policy = nav.referrer_policy;
        r.form_submission = nav.form;
        // (the page's own top-level traversable, or another it hosts: an auxiliary browsing context)
        r.top_level_navigation = container.is_none()
            && self
                .frames
                .borrow()
                .list
                .get(id)
                .is_some_and(|b| b.parent.is_none());
        if let Some(c) = container {
            let dom = self.dom();
            r.destination = if dom.is_html_element_named(c, &local_name!("frame")) {
                snowlark_net::Destination::Frame
            } else if dom.is_html_element_named(c, &local_name!("object")) {
                snowlark_net::Destination::Object
            } else if dom.is_html_element_named(c, &local_name!("embed")) {
                snowlark_net::Destination::Embed
            } else {
                snowlark_net::Destination::Iframe
            };
        }
        r.client = Some(self.request_client(source));
        (r, Some(source))
    }

    /// HTML "CORS settings attribute": the credentials mode an element's `crossorigin` attribute
    /// asks for (Use Credentials for `use-credentials`, Anonymous for any other value), or `None`
    /// without the attribute, when the request is a no-CORS one.
    pub(crate) fn cors_settings(&self, el: NodeId) -> Option<snowlark_net::CredentialsMode> {
        let dom = self.dom();
        let value = dom.attr(el, &ns!(), &snowlark_dom::LocalName::from("crossorigin"))?;
        Some(if value.eq_ignore_ascii_case("use-credentials") {
            snowlark_net::CredentialsMode::Include
        } else {
            snowlark_net::CredentialsMode::SameOrigin
        })
    }

    /// An element's `referrerpolicy` attribute's state (HTML "referrer policy attributes", an
    /// enumerated attribute: missing and unknown values are the empty string's state).
    pub(crate) fn referrer_policy_attr(&self, el: NodeId) -> snowlark_net::ReferrerPolicy {
        self.dom()
            .attr(el, &ns!(), &local_name!("referrerpolicy"))
            .and_then(|v| snowlark_net::ReferrerPolicy::parse(&v.to_ascii_lowercase()))
            .unwrap_or(snowlark_net::ReferrerPolicy::Empty)
    }

    /// HTML's "fetch a classic script" and "fetch a single module script" take the body as it
    /// arrives here: once the response head says what the script is, V8 starts parsing and compiling
    /// it on a thread of its own, from the bytes the network thread feeds it, so that only the
    /// finalization is left for the page thread when the script runs (Chromium's script streaming).
    /// Small scripts, encodings V8 does not stream, and non-JavaScript module responses compile as
    /// before.
    ///
    /// A script with a code cache entry is read first (off the page thread; the body's bytes are
    /// kept meanwhile): code made from the response that arrives is used instead of streaming
    /// ([`Engine::cache_entry_read`]); a timestamp, or no entry, streams as before.
    fn start_streaming(&self, cx: &mut Cx, i: usize) {
        let (feed, head) = {
            let mut l = self.loader.borrow_mut();
            let job = &mut l.jobs[i];
            let Some(tap) = job.tap.as_ref() else { return };
            (tap.feed.clone(), job.head.clone())
        };
        let Some(head) = head.filter(|h| h.ok()) else {
            feed.disable();
            return;
        };
        if let (Some(cache), Some(url)) = (self.code_cache(), head.url()) {
            let key = self.code_cache_key(url.as_str());
            if cache.has(&key) {
                let read = cache.read_async(&key);
                if let Some(job) = self.loader().jobs.get_mut(i) {
                    job.cache_read = Some(read);
                }
                return;
            }
        }
        self.stream_script(cx, i);
    }

    /// What a script's code cache entry decides, once it is read: code is deserialized on a thread
    /// of its own while the body arrives (used if the body is the response it was made from); a
    /// timestamp says whether the script is hot. Without code to use, the script streams (from the
    /// bytes kept, if its body is already complete; the job then waits for the stream).
    fn cache_entry_read(&self, cx: &mut Cx, i: usize, entry: Option<Entry>) {
        let feed = {
            let l = self.loader.borrow();
            let Some(job) = l.jobs.get(i) else { return };
            job.tap.as_ref().map(|t| t.feed.clone())
        };
        match entry {
            Some(e) if e.kind != CacheKind::Timestamp => {
                if let Some(feed) = feed {
                    feed.disable();
                }
                let waker = self.waker();
                let consumer = {
                    let scope = cx.scope();
                    CacheConsumer::start(scope, e.data, Box::new(move || waker.wake()))
                };
                if let (Some(c), Some(job)) = (consumer, self.loader().jobs.get_mut(i)) {
                    job.cache_consumer = Some((c, e.response_hash, e.kind));
                }
            }
            entry => {
                let now = std::time::SystemTime::now();
                if let (Some(e), Some(job)) = (entry, self.loader().jobs.get_mut(i)) {
                    if snowlark_js::code_cache::is_hot(&e.data, now) {
                        job.cache_hot = Some(e.response_hash);
                    }
                }
                self.stream_script(cx, i);
            }
        }
    }

    /// Starts V8's background parse and compile of the script of job `i` from its feed.
    fn stream_script(&self, cx: &mut Cx, i: usize) {
        let (feed, purpose, head) = {
            let mut l = self.loader.borrow_mut();
            let job = &mut l.jobs[i];
            let Some(tap) = job.tap.as_ref() else { return };
            (tap.feed.clone(), job.purpose.clone(), job.head.clone())
        };
        let Some(head) = head.filter(|h| h.ok()) else {
            feed.disable();
            return;
        };
        if !self.loader().streaming_enabled() {
            feed.disable();
            return;
        }
        let content_length = head
            .headers
            .get("content-length")
            .and_then(|v| v.trim().parse::<usize>().ok());
        let encoded = head.headers.get("content-encoding").is_some();
        // A compressed script is several times its transfer size.
        let small =
            content_length.is_some_and(|n| n * if encoded { 4 } else { 1 } < STREAMING_MIN_BYTES);
        let (encoding, kind) = match &purpose {
            Purpose::Script(el) => {
                let charset = snowlark_net::encoding::legacy_extract_encoding(&head.headers);
                let enc = charset.unwrap_or_else(|| self.script_encoding(*el));
                let encoding = if enc == encoding_rs::UTF_8 {
                    Some(StreamEncoding::Utf8)
                } else if enc == encoding_rs::WINDOWS_1252 {
                    Some(StreamEncoding::Windows1252)
                } else {
                    None
                };
                (encoding, StreamKind::Classic)
            }
            Purpose::Module(_) => {
                let js = snowlark_net::mime::extract_mime_type(&head.headers)
                    .is_some_and(|m| snowlark_net::mime::is_javascript_essence(&m.essence()));
                (js.then_some(StreamEncoding::Utf8), StreamKind::Module)
            }
            _ => (None, StreamKind::Classic),
        };
        let streams = self
            .loader
            .borrow()
            .jobs
            .iter()
            .filter(|j| j.streamer.is_some())
            .count();
        let Some(encoding) = encoding.filter(|_| !small && streams < MAX_STREAMS) else {
            feed.disable();
            return;
        };
        let waker = self.waker();
        let streamer = {
            let scope = cx.scope();
            ScriptStreamer::start(
                scope,
                feed.clone(),
                encoding,
                kind,
                Box::new(move || waker.wake()),
            )
        };
        match streamer {
            Some(s) => {
                if let Some(job) = self.loader().jobs.get_mut(i) {
                    job.streamer = Some(s);
                }
            }
            None => feed.disable(),
        }
    }

    /// Takes the network events that arrived. Returns whether anything happened.
    pub fn poll_network(&self, cx: &mut Cx) -> bool {
        let mut progressed = self.report_pending_violations(cx);
        progressed |= self.poll_api_fetches(cx);
        progressed |= self.poll_workers(cx);
        progressed |= self.poll_websockets(cx);
        progressed |= self.poll_broadcasts(cx);
        progressed |= self.poll_storage();
        progressed |= self.poll_idb(cx);
        progressed |= self.poll_audio(cx);
        progressed |= self.poll_canvas_frames();
        let mut i = 0;
        loop {
            // One job at a time, so handlers can start new fetches.
            let event = {
                let mut l = self.loader.borrow_mut();
                if i >= l.jobs.len() {
                    break;
                }
                let job = &mut l.jobs[i];
                if let Some(imm) = job.immediate.take() {
                    Some(JobEvent::Complete(imm))
                } else if std::mem::take(&mut job.head_pending) {
                    // (the browser took the head; the rest of the response follows in the stream)
                    Some(JobEvent::Head)
                } else if job.stream.is_none() && job.head.is_some() {
                    // (a committed response with no body to read)
                    Some(JobEvent::Done)
                } else if job.finishing {
                    (job.streamer.as_ref().is_none_or(ScriptStreamer::is_done)
                        && job.cache_consumer.as_ref().is_none_or(|c| c.0.is_done()))
                    .then_some(JobEvent::Done)
                } else if let Some(Ok(entry)) = job.cache_read.as_ref().map(|r| r.try_recv()) {
                    job.cache_read = None;
                    Some(JobEvent::CacheEntry(entry))
                } else {
                    match job.stream.as_mut().and_then(|s| s.try_next()) {
                        None => None,
                        // (a child navigable's frame-ancestors violation, found by the network
                        // layer and reported from there: the console message is this page's)
                        Some(FetchEvent::Violation(v))
                            if v.effective_directive == "frame-ancestors" =>
                        {
                            Some(JobEvent::Violation(v))
                        }
                        Some(FetchEvent::Violation(_)) => Some(JobEvent::Nothing),
                        Some(FetchEvent::Head(h)) => {
                            job.head = Some(h);
                            Some(JobEvent::Head)
                        }
                        Some(FetchEvent::Body(b)) => {
                            if let (Purpose::Document(_), Some(doc)) = (&job.purpose, job.document)
                            {
                                job.received += b.len();
                                Some(JobEvent::DocumentBytes(doc, b.to_vec()))
                            } else {
                                job.body.extend_from_slice(&b);
                                Some(JobEvent::Nothing)
                            }
                        }
                        Some(FetchEvent::Done) if job.cache_read.is_some() => {
                            // (the entry is still being read: wait for it, it is a file)
                            let entry = job.cache_read.take().and_then(|r| r.recv().ok().flatten());
                            job.finishing = true;
                            Some(JobEvent::CacheEntry(entry))
                        }
                        Some(FetchEvent::Done)
                            if job.streamer.as_ref().is_some_and(|s| !s.is_done())
                                || job.cache_consumer.as_ref().is_some_and(|c| !c.0.is_done()) =>
                        {
                            job.finishing = true;
                            Some(JobEvent::Nothing)
                        }
                        Some(FetchEvent::Done) => Some(JobEvent::Done),
                        Some(FetchEvent::Error(e)) => Some(JobEvent::Failed(e)),
                    }
                }
            };
            match event {
                None => i += 1,
                Some(JobEvent::Nothing) => progressed = true,
                Some(JobEvent::Violation(v)) => {
                    progressed = true;
                    self.console_print("error", &v.console_message());
                }
                Some(JobEvent::Head) => {
                    progressed = true;
                    let (purpose, head, job_url) = {
                        let l = self.loader.borrow();
                        (
                            l.jobs[i].purpose.clone(),
                            l.jobs[i].head.clone(),
                            l.jobs[i].url.clone(),
                        )
                    };
                    match (purpose, head) {
                        (Purpose::Document(id), Some(h)) if self.object_falls_back(id, &h) => {}
                        (Purpose::Document(id), Some(h)) if self.opener_policy_refuses(id, &h) => {
                            // (a network error: the context shows an error page, in a browsing context
                            // of a new group, as the policy's enforcement, before the refusal, asked:
                            // the sandboxed popup's documents match no policy)
                            let url = h.url().cloned().unwrap_or_else(|| job_url.clone());
                            self.cancel_document_fetches(id);
                            self.frames.borrow_mut().list[id].group_switch = true;
                            self.console_print(
                                "error",
                                "Blocked a navigation of a sandboxed popup to a document with a \
                                 Cross-Origin-Opener-Policy",
                            );
                            self.show_error_page(
                                cx,
                                id,
                                &url,
                                crate::error_page::Failure::Unavailable,
                            );
                        }
                        (Purpose::Document(id), Some(h)) => {
                            let doc = self.document_response(cx, id, &h);
                            if let Some(j) = self.loader().jobs.get_mut(i) {
                                j.document = Some(doc);
                            }
                        }
                        (Purpose::Script(_) | Purpose::Module(_), _) => self.start_streaming(cx, i),
                        _ => {}
                    }
                }
                Some(JobEvent::DocumentBytes(doc, b)) => {
                    progressed = true;
                    self.document_bytes(cx, doc, &b, false);
                }
                Some(JobEvent::CacheEntry(entry)) => {
                    progressed = true;
                    self.cache_entry_read(cx, i, entry);
                }
                Some(JobEvent::Done) => {
                    progressed = true;
                    let mut job = self.loader.borrow_mut().jobs.remove(i);
                    let tap = job.tap.take();
                    let streamer = job.streamer.take();
                    if let (Some(tap), None) = (&tap, &streamer) {
                        // Nobody reads the bytes it kept.
                        tap.feed.disable();
                    }
                    let head = job.head.clone();
                    // The script's code cache: its key and the hash of what arrived.
                    let cache = match (self.code_cache(), tap.and_then(|t| t.hash())) {
                        (Some(cache), Some(hash))
                            if job.body.len() >= CODE_CACHE_MIN_BYTES
                                && head.as_ref().is_some_and(|h| h.ok()) =>
                        {
                            let url = head
                                .as_ref()
                                .and_then(|h| h.url().cloned())
                                .unwrap_or(job.url.clone());
                            let consumer = job.cache_consumer.take();
                            Some(CacheInfo {
                                cache,
                                key: self.code_cache_key(url.as_str()),
                                hash,
                                stale: consumer.as_ref().is_some_and(|c| c.1 != hash),
                                consumer,
                                hot: job.cache_hot == Some(hash),
                            })
                        }
                        _ => None,
                    };
                    let ok = head.as_ref().is_some_and(|h| h.ok() || h.status == 0);
                    let mime = head
                        .as_ref()
                        .and_then(|h| snowlark_net::mime::extract_mime_type(&h.headers))
                        .map(|m| m.essence())
                        .unwrap_or_default();
                    let final_url = head
                        .as_ref()
                        .and_then(|h| h.url().cloned())
                        .unwrap_or(job.url.clone());
                    match job.purpose {
                        // An HTTP error without a body: the top-level context shows an error page.
                        Purpose::Document(id)
                            if self.is_top_level_context(id)
                                && job.received == 0
                                && head.as_ref().is_some_and(|h| h.status >= 400) =>
                        {
                            let status = head.as_ref().map_or(0, |h| h.status);
                            self.show_error_page(
                                cx,
                                id,
                                &job.url,
                                crate::error_page::Failure::Http(status),
                            );
                        }
                        Purpose::Document(id) => {
                            let doc = match job.document {
                                Some(d) => d,
                                None => self.begin_document(cx, id, &job.url, "text/html", None),
                            };
                            self.document_bytes(cx, doc, &[], true)
                        }
                        p => self.subresource_done(
                            cx,
                            p,
                            if ok {
                                Ok((job.body, mime, final_url))
                            } else {
                                Err(())
                            },
                            head.as_ref(),
                            streamer,
                            cache,
                        ),
                    }
                }
                Some(JobEvent::Failed(e)) => {
                    progressed = true;
                    let job = self.loader.borrow_mut().jobs.remove(i);
                    match job.purpose {
                        // An `object`'s fetch that failed: `error`, then its fallback content.
                        Purpose::Document(id)
                            if job.document.is_none()
                                && self.object_result_falls_back(id, Err(())) => {}
                        // A top-level navigation that failed before a response: an error page.
                        Purpose::Document(id)
                            if self.is_top_level_context(id) && job.document.is_none() =>
                        {
                            self.show_error_page(
                                cx,
                                id,
                                &job.url,
                                crate::error_page::Failure::Network(e),
                            );
                        }
                        // A navigation a request policy blocked (frame-src, mixed content...), or
                        // a response that refused framing (X-Frame-Options, frame-ancestors; the
                        // network layer's check, M5 step 3): an empty document with an opaque
                        // origin.
                        Purpose::Document(id)
                            if job.document.is_none()
                                && matches!(e.kind, snowlark_net::NetErrorKind::Blocked(_)) =>
                        {
                            if e.kind
                                == snowlark_net::NetErrorKind::Blocked(
                                    snowlark_net::BlockReason::Framing,
                                )
                            {
                                self.console_print("error", &e.message);
                            }
                            self.show_blocked_frame(cx, id);
                        }
                        Purpose::Document(id) => {
                            // A failed navigation leaves an empty document.
                            let doc = match job.document {
                                Some(d) => d,
                                None => self.begin_document(cx, id, &job.url, "text/html", None),
                            };
                            self.document_bytes(cx, doc, &[], true)
                        }
                        p => self.subresource_done(cx, p, Err(()), None, None, None),
                    }
                }
                Some(JobEvent::Complete(r)) => {
                    progressed = true;
                    let job = self.loader.borrow_mut().jobs.remove(i);
                    match job.purpose {
                        Purpose::Document(id)
                            if self.object_result_falls_back(
                                id,
                                r.as_ref().map(|(_, mime, _)| mime.as_str()).map_err(|_| ()),
                            ) => {}
                        Purpose::Document(id) => match r {
                            Ok((body, mime, url)) => {
                                let doc = self.begin_document(cx, id, &url, &mime, None);
                                // (about:blank: a copy of the creator's policy container)
                                if url.scheme() == "about" {
                                    if let Some(parent) = self.parent_document(doc) {
                                        self.inherit_policy(cx, doc, parent);
                                    }
                                }
                                self.document_bytes(cx, doc, &body, true);
                            }
                            Err(()) if self.is_top_level_context(id) => {
                                self.show_error_page(
                                    cx,
                                    id,
                                    &job.url,
                                    crate::error_page::Failure::Unavailable,
                                );
                            }
                            Err(()) => {
                                let doc = self.begin_document(cx, id, &job.url, "text/html", None);
                                self.document_bytes(cx, doc, &[], true);
                            }
                        },
                        p => self.subresource_done(cx, p, r, None, None, None),
                    }
                }
            }
        }
        progressed
    }

    fn subresource_done(
        &self,
        cx: &mut Cx,
        purpose: Purpose,
        r: Result<(Vec<u8>, String, url::Url), ()>,
        head: Option<&ResponseHead>,
        streamer: Option<ScriptStreamer>,
        cache: Option<CacheInfo>,
    ) {
        match purpose {
            Purpose::Script(el) => {
                let result = match r {
                    Ok((body, _mime, url)) => {
                        let charset = head.and_then(|h| {
                            snowlark_net::encoding::legacy_extract_encoding(&h.headers)
                        });
                        let enc = charset.unwrap_or_else(|| self.script_encoding(el));
                        let (text, _, _) = enc.decode(&body);
                        // HTML "fetch a classic script": the script's base URL is the
                        // response's URL.
                        let options = self.script(el, |s| s.options.clone());
                        // HTML "fetch a classic script" step 8.3: muted errors when the response
                        // is CORS-cross-origin (opaque: a no-cors fetch that went to, or through,
                        // another origin).
                        let muted = head.is_some_and(|h| {
                            matches!(
                                h.response_type,
                                snowlark_net::response::ResponseType::Opaque
                                    | snowlark_net::response::ResponseType::OpaqueRedirect
                            )
                        });
                        ScriptResult::Classic {
                            source: text.into_owned(),
                            referrer: self.referrer_id(url.as_str(), &options),
                            url: url.to_string(),
                            line: 1,
                            muted,
                            streamer,
                            cache: cache.map(Box::new),
                        }
                    }
                    Err(()) => ScriptResult::Null,
                };
                self.mark_ready(cx, el, result);
            }
            Purpose::Module(key) => {
                let (_, ty, _) = crate::script::split_module_key(&key);
                let body = match r {
                    Ok((body, mime, final_url)) => {
                        let ok = match ty {
                            "json" => {
                                mime == "application/json"
                                    || mime == "text/json"
                                    || mime.ends_with("+json")
                            }
                            "css" => mime == "text/css",
                            "text" => true,
                            _ => snowlark_net::mime::is_javascript_essence(&mime),
                        };
                        ok.then(|| {
                            // UTF-8 decode, with a BOM removed (HTML "UTF-8 decode").
                            let (text, _) = encoding_rs::UTF_8.decode_with_bom_removal(&body);
                            (text.into_owned(), final_url.to_string())
                        })
                    }
                    _ => None,
                };
                // HTML "fetch a single module script" step 13.4: a `Referrer-Policy` header is
                // the module script's referrer policy (its descendants fetch with it).
                let policy =
                    head.map(|h| snowlark_net::referrer::parse_referrer_policy_header(&h.headers));
                if let Some(p) = policy.filter(|&p| p != snowlark_net::ReferrerPolicy::Empty) {
                    let mut m = self.modules.borrow_mut();
                    m.options.entry(key.clone()).or_default().referrer_policy = p;
                }
                self.module_fetched(cx, &key, body, streamer, cache);
            }
            Purpose::Stylesheet(el) => {
                let doc = self.dom().node_document(el);
                self.doc_load(doc, |d| d.load_delayers = d.load_delayers.saturating_sub(1));
                // HTML "process this type of linked resource" (stylesheet): the response's
                // Content-Type must be text/css (none, and the unknown type, are let through, as
                // the engines do), except under the "quirks mode stylesheet" rule: in quirks mode
                // a response whose URL is of the document's origin is taken as text/css.
                let r = r.and_then(|(body, mime, url)| {
                    let css = mime.is_empty()
                        || mime == "text/css"
                        || mime == "application/x-unknown-content-type";
                    let lax = self.dom().quirks_mode(doc) == snowlark_dom::QuirksMode::Quirks
                        && snowlark_net::origin::origin_of(&url, None)
                            == self.net_origin_of_document(doc);
                    (css || lax).then_some((body, mime, url)).ok_or(())
                });
                match r {
                    Ok((body, _mime, url)) => {
                        // CSS Syntax §3.2 "decode": BOM, the Content-Type charset, `@charset`,
                        // then the document's encoding.
                        let charset = head.and_then(|h| {
                            snowlark_net::encoding::legacy_extract_encoding(&h.headers)
                        });
                        let env = self.dom().encoding(doc);
                        let (text, _) =
                            snowlark_render::decode_stylesheet(&body, charset, Some(env));
                        // (with a renderer, `load` or `error` waits for the sheet's `@import`s:
                        // `Renderer::take_sheets_settled`)
                        if !self.render_link_sheet(el, Some((text, url))) {
                            self.queue_sheet_event(el, Some(true));
                        }
                    }
                    Err(()) => {
                        self.render_link_sheet(el, None);
                        self.queue_sheet_event(el, Some(false));
                    }
                }
                // (when the link left the set earlier, by being removed or disabled, the parser
                // may be waiting on nothing else)
                if self.doc_load(doc, |d| d.script_blocking_sheets.is_empty())
                    && !self.script_running()
                {
                    self.run_parser(cx, doc, false);
                }
            }
            Purpose::ModulePreload(el, json) => {
                // A module script needs a JavaScript (or JSON) MIME type.
                let ok = r.as_ref().is_ok_and(|(_, mime, _)| {
                    if json {
                        mime == "application/json" || mime == "text/json" || mime.ends_with("+json")
                    } else {
                        snowlark_net::mime::is_javascript_essence(mime)
                    }
                });
                self.queue_element_task_fire(el, if ok { "load" } else { "error" }, false, false);
            }
            Purpose::Preload(el) => {
                // HTML "fetch and process the linked resource" for `preload`: `load` once the
                // response is in, `error` for a network error or an error status.
                let ok = r.is_ok();
                self.queue_element_task_fire(el, if ok { "load" } else { "error" }, false, false);
            }
            Purpose::SvgImage(el) => {
                // SVG 2 §15.3: `load` when the image arrived, `error` otherwise; only for what the
                // element still refers to.
                let ok = r.is_ok();
                if self.svg_image_fetched(el) {
                    self.queue_element_task_fire(
                        el,
                        if ok { "load" } else { "error" },
                        false,
                        false,
                    );
                }
            }
            Purpose::Document(_) => {}
        }
        self.advance_loading(cx);
    }

    fn script_encoding(&self, el: NodeId) -> &'static encoding_rs::Encoding {
        let dom = self.dom();
        dom.attr(el, &ns!(), &local_name!("charset"))
            .and_then(|c| encoding_rs::Encoding::for_label(c.as_bytes()))
            .unwrap_or_else(|| dom.encoding(dom.node_document(el)))
    }

    // ----- the document -------------------------------------------------------------------------------

    /// A navigation of `url` failed: the context shows an `about:error` document saying why (see
    /// [`crate::error_page`]).
    pub(crate) fn show_error_page(
        &self,
        cx: &mut Cx,
        id: ContextId,
        url: &url::Url,
        failure: crate::error_page::Failure,
    ) {
        let html = crate::error_page::html(url, &failure);
        let about = url::Url::parse("about:error").expect("about:error");
        // (an error page's origin is a new opaque one, whoever started the navigation)
        let opaque = self.new_opaque_origin(about.clone());
        self.frames.borrow_mut().list[id].initiator_origin = Some(opaque);
        let doc = self.begin_document(cx, id, &about, "text/html", None);
        self.error_pages.borrow_mut().insert(doc, url.clone());
        self.feed_document_text(cx, doc, &html);
    }

    /// The URL whose navigation failed, for an error page document.
    pub fn error_page_url(&self, doc: NodeId) -> Option<url::Url> {
        self.error_pages.borrow().get(&doc).cloned()
    }

    /// Navigates the top-level browsing context to `url`: the document is created when the response
    /// arrives.
    /// The new document starts a session history of its own: the embedder's navigations (a test
    /// runner's next test) leave no entries for the page's script to traverse to.
    pub fn navigate(&self, url: url::Url) {
        self.loader().stats.navigation_start = Some(std::time::Instant::now());
        self.navigate_request(
            crate::engine::frames::TOP,
            crate::engine::frames::NavigationRequest {
                url,
                post: None,
                source: None,
                form: false,
                reload: false,
                referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                history: crate::engine::frames::HistoryHandling::New,
            },
        );
    }

    /// HTML "reload" of the top-level browsing context, whose document is at `url`.
    pub fn reload(&self, url: url::Url) {
        self.loader().stats.navigation_start = Some(std::time::Instant::now());
        self.navigate_request(
            crate::engine::frames::TOP,
            crate::engine::frames::NavigationRequest {
                url,
                post: None,
                source: None,
                form: false,
                reload: true,
                referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                history: Default::default(),
            },
        );
    }

    /// M5: hands a top-level navigation to the browser ([`crate::embedder::BeginNavigation`]):
    /// the request's parameters as the renderer knows them, and what it keeps for the commit (the
    /// initiator document and the origin an `about:` document takes from it).
    pub(crate) fn begin_in_browser(&self, request: crate::engine::frames::NavigationRequest) {
        let nav = NavInfo {
            source: request.source,
            form: request.form,
            reload: request.reload,
            referrer_policy: request.referrer_policy,
        };
        let (r, source) =
            self.navigation_request(crate::engine::frames::TOP, request.url.clone(), nav);
        let initiator = r.client.as_deref().map(|c| {
            let mut i = snowlark_net::navigation::NavInitiator::from_client(c);
            // (the navigation's own referrer policy, when it has one, rules its referrer)
            if r.referrer_policy != snowlark_net::ReferrerPolicy::Empty {
                i.referrer_policy = r.referrer_policy;
            }
            i
        });
        let user_activation = match source {
            Some(d) => self.has_transient_activation(d),
            None => true,
        };
        let params = snowlark_net::navigation::NavigationParams {
            url: request.url,
            post: request.post,
            initiator,
            form_submission: request.form,
            reload: request.reload,
            user_activation,
        };
        let begun = crate::embedder::Begun {
            source,
            initiator: source.map(|d| self.origin_taken_from(d, false)),
        };
        self.note_begun(params, begun);
    }

    /// M5: commits a top-level navigation the browser began ([`CommitParams`]): the response it
    /// fetched is parsed into a new document (its head is in the parameters, the rest of its body
    /// in `stream`), an error page shows for one that failed, and a `Direct` one the engine makes
    /// itself (an `about:` document). Chromium: `RenderFrameImpl::CommitNavigation` and
    /// `CommitFailedNavigation`.
    pub fn commit_navigation(
        &self,
        cx: &mut Cx,
        params: CommitParams,
        stream: Option<ResponseStream>,
    ) {
        let id = crate::engine::frames::TOP;
        self.loader().stats.navigation_start =
            Some(params.started.unwrap_or_else(std::time::Instant::now));
        let begun = params.begun.and_then(|seq| self.take_begun(seq));
        let source = begun
            .as_ref()
            .and_then(|b| b.source)
            .filter(|d| self.dom().is_alive(*d));
        // The request's CSP violations (the browser's fetch met them) are the initiator's to
        // report, as they are when the page fetches.
        if let Some(doc) = source {
            for v in &params.violations {
                self.report_violation(cx, doc, None, v, None);
            }
        }
        // A navigation replaces any earlier one in flight.
        self.cancel_document_fetches(id);
        match params.response {
            CommitResponse::Direct => {
                // (`fetch_resource_with` takes the initiator from the source document itself)
                self.frames.borrow_mut().list[id].initiator_origin =
                    begun.and_then(|b| b.initiator);
                self.note_frame_sandbox(id);
                self.fetch_resource_with(
                    params.url,
                    Purpose::Document(id),
                    None,
                    NavInfo {
                        source,
                        form: false,
                        reload: params.reload,
                        referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                    },
                );
            }
            CommitResponse::Failed(e) => {
                self.frames.borrow_mut().list[id].initiator_origin = None;
                self.show_error_page(cx, id, &params.url, crate::error_page::Failure::Network(e));
            }
            CommitResponse::Fetched { head } => {
                self.frames.borrow_mut().list[id].initiator_origin =
                    begun.and_then(|b| b.initiator);
                self.note_frame_sandbox(id);
                let mut job = FetchJob::new(Purpose::Document(id), stream, params.url);
                job.head = Some(*head);
                job.head_pending = true;
                self.loader().jobs.push(job);
                self.waker().wake();
            }
        }
    }

    /// Creates the document a response will be parsed into and makes it the browsing context's active
    /// document.
    pub fn begin_document(
        &self,
        cx: &mut Cx,
        id: ContextId,
        url: &url::Url,
        mime: &str,
        charset: Option<String>,
    ) -> NodeId {
        // HTML §7.4.2.3: with a PDF viewer, a PDF type is "inline content that doesn't have a DOM".
        let kind = if self.pdf_viewer_supported() && snowlark_render::source::is_pdf_type(mime) {
            BodyKind::Pdf
        } else {
            body_kind(mime)
        };
        let doc = {
            let mut dom = self.dom_mut();
            let doc = dom.create_document(if kind == BodyKind::Xml {
                DocumentKind::Xml
            } else {
                DocumentKind::Html
            });
            dom.set_document_url(doc, url.as_str());
            dom.set_content_type(doc, if mime.is_empty() { "text/html" } else { mime });
            dom.set_allow_declarative_shadow_roots(doc, kind == BodyKind::Html);
            doc
        };
        self.set_context_document(cx, id, doc);
        let now = self.now();
        self.doc_state(doc, |s| {
            s.ready_state = DocumentReadyState::Loading;
            s.has_browsing_context = true;
            s.last_modified = crate::engine::window::last_modified_now();
            s.load_timing.dom_loading = now;
        });
        let options = snowlark_dom::ParseOptions {
            iframe_srcdoc: url.as_str() == "about:srcdoc",
            ..Default::default()
        };
        let parser =
            (kind != BodyKind::Xml).then(|| HtmlParser::new(self.dom.clone(), doc, options));
        self.doc_load(doc, |d| {
            d.parser = Some(ParserState {
                parser,
                xml: None,
                kind,
                buffered: Vec::new(),
                nesting: 0,
                paused: false,
                insertion_point: false,
                script_created: false,
                eof: false,
                encoding_decided: false,
                pending_bytes: Vec::new(),
                transport_charset: charset,
            });
            d.end = None;
        });
        doc
    }

    fn document_response(&self, cx: &mut Cx, id: ContextId, head: &ResponseHead) -> NodeId {
        if id == crate::engine::frames::TOP {
            let navigation_start = {
                let mut l = self.loader();
                l.stats.response_ms = l.stats.since_navigation();
                l.stats.navigation_start
            };
            // HR Time / Navigation Timing: the new document's time origin is its navigation's
            // start (the embedder's navigate(), else the start of the fetch that brought it).
            let origin = self.event_loop.borrow().origin;
            let start = match (navigation_start, head.timing.start) {
                (Some(n), Some(f)) if n >= origin && n <= f => Some(n),
                (_, Some(f)) => Some(f),
                (n, None) => n.filter(|n| *n >= origin),
            };
            if let Some(start) = start {
                self.rebase_time_origin(start);
            }
        }
        let url = head
            .url()
            .cloned()
            .unwrap_or_else(|| url::Url::parse("about:blank").unwrap());
        let mime = snowlark_net::mime::extract_mime_type(&head.headers);
        let essence = mime
            .as_ref()
            .map(|m| m.essence())
            .unwrap_or_else(|| "text/html".into());
        let charset = mime
            .as_ref()
            .and_then(|m| m.parameter("charset").map(str::to_owned));
        let opener_policy = self.enforce_opener_policy(id, &url, head);
        let doc = self.begin_document(cx, id, &url, &essence, charset);
        if let Some(policy) = opener_policy {
            self.doc_state(doc, |s| s.opener_policy = policy);
        }
        // HTML "create and initialize a Document object": the document's referrer is the
        // serialization of its navigation request's referrer, if that is a URL.
        if let Some(referrer) = &head.referrer {
            self.doc_state(doc, |s| s.referrer = referrer.to_string());
        }
        // The policy container: the response's CSP list (which the early fetches, preloads, are
        // checked against too), or for a local scheme its creator's (a frame's parent).
        let creator = self.parent_document(doc);
        self.document_response_policy(cx, doc, &url, head, creator);
        self.process_link_headers(doc, head);
        self.create_navigation_timing_entry(doc, head);
        // HTML "create and initialize a Document object": a `Refresh` header runs the shared
        // declarative refresh steps with its value, isomorphic decoded.
        if let Some(value) = head.headers.get("refresh") {
            self.shared_declarative_refresh(doc, &value, false);
        }
        doc
    }

    /// HTML "create navigation params by fetching" for a top-level traversable: "obtain an opener
    /// policy" of the response (its reserved environment is a secure context when the response URL is
    /// potentially trustworthy) and "enforce a response's opener policy" against the active
    /// document's: a response that needs a browsing context group switch marks the context, and the
    /// document it brings then gets a browsing context of a new group (`set_context_document`).
    /// `None` for a child navigable's response, which has no opener policy of its own.
    fn enforce_opener_policy(
        &self,
        id: ContextId,
        url: &url::Url,
        head: &ResponseHead,
    ) -> Option<snowlark_net::coop::OpenerPolicy> {
        if !self.is_top_level_context(id) {
            return None;
        }
        let (active, initial, popup_sandbox) = {
            let f = self.frames.borrow();
            let b = &f.list[id];
            (b.document, b.initial_about_blank, b.popup_sandbox)
        };
        // (the COOP enforcement result starts with the active document's policy and origin; each
        // response, redirects first, is checked against the one before it, and a switch any of them
        // needs stays needed)
        let mut current = (
            self.origin_of_document(active),
            self.doc_state(active, |s| s.opener_policy.value),
        );
        let mut switch = false;
        let mut last = None;
        let responses = head
            .redirects
            .iter()
            .map(|hop| (&hop.url, &hop.headers))
            .chain(std::iter::once((url, &head.headers)));
        for (url, headers) in responses {
            let secure = snowlark_net::origin::is_url_potentially_trustworthy(url);
            let policy = snowlark_net::coop::obtain(headers, secure);
            // ("determining the origin" of the response: an opaque one under a sandbox without
            // `allow-same-origin`, unique, so it matches no other)
            let same_origin_allowed = popup_sandbox.is_none_or(|s| s.same_origin)
                && !response_csp_sandbox(headers).is_some_and(|tokens| {
                    !tokens
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case("allow-same-origin"))
                });
            let origin = match url.origin() {
                o @ url::Origin::Tuple(..) if same_origin_allowed => o.ascii_serialization(),
                _ => self.new_opaque_origin(url.clone()).origin,
            };
            switch |= snowlark_net::coop::values_require_switch(
                initial,
                &origin,
                &current.0,
                policy.value,
                current.1,
            );
            current = (origin, policy.value);
            last = Some(policy);
        }
        if switch {
            self.frames.borrow_mut().list[id].group_switch = true;
        }
        last
    }

    /// HTML "create navigation params by fetching": a top-level traversable's response (a redirect
    /// included) whose opener policy is not `unsafe-none` is a network error when the navigation's
    /// sandboxing flags (the context's popup sandboxing flag set) are not empty. (The response's own
    /// CSP `sandbox` only makes its origin opaque, which no policy then matches.)
    fn opener_policy_refuses(&self, id: ContextId, head: &ResponseHead) -> bool {
        if !self.is_top_level_context(id) || self.frames.borrow().list[id].popup_sandbox.is_none() {
            return false;
        }
        let Some(url) = head.url() else {
            return false;
        };
        head.redirects
            .iter()
            .map(|hop| (&hop.url, &hop.headers))
            .chain(std::iter::once((url, &head.headers)))
            .any(|(url, headers)| {
                let secure = snowlark_net::origin::is_url_potentially_trustworthy(url);
                snowlark_net::coop::obtain(headers, secure).value
                    != snowlark_net::coop::OpenerPolicyValue::UnsafeNone
            })
    }
    /// The document a navigation of `id` that a request policy blocked leaves: empty, with an
    /// opaque origin, no scripts, and the container's load event.
    fn show_blocked_frame(&self, cx: &mut Cx, id: ContextId) {
        let blank = url::Url::parse("about:blank").expect("about:blank");
        let doc = self.begin_document(cx, id, &blank, "text/html", None);
        self.mark_blocked_document(cx, doc);
        self.document_bytes(cx, doc, &[], true);
    }

    /// Bytes of a document's response; `last` at the end of the body.
    fn document_bytes(&self, cx: &mut Cx, doc: NodeId, bytes: &[u8], last: bool) {
        self.navigation_body(doc, bytes.len(), last);
        // A same-origin parent document's encoding (HTML §13.2.3.2 step 5).
        let parent_encoding = self.parent_document(doc).and_then(|p| {
            let enc = self.dom().encoding(p);
            let same = self.origin_of_document(p) == self.origin_of_document(doc);
            (same && enc != encoding_rs::UTF_16BE && enc != encoding_rs::UTF_16LE).then_some(enc)
        });
        enum Got {
            NoParser,
            Fed,
            Complete(BodyKind, Vec<u8>, Option<String>),
        }
        let got = self.parser_of(doc, |p| {
            let Some(p) = p else {
                return Got::NoParser;
            };
            if p.kind != BodyKind::Html {
                // XML, text and media documents wait for the whole body.
                p.buffered.extend_from_slice(bytes);
                if last && !p.eof {
                    p.eof = true;
                    p.encoding_decided = true;
                    let body = std::mem::take(&mut p.buffered);
                    return Got::Complete(p.kind, body, p.transport_charset.clone());
                }
                return Got::Fed;
            }
            if !p.encoding_decided {
                p.pending_bytes.extend_from_slice(bytes);
                if p.pending_bytes.len() >= 1024 || last {
                    let mut input = snowlark_net::encoding::SniffInput::new(&p.pending_bytes);
                    input.transport_charset = p.transport_charset.as_deref();
                    input.parent = parent_encoding;
                    let (enc, _) = snowlark_net::encoding::sniff_html_encoding(&input);
                    let pending = std::mem::take(&mut p.pending_bytes);
                    p.encoding_decided = true;
                    if let Some(parser) = p.parser.as_mut() {
                        parser.set_encoding(enc);
                        parser.push_bytes(&pending, last);
                    }
                }
            } else if let Some(parser) = p.parser.as_mut() {
                parser.push_bytes(bytes, last);
            }
            if last {
                p.eof = true;
            }
            Got::Fed
        });
        match got {
            Got::NoParser => return,
            Got::Fed => {}
            Got::Complete(kind, body, charset) => self.complete_body(doc, kind, &body, charset),
        }
        self.run_parser(cx, doc, false);
        self.advance_loading(cx);
    }

    /// The whole body of an XML, text or media document arrived: give its parser the input.
    fn complete_body(&self, doc: NodeId, kind: BodyKind, body: &[u8], charset: Option<String>) {
        let label_encoding = |label: Option<&str>| {
            label.and_then(|l| encoding_rs::Encoding::for_label(l.trim().as_bytes()))
        };
        match kind {
            BodyKind::Html => {}
            BodyKind::Xml => {
                // The encoding: a BOM, the transport layer's charset, the XML declaration, UTF-8.
                let enc = encoding_rs::Encoding::for_bom(body)
                    .map(|(e, _)| e)
                    .or_else(|| label_encoding(charset.as_deref()))
                    .or_else(|| label_encoding(xml_declared_encoding(body).as_deref()))
                    .unwrap_or(encoding_rs::UTF_8);
                let (text, enc, _) = enc.decode(body);
                self.dom_mut().set_encoding(doc, enc);
                let parser = snowlark_dom::XmlParser::new(&text, doc);
                self.parser_of(doc, |p| {
                    if let Some(p) = p {
                        p.xml = Some(parser);
                    }
                });
            }
            BodyKind::Pdf => {
                let markup = self.pdf_viewer_markup(doc, body);
                self.parser_of(doc, |p| {
                    if let Some(parser) = p.and_then(|p| p.parser.as_mut()) {
                        parser.push_str(&markup);
                    }
                });
            }
            BodyKind::Plain | BodyKind::Media => {
                let markup = if kind == BodyKind::Plain {
                    let enc = encoding_rs::Encoding::for_bom(body)
                        .map(|(e, _)| e)
                        .or_else(|| label_encoding(charset.as_deref()))
                        .unwrap_or(encoding_rs::UTF_8);
                    let (text, enc, _) = enc.decode(body);
                    self.dom_mut().set_encoding(doc, enc);
                    let mut m = String::from("<pre>");
                    snowlark_dom::serialize::escape(&mut m, &text, false);
                    m
                } else {
                    let (url, mime) = {
                        let dom = self.dom();
                        (
                            dom.document_url(doc).to_owned(),
                            dom.content_type(doc).to_owned(),
                        )
                    };
                    let mut src = String::new();
                    snowlark_dom::serialize::escape(&mut src, &url, true);
                    // HTML §4.8.7: an `object` or `embed` whose resource is an image represents the
                    // image; its navigable, made on the guess that it was a document, goes.
                    if mime.starts_with("image/") {
                        if let Some(el) = self.rendered_container(doc).filter(|&el| {
                            let dom = self.dom();
                            dom.is_html_element_named(el, &local_name!("object"))
                                || dom.is_html_element_named(el, &local_name!("embed"))
                        }) {
                            let essence = mime.clone();
                            self.html.borrow_mut().embed_types.insert(el, mime.clone());
                            self.with_node_renderer(el, |r, st| {
                                r.set_embed_type(el, &essence);
                                st.dirty = true;
                            });
                            self.queue_task(
                                crate::event_loop::TaskSource::DomManipulation,
                                move |e, _| e.frame_removed(el),
                            );
                        }
                    }
                    if mime.starts_with("image/") {
                        format!("<body style=\"margin: 0px;\"><img src=\"{src}\">")
                    } else {
                        format!("<body><video controls autoplay name=media><source src=\"{src}\" type=\"{mime}\"></video>")
                    }
                };
                self.parser_of(doc, |p| {
                    if let Some(parser) = p.and_then(|p| p.parser.as_mut()) {
                        parser.push_str(&markup);
                    }
                });
            }
        }
    }

    /// HTML §7.4.6 "page load processing model for inline content that doesn't have a DOM", the PDF
    /// viewer's way: the document is mutated "until it represents the content", a column of the
    /// file's pages as images on the viewer's dark background (each page a PNG the PDF renderer
    /// made, at [`PDF_SCALE`] device pixels per CSS px, under a `blob:` URL of the document's
    /// origin), titled with the file's name. A file the renderer cannot open shows why.
    fn pdf_viewer_markup(&self, doc: NodeId, body: &[u8]) -> String {
        let url = self.dom().document_url(doc).to_owned();
        let name = url::Url::parse(&url)
            .ok()
            .and_then(|u| {
                u.path_segments()
                    .and_then(|mut s| s.next_back().map(str::to_owned))
                    .filter(|s| !s.is_empty())
            })
            .map(|s| percent_decoded(&s))
            .unwrap_or_else(|| url.clone());
        let mut m = String::from("<title>");
        snowlark_dom::serialize::escape(&mut m, &name, false);
        m.push_str(
            "</title><body style=\"margin: 0px; background: rgb(82, 86, 89); min-height: 100vh;\">",
        );
        match snowlark_paint::pdf::render(body, PDF_SCALE, PDF_MAX_PAGES) {
            Ok(pdf) => {
                let origin = self.net_origin_of_document(doc);
                let net = self.hooks.borrow().net.clone();
                m.push_str("<div style=\"display: flex; flex-direction: column; align-items: center; gap: 8px; padding: 8px 0px;\">");
                for (i, page) in pdf.pages.into_iter().enumerate() {
                    let src = match &net {
                        Some(net) => net.register_blob(
                            &origin,
                            snowlark_net::blob::BlobData::new(page.png.into(), "image/png"),
                        ),
                        None => {
                            use base64::Engine as _;
                            format!(
                                "data:image/png;base64,{}",
                                base64::engine::general_purpose::STANDARD.encode(&page.png)
                            )
                        }
                    };
                    m.push_str(&format!(
                        "<img src=\"{src}\" alt=\"Page {}\" width=\"{}\" height=\"{}\" style=\"display: block; max-width: calc(100% - 16px); height: auto; background: white; box-shadow: rgba(0, 0, 0, 0.4) 0px 1px 4px;\">",
                        i + 1,
                        page.width.round(),
                        page.height.round()
                    ));
                }
                if pdf.page_count > PDF_MAX_PAGES {
                    m.push_str(&format!(
                        "<p style=\"color: white; font: 14px sans-serif;\">The first {PDF_MAX_PAGES} of {} pages.</p>",
                        pdf.page_count
                    ));
                }
                m.push_str("</div>");
            }
            Err(why) => {
                m.push_str("<p style=\"margin: 0px; padding: 24px; color: white; font: 16px sans-serif;\">This PDF cannot be shown (");
                snowlark_dom::serialize::escape(&mut m, &why, false);
                m.push_str(").</p>");
            }
        }
        m
    }

    /// Feeds a whole document as text (`--eval`, `--html`, `srcdoc`, tests) and runs its parser.
    pub(crate) fn feed_document_text(&self, cx: &mut Cx, doc: NodeId, text: &str) {
        self.parser_of(doc, |p| {
            if let Some(p) = p {
                p.encoding_decided = true;
                if let Some(parser) = p.parser.as_mut() {
                    parser.push_str(text);
                }
                p.eof = true;
            }
        });
        self.dom_mut().set_encoding(doc, encoding_rs::UTF_8);
        self.run_parser(cx, doc, false);
        self.advance_loading(cx);
    }

    /// Parses a document from a string into the top-level browsing context, with scripts.
    pub fn load_html(&self, cx: &mut Cx, html: &str, url: &url::Url) -> NodeId {
        let doc = self.begin_document(cx, crate::engine::frames::TOP, url, "text/html", None);
        self.feed_document_text(cx, doc, html);
        doc
    }

    // ----- the parser ---------------------------------------------------------------------------------

    /// Runs `doc`'s tokenizer until it needs input, a script blocks it, or (nested in `document.write`)
    /// a parsing-blocking script appears.
    pub(crate) fn run_parser(&self, cx: &mut Cx, doc: NodeId, nested: bool) {
        loop {
            // The pending parsing-blocking script (HTML §13.2.6.4.8 step 9.2).
            let blocking = self.doc_load(doc, |d| d.pending_parsing_blocking);
            if let Some(script) = blocking {
                if nested {
                    return;
                }
                let ready = self.script(script, |s| s.ready_to_be_parser_executed)
                    && self.doc_load(doc, |d| d.script_blocking_sheets.is_empty());
                if !ready {
                    // The parser waits: fetch ahead what the input after it holds.
                    self.speculate(doc);
                    return;
                }
                self.stop_speculation(doc);
                self.doc_load(doc, |d| d.pending_parsing_blocking = None);
                // 9.2.7-9.2.9: the insertion point just before the next input character while the
                // script runs, then undefined again.
                self.parser_of(doc, |p| {
                    if let Some(p) = p {
                        p.insertion_point = true;
                        p.nesting += 1;
                        if let Some(parser) = p.parser.as_mut() {
                            parser.set_insertion_point();
                        }
                    }
                });
                self.execute_script(cx, script);
                self.parser_of(doc, |p| {
                    if let Some(p) = p {
                        p.nesting -= 1;
                        if p.nesting == 0 {
                            p.paused = false;
                        }
                        p.insertion_point = false;
                        if let Some(parser) = p.parser.as_mut() {
                            parser.restore_insertion_point();
                        }
                    }
                });
                continue;
            }
            let step = {
                let mut l = self.loader();
                let Some(p) = l.docs.get_mut(&doc).and_then(|d| d.parser.as_mut()) else {
                    return;
                };
                if p.paused && nested {
                    return;
                }
                if let Some(xml) = p.xml.as_mut() {
                    rt_host_push(self);
                    let s = xml.run(&mut self.dom_mut());
                    match s {
                        snowlark_dom::XmlStep::Script(el) => ParseStep::Script(el),
                        snowlark_dom::XmlStep::Done => ParseStep::NeedInput,
                        snowlark_dom::XmlStep::Error(m) => {
                            // A fatal error: the document becomes the parsererror, parsing stops.
                            p.xml = None;
                            drop(l);
                            rt_host_pop(self, cx);
                            self.dom_mut().xml_parse_error(doc, &m);
                            self.process_reactions(cx);
                            rt_host_push(self);
                            ParseStep::NeedInput
                        }
                    }
                } else {
                    let Some(parser) = p.parser.as_mut() else {
                        if p.kind == BodyKind::Xml && p.eof {
                            drop(l);
                            self.maybe_stop_parsing(cx, doc);
                        }
                        return;
                    };
                    rt_host_push(self);
                    parser.run()
                }
            };
            rt_host_pop(self, cx);
            match step {
                ParseStep::NeedInput => {
                    if !nested {
                        self.maybe_stop_parsing(cx, doc);
                    }
                    return;
                }
                ParseStep::Script(el) => {
                    self.parser_script_end_tag(cx, doc, el);
                    let paused = self.parser_of(doc, |p| p.is_some_and(|p| p.paused));
                    if nested && paused {
                        return;
                    }
                }
                ParseStep::EncodingIndicator(_) => {}
            }
        }
    }

    /// HTML §13.2.6.4.8, "an end tag whose tag name is script" in the "text" insertion mode.
    fn parser_script_end_tag(&self, cx: &mut Cx, doc: NodeId, el: NodeId) {
        if !self.dom().is_script_element(el) {
            return;
        }
        // 1.
        if !self.script_running() {
            self.microtask_checkpoint(cx);
        }
        // 4-5.
        let mut start_line = None;
        let old_ip = self.parser_of(doc, |p| {
            p.map(|p| {
                let old = p.insertion_point;
                p.insertion_point = true;
                p.nesting += 1;
                if let Some(parser) = p.parser.as_mut() {
                    start_line = parser.take_script_line(el);
                    parser.set_insertion_point();
                }
                old
            })
        });
        let Some(old_ip) = old_ip else { return };
        // The element's parser document is this parser's Document, whichever document the element is in
        // now: a script inserted into an element that script moved to another document keeps it, and
        // "prepare the script element" 16 then returns (moving-between-documents).
        self.script(el, |s| {
            if let Some(line) = start_line {
                s.start_line = line;
            }
            if s.parser_document.is_some() {
                s.parser_document = Some(doc);
            }
        });
        // 6.
        self.prepare_script(cx, el);
        // 7-8.
        let nesting = self.parser_of(doc, |p| {
            p.map(|p| {
                p.nesting -= 1;
                if p.nesting == 0 {
                    p.paused = false;
                }
                p.insertion_point = old_ip;
                if let Some(parser) = p.parser.as_mut() {
                    parser.restore_insertion_point();
                }
                p.nesting
            })
        });
        let Some(nesting) = nesting else { return };
        // 9.1: inside document.write: yield to the outer invocation.
        if self.doc_load(doc, |d| d.pending_parsing_blocking.is_some()) && nesting > 0 {
            self.parser_of(doc, |p| {
                if let Some(p) = p {
                    p.paused = true;
                }
            });
        }
    }

    /// The parser reached the end of its input: "stop parsing" (HTML §13.2.7 "the end", steps 1-4).
    fn maybe_stop_parsing(&self, cx: &mut Cx, doc: NodeId) {
        let finish = {
            let l = self.loader.borrow();
            let Some(d) = l.docs.get(&doc) else { return };
            let Some(p) = d.parser.as_ref() else { return };
            p.eof && d.pending_parsing_blocking.is_none() && p.nesting == 0
        };
        if !finish {
            return;
        }
        let state = self.doc_load(doc, |d| d.parser.take());
        let Some(state) = state else { return };
        if let Some(parser) = state.parser {
            rt_host_push(self);
            parser.finish();
            rt_host_pop(self, cx);
        }
        self.set_ready_state(cx, doc, DocumentReadyState::Interactive);
        self.doc_load(doc, |d| d.end = Some(EndStage::Defer));
        self.advance_loading(cx);
    }

    /// Advances "the end" of every loading document.
    pub(crate) fn advance_loading(&self, cx: &mut Cx) {
        let docs: Vec<NodeId> = {
            let l = self.loader.borrow();
            l.docs
                .iter()
                .filter(|(_, d)| matches!(d.end, Some(EndStage::Defer | EndStage::WaitAsap)))
                .map(|(&doc, _)| doc)
                .collect()
        };
        for doc in docs {
            self.advance_document(cx, doc);
        }
    }

    /// Whether a child browsing context of `doc` still loads (its iframe delays `doc`'s load event).
    /// HTML: an `<img>` whose request is in progress delays its document's load event. The images
    /// are found by a rendering update, so the document waits for one (asked for here), then for
    /// the images it requested; never more than [`IMAGES_LOAD_DELAY_MAX`].
    fn images_delay_load(&self, doc: NodeId) -> bool {
        let Some(updates) = self.with_doc_renderer(doc, |r, _| r.rendering_updates()) else {
            return false;
        };
        let since = match self.doc_load(doc, |d| d.images_wait) {
            Some(s) => s,
            None => {
                let any = {
                    let dom = self.dom();
                    dom.descendants(doc)
                        .any(|n| dom.is_html_element_named(n, &local_name!("img")))
                };
                let s = (updates, std::time::Instant::now(), any);
                self.doc_load(doc, |d| d.images_wait = Some(s));
                s
            }
        };
        // The images the document's styles use delay its load event too, as in Chrome (whose
        // image resources fetched for style block the load event through the document's
        // fetcher): they are requested when styles are computed, so style is brought up to
        // date first (the parser is done here; this runs while nothing else delays the load).
        // The web fonts in fetch do too (Chrome's are in the same fetcher).
        let style_images = since.1.elapsed() < IMAGES_LOAD_DELAY_MAX
            && self
                .with_doc_renderer(doc, |r, _| {
                    if r.needs_restyle() {
                        r.restyle();
                    }
                    r.any_image_pending() || r.fonts_loading() > 0
                })
                .unwrap_or(false);
        if style_images {
            return true;
        }
        if !since.2 {
            return false;
        }
        if since.0 == updates {
            if since.1.elapsed() > IMAGES_LOAD_DELAY_MAX {
                return false;
            }
            self.with_doc_renderer(doc, |_, st| st.dirty = true);
            return true;
        }
        since.1.elapsed() < IMAGES_LOAD_DELAY_MAX
            && self
                .with_doc_renderer(doc, |r, _| r.images_pending())
                .unwrap_or(false)
    }

    fn child_contexts_loading(&self, doc: NodeId) -> bool {
        let Some(id) = self.context_of_document(doc) else {
            return false;
        };
        let f = self.frames.borrow();
        f.list.iter().any(|b| {
            b.parent == Some(id) && !b.discarded && b.pending_load_event && !b.lazily_loaded
        })
    }

    /// "The end" for one document: deferred scripts, `DOMContentLoaded`, then `load` once nothing
    /// delays it.
    fn advance_document(&self, cx: &mut Cx, doc: NodeId) {
        loop {
            let stage = self.doc_load(doc, |d| d.end);
            match stage {
                Some(EndStage::Defer) => {
                    let first = self.doc_load(doc, |d| d.defer_scripts.front().copied());
                    match first {
                        Some(s) => {
                            // HTML "the end" step 3: until the script is ready to be
                            // parser-executed and the Document has no style sheet that is
                            // blocking scripts.
                            let ready = self.script(s, |st| st.ready_to_be_parser_executed)
                                && self.doc_load(doc, |d| d.script_blocking_sheets.is_empty());
                            if !ready {
                                return;
                            }
                            self.doc_load(doc, |d| d.defer_scripts.pop_front());
                            self.execute_script(cx, s);
                        }
                        None => {
                            self.doc_load(doc, |d| d.end = Some(EndStage::WaitAsap));
                            self.queue_document_task(
                                TaskSource::DomManipulation,
                                doc,
                                move |e, cx| {
                                    let start = e.now();
                                    e.doc_state(doc, |s| {
                                        s.load_timing.dom_content_loaded_event_start = start
                                    });
                                    if e.document() == Some(doc) {
                                        let mut l = e.loader();
                                        l.stats.dom_content_loaded_ms = l.stats.since_navigation();
                                    }
                                    // HTML §7.4.6.4 "try to scroll to the fragment" for the new
                                    // document, once its parser has stopped.
                                    let fragment = url::Url::parse(e.dom().document_url(doc))
                                        .ok()
                                        .and_then(|u| u.fragment().map(str::to_owned));
                                    if let Some(fragment) = fragment {
                                        e.scroll_to_fragment(doc, &fragment);
                                    }
                                    e.fire_event(
                                        cx,
                                        node_handle(doc),
                                        "DOMContentLoaded",
                                        true,
                                        false,
                                    );
                                    let end = e.now();
                                    e.doc_state(doc, |s| {
                                        s.load_timing.dom_content_loaded_event_end = end
                                    });
                                },
                            );
                        }
                    }
                }
                Some(EndStage::WaitAsap) => {
                    let waiting = {
                        let l = self.loader.borrow();
                        let d = l.docs.get(&doc);
                        d.is_some_and(|d| {
                            !d.asap_scripts.is_empty()
                                || !d.in_order_scripts.is_empty()
                                || d.load_delayers > 0
                        }) || l
                            .scripts
                            .values()
                            .any(|s| s.delaying_load && s.preparation_document == Some(doc))
                    };
                    // (a `link` or `style` sheet whose critical subresources are still being
                    // obtained delays its document's load event: HTML §4.2.4, §4.2.6)
                    let sheets_waiting = self
                        .with_doc_renderer(doc, |r, _| r.sheets_pending())
                        .unwrap_or(false);
                    if waiting
                        || sheets_waiting
                        || self.child_contexts_loading(doc)
                        || self.images_delay_load(doc)
                    {
                        return;
                    }
                    self.doc_load(doc, |d| d.end = Some(EndStage::Done));
                    self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                        if e.document_destroyed(doc) {
                            // (a parent document may have waited for this one all the same)
                            e.advance_loading(cx);
                            return;
                        }
                        e.set_ready_state(cx, doc, DocumentReadyState::Complete);
                        if let Some(w) = e.window_of_document(doc) {
                            let ev = e.trusted_event(
                                snowlark_bindings::InterfaceId::Event,
                                "load",
                                false,
                                false,
                                crate::engine::events::EventExt::None,
                            );
                            let start = e.now();
                            e.doc_state(doc, |s| s.load_timing.load_event_start = start);
                            if e.document() == Some(doc) {
                                let mut l = e.loader();
                                l.stats.load_ms = l.stats.since_navigation();
                                drop(l);
                                // Code caches of the scripts that ran are made after `load`.
                                e.queue_code_caches();
                            }
                            e.dispatch(cx, ev, w, true);
                            e.smil_document_loaded(doc);
                            let end = e.now();
                            e.doc_state(doc, |s| s.load_timing.load_event_end = end);
                            // (completely loaded: a declarative refresh's delay starts)
                            e.start_declarative_refresh(doc);
                            // HTML "the end": queue the navigation timing entry.
                            e.queue_navigation_timing_entry(doc);
                            e.doc_state(doc, |s| s.page_showing = true);
                            let ev = e.trusted_event(
                                snowlark_bindings::InterfaceId::PageTransitionEvent,
                                "pageshow",
                                false,
                                false,
                                crate::engine::events::EventExt::PageTransition(false),
                            );
                            e.dispatch(cx, ev, w, true);
                        }
                        // "Completely finish loading": the container's load event.
                        if let Some(id) = e.context_of_document(doc) {
                            e.context_loaded(cx, id);
                        }
                        // The parent document may have waited for this one.
                        e.advance_loading(cx);
                    });
                    return;
                }
                _ => return,
            }
        }
    }

    /// The early fetches and connections so far (speculative parser, preloads).
    pub fn early_stats(&self) -> crate::speculative::EarlyStats {
        self.loader.borrow().early
    }

    /// What loading cost so far.
    pub fn load_stats(&self) -> LoadStats {
        self.loader.borrow().stats.clone()
    }

    /// One line on the top-level navigation: its response, `DOMContentLoaded` and `load` (ms since
    /// it started), then the rest of [`LoadStats`].
    pub fn load_summary(&self) -> String {
        let s = self.load_stats();
        let early = self.loader.borrow().early;
        let (started, consumed) = self
            .hooks
            .borrow()
            .net
            .as_ref()
            .map_or((0, 0), |n| n.preload_counts());
        format!(
            "[load] response {:.0} ms, DOMContentLoaded {:.0} ms, load {:.0} ms; scripts: {:.1} ms compiling and running, {} compiled on the page thread in {:.1} ms, {} streamed ({} KiB, {:.1} ms to finish), {} stream fallbacks; code cache: {} hits ({} KiB), {} rejected, {} stale, {} timestamps, {} made in {:.1} ms; early: {} speculative, {} from links, {} connects, {} preloads consumed of {}",
            s.response_ms,
            s.dom_content_loaded_ms,
            s.load_ms,
            s.script_ms,
            s.compiled,
            s.compile_ms,
            s.streamed,
            s.streamed_bytes / 1024,
            s.streamed_finish_ms,
            s.stream_fallbacks,
            s.cache_hits,
            s.cache_bytes / 1024,
            s.cache_rejected,
            s.cache_stale,
            s.cache_timestamps,
            s.cache_produced,
            s.cache_produce_ms,
            early.speculative,
            early.links,
            early.connects,
            consumed,
            started,
        )
    }

    /// Whether the top-level document has fired `load` (for the headless modes).
    pub fn load_complete(&self) -> bool {
        let Some(doc) = self.document() else {
            return false;
        };
        let l = self.loader.borrow();
        l.docs
            .get(&doc)
            .is_some_and(|d| d.end == Some(EndStage::Done) && d.parser.is_none())
    }

    /// HTML §3.1.10 "update the current document readiness".
    fn set_ready_state(&self, cx: &mut Cx, doc: NodeId, state: DocumentReadyState) {
        let now = self.now();
        let changed = self.doc_state(doc, |s| {
            let c = s.ready_state != state;
            s.ready_state = state;
            let t = &mut s.load_timing;
            match state {
                DocumentReadyState::Interactive if t.dom_interactive == 0.0 => {
                    t.dom_interactive = now
                }
                DocumentReadyState::Complete if t.dom_complete == 0.0 => t.dom_complete = now,
                _ => {}
            }
            c
        });
        if changed {
            self.fire_event(cx, node_handle(doc), "readystatechange", false, false);
        }
    }

    // ----- the script element (HTML §4.12.1.1) ----------------------------------------------------------

    /// The post-connection steps and children changed steps of `script`: prepare it when it has no parser
    /// document.
    pub(crate) fn script_post_connection(&self, cx: &mut Cx, el: NodeId) {
        if !self.dom().is_connected(el) {
            return;
        }
        if self.script(el, |s| s.parser_document.is_some()) {
            return;
        }
        self.prepare_script(cx, el);
    }

    /// "Prepare the script element".
    pub(crate) fn prepare_script(&self, cx: &mut Cx, el: NodeId) {
        // 1.
        if self.dom().element_flags(el) & element_flags::SCRIPT_ALREADY_STARTED != 0 {
            return;
        }
        // 2-4.
        let parser_document = self.script(el, |s| s.parser_document.take());
        let has_async = self.dom().attr(el, &ns!(), &local_name!("async")).is_some();
        if parser_document.is_some() && !has_async {
            self.script(el, |s| s.force_async = true);
        }
        // 5-7.
        let (source, has_src, connected, type_attr, language) = {
            let dom = self.dom();
            (
                dom.child_text_content(el),
                script_source(&dom, el).is_some(),
                dom.is_connected(el),
                dom.attr(el, &ns!(), &local_name!("type"))
                    .map(str::to_owned),
                dom.attr(el, &ns!(), &local_name!("language"))
                    .map(str::to_owned),
            )
        };
        if !has_src && source.is_empty() {
            return;
        }
        if !connected {
            return;
        }
        // 10: the script block's type string (the attribute's value as it is).
        let type_string = match (&type_attr, &language) {
            (Some(t), _) if t.is_empty() => "text/javascript".to_owned(),
            (None, Some(l)) if l.is_empty() => "text/javascript".to_owned(),
            (None, None) => "text/javascript".to_owned(),
            (Some(t), _) => t.clone(),
            (None, Some(l)) => format!("text/{l}"),
        };
        let is_js = |s: &str| snowlark_net::mime::is_javascript_essence(&s.to_ascii_lowercase());
        // 11: "classic" also when the type attribute with leading and trailing ASCII whitespace stripped
        // matches; 12-15 compare the type string unstripped (`type=" module"` runs nothing).
        let (module, import_map) = if is_js(&type_string)
            || type_attr.as_deref().is_some_and(|t| {
                is_js(t.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')))
            }) {
            (false, false)
        } else if type_string.eq_ignore_ascii_case("module") {
            (true, false)
        } else if type_string.eq_ignore_ascii_case("importmap") {
            (false, true)
        } else {
            return;
        };
        // 13.
        if let Some(pd) = parser_document {
            self.script(el, |s| {
                s.parser_document = Some(pd);
                s.force_async = false;
            });
        }
        // 14-15.
        self.dom_mut()
            .set_element_flag(el, element_flags::SCRIPT_ALREADY_STARTED, true);
        let doc = self.dom().node_document(el);
        // 23-29: the script fetch options.
        let options = self.element_module_options(el);
        self.script(el, |s| {
            s.module = module;
            s.preparation_document = Some(doc);
            s.options = options;
        });
        // 16.
        if parser_document.is_some_and(|pd| pd != doc) {
            return;
        }
        // 17: scripting is disabled for documents without a browsing context.
        if !self.is_active_document(doc) {
            return;
        }
        // 18.
        if !module
            && !import_map
            && self
                .dom()
                .attr(el, &ns!(), &local_name!("nomodule"))
                .is_some()
        {
            return;
        }
        // 17 (CSP sandbox) and 19: an inline script the document's Content Security Policy blocks.
        if self.policy.borrow().any
            && (self.scripts_sandboxed(doc)
                || (!has_src
                    && self.inline_blocked(cx, el, snowlark_net::csp::InlineType::Script, &source)))
        {
            return;
        }
        // 20: for/event.
        {
            let dom = self.dom();
            let event = dom.attr(el, &ns!(), &local_name!("event"));
            let for_ = dom.attr(el, &ns!(), &local_name!("for"));
            if let (Some(event), Some(for_), false) = (event, for_, module) {
                let f = for_.trim_matches(|c: char| c.is_ascii_whitespace());
                let e = event.trim_matches(|c: char| c.is_ascii_whitespace());
                if !f.eq_ignore_ascii_case("window")
                    || !(e.eq_ignore_ascii_case("onload") || e.eq_ignore_ascii_case("onload()"))
                {
                    return;
                }
            }
        }
        // 30: an external script.
        if has_src && import_map {
            // (external import maps are not supported)
            self.queue_element_task_fire(el, "error", false, false);
            return;
        }
        if import_map {
            // 31 "importmap": the import map parse result; 33: executed immediately.
            let base = self.base_url_of(el);
            self.in_document_realm(cx, doc, |cx| {
                let result = self.parse_import_map(cx, &source, &base);
                self.script(el, |s| s.result = ScriptResult::ImportMap(result));
            });
            self.execute_script(cx, el);
            return;
        }
        if has_src {
            let src = script_source(&self.dom(), el).unwrap_or("").to_owned();
            if src.is_empty() {
                self.queue_element_task_fire(el, "error", false, false);
                return;
            }
            self.script(el, |s| s.from_external = true);
            let Ok(url) = self.base_url_of(el).join(&src) else {
                self.queue_element_task_fire(el, "error", false, false);
                return;
            };
            self.script(el, |s| s.delaying_load = true);
            if module {
                self.in_document_realm(cx, doc, |cx| self.load_element_module_graph(cx, &url, el));
            } else {
                self.fetch_resource(url, Purpose::Script(el));
            }
        } else {
            // 31: an inline script.
            let base = self.base_url_of(el).to_string();
            let line = self.script(el, |s| s.start_line.max(1));
            if module {
                self.script(el, |s| s.delaying_load = true);
                self.in_document_realm(cx, doc, |cx| {
                    self.inline_module(cx, el, &source, &base, line)
                });
            } else {
                let line = i32::try_from(line).unwrap_or(i32::MAX);
                let url = self.dom().document_url(doc).to_owned();
                let options = self.script(el, |s| s.options.clone());
                let referrer = self.referrer_id(&base, &options);
                self.script(el, |s| {
                    s.result = ScriptResult::Classic {
                        source,
                        url,
                        line,
                        referrer,
                        muted: false,
                        streamer: None,
                        cache: None,
                    }
                });
            }
        }
        // 32: the lists, or 33: immediate execution.
        let (uninitialized, parser_inserted) = self.script(el, |s| {
            (s.result.is_uninitialized(), s.parser_document.is_some())
        });
        if (!module && has_src) || module {
            let has_async = self.dom().attr(el, &ns!(), &local_name!("async")).is_some();
            let force_async = self.script(el, |s| s.force_async);
            let has_defer = self.dom().attr(el, &ns!(), &local_name!("defer")).is_some();
            if has_async || force_async {
                self.doc_load(doc, |d| d.asap_scripts.push(el));
                self.script(el, |s| s.on_ready = OnReady::Asap);
            } else if !parser_inserted {
                self.doc_load(doc, |d| d.in_order_scripts.push_back(el));
                self.script(el, |s| s.on_ready = OnReady::InOrder);
            } else if has_defer || module {
                self.doc_load(doc, |d| d.defer_scripts.push_back(el));
                self.script(el, |s| s.on_ready = OnReady::ParserExecute);
            } else {
                self.doc_load(doc, |d| d.pending_parsing_blocking = Some(el));
                self.script(el, |s| s.on_ready = OnReady::ParserExecute);
            }
            // A result that is already there (an inline module script): "fetch an inline module
            // script graph" queues an element task on the networking task source to mark el as
            // ready, so even one without dependencies, or one with a parse error, never runs or
            // reports synchronously, nor in a microtask.
            if !uninitialized {
                self.queue_task(crate::event_loop::TaskSource::Networking, move |e, cx| {
                    if e.dom().is_alive(el) {
                        e.script_ready_steps(cx, el);
                        let doc = e.script(el, |s| s.preparation_document);
                        e.advance_after_script(cx, doc);
                    }
                });
            }
        } else if parser_inserted && self.doc_load(doc, |d| !d.script_blocking_sheets.is_empty()) {
            // An inline classic script waits for the style sheets that block scripts.
            self.doc_load(doc, |d| d.pending_parsing_blocking = Some(el));
            self.script(el, |s| {
                s.on_ready = OnReady::ParserExecute;
                s.ready_to_be_parser_executed = true;
            });
        } else {
            self.execute_script(cx, el);
        }
    }

    /// "Mark as ready": the result, then the steps to run when the result is ready.
    pub(crate) fn mark_ready(&self, cx: &mut Cx, el: NodeId, result: ScriptResult) {
        self.script(el, |s| {
            s.result = result;
            s.delaying_load = false;
        });
        self.script_ready_steps(cx, el);
        let doc = self.script(el, |s| s.preparation_document);
        self.advance_after_script(cx, doc);
    }

    /// A module graph for a script element finished.
    pub(crate) fn module_script_ready(
        &self,
        cx: &mut Cx,
        el: NodeId,
        result: crate::script::GraphResult,
    ) {
        let result = match result {
            crate::script::GraphResult::Module(m) => ScriptResult::Module(m),
            crate::script::GraphResult::Error(e) => ScriptResult::ModuleError(e),
            crate::script::GraphResult::Null => ScriptResult::Null,
        };
        let already_listed = self.script(el, |s| s.on_ready != OnReady::Nothing);
        if already_listed {
            self.mark_ready(cx, el, result);
        } else {
            // Inline modules resolve during prepare, before the lists are chosen.
            self.script(el, |s| {
                s.result = result;
                s.delaying_load = false;
            });
        }
    }

    fn script_ready_steps(&self, cx: &mut Cx, el: NodeId) {
        let (on_ready, doc) = self.script(el, |s| (s.on_ready, s.preparation_document));
        let Some(doc) = doc else { return };
        match on_ready {
            OnReady::Nothing => {}
            OnReady::Asap => {
                self.doc_load(doc, |d| d.asap_scripts.retain(|&s| s != el));
                self.execute_script(cx, el);
            }
            OnReady::InOrder => loop {
                let first = self.doc_load(doc, |d| d.in_order_scripts.front().copied());
                let Some(first) = first else { break };
                if self.script(first, |s| s.result.is_uninitialized()) {
                    break;
                }
                self.doc_load(doc, |d| d.in_order_scripts.pop_front());
                self.execute_script(cx, first);
            },
            OnReady::ParserExecute => {
                self.script(el, |s| s.ready_to_be_parser_executed = true);
            }
        }
    }

    /// After a script result arrives: the parser may resume, "the end" may advance.
    fn advance_after_script(&self, cx: &mut Cx, doc: Option<NodeId>) {
        if !self.script_running() {
            if let Some(doc) = doc {
                self.run_parser(cx, doc, false);
            }
        }
        self.advance_loading(cx);
    }

    /// "Execute the script element".
    pub(crate) fn execute_script(&self, cx: &mut Cx, el: NodeId) {
        let doc = self.dom().node_document(el);
        let (prep_doc, result, from_external, module) = self.script(el, |s| {
            (
                s.preparation_document,
                std::mem::take(&mut s.result),
                s.from_external,
                s.module,
            )
        });
        if prep_doc != Some(doc) {
            return;
        }
        let result = match result {
            ScriptResult::Null | ScriptResult::Uninitialized => {
                self.fire_event(cx, node_handle(el), "error", false, false);
                return;
            }
            r => r,
        };
        let counted = from_external || module;
        if counted {
            self.doc_state(doc, |s| s.ignore_destructive_writes += 1);
        }
        let started = std::time::Instant::now();
        self.in_document_realm(cx, doc, |cx| match result {
            ScriptResult::Classic {
                source,
                url,
                line,
                referrer,
                muted,
                streamer,
                cache,
            } => {
                let in_shadow = self.dom().is_in_shadow_tree(el);
                let old = self
                    .current_script
                    .replace(if in_shadow { None } else { Some(el) });
                let old_referrer =
                    std::mem::replace(&mut self.modules.borrow_mut().running_classic, referrer);
                self.run_classic_with(
                    cx,
                    &source,
                    &url,
                    line - 1,
                    referrer,
                    muted,
                    streamer,
                    cache.map(|c| *c),
                );
                self.modules.borrow_mut().running_classic = old_referrer;
                self.current_script.set(old);
            }
            ScriptResult::Module(m) => self.run_module(cx, &m, None),
            // HTML "run a module script" step 5: the error to rethrow is reported.
            ScriptResult::ModuleError(e) => self.report_module_exception(cx, e, None),
            // "register an import map"
            ScriptResult::ImportMap(r) => self.register_import_map(cx, r),
            _ => {}
        });
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.loader().stats.script_ms += ms;
        // SNOWLARK_TRACE=1: an `[exec]` line per script element run (when it ended, how long it ran).
        if crate::media::trace_on() {
            let src = self
                .dom()
                .get_attribute(el, "src")
                .map(|s| s.chars().take(110).collect::<String>())
                .unwrap_or_else(|| "(inline)".into());
            eprintln!(
                "[exec] {:8.3}s {ms:8.1} ms {src}",
                crate::media::trace_clock()
            );
        }
        if counted {
            self.doc_state(doc, |s| s.ignore_destructive_writes -= 1);
        }
        if from_external {
            self.fire_event(cx, node_handle(el), "load", false, false);
        }
    }

    // ----- dynamic markup insertion (HTML §8.4) --------------------------------------------------------

    /// `document.write(text)`.
    pub(crate) fn document_write(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        text: &str,
    ) -> snowlark_bindings::rt::Result<()> {
        use snowlark_bindings::rt::ExceptionName;
        // 1-2.
        if !self.dom().is_html_document(doc) {
            return Err(crate::engine::exception(
                ExceptionName::InvalidStateError,
                "document.write on an XML document",
            ));
        }
        if self.doc_state(doc, |s| s.throw_on_dynamic_markup_insertion) > 0 {
            return Err(crate::engine::exception(
                ExceptionName::InvalidStateError,
                "document.write is not allowed now",
            ));
        }
        // (the document write steps 8-9.1)
        if self.doc_state(doc, |s| s.active_parser_aborted) {
            return Ok(());
        }
        let insertion_point = self.parser_of(doc, |p| p.is_some_and(|p| p.insertion_point));
        if !insertion_point {
            if self.doc_state(doc, |s| {
                s.unload_counter > 0 || s.ignore_destructive_writes > 0
            }) {
                return Ok(());
            }
            self.document_open(cx, doc)?;
        }
        // 5.
        self.parser_of(doc, |p| {
            if let Some(parser) = p.and_then(|p| p.parser.as_mut()) {
                parser.insert_input(text);
            }
        });
        // 6.
        if self.doc_load(doc, |d| d.pending_parsing_blocking.is_none()) {
            self.run_parser(cx, doc, true);
        }
        Ok(())
    }

    /// The document open steps.
    pub(crate) fn document_open(
        &self,
        cx: &mut Cx,
        doc: NodeId,
    ) -> snowlark_bindings::rt::Result<()> {
        use snowlark_bindings::rt::ExceptionName;
        if !self.dom().is_html_document(doc) {
            return Err(crate::engine::exception(
                ExceptionName::InvalidStateError,
                "document.open on an XML document",
            ));
        }
        if self.doc_state(doc, |s| s.throw_on_dynamic_markup_insertion) > 0 {
            return Err(crate::engine::exception(
                ExceptionName::InvalidStateError,
                "document.open is not allowed now",
            ));
        }
        // HTML "document open steps" step 3: same origin (not origin-domain) with the entry settings
        // object's.
        let entry = self.entry_document(cx)?;
        if self.origin_of_document(doc) != self.origin_of_document(entry) {
            return Err(crate::engine::exception(
                ExceptionName::SecurityError,
                "document.open on a document of another origin",
            ));
        }
        // 5: an active parser with scripts running.
        if self.parser_of(doc, |p| p.is_some_and(|p| p.nesting > 0)) {
            return Ok(());
        }
        // 6: the document is being unloaded (`beforeunload`, `pagehide`, `visibilitychange`,
        // `unload`).
        if self.doc_state(doc, |s| s.unload_counter > 0) {
            return Ok(());
        }
        // 7: its parser was aborted.
        if self.doc_state(doc, |s| s.active_parser_aborted) {
            return Ok(());
        }
        // 8: stop loading.
        if let Some(id) = self.context_of_document(doc) {
            self.cancel_document_fetches(id);
        }
        // 9-10: erase listeners and handlers.
        let nodes: Vec<NodeId> = self
            .dom()
            .shadow_including_inclusive_descendants(doc)
            .collect();
        for n in nodes {
            self.erase_listeners(node_handle(n));
        }
        if let Some(w) = self.window_of_document(doc) {
            self.erase_listeners(w);
        }
        // 11.
        self.dom_mut().replace_all(None, doc);
        self.process_reactions(cx);
        // 12: a fully active document takes the entry document's URL (without its fragment when
        // it is another document).
        if self.is_fully_active(doc) {
            let entry_url = self.dom().document_url(entry).to_owned();
            if let Ok(mut url) = url::Url::parse(&entry_url) {
                if entry != doc {
                    url.set_fragment(None);
                }
                self.document_open_url(doc, url.as_str());
            }
        }
        // 13: no longer the initial `about:blank` (a navigation from it gets a new Window).
        if let Some(id) = self.context_of_document(doc) {
            let mut f = self.frames.borrow_mut();
            if f.list[id].document == doc {
                f.list[id].initial_about_blank = false;
            }
        }
        // 15.
        self.dom_mut()
            .set_quirks_mode(doc, snowlark_dom::QuirksMode::NoQuirks);
        // 16-17: a script-created parser with the insertion point at the end.
        let parser = HtmlParser::new(self.dom.clone(), doc, snowlark_dom::ParseOptions::default());
        self.doc_load(doc, |d| {
            d.parser = Some(ParserState {
                parser: Some(parser),
                xml: None,
                kind: BodyKind::Html,
                buffered: Vec::new(),
                nesting: 0,
                paused: false,
                insertion_point: true,
                script_created: true,
                eof: false,
                encoding_decided: true,
                pending_bytes: Vec::new(),
                transport_charset: None,
            });
            d.end = None;
        });
        // 18.
        self.set_ready_state(cx, doc, DocumentReadyState::Loading);
        Ok(())
    }

    /// `document.close()`.
    pub(crate) fn document_close(
        &self,
        cx: &mut Cx,
        doc: NodeId,
    ) -> snowlark_bindings::rt::Result<()> {
        use snowlark_bindings::rt::ExceptionName;
        if !self.dom().is_html_document(doc) {
            return Err(crate::engine::exception(
                ExceptionName::InvalidStateError,
                "document.close on an XML document",
            ));
        }
        if self.doc_state(doc, |s| s.throw_on_dynamic_markup_insertion) > 0 {
            return Err(crate::engine::exception(
                ExceptionName::InvalidStateError,
                "document.close is not allowed now",
            ));
        }
        let script_created = self.parser_of(doc, |p| p.is_some_and(|p| p.script_created));
        if !script_created {
            return Ok(());
        }
        // (the document's "the end" ends in "completely finish loading", whose step 5 runs the
        // iframe load event steps at a container)
        if let Some(id) = self.context_of_document(doc) {
            let mut f = self.frames.borrow_mut();
            if f.list[id].container.is_some() {
                f.list[id].pending_load_event = true;
            }
        }
        self.parser_of(doc, |p| {
            if let Some(p) = p {
                p.eof = true;
                p.insertion_point = false;
            }
        });
        if self.doc_load(doc, |d| d.pending_parsing_blocking.is_some()) {
            return Ok(());
        }
        self.run_parser(cx, doc, false);
        Ok(())
    }

    // ----- stylesheets --------------------------------------------------------------------------------

    /// A `link` element was inserted, or its `rel` or `href` changed: fetch its stylesheet if it is one
    /// (the renderer takes `<style>` sheets from the DOM's reactions itself).
    pub(crate) fn link_element_changed(&self, el: NodeId) {
        if self.dom().is_connected(el) {
            self.link_fetches_ahead(el);
        }
        let (is_sheet, href) = {
            let dom = self.dom();
            let rel = dom.attr(el, &ns!(), &local_name!("rel")).unwrap_or("");
            let is_sheet = rel
                .split_ascii_whitespace()
                .any(|t| t.eq_ignore_ascii_case("stylesheet"))
                && !rel
                    .split_ascii_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("alternate"));
            (
                is_sheet,
                dom.attr(el, &ns!(), &local_name!("href"))
                    .map(str::to_owned),
            )
        };
        // (a link with the `disabled` attribute has no sheet: HTML §4.2.4)
        let disabled = self
            .dom()
            .attr(el, &ns!(), &local_name!("disabled"))
            .is_some();
        if !is_sheet || disabled || !self.dom().is_connected(el) {
            self.doc_load(self.dom().node_document(el), |d| {
                d.script_blocking_sheets.remove(&el);
            });
            self.render_link_removed(el);
            return;
        }
        let Some(href) = href.filter(|h| !h.is_empty()) else {
            self.render_link_removed(el);
            return;
        };
        let Ok(url) = self.base_url_of(el).join(&href) else {
            self.queue_element_task_fire(el, "error", false, false);
            return;
        };
        let doc = self.dom().node_document(el);
        // HTML "The link element": the linked resource is fetched and processed when the element
        // is browsing-context connected; in a document without a browsing context (one of
        // `createHTMLDocument` or `DOMParser`) nothing is fetched and no event fires.
        if self.window_of_document(doc).is_none() {
            return;
        }
        self.doc_load(doc, |d| d.load_delayers += 1);
        // A style sheet link the parser made whose media match is script-blocking until it has loaded
        // (HTML "script-blocking style sheet set"); one a script made is not.
        let parser_made =
            self.dom().element_flags(el) & snowlark_dom::element_flags::PARSER_INSERTED != 0;
        let media = self
            .dom()
            .attr(el, &ns!(), &local_name!("media"))
            .map(str::to_owned);
        if parser_made && media.is_none_or(|m| self.media_query_matches(el, &m)) {
            self.doc_load(doc, |d| d.script_blocking_sheets.insert(el));
        }
        self.fetch_resource(url, Purpose::Stylesheet(el));
    }

    /// HTML "process the linked resource" (style sheet) and "update a `style` block" step 9: in an
    /// element task, `load` (`Some(true)`) or `error` (`Some(false)`) at the sheet's owner, after
    /// which the owner no longer contributes a script-blocking style sheet, and a parser that
    /// waited on the set goes on. `None` fires nothing (the owner lost its sheet).
    pub(crate) fn queue_sheet_event(&self, el: NodeId, outcome: Option<bool>) {
        let doc = self.dom().node_document(el);
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if let Some(ok) = outcome {
                if e.dom().is_alive(el) {
                    let type_ = if ok { "load" } else { "error" };
                    e.fire_event(cx, crate::engine::node_handle(el), type_, false, false);
                }
            }
            let released = e.doc_load(doc, |d| d.script_blocking_sheets.remove(&el));
            if released
                && e.doc_load(doc, |d| d.script_blocking_sheets.is_empty())
                && !e.script_running()
            {
                e.run_parser(cx, doc, false);
            }
        });
    }

    /// The script element's cloning steps: the copy is "already started" too.
    pub(crate) fn script_cloning_steps(&self, node: NodeId, copy: NodeId) {
        let started = self.dom().element_flags(node) & element_flags::SCRIPT_ALREADY_STARTED != 0;
        if started {
            self.dom_mut()
                .set_element_flag(copy, element_flags::SCRIPT_ALREADY_STARTED, true);
        }
    }
}

enum JobEvent {
    Nothing,
    /// A frame-ancestors violation the network layer found (its console message).
    Violation(snowlark_net::csp::Violation),
    /// The script's code cache entry was read (`None`: there was none to read).
    CacheEntry(Option<Entry>),
    Head,
    DocumentBytes(NodeId, Vec<u8>),
    Done,
    Failed(snowlark_net::NetError),
    Complete(Result<(Vec<u8>, String, url::Url), ()>),
}

fn rt_host_push(e: &Engine) {
    snowlark_bindings::rt::Host::push_ce_reactions(e);
}

fn rt_host_pop(e: &Engine, cx: &mut Cx) {
    snowlark_bindings::rt::Host::pop_ce_reactions(e, cx);
}

/// The `encoding` of an XML declaration at the start of `body`, read as ASCII.
/// The tokens of the first `sandbox` directive of a response's enforced CSP headers (the
/// `Content-Security-Policy` header; a report-only policy sets no flags), if any.
fn response_csp_sandbox(headers: &snowlark_net::HeaderList) -> Option<Vec<String>> {
    headers
        .get_all("content-security-policy")
        .into_iter()
        .flat_map(|v| v.split(','))
        .flat_map(|p| p.split(';'))
        .find_map(|d| {
            let mut tokens = d.split_ascii_whitespace();
            tokens
                .next()
                .filter(|n| n.eq_ignore_ascii_case("sandbox"))
                .map(|_| tokens.map(str::to_owned).collect())
        })
}

pub(crate) fn xml_declared_encoding(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(1024)];
    let text = std::str::from_utf8(head)
        .ok()
        .map(str::to_owned)
        .unwrap_or_else(|| head.iter().map(|&b| b as char).collect());
    let decl = text.strip_prefix("<?xml")?;
    let end = decl.find("?>")?;
    let decl = &decl[..end];
    let i = decl.find("encoding")?;
    let rest = decl[i + 8..].trim_start().strip_prefix('=')?.trim_start();
    let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &rest[1..];
    Some(rest[..rest.find(q)?].to_owned())
}

/// A MIME type for a file: URL by extension.
fn mime_for_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html"
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") {
        "text/javascript"
    } else if lower.ends_with(".css") {
        "text/css"
    } else if lower.ends_with(".xhtml") {
        "application/xhtml+xml"
    } else if lower.ends_with(".xml") {
        "application/xml"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "text/plain"
    }
}
