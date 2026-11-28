// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Running script (HTML §8.1.4-§8.1.5): every entry from Rust into JS goes through here, so the
//! JavaScript execution context stack depth is known and "clean up after running script" performs the
//! microtask checkpoint when it empties; exceptions are reported (HTML §8.1.4.7); classic scripts,
//! timer code, callbacks and event handlers are compiled and run; module scripts are fetched as graphs,
//! linked, evaluated, and `import()` / `import.meta` work. Plus the promise rejection tracker and
//! structured clone.

use std::collections::HashMap;

use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsObject, JsValue, ToJs};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, ns, NodeId};
use snowlark_js::code_cache::CacheKind;
use snowlark_js::streaming::{ScriptStreamer, Streamed};

use crate::loader::{CacheInfo, CacheSource};
use v8::{ValueDeserializerHelper, ValueSerializerHelper};

use crate::engine::{node_handle, Engine};

/// What a module graph load reports to when it completes.
pub enum GraphDone {
    /// A `<script type=module>` element (its result becomes the module, or null on failure).
    Element(NodeId),
    /// An `import()`: its promise.
    Import(v8::Global<v8::PromiseResolver>),
    /// A module worker's main script (HTML §10.2.4 "run a worker").
    WorkerMain,
    /// A module service worker's main script (W3C Service Workers §8 Run Service Worker).
    ServiceWorkerMain,
}

/// How a module worker's module scripts are fetched (HTML "fetch a module worker script graph"):
/// destination `worker` and the worker's credentials mode for all of them; the main script in
/// `same-origin` mode with the creator as its client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerModuleFetch {
    pub credentials: snowlark_net::CredentialsMode,
    pub top_level: bool,
}

/// A module in the module map (HTML §8.1.5.5).
pub enum ModuleState {
    Fetching,
    Ready(v8::Global<v8::Module>),
    Failed,
    /// A module script whose parse error is set: one of its requests failed to resolve (HTML
    /// "create a JavaScript module script" step 9), the TypeError.
    Errored(v8::Global<v8::Value>),
}

/// What a module graph load completes with.
pub enum GraphResult {
    /// The root module script, linked and ready to evaluate.
    Module(v8::Global<v8::Module>),
    /// A module script whose error to rethrow is the graph's first parse error.
    Error(v8::Global<v8::Value>),
    /// Null: a fetch failed.
    Null,
}

/// The module map and the graph loads waiting on it.
#[derive(Default)]
pub struct ModuleMap {
    pub by_url: HashMap<String, ModuleState>,
    /// Module identity hash → (module URL, base URL for its imports).
    pub url_of: HashMap<i32, (String, String)>,
    /// Module identity hash -> the module map it belongs to ([`Engine::module_map_id`]).
    pub map_of: HashMap<i32, u64>,
    pub graphs: Vec<(String, GraphDone)>,
    /// The resolved requests of each module, by module URL.
    pub children: HashMap<String, Vec<String>>,
    /// Evaluations that have not settled (top-level await), with their completion.
    pub evaluating: Vec<Evaluation>,
    pub inline_count: u32,
    /// `modulepreload` links waiting for a module the module map is fetching.
    pub preload_waiters: Vec<(String, NodeId)>,
    /// The script fetch options of module fetches with a nonce, integrity metadata or parser
    /// metadata (HTML §8.1.4.1), by module map key: a graph's root has its script element's, a
    /// descendant its importer's ("descendant script fetch options").
    pub options: HashMap<String, ModuleOptions>,
    /// The scripts `import()` and string timers may find running, by id - 1 (each distinct base URL
    /// and fetch options once: V8's compilation cache compares host-defined options by value).
    pub referrers: Vec<ReferrerScript>,
    pub referrer_ids: HashMap<String, u32>,
    /// The [`ReferrerScript`] id of the script element's classic script running now (0: none).
    pub running_classic: u32,
    /// Each Window's import map and resolved module set (HTML §8.1.5.3), by module map id.
    pub imports: rustc_hash::FxHashMap<u64, crate::import_map::GlobalImports>,
    /// `import()`s whose graph has loaded, in order, waiting for their microtask to link and
    /// evaluate the module ([`Engine::continue_dynamic_import`]), with the document whose realm
    /// loaded it.
    pub imports_loaded: std::collections::VecDeque<(
        Option<NodeId>,
        v8::Global<v8::Module>,
        v8::Global<v8::PromiseResolver>,
    )>,
}

/// HTML "script fetch options" as the module fetches use them (CSP and SRI).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleOptions {
    pub nonce: String,
    pub integrity: String,
    /// The integrity metadata was given (a script element's `integrity` attribute, even empty):
    /// the import map's does not replace it.
    pub integrity_given: bool,
    pub parser_metadata: snowlark_net::ParserMetadata,
    /// The element's `blocking=render` (Fetch's request "render-blocking"; descendants have none).
    pub render_blocking: bool,
    /// A module worker's script (its main script or one it imports).
    pub worker: Option<WorkerModuleFetch>,
    /// The credentials mode (the script element's `crossorigin`, "same-origin" without it).
    pub credentials: snowlark_net::CredentialsMode,
    /// The referrer policy (the script element's `referrerpolicy`; a module script's
    /// `Referrer-Policy` response header for its descendants).
    pub referrer_policy: snowlark_net::ReferrerPolicy,
    /// Not a fetch option: the fetch's referrer, the importing script's base URL ("" for the
    /// client, as a script element's own fetch has it).
    pub referrer: String,
}

impl Default for ModuleOptions {
    fn default() -> Self {
        ModuleOptions {
            nonce: String::new(),
            integrity: String::new(),
            integrity_given: false,
            parser_metadata: snowlark_net::ParserMetadata::None,
            render_blocking: false,
            worker: None,
            credentials: snowlark_net::CredentialsMode::SameOrigin,
            referrer_policy: snowlark_net::ReferrerPolicy::Empty,
            referrer: String::new(),
        }
    }
}

impl ModuleOptions {
    /// HTML "get the descendant script fetch options": the same, without integrity metadata.
    fn descendant(&self) -> ModuleOptions {
        ModuleOptions {
            integrity: String::new(),
            render_blocking: false,
            integrity_given: false,
            worker: self.worker.map(|w| WorkerModuleFetch {
                top_level: false,
                ..w
            }),
            ..self.clone()
        }
    }

    /// The descendant fetch options of a script with these options and base URL `base`, with
    /// that base URL as the fetch's referrer (HTML "fetch a single imported module script").
    fn imported_from(&self, base: &str) -> ModuleOptions {
        ModuleOptions {
            referrer: base.to_owned(),
            ..self.descendant()
        }
    }
}

/// An HTML "script" as `import()` and string timers find it (the active script): its base URL
/// and fetch options. Its id is what the script's host-defined options carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferrerScript {
    pub base: String,
    pub options: ModuleOptions,
}

/// The host-defined options of a script whose [`ReferrerScript`] has id `id` (none for 0).
fn host_options<'s>(scope: &mut v8::PinScope<'s, '_>, id: u32) -> Option<v8::Local<'s, v8::Data>> {
    if id == 0 {
        return None;
    }
    let a = v8::PrimitiveArray::new(scope, 1);
    let n = v8::Integer::new_from_unsigned(scope, id);
    a.set(scope, 0, n.into());
    Some(a.into())
}

/// The [`ReferrerScript`] id a script's host-defined options carry (0: none).
fn host_options_id(scope: &mut v8::PinScope<'_, '_>, data: Option<v8::Local<v8::Data>>) -> u32 {
    let Some(a) = data.and_then(|d| v8::Local::<v8::FixedArray>::try_from(d).ok()) else {
        return 0;
    };
    if a.length() != 1 {
        return 0;
    }
    a.get(scope, 0)
        .and_then(|v| v8::Local::<v8::Value>::try_from(v).ok())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(0)
}

/// A module evaluation that has not settled: its promise, the promise to settle with it (a
/// dynamic import), the module.
pub type Evaluation = (
    v8::Global<v8::Promise>,
    Option<v8::Global<v8::PromiseResolver>>,
    Option<v8::Global<v8::Module>>,
);

/// A promise rejection the tracker saw (HTML §8.1.5.7).
pub struct Rejection {
    pub promise: v8::Global<v8::Promise>,
    pub reason: Option<v8::Global<v8::Value>>,
}

/// The key of a module script in the event loop's module maps: the module map it is in (each
/// Document has its own, HTML §8.1.5.5, so a frame that loads a module another document already
/// loaded gets its own module record, evaluated in its own realm), its module type and its URL (the
/// map is keyed by (URL, module type)).
pub(crate) fn module_key(map: u64, url: &str, ty: &str) -> String {
    format!("{map:x} {ty} {url}")
}

/// The (module map, module type, URL) of a module map key.
pub(crate) fn split_module_key(key: &str) -> (u64, &str, &str) {
    let mut parts = key.splitn(3, ' ');
    let map = parts
        .next()
        .and_then(|m| u64::from_str_radix(m, 16).ok())
        .unwrap_or(0);
    let ty = parts.next().unwrap_or("javascript");
    let url = parts.next().unwrap_or("");
    (map, ty, url)
}

/// The module type of a module map key for an import whose type is not allowed: its fetch fails.
const UNSUPPORTED_TYPE: &str = "(unsupported)";

/// HTML "module type allowed": the module types this engine supports, `css` only where
/// `CSSStyleSheet` is exposed (a Window, not a worker).
fn supported_module_type(engine: &Engine, ty: &str) -> bool {
    match ty {
        "javascript" | "json" | "text" => true,
        "css" => engine.worker_scope.borrow().is_none(),
        _ => false,
    }
}

/// The origin of a classic script at `url`, line 0, whose [`ReferrerScript`] id is `referrer`.
pub(crate) fn script_origin<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    url: &str,
    referrer: u32,
) -> Option<v8::ScriptOrigin<'s>> {
    origin(scope, url, 0, false, referrer)
}

/// A script's origin: its resource name, line offset, kind, and the id of its
/// [`ReferrerScript`] in its host-defined options. Its errors are not muted.
fn origin<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    url: &str,
    line: i32,
    module: bool,
    referrer: u32,
) -> Option<v8::ScriptOrigin<'s>> {
    origin_muted(scope, url, line, module, referrer, false)
}

/// [`origin`] for a script whose errors are muted (HTML "muted errors": a classic script from a
/// CORS-cross-origin response) or not. V8 marks the messages of a muted script's code not
/// "shared cross-origin", and so does it for code that code evaluates (`eval`, `Function`);
/// [`Engine::report_exception`] then reports them as "Script error.".
fn origin_muted<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    url: &str,
    line: i32,
    module: bool,
    referrer: u32,
    muted: bool,
) -> Option<v8::ScriptOrigin<'s>> {
    let name = v8::String::new(scope, url)?;
    let options = host_options(scope, referrer);
    Some(v8::ScriptOrigin::new(
        scope,
        name.into(),
        line,
        0,
        !muted,
        0,
        None,
        false,
        false,
        module,
        options,
    ))
}

/// HTML "resolve a URL-like module specifier" (module specifier resolution without an import map:
/// workers, `modulepreload`).
pub fn resolve_specifier(specifier: &str, base: &str) -> Option<url::Url> {
    crate::import_map::resolve_url_like(specifier, &url::Url::parse(base).ok()?)
}

impl Engine {
    // ----- scripts as `import()` finds them ---------------------------------------------------------------

    /// The id of the [`ReferrerScript`] with base URL `base` and fetch options `options`, for a
    /// script's host-defined options.
    pub(crate) fn referrer_id(&self, base: &str, options: &ModuleOptions) -> u32 {
        let key = format!("{base}\u{0}{options:?}");
        let mut m = self.modules.borrow_mut();
        if let Some(&id) = m.referrer_ids.get(&key) {
            return id;
        }
        m.referrers.push(ReferrerScript {
            base: base.to_owned(),
            options: options.clone(),
        });
        let id = m.referrers.len() as u32;
        m.referrer_ids.insert(key, id);
        id
    }

    fn referrer_script(&self, id: u32) -> Option<ReferrerScript> {
        let i = (id as usize).checked_sub(1)?;
        self.modules.borrow().referrers.get(i).cloned()
    }

    /// HTML timer initialization steps 9.3-9.6: the base URL and fetch options a string handler's
    /// classic script gets from the script that set the timer (the active script, as V8 finds it:
    /// the code that called, or the script that compiled it from a string), as a [`ReferrerScript`]
    /// id; 0 when no script is running, for the realm's API base URL and the default options.
    pub(crate) fn timer_referrer(&self, cx: &mut Cx) -> u32 {
        let id = {
            let scope = cx.scope();
            let o = scope.get_current_host_defined_options();
            host_options_id(scope, o)
        };
        let Some(r) = self.referrer_script(id) else {
            return 0;
        };
        let options = ModuleOptions {
            nonce: r.options.nonce,
            parser_metadata: snowlark_net::ParserMetadata::NotParserInserted,
            credentials: r.options.credentials,
            referrer_policy: r.options.referrer_policy,
            ..ModuleOptions::default()
        };
        self.referrer_id(&r.base, &options)
    }

    // ----- the execution context stack ------------------------------------------------------------------

    pub(crate) fn enter_script(&self) {
        self.js_depth.set(self.js_depth.get() + 1);
    }

    /// "Clean up after running script": when the stack empties, a microtask checkpoint.
    pub(crate) fn leave_script(&self, cx: &mut Cx) {
        let d = self.js_depth.get().saturating_sub(1);
        self.js_depth.set(d);
        if d == 0 {
            self.microtask_checkpoint(cx);
        }
    }

    /// Whether script is on the stack.
    pub fn script_running(&self) -> bool {
        self.js_depth.get() > 0
    }

    /// HTML §8.1.7.4 "perform a microtask checkpoint".
    pub fn microtask_checkpoint(&self, cx: &mut Cx) {
        if self.performing_checkpoint.replace(true) {
            return;
        }
        // A settled module evaluation resolves an `import()` promise, which queues more
        // microtasks (the importer's reactions): they run in this checkpoint too.
        loop {
            cx.scope().perform_microtask_checkpoint();
            if !self.check_module_evaluations(cx) {
                break;
            }
        }
        self.notify_rejected_promises(cx);
        // "Cleanup Indexed Database transactions" (IndexedDB §2.7.1).
        self.idb_cleanup();
        // WebGL 2.0 fences may change state from here on.
        self.webgl_checkpoint();
        // ClearKeptObjects(): WeakRef targets kept during the job may go now.
        cx.scope().clear_kept_objects();
        self.performing_checkpoint.set(false);
    }

    /// Takes the exception of a TryCatch and reports it.
    fn report_caught(
        &self,
        tc: &mut v8::PinScope,
        exception: v8::Local<v8::Value>,
        message: Option<v8::Local<v8::Message>>,
    ) {
        let e = v8::Global::new(tc, exception);
        let m = message.map(|m| v8::Global::new(tc, m));
        let mut cx = Cx::new(tc);
        self.report_exception(&mut cx, e, m);
    }

    // ----- classic scripts ------------------------------------------------------------------------------

    /// HTML §8.1.4.2 "run a classic script": compile and run `source` as a script named `url`, reporting
    /// a thrown exception. Returns the completion value.
    pub fn run_classic(
        &self,
        cx: &mut Cx,
        source: &str,
        url: &str,
        line: i32,
    ) -> Option<v8::Global<v8::Value>> {
        self.run_classic_with(cx, source, url, line, 0, false, None, None)
    }

    /// [`Engine::run_classic`] for a script element's script or a timer's: `referrer` is its
    /// [`ReferrerScript`] id (its base URL and fetch options). An external script is compiled
    /// from its code cache entry when there is one for this response, or finished from what V8
    /// parsed while it downloaded (`streamer`), or compiled from `source`; its code cache is made
    /// after it has run. `muted`: the script's "muted errors".
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_classic_with(
        &self,
        cx: &mut Cx,
        source: &str,
        url: &str,
        line: i32,
        referrer: u32,
        muted: bool,
        streamer: Option<ScriptStreamer>,
        mut cache: Option<CacheInfo>,
    ) -> Option<v8::Global<v8::Value>> {
        self.enter_script();
        let result = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let r = (|| {
                let started = std::time::Instant::now();
                let code = v8::String::new(tc, source)?;
                let string_ms = started.elapsed().as_secs_f64() * 1000.0;
                let origin = origin_muted(tc, url, line, false, referrer, muted)?;
                let consumer = cache
                    .as_mut()
                    .and_then(|c| c.take_consumer(CacheKind::Classic));
                let (script, streamed, consumed) = if let Some(consumer) = consumer {
                    let bytes = consumer.len();
                    let (script, rejected) = consumer.consume(code, &origin, |src| {
                        v8::script_compiler::compile(
                            tc,
                            src,
                            v8::script_compiler::CompileOptions::ConsumeCodeCache,
                            v8::script_compiler::NoCacheReason::NoReason,
                        )
                    });
                    (script, false, Some((rejected, bytes)))
                } else {
                    match streamer {
                        Some(s) => match s.compile_script(tc, code, &origin) {
                            Streamed::Compiled(script) => (script, true, None),
                            Streamed::Fallback => {
                                self.loader.borrow_mut().stats.stream_fallbacks += 1;
                                (v8::Script::compile(tc, code, Some(&origin)), false, None)
                            }
                        },
                        None => (v8::Script::compile(tc, code, Some(&origin)), false, None),
                    }
                };
                self.count_compile(started, streamed, source.len());
                self.trace_compile(url, source.len(), streamed, string_ms, started);
                let script = script?;
                if let Some(c) = cache.take() {
                    let unbound = script.get_unbound_script(tc);
                    let unbound = v8::Global::new(tc, unbound);
                    self.cache_outcome(c, consumed, CacheSource::Classic(unbound));
                }
                script.run(tc)
            })();
            match r {
                Some(v) => Some(v8::Global::new(tc, v)),
                None => {
                    if let Some(e) = tc.exception() {
                        let m = tc.message();
                        tc.reset();
                        self.report_caught(tc, e, m);
                    }
                    None
                }
            }
        };
        self.leave_script(cx);
        result
    }

    /// Evaluates `source` and returns the completion value or the exception, as strings (`--eval`).
    pub fn eval_to_string(&self, cx: &mut Cx, source: &str, name: &str) -> Result<String, String> {
        self.enter_script();
        let result = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let r = (|| {
                let code = v8::String::new(tc, source)?;
                let origin = origin(tc, name, 0, false, 0)?;
                let script = v8::Script::compile(tc, code, Some(&origin))?;
                script.run(tc)
            })();
            match r {
                Some(v) => Ok(v.to_rust_string_lossy(tc)),
                None => {
                    let text = match tc.exception() {
                        Some(e) => {
                            let s = e.to_rust_string_lossy(tc);
                            let m = tc.message();
                            let loc = m
                                .map(|m| {
                                    format!(" at {}:{}", name, m.get_line_number(tc).unwrap_or(0))
                                })
                                .unwrap_or_default();
                            format!("{s}{loc}")
                        }
                        None => "unknown error".into(),
                    };
                    tc.reset();
                    Err(text)
                }
            }
        };
        self.leave_script(cx);
        result
    }

    /// Timer code (a string handler, or a `javascript:` URL's): a classic script whose base URL and
    /// fetch options are those of the [`ReferrerScript`] `referrer` ([`Engine::timer_referrer`];
    /// 0: the realm's API base URL and the default options).
    pub(crate) fn run_timer_code(&self, cx: &mut Cx, code: &str, window: Handle, referrer: u32) {
        let Some(doc) = self.document_of_window(window) else {
            return;
        };
        let url = self.dom().document_url(doc).to_owned();
        self.in_document_realm(cx, doc, |cx| {
            self.run_classic_with(cx, code, &url, 0, referrer, false, None, None);
        });
    }

    /// Calls `f` with `this` and arguments, reporting what it throws.
    pub(crate) fn call_reporting(
        &self,
        cx: &mut Cx,
        f: &JsObject,
        this: Option<Handle>,
        args: &[v8::Global<v8::Value>],
    ) -> Option<v8::Global<v8::Value>> {
        self.enter_script();
        // Web IDL "invoke a callback function": the script runs, and an exception it throws is
        // reported, in the callback's realm (a frame's timer or animation frame callback reports
        // to the frame's window, not to the realm that happens to be current).
        let r = rt::in_callback_realm(cx, f, None, |cx| {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let func = v8::Local::new(tc, f.global());
            match v8::Local::<v8::Function>::try_from(func) {
                Err(_) => None,
                Ok(func) => {
                    let recv: v8::Local<v8::Value> = match this.and_then(|h| rt::wrap(tc, h)) {
                        Some(w) => w.into(),
                        None => v8::undefined(tc).into(),
                    };
                    let argv: Vec<v8::Local<v8::Value>> =
                        args.iter().map(|a| v8::Local::new(tc, a)).collect();
                    match func.call(tc, recv, &argv) {
                        Some(v) => Some(v8::Global::new(tc, v)),
                        None => {
                            if let Some(e) = tc.exception() {
                                let m = tc.message();
                                tc.reset();
                                self.report_caught(tc, e, m);
                            }
                            None
                        }
                    }
                }
            }
        });
        self.leave_script(cx);
        r
    }

    pub(crate) fn call_timer_function(
        &self,
        cx: &mut Cx,
        f: &JsObject,
        args: &[JsValue],
        window: Handle,
    ) {
        let args: Vec<v8::Global<v8::Value>> = args.iter().map(|a| a.global().clone()).collect();
        self.call_reporting(cx, f, Some(window), &args);
    }

    pub(crate) fn call_frame_callback(&self, cx: &mut Cx, f: &JsObject, now: f64, window: Handle) {
        let arg = {
            let scope = cx.scope();
            let v: v8::Local<v8::Value> = v8::Number::new(scope, now).into();
            v8::Global::new(scope, v)
        };
        self.call_reporting(cx, f, Some(window), &[arg]);
    }

    pub(crate) fn call_idle_callback(
        &self,
        cx: &mut Cx,
        f: &JsObject,
        deadline: Handle,
        window: Handle,
    ) {
        let arg = {
            let scope = cx.scope();
            let v: v8::Local<v8::Value> = match rt::wrap(scope, deadline) {
                Some(w) => w.into(),
                None => v8::undefined(scope).into(),
            };
            v8::Global::new(scope, v)
        };
        self.call_reporting(cx, f, Some(window), &[arg]);
    }

    // ----- event handlers (HTML §8.1.8.1 "getting the current value of the event handler") --------------

    pub(crate) fn compile_event_handler(
        &self,
        cx: &mut Cx,
        target: Handle,
        event: &'static str,
        body: &str,
        element: Option<NodeId>,
        line: u32,
    ) -> Option<JsObject> {
        let is_window = self.context_of_window(target).is_some();
        // The handler's realm: its element's document's, or its window's.
        let realm_doc = match element {
            Some(el) => Some(self.dom().node_document(el)),
            None => self.document_of_window(target),
        };
        // (scripting is disabled in a document its CSP sandboxes without allow-scripts)
        if realm_doc.is_some_and(|d| self.scripts_sandboxed(d)) {
            return None;
        }
        if let Some(doc) = realm_doc {
            if let Some(id) = self.context_of_document(doc) {
                let realm = self.realm_of_context(id);
                let current = {
                    let scope = cx.scope();
                    realm
                        .as_ref()
                        .is_some_and(|r| v8::Local::new(scope, r) == scope.get_current_context())
                };
                if !current && realm.is_some() {
                    return self.in_realm(cx, realm, |cx| {
                        self.compile_event_handler(cx, target, event, body, element, line)
                    });
                }
            }
        }
        let params: &[&str] = if event == "error" && is_window {
            &["event", "source", "lineno", "colno", "error"]
        } else {
            &["event"]
        };
        // Step 3.?: the scope has the element, its form owner and its document only for an
        // element's event handler; a Window's (a `body`'s forwarded content attribute) has the
        // window alone.
        let scope_element = element.filter(|_| !is_window);
        let (document, form) = match scope_element {
            Some(el) => {
                let dom = self.dom();
                let doc = dom.node_document(el);
                let form = if crate::engine::is_form_associated(&dom, el) {
                    self.form_owner_of_dom(&dom, el)
                } else {
                    None
                };
                (Some(doc), form)
            }
            None => (None, None),
        };
        let url = realm_doc
            .map(|d| self.dom().document_url(d).to_owned())
            .unwrap_or_default();
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let r = (|| {
            let code = v8::String::new(tc, body)?;
            // (an event handler's function has no script: `import()` in it uses the realm's
            // API base URL, HTML "getting the current value of the event handler" step 3.11)
            let o = origin(tc, &url, line.saturating_sub(1) as i32, false, 0)?;
            let mut source = v8::script_compiler::Source::new(code, Some(&o));
            let args: Vec<v8::Local<v8::String>> = params.iter().map(|p| rt::name(tc, p)).collect();
            let mut extensions: Vec<v8::Local<v8::Object>> = Vec::new();
            for h in [
                document.map(node_handle),
                form.map(node_handle),
                scope_element.map(node_handle),
            ]
            .into_iter()
            .flatten()
            {
                extensions.push(rt::wrap(tc, h)?);
            }
            let f = v8::script_compiler::compile_function(
                tc,
                &mut source,
                &args,
                &extensions,
                v8::script_compiler::CompileOptions::NoCompileOptions,
                v8::script_compiler::NoCacheReason::NoReason,
            )?;
            // The function is named after the handler: its source text is "function
            // onclick(event) {\n<body>\n}" (the handler of `webkitAnimationEnd` is
            // `onwebkitanimationend`, an SVG animation's of `endEvent` `onend`).
            let handler = event.strip_suffix("Event").unwrap_or(event);
            let name = v8::String::new(tc, &format!("on{}", handler.to_ascii_lowercase()))?;
            f.set_name(name);
            Some(f)
        })();
        match r {
            Some(f) => Some(JsObject::new(tc, f.into())),
            None => {
                if let Some(e) = tc.exception() {
                    let m = tc.message();
                    tc.reset();
                    self.report_caught(tc, e, m);
                }
                None
            }
        }
    }

    // ----- module scripts (HTML §8.1.5) -----------------------------------------------------------------

    /// Drops the module maps of `docs`, documents whose browsing context was discarded or that a
    /// navigation replaced: no script of theirs fetches a module again (their fetches are aborted).
    /// A module record keeps its realm alive, so a module map left behind would keep the document's
    /// Window, and everything its script reaches, for good.
    pub(crate) fn forget_module_maps(&self, docs: &[snowlark_dom::NodeId]) {
        let ids: Vec<u64> = docs.iter().map(|d| d.to_bits()).collect();
        let theirs = |key: &str| ids.contains(&split_module_key(key).0);
        let mut m = self.modules.borrow_mut();
        m.by_url.retain(|k, _| !theirs(k));
        m.children.retain(|k, _| !theirs(k));
        m.options.retain(|k, _| !theirs(k));
        m.graphs.retain(|(k, _)| !theirs(k));
        m.preload_waiters.retain(|(k, _)| !theirs(k));
        m.imports.retain(|id, _| !ids.contains(id));
        m.map_of.retain(|_, id| !ids.contains(id));
        let ModuleMap { url_of, map_of, .. } = &mut *m;
        url_of.retain(|hash, _| map_of.contains_key(hash));
        drop(m);
        // (and the scripts of theirs that never ran: a module record or an error value)
        let dom = self.dom();
        for (&n, s) in self.loader.borrow_mut().scripts.iter_mut() {
            if dom.is_alive(n) && docs.contains(&dom.node_document(n)) {
                s.result = crate::loader::ScriptResult::Null;
            }
        }
    }

    /// The module map of the current realm: its window's document's (as node id bits), or 0 in a
    /// worker, whose engine has one realm.
    pub(crate) fn module_map_id(&self, cx: &mut Cx) -> u64 {
        self.current_window(cx)
            .and_then(|w| self.associated_document(w))
            .map_or(0, |d| d.to_bits())
    }

    /// With `--trace` (the network client's trace), a line per script of at least 1 KiB: its size,
    /// whether it streamed, the page-thread time making its string and compiling it.
    fn trace_compile(
        &self,
        url: &str,
        bytes: usize,
        streamed: bool,
        string_ms: f64,
        started: std::time::Instant,
    ) {
        let trace = self.hooks.borrow().net.as_ref().is_some_and(|n| n.trace());
        if trace && bytes >= 1024 {
            let total = started.elapsed().as_secs_f64() * 1000.0;
            eprintln!(
                "[script] {:>6.1} ms compile ({:.1} ms string) {:>5} KiB{} {url}",
                total,
                string_ms,
                bytes / 1024,
                if streamed { " streamed" } else { "" },
            );
        }
    }

    /// The loading statistics: one more script compiled on the page thread since `started`.
    fn count_compile(&self, started: std::time::Instant, streamed: bool, bytes: usize) {
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let stats = &mut self.loader.borrow_mut().stats;
        stats.compiled += 1;
        stats.compile_ms += ms;
        if streamed {
            stats.streamed += 1;
            stats.streamed_finish_ms += ms;
            stats.streamed_bytes += bytes;
        }
    }

    /// What a script's code cache did: `consumed` is whether an entry was offered and whether V8
    /// rejected it, with its size. A rejected entry is removed (the next load starts over). A
    /// script compiled without code is hot if this response was loaded less than
    /// [`snowlark_js::code_cache::HOT_FOR`] ago: its code cache is made after it has run; otherwise
    /// this load leaves a timestamp (Chromium's heat check, `V8CodeCache::GetCompileOptions`).
    fn cache_outcome(
        &self,
        cache: CacheInfo,
        consumed: Option<(bool, usize)>,
        source: CacheSource,
    ) {
        let mut l = self.loader.borrow_mut();
        match consumed {
            Some((false, bytes)) => {
                l.stats.cache_hits += 1;
                l.stats.cache_bytes += bytes;
                return;
            }
            Some((true, _)) => {
                l.stats.cache_rejected += 1;
                cache.cache.remove(&cache.key);
                return;
            }
            None => {}
        }
        if cache.stale {
            l.stats.cache_stale += 1;
        }
        if !cache.hot {
            l.stats.cache_timestamps += 1;
            let now = snowlark_js::code_cache::timestamp(std::time::SystemTime::now());
            cache
                .cache
                .store(&cache.key, CacheKind::Timestamp, cache.hash, now);
            return;
        }
        l.pending_caches.push((source, cache.key, cache.hash));
        let loaded = l.stats.load_ms > 0.0;
        drop(l);
        if loaded {
            self.queue_code_caches();
        }
    }

    /// Queues a task making the code caches of the scripts that have run (after `load`, and for
    /// scripts that run later, after they have run).
    pub(crate) fn queue_code_caches(&self) {
        {
            let mut l = self.loader.borrow_mut();
            if l.caches_queued || l.pending_caches.is_empty() {
                return;
            }
            l.caches_queued = true;
        }
        self.queue_task(crate::event_loop::TaskSource::Networking, |e, cx| {
            e.produce_code_caches(cx)
        });
    }

    /// `ScriptCompiler::CreateCodeCache` for each script that has run, stored by the code cache's
    /// own thread.
    fn produce_code_caches(&self, cx: &mut Cx) {
        let pending = {
            let mut l = self.loader.borrow_mut();
            l.caches_queued = false;
            std::mem::take(&mut l.pending_caches)
        };
        let Some(cache) = self.code_cache() else {
            return;
        };
        let started = std::time::Instant::now();
        let mut made = 0;
        let scope = cx.scope();
        for (source, key, hash) in pending {
            let (data, kind) = match &source {
                CacheSource::Classic(s) => (
                    v8::Local::new(scope, s).create_code_cache(),
                    CacheKind::Classic,
                ),
                CacheSource::Module(m) => {
                    let m = v8::Local::new(scope, m);
                    (
                        m.get_unbound_module_script(scope).create_code_cache(),
                        CacheKind::Module,
                    )
                }
            };
            if let Some(data) = data {
                cache.store(&key, kind, hash, data.to_vec());
                made += 1;
            }
        }
        let mut l = self.loader.borrow_mut();
        l.stats.cache_produced += made;
        l.stats.cache_produce_ms += started.elapsed().as_secs_f64() * 1000.0;
    }

    /// Compiles a module from its code cache entry for this response, or finishes `streamer`'s
    /// compilation of it, or compiles `source`; `Err` is the parse error (`None`: no exception).
    /// `base` and `options` are the module script's base URL and fetch options (what `import()`
    /// in it uses). `line` is the source line (1-based) the module's text starts on.
    #[allow(clippy::too_many_arguments)]
    fn compile_module(
        &self,
        cx: &mut Cx,
        source: &str,
        url: &str,
        line: u32,
        base: &str,
        options: &ModuleOptions,
        map: u64,
        streamer: Option<ScriptStreamer>,
        mut cache: Option<CacheInfo>,
    ) -> Result<v8::Global<v8::Module>, Option<v8::Global<v8::Value>>> {
        let referrer = self.referrer_id(
            base,
            &ModuleOptions {
                referrer: String::new(),
                ..options.clone()
            },
        );
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let r = (|| {
            let started = std::time::Instant::now();
            let code = v8::String::new(tc, source)?;
            let string_ms = started.elapsed().as_secs_f64() * 1000.0;
            let line = i32::try_from(line.max(1) - 1).unwrap_or(i32::MAX);
            let o = origin(tc, url, line, true, referrer)?;
            let consumer = cache
                .as_mut()
                .and_then(|c| c.take_consumer(CacheKind::Module));
            let (module, streamed, consumed) = if let Some(consumer) = consumer {
                let bytes = consumer.len();
                let (module, rejected) = consumer.consume(code, &o, |src| {
                    v8::script_compiler::compile_module2(
                        tc,
                        src,
                        v8::script_compiler::CompileOptions::ConsumeCodeCache,
                        v8::script_compiler::NoCacheReason::NoReason,
                    )
                });
                (module, false, Some((rejected, bytes)))
            } else {
                match streamer {
                    Some(s) => match s.compile_module(tc, code, &o) {
                        Streamed::Compiled(m) => (m, true, None),
                        Streamed::Fallback => {
                            self.loader.borrow_mut().stats.stream_fallbacks += 1;
                            let mut src = v8::script_compiler::Source::new(code, Some(&o));
                            (
                                v8::script_compiler::compile_module(tc, &mut src),
                                false,
                                None,
                            )
                        }
                    },
                    None => {
                        let mut src = v8::script_compiler::Source::new(code, Some(&o));
                        (
                            v8::script_compiler::compile_module(tc, &mut src),
                            false,
                            None,
                        )
                    }
                }
            };
            self.count_compile(started, streamed, source.len());
            self.trace_compile(url, source.len(), streamed, string_ms, started);
            self.loader.borrow_mut().stats.script_ms += started.elapsed().as_secs_f64() * 1000.0;
            if let (Some(m), Some(c)) = (module, cache.take()) {
                let g = v8::Global::new(tc, m);
                self.cache_outcome(c, consumed, CacheSource::Module(g));
            }
            module
        })();
        match r {
            Some(m) => {
                let hash = m.get_identity_hash().get();
                let mut modules = self.modules.borrow_mut();
                modules
                    .url_of
                    .insert(hash, (url.to_owned(), base.to_owned()));
                modules.map_of.insert(hash, map);
                drop(modules);
                Ok(v8::Global::new(tc, m))
            }
            // HTML "create a JavaScript module script" step 7: a parse error is the script's
            // parse error (and its error to rethrow), reported only when the script runs.
            None => {
                let e = tc.exception().map(|e| v8::Global::new(tc, e));
                tc.reset();
                Err(e)
            }
        }
    }

    /// HTML §8.1.5.4 "resolve a module specifier" for a script with base URL `base` in module map
    /// `map`: a Window's import map applies (and records the resolution); a worker's realm (map 0) has
    /// none. `Err` is the TypeError's message.
    pub(crate) fn resolve_module_specifier(
        &self,
        map: u64,
        specifier: &str,
        base: &str,
    ) -> Result<url::Url, String> {
        let Ok(base) = url::Url::parse(base) else {
            return resolve_specifier(specifier, base)
                .ok_or_else(|| format!("Failed to resolve module specifier \"{specifier}\""));
        };
        if map == 0 {
            return crate::import_map::GlobalImports::default().resolve(specifier, &base);
        }
        self.modules
            .borrow_mut()
            .imports
            .entry(map)
            .or_default()
            .resolve(specifier, &base)
    }

    /// HTML "register an import map": an import map parse result's error is reported, an import map
    /// merges into the current Window's.
    pub(crate) fn register_import_map(
        &self,
        cx: &mut Cx,
        result: Result<crate::import_map::ImportMap, v8::Global<v8::Value>>,
    ) {
        match result {
            Err(e) => self.report_module_exception(cx, e, None),
            Ok(map) => {
                let id = self.module_map_id(cx);
                let mut warnings = Vec::new();
                self.modules
                    .borrow_mut()
                    .imports
                    .entry(id)
                    .or_default()
                    .merge(map, &mut |w| warnings.push(w));
                for w in warnings {
                    self.console_print("warn", &w);
                }
            }
        }
    }

    /// HTML "create an import map parse result" from a `<script type=importmap>`'s text, in the
    /// current realm (the error to rethrow is one of its exceptions).
    pub(crate) fn parse_import_map(
        &self,
        cx: &mut Cx,
        source: &str,
        base: &url::Url,
    ) -> Result<crate::import_map::ImportMap, v8::Global<v8::Value>> {
        let mut warnings = Vec::new();
        let r = crate::import_map::parse(source, base, &mut |w| warnings.push(w));
        for w in warnings {
            self.console_print("warn", &w);
        }
        r.map_err(|e| {
            let scope = cx.scope();
            let (syntax, msg) = match e {
                crate::import_map::ParseError::Syntax(m) => (true, m),
                crate::import_map::ParseError::Type(m) => (false, m),
            };
            let msg = rt::name(scope, &msg);
            let e = if syntax {
                v8::Exception::syntax_error(scope, msg)
            } else {
                v8::Exception::type_error(scope, msg)
            };
            v8::Global::new(scope, e)
        })
    }

    /// HTML "resolve a module integrity metadata" for a module map key: the integrity the module map's
    /// import map gives the URL, if any.
    fn import_map_integrity(&self, key: &str) -> Option<String> {
        let (map, _, url) = split_module_key(key);
        let m = self.modules.borrow();
        m.imports.get(&map)?.integrity(url).map(str::to_owned)
    }

    /// The specifiers a module requests, resolved against its base (HTML "create a JavaScript module
    /// script" step 9): module map keys, `Err` with the TypeError's message for a specifier that does
    /// not resolve.
    fn module_requests(
        &self,
        cx: &mut Cx,
        module: &v8::Global<v8::Module>,
    ) -> Vec<std::result::Result<String, String>> {
        let scope = cx.scope();
        let m = v8::Local::new(scope, module);
        let hash = m.get_identity_hash().get();
        let (base, map) = {
            let modules = self.modules.borrow();
            (
                modules
                    .url_of
                    .get(&hash)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_default(),
                modules.map_of.get(&hash).copied().unwrap_or(0),
            )
        };
        let module_requests = m.get_module_requests();
        let mut requests = Vec::new();
        for i in 0..module_requests.length() {
            let Some(d) = module_requests.get(scope, i) else {
                continue;
            };
            let Ok(req) = v8::Local::<v8::ModuleRequest>::try_from(d) else {
                continue;
            };
            let spec = req.get_specifier().to_rust_string_lossy(scope);
            let attrs = req.get_import_attributes();
            let ty = snowlark_js::hooks::import_type(scope, attrs, 3)
                .filter(|t| supported_module_type(self, t));
            requests.push((spec, ty));
        }
        requests
            .into_iter()
            .map(|(spec, ty)| {
                let url = self.resolve_module_specifier(map, &spec, &base)?;
                // (HTML "fetch a single imported module script" step 3: a module type this user
                // agent does not allow fails like a fetch, see `fetch_module`)
                let ty = ty.unwrap_or_else(|| UNSUPPORTED_TYPE.to_owned());
                Ok(module_key(map, url.as_str(), &ty))
            })
            .collect()
    }

    /// A TypeError with `message`, in the current realm.
    fn type_error_value(&self, cx: &mut Cx, message: &str) -> v8::Global<v8::Value> {
        let scope = cx.scope();
        let msg = rt::name(scope, message);
        let e = v8::Exception::type_error(scope, msg);
        v8::Global::new(scope, e)
    }

    /// Starts fetching a module and its descendants (HTML "fetch the descendants of and link"); `key`
    /// is its module map key.
    pub(crate) fn fetch_module(&self, key: &str) {
        if self.modules.borrow().by_url.contains_key(key) {
            return;
        }
        let (_, ty, url) = split_module_key(key);
        let Ok(url) = url::Url::parse(url).map_err(|_| ()).and_then(|u| {
            if ty == UNSUPPORTED_TYPE {
                Err(())
            } else {
                Ok(u)
            }
        }) else {
            self.modules
                .borrow_mut()
                .by_url
                .insert(key.to_owned(), ModuleState::Failed);
            return;
        };
        self.modules
            .borrow_mut()
            .by_url
            .insert(key.to_owned(), ModuleState::Fetching);
        // A service worker's modules come from its script resource map, or are fetched into it
        // while it is parsed or installing (as its classic imports are); the result is taken in
        // a task, as a fetch's is.
        if self.is_service_worker_global() {
            let body = self.sw_imported_script(&url, true).ok().map(|s| {
                let (text, _) = encoding_rs::UTF_8.decode_with_bom_removal(&s.source);
                (text.into_owned(), s.final_url.to_string())
            });
            let key = key.to_owned();
            self.queue_task(crate::event_loop::TaskSource::Networking, move |e, cx| {
                e.module_fetched(cx, &key, body, None, None)
            });
            return;
        }
        // (a module fetched without integrity metadata of its own gets its import map's, HTML "get
        // the descendant script fetch options" and "fetch a module script graph")
        if let Some(integrity) = self.import_map_integrity(key) {
            let mut m = self.modules.borrow_mut();
            let o = m.options.entry(key.to_owned()).or_default();
            if !o.integrity_given && o.integrity.is_empty() {
                o.integrity = integrity;
            }
        }
        self.fetch_resource(url, crate::loader::Purpose::Module(key.to_owned()));
    }

    /// A JSON, CSS or text module: a synthetic module whose default export is the parsed JSON, a
    /// constructed style sheet or the text.
    /// `Err` is a JSON parse error (HTML "create a JSON module script" step 3: the script's parse
    /// error), `None` when there is no exception.
    fn synthetic_module(
        &self,
        cx: &mut Cx,
        ty: &str,
        text: &str,
        url: &str,
        map: u64,
    ) -> Result<v8::Global<v8::Module>, Option<v8::Global<v8::Value>>> {
        let value = if ty == "json" {
            match self.parse_json(cx, text.as_bytes()) {
                Ok(v) => v,
                Err(e) => return Err(Some(e.global().clone())),
            }
        } else if ty == "css" {
            // HTML "create a CSS module script": a constructed sheet of the module map's
            // document (the realm's), its rules the module's text.
            let doc = NodeId::from_bits(map)
                .filter(|&d| self.dom().is_alive(d))
                .or_else(|| self.document())
                .ok_or(None)?;
            let base = url::Url::parse(url).map_err(|_| None)?;
            let sheet = self.css_module_sheet(doc, text, &base);
            crate::engine::streams::handle_value(cx, sheet)
        } else {
            let scope = cx.scope();
            let s = v8::String::new(scope, text).ok_or(None)?;
            JsValue::new(scope, s.into())
        };
        let scope = cx.scope();
        let v = v8::Local::new(scope, value.global());
        let m = snowlark_js::hooks::synthetic_module(scope, url, v).ok_or(None)?;
        let hash = m.get_identity_hash().get();
        let mut modules = self.modules.borrow_mut();
        modules
            .url_of
            .insert(hash, (url.to_owned(), url.to_owned()));
        modules.map_of.insert(hash, map);
        drop(modules);
        Ok(v8::Global::new(scope, m))
    }

    /// A module script's fetch completed (or failed with `None`).
    pub(crate) fn module_fetched(
        &self,
        cx: &mut Cx,
        url: &str,
        body: Option<(String, String)>,
        streamer: Option<ScriptStreamer>,
        cache: Option<CacheInfo>,
    ) {
        let (map, ty, _) = split_module_key(url);
        let ty = ty.to_owned();
        let fetched = body.is_some();
        // `modulepreload` links waiting for this module: `load` if it was fetched (a module with a
        // syntax error is still a module script), `error` if not.
        let waiters: Vec<NodeId> = {
            let mut m = self.modules.borrow_mut();
            let (mine, rest) = std::mem::take(&mut m.preload_waiters)
                .into_iter()
                .partition(|(k, _)| k == url);
            m.preload_waiters = rest;
            mine.into_iter().map(|(_, el)| el).collect()
        };
        for el in waiters {
            self.queue_element_task_fire(el, if fetched { "load" } else { "error" }, false, false);
        }
        // (its fetch options, a `Referrer-Policy` header's policy in them; its base URL is its
        // response's URL)
        let options = self
            .modules
            .borrow()
            .options
            .get(url)
            .cloned()
            .unwrap_or_default();
        let base = body.as_ref().map(|(_, u)| u.clone()).unwrap_or_default();
        let module = match body {
            Some((source, final_url)) if ty == "javascript" => self.compile_module(
                cx, &source, &final_url, 1, &final_url, &options, map, streamer, cache,
            ),
            Some((source, final_url)) => self.synthetic_module(cx, &ty, &source, &final_url, map),
            None => Err(None),
        };
        match module {
            Err(Some(e)) => {
                self.modules
                    .borrow_mut()
                    .by_url
                    .insert(url.to_owned(), ModuleState::Errored(e));
            }
            Ok(m) => {
                let children = match self.module_children(cx, &m) {
                    Ok(c) => c,
                    Err(e) => {
                        self.modules
                            .borrow_mut()
                            .by_url
                            .insert(url.to_owned(), ModuleState::Errored(e));
                        self.check_module_graphs(cx);
                        return;
                    }
                };
                {
                    let mut map = self.modules.borrow_mut();
                    map.by_url.insert(url.to_owned(), ModuleState::Ready(m));
                    map.children.insert(url.to_owned(), children.clone());
                    // HTML "fetch the descendants of a module script": its descendant fetch
                    // options, its base URL the referrer.
                    let o = options.imported_from(&base);
                    for c in &children {
                        map.options.entry(c.clone()).or_insert_with(|| o.clone());
                    }
                }
                for c in children {
                    self.fetch_module(&c);
                }
            }
            Err(None) => {
                self.modules
                    .borrow_mut()
                    .by_url
                    .insert(url.to_owned(), ModuleState::Failed);
            }
        }
        self.check_module_graphs(cx);
    }

    /// An inline module script (`<script type=module>` without `src`).
    /// `line`: the source line its text starts on.
    pub(crate) fn inline_module(
        &self,
        cx: &mut Cx,
        el: NodeId,
        source: &str,
        base: &str,
        line: u32,
    ) {
        let n = {
            let mut m = self.modules.borrow_mut();
            m.inline_count += 1;
            m.inline_count
        };
        let map = self.module_map_id(cx);
        let key = module_key(map, &format!("{base}#inline-module-{n}"), "javascript");
        let options = self.element_module_options(el);
        match self.compile_module(cx, source, base, line, base, &options, map, None, None) {
            Ok(m) => {
                let children = self.module_children(cx, &m);
                let hash = {
                    let scope = cx.scope();
                    v8::Local::new(scope, &m).get_identity_hash().get()
                };
                self.modules
                    .borrow_mut()
                    .url_of
                    .insert(hash, (format!("{base}#inline-module-{n}"), base.to_owned()));
                let children = match children {
                    Ok(c) => c,
                    Err(e) => {
                        self.module_graph_done(cx, GraphDone::Element(el), GraphResult::Error(e));
                        return;
                    }
                };
                self.modules
                    .borrow_mut()
                    .by_url
                    .insert(key.clone(), ModuleState::Ready(m));
                self.modules
                    .borrow_mut()
                    .children
                    .insert(key.clone(), children.clone());
                // The inline module script's fetch options go to its imports, its base URL
                // (the document's base URL) as their referrer.
                {
                    let options = options.imported_from(base);
                    let mut map = self.modules.borrow_mut();
                    for c in &children {
                        map.options
                            .entry(c.clone())
                            .or_insert_with(|| options.clone());
                    }
                }
                for c in children {
                    self.fetch_module(&c);
                }
                self.modules
                    .borrow_mut()
                    .graphs
                    .push((key, GraphDone::Element(el)));
                self.check_module_graphs(cx);
            }
            Err(Some(e)) => {
                self.module_graph_done(cx, GraphDone::Element(el), GraphResult::Error(e))
            }
            Err(None) => self.module_graph_done(cx, GraphDone::Element(el), GraphResult::Null),
        }
    }

    /// The module map keys of a module's requests, or the TypeError of the first that does not
    /// resolve.
    fn module_children(
        &self,
        cx: &mut Cx,
        m: &v8::Global<v8::Module>,
    ) -> std::result::Result<Vec<String>, v8::Global<v8::Value>> {
        let mut children = Vec::new();
        for r in self.module_requests(cx, m) {
            match r {
                Ok(k) => children.push(k),
                Err(msg) => return Err(self.type_error_value(cx, &msg)),
            }
        }
        Ok(children)
    }

    /// Starts a graph load rooted at `url`.
    pub(crate) fn load_module_graph(&self, cx: &mut Cx, url: &url::Url, done: GraphDone) {
        self.load_module_graph_of_type(cx, url, "javascript", done);
    }

    /// Starts the graph load of an external module script element: the root's fetch options are
    /// the element's.
    pub(crate) fn load_element_module_graph(&self, cx: &mut Cx, url: &url::Url, el: NodeId) {
        let options = self.element_module_options(el);
        let key = module_key(self.module_map_id(cx), url.as_str(), "javascript");
        self.modules
            .borrow_mut()
            .options
            .entry(key)
            .or_insert(options);
        self.load_module_graph(cx, url, GraphDone::Element(el));
    }

    /// A script element's fetch options (HTML "prepare the script element" steps 23-29): its
    /// nonce, integrity metadata, parser metadata, credentials mode (its `crossorigin`'s CORS
    /// settings attribute credentials mode, "same-origin" without one) and referrer policy.
    pub(crate) fn element_module_options(&self, el: NodeId) -> ModuleOptions {
        let integrity = self
            .dom()
            .attr(el, &ns!(), &local_name!("integrity"))
            .map(str::to_owned);
        let referrer_policy = self.referrer_policy_attr(el);
        let parser_inserted = self.script_parser_inserted(el);
        ModuleOptions {
            nonce: self.nonce_of(el),
            integrity_given: integrity.is_some(),
            integrity: integrity.unwrap_or_default(),
            parser_metadata: if parser_inserted {
                snowlark_net::ParserMetadata::ParserInserted
            } else {
                snowlark_net::ParserMetadata::NotParserInserted
            },
            render_blocking: self.render_blocking_element(el, false),
            worker: None,
            credentials: self
                .cors_settings(el)
                .unwrap_or(snowlark_net::CredentialsMode::SameOrigin),
            referrer_policy,
            referrer: String::new(),
        }
    }

    /// Starts a graph load rooted at a module of type `ty` at `url`.
    pub(crate) fn load_module_graph_of_type(
        &self,
        cx: &mut Cx,
        url: &url::Url,
        ty: &str,
        done: GraphDone,
    ) {
        let key = module_key(self.module_map_id(cx), url.as_str(), ty);
        self.fetch_module(&key);
        self.modules.borrow_mut().graphs.push((key, done));
        self.check_module_graphs(cx);
    }

    /// Completes the graph loads whose modules are all fetched (or one failed).
    fn check_module_graphs(&self, cx: &mut Cx) {
        // A module a graph reaches that the map no longer has (its earlier fetch failed) is
        // fetched again (HTML "fetch a single module script" steps 5-7 find no entry).
        let missing: Vec<String> = {
            let m = self.modules.borrow();
            let mut seen = std::collections::HashSet::new();
            m.graphs
                .iter()
                .flat_map(|(root, _)| missing_modules(&m, root))
                .filter(|k| seen.insert(k.clone()))
                .collect()
        };
        for k in missing {
            self.fetch_module(&k);
        }
        loop {
            // The graphs whose modules are all fetched (or one failed), with their results.
            let finished: Vec<(String, GraphDone, GraphResult)> = {
                let mut m = self.modules.borrow_mut();
                let mut finished = Vec::new();
                let mut i = 0;
                while i < m.graphs.len() {
                    let Some(state) = graph_state(&m, &m.graphs[i].0) else {
                        i += 1;
                        continue;
                    };
                    let (root, done) = m.graphs.remove(i);
                    let result = match state {
                        GraphState::Ok => match m.by_url.get(&root) {
                            Some(ModuleState::Ready(module)) => GraphResult::Module(module.clone()),
                            _ => GraphResult::Null,
                        },
                        GraphState::ParseError(e) => GraphResult::Error(e),
                        GraphState::Failed => GraphResult::Null,
                    };
                    finished.push((root, done, result));
                }
                // A fetch that failed leaves the module map before the loads waiting on it are
                // given its null result, so one they start then fetches it again (HTML "fetch a
                // single module script": "remove moduleMap[(url, moduleType)]", then each
                // callback runs given null).
                m.by_url.retain(|_, s| !matches!(s, ModuleState::Failed));
                finished
            };
            if finished.is_empty() {
                return;
            }
            for (root, done, result) in finished {
                // HTML §8.1.6.1 HostLoadImportedModule: the graph belongs to the realm whose
                // module map it was loaded into (a frame's `import()`), whatever realm a fetch
                // completes in; that realm links and evaluates it. (A worker has one realm: map 0.)
                let doc = snowlark_dom::NodeId::from_bits(split_module_key(&root).0);
                match doc.filter(|&d| self.context_of_document(d).is_some()) {
                    Some(d) => self.in_document_realm(cx, d, |cx| {
                        self.module_graph_done_in(cx, done, result, Some(d))
                    }),
                    None => self.module_graph_done_in(cx, done, result, None),
                }
            }
        }
    }

    /// ECMA-262 ContinueDynamicImport's reaction to the load: links and evaluates the oldest
    /// loaded `import()`'s module, in the realm that loaded it.
    pub(crate) fn continue_dynamic_import(&self, cx: &mut Cx) {
        let Some((doc, module, resolver)) = self.modules.borrow_mut().imports_loaded.pop_front()
        else {
            return;
        };
        match doc.filter(|&d| self.context_of_document(d).is_some()) {
            Some(d) => {
                self.in_document_realm(cx, d, |cx| self.run_module(cx, &module, Some(resolver)))
            }
            // (a realm a navigation replaced since: its import never settles, as its other tasks)
            None if doc.is_some() => {}
            None => self.run_module(cx, &module, Some(resolver)),
        }
    }

    fn module_graph_done(&self, cx: &mut Cx, done: GraphDone, result: GraphResult) {
        self.module_graph_done_in(cx, done, result, None)
    }

    fn module_graph_done_in(
        &self,
        cx: &mut Cx,
        done: GraphDone,
        result: GraphResult,
        doc: Option<NodeId>,
    ) {
        match done {
            GraphDone::Element(el) => self.module_script_ready(cx, el, result),
            GraphDone::Import(resolver) => match result {
                // ECMA-262 ContinueDynamicImport links and evaluates the module in a reaction to
                // its load promise, a microtask: never inside the `import()` call itself, where a
                // module importing itself would still be evaluating (its graph loads at once when
                // the module map has it).
                GraphResult::Module(m) => {
                    self.modules
                        .borrow_mut()
                        .imports_loaded
                        .push_back((doc, m, resolver));
                    self.queue_continue_dynamic_import(cx);
                }
                GraphResult::Error(e) => {
                    let scope = cx.scope();
                    let r = v8::Local::new(scope, &resolver);
                    let e = v8::Local::new(scope, &e);
                    r.reject(scope, e);
                }
                GraphResult::Null => {
                    let scope = cx.scope();
                    let r = v8::Local::new(scope, &resolver);
                    let msg = rt::name(scope, "Failed to fetch dynamically imported module");
                    let e = v8::Exception::type_error(scope, msg);
                    r.reject(scope, e);
                }
            },
            // HTML §10.2.4 "run a worker", the module case's onComplete: a script that is null or
            // has an error to rethrow fails the worker (a plain `error` at the Worker object);
            // else it runs.
            GraphDone::WorkerMain => match result {
                GraphResult::Module(m) => {
                    self.run_module(cx, &m, None);
                    self.microtask_checkpoint(cx);
                    self.enable_worker_messages();
                }
                GraphResult::Error(_) | GraphResult::Null => self.worker_load_failed(),
            },
            GraphDone::ServiceWorkerMain => {
                let ok = match result {
                    GraphResult::Module(m) => self.run_service_worker_module(cx, &m),
                    GraphResult::Error(_) | GraphResult::Null => false,
                };
                self.service_worker_evaluated(cx, ok);
            }
        }
    }

    /// W3C Service Workers §8 Run Service Worker step 9.5 for a module script: evaluated without
    /// reporting its error; a graph with top-level await is refused (§8 Update step 8, "Is Async
    /// Module"). Whether it ran to completion.
    fn run_service_worker_module(&self, cx: &mut Cx, module: &v8::Global<v8::Module>) -> bool {
        self.enter_script();
        let ok = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let m = v8::Local::new(tc, module);
            let linked = m.get_status() != v8::ModuleStatus::Uninstantiated
                || snowlark_js::hooks::instantiate(tc, m).unwrap_or(false);
            let ok = linked && !m.is_graph_async() && {
                let result = m.evaluate(tc);
                let rejected = result
                    .and_then(|v| v8::Local::<v8::Promise>::try_from(v).ok())
                    .map(|p| {
                        p.mark_as_handled();
                        p.state() == v8::PromiseState::Rejected
                    });
                rejected == Some(false)
            };
            tc.reset();
            ok
        };
        self.leave_script(cx);
        ok
    }

    /// HTML §8.1.4.3 "run a module script": link, evaluate, report a rejection.
    pub(crate) fn run_module(
        &self,
        cx: &mut Cx,
        module: &v8::Global<v8::Module>,
        resolver: Option<v8::Global<v8::PromiseResolver>>,
    ) {
        self.enter_script();
        {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let m = v8::Local::new(tc, module);
            let linked = m.get_status() != v8::ModuleStatus::Uninstantiated
                || snowlark_js::hooks::instantiate(tc, m).unwrap_or(false);
            let result = if linked { m.evaluate(tc) } else { None };
            match result {
                Some(v) => {
                    let promise = v8::Local::<v8::Promise>::try_from(v).ok();
                    if let Some(p) = promise {
                        // ("run a module script" step 9 reacts to its rejection, reporting the
                        // exception or rejecting `import()`'s promise: it is never unhandled)
                        p.mark_as_handled();
                        let g = v8::Global::new(tc, p);
                        self.modules.borrow_mut().evaluating.push((
                            g,
                            resolver,
                            Some(module.clone()),
                        ));
                    } else if let Some(r) = resolver {
                        let r = v8::Local::new(tc, &r);
                        let ns = m.get_module_namespace();
                        r.resolve(tc, ns);
                    }
                }
                None => {
                    let e = tc.exception();
                    let msg = tc.message();
                    tc.reset();
                    if let Some(e) = e {
                        match resolver {
                            Some(r) => {
                                let r = v8::Local::new(tc, &r);
                                r.reject(tc, e);
                            }
                            None => self.report_caught(tc, e, msg),
                        }
                    }
                }
            }
        }
        self.leave_script(cx);
    }

    /// Settled module evaluation promises: report rejections, resolve `import()` promises.
    /// Returns whether any had settled.
    fn check_module_evaluations(&self, cx: &mut Cx) -> bool {
        let pending = std::mem::take(&mut self.modules.borrow_mut().evaluating);
        if pending.is_empty() {
            return false;
        }
        let count = pending.len();
        let mut keep = Vec::new();
        for (p, resolver, module) in pending {
            let scope = cx.scope();
            let pl = v8::Local::new(scope, &p);
            match pl.state() {
                v8::PromiseState::Pending => keep.push((p, resolver, module)),
                v8::PromiseState::Fulfilled => {
                    if let (Some(r), Some(m)) = (resolver, module) {
                        let r = v8::Local::new(scope, &r);
                        let m = v8::Local::new(scope, &m);
                        let ns = m.get_module_namespace();
                        r.resolve(scope, ns);
                    }
                }
                v8::PromiseState::Rejected => {
                    let e = pl.result(scope);
                    match resolver {
                        Some(r) => {
                            let r = v8::Local::new(scope, &r);
                            r.reject(scope, e);
                        }
                        None => {
                            let g = v8::Global::new(scope, e);
                            self.report_module_exception(cx, g, None);
                        }
                    }
                }
            }
        }
        let settled = keep.len() < count;
        self.modules.borrow_mut().evaluating.extend(keep);
        settled
    }

    // ----- promise rejections (HTML §8.1.5.7) -----------------------------------------------------------

    fn notify_rejected_promises(&self, cx: &mut Cx) {
        let list = std::mem::take(&mut *self.about_to_be_notified.borrow_mut());
        for r in list {
            let still_unhandled = {
                let scope = cx.scope();
                let p = v8::Local::new(scope, &r.promise);
                !p.has_handler()
            };
            if !still_unhandled {
                continue;
            }
            self.queue_task(
                crate::event_loop::TaskSource::DomManipulation,
                move |e, cx| {
                    e.fire_unhandled_rejection(cx, r);
                },
            );
        }
    }

    /// The global a promise's rejection events go to: that of the realm that made the promise, while
    /// that Window is its browsing context's. A realm a navigation replaced fires nothing, and no
    /// other window may be handed the promise and its reason.
    fn window_of_promise(&self, cx: &mut Cx, promise: &v8::Global<v8::Promise>) -> Option<Handle> {
        let scope = cx.scope();
        let p = v8::Local::new(scope, promise);
        let window = rt::realm_global(p.get_creation_context(scope)?)?;
        self.context_of_window(window).map(|_| window)
    }

    fn fire_unhandled_rejection(&self, cx: &mut Cx, r: Rejection) {
        let Some(window) = self.window_of_promise(cx, &r.promise) else {
            return;
        };
        let (promise, reason) = {
            let scope = cx.scope();
            let p = v8::Local::new(scope, &r.promise);
            if p.has_handler() {
                return;
            }
            let po: v8::Local<v8::Object> = p.into();
            (
                JsObject::new(scope, po),
                r.reason.clone().map(JsValue::from_global),
            )
        };
        let reason_text = {
            let scope = cx.scope();
            r.reason
                .as_ref()
                .map(|g| v8::Local::new(scope, g).to_rust_string_lossy(scope))
                .unwrap_or_else(|| "undefined".into())
        };
        let ev = self.trusted_event(
            InterfaceId::PromiseRejectionEvent,
            "unhandledrejection",
            false,
            true,
            crate::engine::events::EventExt::Rejection {
                promise: crate::engine::held::Held::object(promise),
                reason: reason.map(crate::engine::held::Held::value),
            },
        );
        let not_canceled = self.dispatch(cx, ev, window, false);
        {
            // (the outstanding rejected promises *weak* set: it keeps none of them, and with them
            // their realms, alive)
            let scope = cx.scope();
            let p = v8::Local::new(scope, &r.promise);
            let mut n = self.notified_rejections.borrow_mut();
            n.retain(|w| !w.is_empty());
            n.push(v8::Weak::new(scope, p));
        }
        if not_canceled {
            self.console_rejection(cx, r.reason.as_ref(), &reason_text);
        }
    }

    /// The tracker's "handle" operation: a handler was added to a rejected promise.
    fn rejection_handled(&self, cx: &mut Cx, promise: v8::Local<v8::Promise>) {
        let scope = cx.scope();
        self.about_to_be_notified
            .borrow_mut()
            .retain(|r| v8::Local::new(scope, &r.promise) != promise);
        let notified = {
            let mut n = self.notified_rejections.borrow_mut();
            let before = n.len();
            n.retain(|p| p.to_local(scope).is_some_and(|p| p != promise));
            before != n.len()
        };
        if notified {
            let g = v8::Global::new(scope, promise);
            self.queue_task(
                crate::event_loop::TaskSource::DomManipulation,
                move |e, cx| {
                    let Some(window) = e.window_of_promise(cx, &g) else {
                        return;
                    };
                    let (promise, reason) = {
                        let scope = cx.scope();
                        let p = v8::Local::new(scope, &g);
                        let reason = p.result(scope);
                        let po: v8::Local<v8::Object> = p.into();
                        (JsObject::new(scope, po), Some(JsValue::new(scope, reason)))
                    };
                    let ev = e.trusted_event(
                        InterfaceId::PromiseRejectionEvent,
                        "rejectionhandled",
                        false,
                        false,
                        crate::engine::events::EventExt::Rejection {
                            promise: crate::engine::held::Held::object(promise),
                            reason: reason.map(crate::engine::held::Held::value),
                        },
                    );
                    e.dispatch(cx, ev, window, false);
                },
            );
        }
    }

    // ----- structured clone (HTML §2.7.3) -----------------------------------------------------------

    /// StructuredSerialize then StructuredDeserialize through V8's value serializer.
    pub(crate) fn structured_clone_value(
        &self,
        cx: &mut Cx,
        value: &JsValue,
    ) -> rt::Result<JsValue> {
        let scope = cx.scope();
        let context = scope.get_current_context();
        let v = v8::Local::new(scope, value.global());
        let ser = v8::ValueSerializer::new(scope, Box::new(CloneDelegate));
        ser.write_header();
        // (the same realm: WebAssembly modules are shared)
        let (written, modules) = self.sharing_wasm_modules(0, || ser.write_value(context, v));
        if written != Some(true) {
            return Err(rt::Error::Pending);
        }
        let bytes = ser.release();
        let prev = self.receive_modules(modules);
        let de = v8::ValueDeserializer::new(scope, Box::new(CloneDelegate), &bytes);
        let out = if de.read_header(context) == Some(true) {
            de.read_value(context)
        } else {
            None
        };
        self.transfer_in.replace(prev);
        let out = out.ok_or(rt::Error::Pending)?;
        Ok(JsValue::new(scope, out))
    }

    /// Puts `modules` beside the bytes about to be deserialized (from index 0); what was there
    /// before, to put back after.
    pub(crate) fn receive_modules(
        &self,
        modules: Vec<v8::CompiledWasmModule>,
    ) -> Vec<Option<crate::engine::offscreen::Transferred>> {
        use crate::engine::offscreen::Transferred;
        self.transfer_in.replace(
            modules
                .into_iter()
                .map(|m| Some(Transferred::WasmModule(m)))
                .collect(),
        )
    }

    /// StructuredClone with what it throws (a DataCloneError) caught: `Err` holds the exception.
    pub(crate) fn structured_clone_caught(
        &self,
        cx: &mut Cx,
        value: &JsValue,
    ) -> Result<JsValue, JsValue> {
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let context = tc.get_current_context();
        let v = v8::Local::new(tc, value.global());
        let ser = v8::ValueSerializer::new(tc, Box::new(CloneDelegate));
        ser.write_header();
        let mut out = None;
        let (written, modules) = self.sharing_wasm_modules(0, || ser.write_value(context, v));
        if written == Some(true) {
            let bytes = ser.release();
            let prev = self.receive_modules(modules);
            let de = v8::ValueDeserializer::new(tc, Box::new(CloneDelegate), &bytes);
            if de.read_header(context) == Some(true) {
                out = de.read_value(context);
            }
            self.transfer_in.replace(prev);
        }
        if let Some(v) = out {
            return Ok(JsValue::new(tc, v));
        }
        let ex = tc.exception();
        let e = match ex {
            Some(e) => JsValue::new(tc, e),
            None => rt::undefined_value(tc),
        };
        if !tc.has_terminated() {
            tc.reset();
        }
        Err(e)
    }

    /// Runs `f`, a serialization, as StructuredSerializeForStorage: with forStorage true, the
    /// serializable objects that only cross within an agent cluster throw a DataCloneError.
    pub(crate) fn serializing_for_storage<R>(&self, f: impl FnOnce() -> R) -> R {
        let prev = self.for_storage.replace(true);
        let r = f();
        self.for_storage.set(prev);
        r
    }

    /// StructuredSerialize to bytes (for a value crossing to another isolate).
    pub(crate) fn serialize_to_bytes(&self, cx: &mut Cx, value: &JsValue) -> rt::Result<Vec<u8>> {
        let scope = cx.scope();
        let context = scope.get_current_context();
        let v = v8::Local::new(scope, value.global());
        let ser = v8::ValueSerializer::new(scope, Box::new(CloneDelegate));
        ser.write_header();
        if ser.write_value(context, v) != Some(true) {
            return Err(rt::Error::Pending);
        }
        Ok(ser.release())
    }

    /// StructuredDeserialize from bytes; `Err` when they do not deserialize (a `messageerror`).
    pub(crate) fn deserialize_value(&self, cx: &mut Cx, bytes: &[u8]) -> Result<JsValue, ()> {
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let context = tc.get_current_context();
        let de = v8::ValueDeserializer::new(tc, Box::new(CloneDelegate), bytes);
        if de.read_header(context) != Some(true) {
            tc.reset();
            return Err(());
        }
        match de.read_value(context) {
            Some(v) => Ok(JsValue::new(tc, v)),
            None => {
                tc.reset();
                Err(())
            }
        }
    }

    /// Fires a `message` event at the window (postMessage to itself).
    pub(crate) fn fire_message_event(
        &self,
        cx: &mut Cx,
        target: Handle,
        data: JsValue,
        origin: &str,
        source: Handle,
        ports: Vec<Handle>,
    ) {
        let ev = self.trusted_event(
            InterfaceId::MessageEvent,
            "message",
            false,
            false,
            crate::engine::events::EventExt::Message {
                data: Some(crate::engine::held::Held::value(data)),
                origin: origin.to_owned(),
                last_event_id: String::new(),
                source: Some(source),
                ports,
            },
        );
        self.dispatch(cx, ev, target, false);
    }
}

/// Whether every module reachable from `root` is fetched: `Some(true)` when the graph is ready,
/// `Some(false)` when one failed, `None` while fetching.
/// Where a module graph load stands.
enum GraphState {
    /// Every module is fetched and has its requests resolved.
    Ok,
    /// A fetch failed.
    Failed,
    /// Every module is fetched; this is the first parse error in the graph (depth first, in request
    /// order: HTML "find the first parse error").
    ParseError(v8::Global<v8::Value>),
}

/// The state of the graph rooted at `root`, `None` while a module is being fetched.
fn graph_state(m: &ModuleMap, root: &str) -> Option<GraphState> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root.to_owned()];
    let mut fetching = false;
    let mut first_error = None;
    while let Some(u) = stack.pop() {
        if !seen.insert(u.clone()) {
            continue;
        }
        match m.by_url.get(&u) {
            None | Some(ModuleState::Fetching) => fetching = true,
            Some(ModuleState::Failed) => return Some(GraphState::Failed),
            Some(ModuleState::Errored(e)) => {
                if first_error.is_none() {
                    first_error = Some(e.clone());
                }
            }
            Some(ModuleState::Ready(_)) => {
                if let Some(children) = m_children(m, &u) {
                    stack.extend(children.into_iter().rev());
                }
            }
        }
    }
    if fetching {
        None
    } else if let Some(e) = first_error {
        Some(GraphState::ParseError(e))
    } else {
        Some(GraphState::Ok)
    }
}

/// The modules of the graph rooted at `root` that the map has no entry for, below fetched ones.
fn missing_modules(m: &ModuleMap, root: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root.to_owned()];
    let mut missing = Vec::new();
    while let Some(u) = stack.pop() {
        if !seen.insert(u.clone()) {
            continue;
        }
        match m.by_url.get(&u) {
            None => missing.push(u),
            Some(ModuleState::Ready(_)) => {
                if let Some(children) = m_children(m, &u) {
                    stack.extend(children);
                }
            }
            Some(_) => {}
        }
    }
    missing
}

/// The resolved requests of a module, recorded when it was compiled.
fn m_children(m: &ModuleMap, url: &str) -> Option<Vec<String>> {
    m.children.get(url).cloned()
}

struct CloneDelegate;

impl v8::ValueSerializerImpl for CloneDelegate {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        message: v8::Local<'s, v8::String>,
    ) {
        let m = message.to_rust_string_lossy(scope);
        rt::throw(
            scope,
            rt::Error::Dom(ExceptionName::DataCloneError, m.into()),
        );
    }

    /// HTML §2.7.3 StructuredSerializeInternal steps 25–26: a serializable platform object writes its
    /// serialization steps' data (a tag, then the bytes); any other throws a "DataCloneError".
    fn write_host_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        object: v8::Local<'s, v8::Object>,
        value_serializer: &dyn v8::ValueSerializerHelper,
    ) -> Option<bool> {
        use snowlark_bindings::generated::ids;
        if let Some(h) = rt::unwrap(&object, ids::CRYPTO_KEY, ids::CRYPTO_KEY_END) {
            let st = rt::state(scope);
            if let Some(bytes) = st.engine::<Engine>().serialize_crypto_key(h) {
                value_serializer.write_uint32(HOST_CRYPTO_KEY);
                value_serializer.write_uint32(bytes.len() as u32);
                value_serializer.write_raw_bytes(&bytes);
                return Some(true);
            }
        }
        // HTML §9.4.4: a MessagePort in the transfer list is written as the port it becomes.
        if let Some(h) = rt::unwrap(&object, ids::MESSAGE_PORT, ids::MESSAGE_PORT_END) {
            let st = rt::state(scope);
            if let Some(new) = st.engine::<Engine>().transferred_port(h) {
                value_serializer.write_uint32(HOST_PORT);
                let bits = new.get();
                value_serializer.write_uint32(bits as u32);
                value_serializer.write_uint32((bits >> 32) as u32);
                return Some(true);
            }
        }
        // HTML §2.7.5 StructuredSerializeWithTransfer: an OffscreenCanvas in the transfer list is
        // written as its index there; its data crosses beside the bytes.
        if let Some(h) = rt::unwrap(&object, ids::OFFSCREEN_CANVAS, ids::OFFSCREEN_CANVAS_END) {
            let st = rt::state(scope);
            if let Some(i) = st.engine::<Engine>().transfer_index(h) {
                value_serializer.write_uint32(HOST_TRANSFERRED);
                value_serializer.write_uint32(i);
                return Some(true);
            }
        }
        // File API §3.1 / §4.1: a Blob's serialization is its type and bytes; a File's also its name
        // and last modification date.
        if let Some(h) = rt::unwrap(&object, ids::BLOB, ids::BLOB_END) {
            let st = rt::state(scope);
            let engine = st.engine::<Engine>();
            let (bytes, type_) = engine.blob_contents(h);
            let file = engine.blob_file(h);
            value_serializer.write_uint32(HOST_BLOB);
            let write_str = |s: &str| {
                value_serializer.write_uint32(s.len() as u32);
                value_serializer.write_raw_bytes(s.as_bytes());
            };
            write_str(&type_);
            match &file {
                Some((name, modified)) => {
                    value_serializer.write_uint32(1);
                    write_str(name);
                    value_serializer.write_double(*modified as f64);
                }
                None => value_serializer.write_uint32(0),
            }
            value_serializer.write_uint32(bytes.len() as u32);
            value_serializer.write_raw_bytes(&bytes);
            return Some(true);
        }
        // HTML §4.12.5 ImageData's serialization steps: its data, width and height (its color
        // space and pixel format are always "srgb" and "rgba-unorm8" here).
        if let Some(h) = rt::unwrap(&object, ids::IMAGE_DATA, ids::IMAGE_DATA_END) {
            let st = rt::state(scope);
            let image = {
                let mut cx = Cx::new(scope);
                st.engine::<Engine>().image_data_bytes(&mut cx, h)
            };
            if let Some((width, height, bytes)) = image {
                value_serializer.write_uint32(HOST_IMAGE_DATA);
                value_serializer.write_uint32(width);
                value_serializer.write_uint32(height);
                value_serializer.write_uint32(bytes.len() as u32);
                value_serializer.write_raw_bytes(&bytes);
                return Some(true);
            }
        }
        // HTML §8.10 ImageBitmap's serialization steps: a copy of its bitmap data (one in the
        // transfer list goes the same way, and its transfer steps detach it afterwards).
        if let Some(h) = rt::unwrap(&object, ids::IMAGE_BITMAP, ids::IMAGE_BITMAP_END) {
            let made = rt::state(scope).engine::<Engine>().bitmap_serialization(h);
            match made {
                Ok((frame, straight)) => {
                    value_serializer.write_uint32(HOST_IMAGE_BITMAP);
                    value_serializer.write_uint32(frame.width);
                    value_serializer.write_uint32(frame.height);
                    value_serializer.write_uint32(u32::from(straight));
                    value_serializer.write_uint32(frame.pixels.len() as u32);
                    value_serializer.write_raw_bytes(&frame.pixels);
                    return Some(true);
                }
                Err(m) => {
                    rt::throw(
                        scope,
                        rt::Error::Dom(ExceptionName::DataCloneError, m.into()),
                    );
                    return None;
                }
            }
        }
        rt::throw(
            scope,
            rt::Error::Dom(
                ExceptionName::DataCloneError,
                "A platform object could not be cloned".into(),
            ),
        );
        None
    }

    /// WebAssembly JS API, Module's serialization steps: within the agent cluster, the module
    /// goes beside the bytes ([`Engine::sharing_wasm_modules`]); for storage, or where nothing
    /// takes it, "throw a DataCloneError".
    fn get_wasm_module_transfer_id(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        module: v8::Local<v8::WasmModuleObject>,
    ) -> Option<u32> {
        let st = rt::state(scope);
        if let Some(id) = st
            .engine::<Engine>()
            .share_wasm_module(module.get_compiled_module())
        {
            return Some(id);
        }
        rt::throw(
            scope,
            rt::Error::Dom(
                ExceptionName::DataCloneError,
                "A WebAssembly.Module cannot be serialized here".into(),
            ),
        );
        None
    }
}

/// The tag of a serialized CryptoKey (Web Cryptography §13.5) in a host object's data.
const HOST_CRYPTO_KEY: u32 = 1;
/// The tag of a serialized Blob or File.
const HOST_BLOB: u32 = 2;
/// The tag of a transferred MessagePort (the engine's handle of the port it became).
const HOST_PORT: u32 = 3;
/// The tag of a serialized ImageData: width, height, then its bytes.
const HOST_IMAGE_DATA: u32 = 4;
/// The tag of a transferred object: its index among those transferred beside the bytes.
const HOST_TRANSFERRED: u32 = 5;
/// The tag of a serialized ImageBitmap: width, height, whether its alpha is straight, then its
/// pixels (premultiplied BGRA unless straight, top row first).
const HOST_IMAGE_BITMAP: u32 = 6;

/// A serialized Blob or File: (type, (name, last modified) for a File, bytes).
type BlobRecord = (String, Option<(String, i64)>, Vec<u8>);

fn read_blob(d: &dyn v8::ValueDeserializerHelper) -> Option<BlobRecord> {
    let read_bytes = |d: &dyn v8::ValueDeserializerHelper| {
        let mut len = 0u32;
        if !d.read_uint32(&mut len) {
            return None;
        }
        d.read_raw_bytes(len as usize).map(<[u8]>::to_vec)
    };
    let type_ = String::from_utf8(read_bytes(d)?).ok()?;
    let mut is_file = 0u32;
    if !d.read_uint32(&mut is_file) {
        return None;
    }
    let file = if is_file == 1 {
        let name = String::from_utf8(read_bytes(d)?).ok()?;
        let mut modified = 0f64;
        if !d.read_double(&mut modified) {
            return None;
        }
        Some((name, modified as i64))
    } else {
        None
    };
    Some((type_, file, read_bytes(d)?))
}

impl v8::ValueDeserializerImpl for CloneDelegate {
    /// StructuredDeserialize step for serializable platform objects: a new object of the tag's
    /// interface, from its deserialization steps.
    fn read_host_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        value_deserializer: &dyn v8::ValueDeserializerHelper,
    ) -> Option<v8::Local<'s, v8::Object>> {
        let (mut tag, mut len) = (0u32, 0u32);
        let ok = value_deserializer.read_uint32(&mut tag);
        if ok && tag == HOST_PORT {
            let (mut lo, mut hi) = (0u32, 0u32);
            if value_deserializer.read_uint32(&mut lo) && value_deserializer.read_uint32(&mut hi) {
                let bits = u64::from(lo) | (u64::from(hi) << 32);
                let st = rt::state(scope);
                let engine = st.engine::<Engine>();
                // (from another event loop: the port it transferred under that key, made here)
                let h = match engine.port_for_key(bits) {
                    None => Handle::new(bits),
                    Some(Some(h)) => h,
                    Some(None) => return None,
                };
                let received = {
                    let mut cx = Cx::new(scope);
                    engine.receive_port(&mut cx, h)
                };
                if received {
                    return rt::wrap(scope, h);
                }
            }
        }
        if ok && tag == HOST_TRANSFERRED && value_deserializer.read_uint32(&mut len) {
            let st = rt::state(scope);
            if let Some(h) = st.engine::<Engine>().receive_transferred(len) {
                return rt::wrap(scope, h);
            }
        }
        if ok && tag == HOST_BLOB {
            if let Some((type_, file, bytes)) = read_blob(value_deserializer) {
                let st = rt::state(scope);
                let engine = st.engine::<Engine>();
                let h = match file {
                    Some((name, modified)) => engine.new_file(bytes, &type_, &name, modified),
                    None => engine.new_blob(bytes, &type_),
                };
                return rt::wrap(scope, h);
            }
        }
        if ok && tag == HOST_IMAGE_DATA {
            let (mut width, mut height) = (0u32, 0u32);
            if value_deserializer.read_uint32(&mut width)
                && value_deserializer.read_uint32(&mut height)
                && value_deserializer.read_uint32(&mut len)
            {
                if let Some(bytes) = value_deserializer.read_raw_bytes(len as usize) {
                    let bytes = bytes.to_vec();
                    let st = rt::state(scope);
                    let made = {
                        let mut cx = Cx::new(scope);
                        st.engine::<Engine>()
                            .new_image_data(&mut cx, width, height, Some(&bytes))
                    };
                    if let Ok(h) = made {
                        return rt::wrap(scope, h);
                    }
                }
            }
        }
        // HTML §8.10 ImageBitmap's deserialization steps: "Set value's bitmap data to
        // serialized.[[BitmapData]]" (only an origin-clean bitmap is serialized).
        if ok && tag == HOST_IMAGE_BITMAP {
            let (mut width, mut height, mut straight) = (0u32, 0u32, 0u32);
            if value_deserializer.read_uint32(&mut width)
                && value_deserializer.read_uint32(&mut height)
                && value_deserializer.read_uint32(&mut straight)
                && value_deserializer.read_uint32(&mut len)
                && len as u64 == width as u64 * height as u64 * 4
            {
                if let Some(bytes) = value_deserializer.read_raw_bytes(len as usize) {
                    let frame = snowlark_paint::image::Frame {
                        width,
                        height,
                        pixels: bytes.to_vec(),
                        delay_ms: 0,
                    };
                    let st = rt::state(scope);
                    let h = st.engine::<Engine>().new_bitmap(frame, true, straight == 1);
                    return rt::wrap(scope, h);
                }
            }
        }
        if ok && tag == HOST_CRYPTO_KEY && value_deserializer.read_uint32(&mut len) {
            if let Some(bytes) = value_deserializer.read_raw_bytes(len as usize) {
                let bytes = bytes.to_vec();
                let st = rt::state(scope);
                if let Some(h) = st.engine::<Engine>().deserialize_crypto_key(&bytes) {
                    return rt::wrap(scope, h);
                }
            }
        }
        rt::throw(
            scope,
            rt::Error::Dom(
                ExceptionName::DataCloneError,
                "A serialized platform object could not be read".into(),
            ),
        );
        None
    }

    /// WebAssembly JS API, Module's deserialization steps: a Module of the compiled module that
    /// came beside the bytes (shared, not compiled again).
    fn get_wasm_module_from_id<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        clone_id: u32,
    ) -> Option<v8::Local<'s, v8::WasmModuleObject>> {
        let st = rt::state(scope);
        let module = st.engine::<Engine>().receive_wasm_module(clone_id);
        if let Some(m) = module.and_then(|m| v8::WasmModuleObject::from_compiled_module(scope, &m))
        {
            return Some(m);
        }
        rt::throw(
            scope,
            rt::Error::Dom(
                ExceptionName::DataCloneError,
                "A serialized WebAssembly.Module could not be read".into(),
            ),
        );
        None
    }
}

// ----- the V8 hooks ---------------------------------------------------------------------------------------

/// The module resolution hook (installed with `snowlark_js::hooks::set_resolve_hook`).
pub fn resolve_hook<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
    module_type: &str,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    let hash = referrer.get_identity_hash().get();
    let (base, map) = {
        let modules = engine.modules.borrow();
        (
            modules
                .url_of
                .get(&hash)
                .map(|(_, b)| b.clone())
                .unwrap_or_default(),
            modules.map_of.get(&hash).copied().unwrap_or(0),
        )
    };
    let resolved = engine.resolve_module_specifier(map, specifier, &base).ok();
    let module = resolved.and_then(|u| {
        match engine
            .modules
            .borrow()
            .by_url
            .get(&module_key(map, u.as_str(), module_type))
        {
            Some(ModuleState::Ready(m)) => Some(m.clone()),
            _ => None,
        }
    });
    match module {
        Some(m) => Some(v8::Local::new(scope, &m)),
        None => {
            let msg = rt::name(
                scope,
                &format!("Failed to resolve module specifier \"{specifier}\""),
            );
            let e = v8::Exception::type_error(scope, msg);
            scope.throw_exception(e);
            None
        }
    }
}

fn import_meta_resolve(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // (the data is the module's map and base URL, "<map> <base>")
    let data = args.data().to_rust_string_lossy(scope);
    let (map, base) = data.split_once(' ').unwrap_or(("0", &data));
    let map = u64::from_str_radix(map, 16).unwrap_or(0);
    let Some(spec) = args.get(0).to_string(scope) else {
        return;
    };
    let spec = spec.to_rust_string_lossy(scope);
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    match engine.resolve_module_specifier(map, &spec, base) {
        Ok(u) => {
            if let Some(s) = rt::str(scope, u.as_str()) {
                rv.set(s.into());
            }
        }
        Err(msg) => rt::throw_type_error(scope, &msg),
    }
}

/// HTML §8.1.6.2 HostGetImportMetaProperties: `url` and `resolve`.
pub fn import_meta_hook<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    module: v8::Local<'s, v8::Module>,
    meta: v8::Local<'s, v8::Object>,
) {
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    let hash = module.get_identity_hash().get();
    let Some((url, base)) = engine.modules.borrow().url_of.get(&hash).cloned() else {
        return;
    };
    let map = engine
        .modules
        .borrow()
        .map_of
        .get(&hash)
        .copied()
        .unwrap_or(0);
    let url = if url.contains("#inline-module-") {
        base.clone()
    } else {
        url
    };
    let k = rt::name(scope, "url");
    if let Some(v) = rt::str(scope, &url) {
        meta.create_data_property(scope, k.into(), v.into());
    }
    let data = rt::name(scope, &format!("{map:x} {base}"));
    if let Some(f) = v8::Function::builder(rt::guarded(import_meta_resolve))
        .data(data.into())
        .build(scope)
    {
        let k = rt::name(scope, "resolve");
        meta.create_data_property(scope, k.into(), f.into());
    }
}

/// The promise rejection tracker.
pub fn rejection_hook<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    handled: bool,
    promise: v8::Local<'s, v8::Promise>,
    reason: Option<v8::Local<'s, v8::Value>>,
) {
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    if handled {
        let mut cx = Cx::new(scope);
        engine.rejection_handled(&mut cx, promise);
        return;
    }
    let p = v8::Global::new(scope, promise);
    let r = reason.map(|r| v8::Global::new(scope, r));
    engine.about_to_be_notified.borrow_mut().push(Rejection {
        promise: p,
        reason: r,
    });
}

/// HTML §8.1.6.1 HostLoadImportedModule for `import()`.
pub fn dynamic_import<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    host_defined_options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let module_type = snowlark_js::hooks::import_type(scope, attributes, 2);
    let promise = resolver.get_promise(scope);
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    let spec = specifier.to_rust_string_lossy(scope);
    // HTML HostLoadImportedModule steps 1-6: the referencing script (V8 gives the script that
    // compiled code from a string, `eval` or `Function`, in that code's place) supplies the base
    // URL, the fetch options and the referrer; without one (an event handler, a `javascript:`
    // URL), the realm's API base URL and the default fetch options.
    let mut id = host_options_id(scope, Some(host_defined_options));
    // (V8 keeps the code it compiled from a string, and the script it names as that code's
    // referrer, for the same string from another script, in any realm: while a script element's
    // classic script of the same base URL runs, it is the one that compiled the code)
    let running = engine.modules.borrow().running_classic;
    if running != id {
        if let (Some(r), Some(now)) = (engine.referrer_script(id), engine.referrer_script(running))
        {
            if r.base == now.base {
                id = running;
            }
        }
    }
    let (base, options) = match engine.referrer_script(id) {
        Some(r) => {
            let options = r.options.imported_from(&r.base);
            (r.base, options)
        }
        None => {
            let mut cx = Cx::new(scope);
            let doc = engine
                .current_window(&mut cx)
                .and_then(|w| engine.document_of_window(w));
            let base = match doc {
                Some(d) => engine.base_url_of(d).to_string(),
                // (a worker's script: its URL)
                None => resource_name.to_rust_string_lossy(scope),
            };
            (base, ModuleOptions::default())
        }
    };
    let Some(module_type) = module_type.filter(|t| supported_module_type(engine, t)) else {
        let msg = rt::name(scope, "Unsupported module type");
        let e = v8::Exception::type_error(scope, msg);
        resolver.reject(scope, e);
        return Some(promise);
    };
    // A realm a navigation replaced has no module map left (`forget_module_maps`).
    let replaced = {
        let mut cx = Cx::new(scope);
        engine
            .current_window(&mut cx)
            .is_some_and(|w| engine.context_of_window(w).is_none())
    };
    // HTML HostLoadImportedModule step 1: `import()` is refused in a service worker.
    if engine.is_service_worker_global() {
        let msg = rt::name(scope, "import() is not allowed in a service worker");
        let e = v8::Exception::type_error(scope, msg);
        resolver.reject(scope, e);
        return Some(promise);
    }
    if replaced {
        let msg = rt::name(scope, "The document of this realm is gone");
        let e = v8::Exception::type_error(scope, msg);
        resolver.reject(scope, e);
        return Some(promise);
    }
    let map = {
        let mut cx = Cx::new(scope);
        engine.module_map_id(&mut cx)
    };
    match engine.resolve_module_specifier(map, &spec, &base) {
        Err(msg) => {
            let msg = rt::name(scope, &msg);
            let e = v8::Exception::type_error(scope, msg);
            resolver.reject(scope, e);
        }
        Ok(u) => {
            let g = v8::Global::new(scope, resolver);
            let mut cx = Cx::new(scope);
            if options != ModuleOptions::default() {
                let key = module_key(map, u.as_str(), &module_type);
                engine
                    .modules
                    .borrow_mut()
                    .options
                    .entry(key)
                    .or_insert(options);
            }
            engine.load_module_graph_of_type(&mut cx, &u, &module_type, GraphDone::Import(g));
        }
    }
    Some(promise)
}

/// Keeps `ToJs` in scope for the event constructors above.
#[allow(dead_code)]
fn _to_js(_: &dyn ToJs) {}
