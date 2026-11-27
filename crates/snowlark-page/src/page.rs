// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`Page`]: one window with its realm, its isolate and the event loop that drives it.

use std::rc::Rc;
use std::time::{Duration, Instant};

use snowlark_bindings::rt::{self, Cx};

use crate::engine::window::ConsoleOutput;
use crate::engine::Engine;
use crate::render::RenderSetup;

/// How a page is set up.
pub struct PageOptions {
    pub net: Option<snowlark_net::NetClient>,
    pub console: ConsoleOutput,
    pub viewport: (i32, i32),
    /// Style, layout and paint for the top-level document (`None`: no rendering; geometry reads as
    /// zeros and `getComputedStyle` has nothing to read).
    pub render: Option<RenderSetup>,
    pub resource_override: Option<crate::engine::window::ResourceOverride>,
    /// The V8 heap limit in bytes (`None`: V8's default); [`Page::heap_limit_reached`] says when it
    /// was neared.
    pub heap_limit: Option<usize>,
    /// The embedder's content filter (cosmetic filters and scriptlets of each document).
    pub content_filter: Option<std::sync::Arc<dyn crate::ContentFilter>>,
    /// The BroadcastChannel registry the page shares with others (a browser's tabs); `None`: one
    /// of its own (its frames and workers share it).
    pub broadcast: Option<crate::BroadcastHub>,
    /// The IndexedDB backend the page shares with others (a browser's tabs, persisted in its
    /// profile); `None`: one of its own, in memory (its frames and workers share it).
    pub idb: Option<crate::IdbBackend>,
    /// The Cache API's storage the page shares with others (a browser's tabs, persisted in its
    /// profile); `None`: one of its own, in memory (its frames and workers share it).
    pub caches: Option<crate::CacheBackend>,
    /// The `localStorage` service the page shares with others (a browser's tabs, persisted in its
    /// profile); `None`: one of its own, in memory (its frames share it).
    pub web_storage: Option<std::sync::Arc<dyn crate::web_storage::WebStorage>>,
    /// The pointing devices the page sees: the `pointer` and `hover` media features,
    /// `navigator.maxTouchPoints` and whether the legacy touch event APIs are exposed.
    pub input: snowlark_style::InputDevices,
    /// The browser's password manager's half in the page: report login forms, fill what the
    /// browser sends (`docs/password-manager.md`). Off by default and in the headless modes, unless
    /// `SNOWLARK_PASSWORD_FORMS=1` (to show a run with it on observes the same).
    pub password_forms: bool,
}

impl Default for PageOptions {
    fn default() -> Self {
        PageOptions {
            net: None,
            console: if std::env::var("SNOWLARK_CONSOLE").is_ok_and(|v| v == "1") {
                ConsoleOutput::Stderr
            } else {
                ConsoleOutput::Quiet
            },
            viewport: (800, 600),
            render: None,
            resource_override: None,
            heap_limit: None,
            content_filter: None,
            broadcast: None,
            idb: None,
            caches: None,
            web_storage: None,
            input: snowlark_style::InputDevices::MOUSE,
            password_forms: std::env::var("SNOWLARK_PASSWORD_FORMS").is_ok_and(|v| v == "1"),
        }
    }
}

/// A top-level window with its child browsing contexts: the V8 isolate, the engine (which holds each
/// window's realm) and the event loop that drives them.
/// What a page's memory is made of ([`Page::memory`]), in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageMemory {
    /// V8's heap, the physical memory it holds (every document and frame in the tab's isolate).
    pub js_heap: usize,
    /// Memory outside the heap that JavaScript objects keep alive (array buffers and the like).
    pub js_external: usize,
    /// The DOM arena's slots.
    pub dom: usize,
}

pub struct Page {
    /// The realm the last call entered: what the calls of the pass in which the page's last window
    /// closed enter (the page's own top-level traversable gone: [`Engine::close_own_top_level`]).
    entered: Option<v8::Global<v8::Context>>,
    // The engine is dropped before the isolate (field order).
    engine: Rc<Engine>,
    isolate: v8::OwnedIsolate,
    heap_near_limit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// V8's sampling profiler and the native one, when profiling (`SNOWLARK_PROFILE=1`).
    cpu: Option<snowlark_js::cpu_profile::CpuProfiler>,
    native: Option<snowlark_js::native_profile::NativeSampler>,
    /// V8's foreground tasks for this isolate (its garbage collector's jobs, an asynchronous
    /// WebAssembly compilation finishing), which the loop runs ([`Page::run_v8_tasks`]). A task's
    /// arrival wakes the loop.
    v8_tasks: std::sync::Arc<snowlark_js::platform::TaskQueue>,
    /// When the loop last ran V8's foreground tasks.
    v8_tasks_ran_at: Instant,
}

/// How long V8's foreground tasks wait at most while the page always has a task ready or a
/// rendering update due ([`Page::turn`]).
const V8_TASKS_MAX_WAIT: Duration = Duration::from_millis(100);

/// How often the native sampler samples the page thread.
const NATIVE_INTERVAL: Duration = Duration::from_micros(250);

impl Drop for Page {
    /// Wrapper handles go while the isolate is alive (they live in its slot otherwise, and V8's
    /// teardown collection would reach weak ones as they are dropped).
    fn drop(&mut self) {
        // (IndexedDB: live transactions roll back, the backend forgets this loop's connections)
        self.engine.idb_shutdown();
        if self.cpu.is_some() || self.native.is_some() {
            // SNOWLARK_PROFILE=1 (or SNOWLARK_PROFILE_NATIVE=only): the page's profile, as it closes.
            let js = match self.cpu.take() {
                Some(cpu) => cpu
                    .stop()
                    .map(|p| {
                        // SNOWLARK_PROFILE_JSON=<file>: V8's profile as DevTools saves it
                        // (`.cpuprofile`), to compare with another browser's by function.
                        if let Some(path) = std::env::var_os("SNOWLARK_PROFILE_JSON") {
                            let _ = std::fs::write(path, p.to_string());
                        }
                        snowlark_js::cpu_profile::summarize(&p).report(40)
                    })
                    .unwrap_or_else(|| "[profile] js: no profile\n".into()),
                None => String::new(),
            };
            let native = self
                .native
                .take()
                .map(|n| {
                    let (samples, wall) = n.stop();
                    let per_sample = wall / (samples.len() as u32).max(1);
                    let marks = self.engine.sample_marks.clone();
                    let s = snowlark_js::native_profile::summarize(
                        &samples,
                        &marks.unwrap_or_default(),
                    );
                    // SNOWLARK_PROFILE_FOLDED=<file>: every sampled stack, for a flame graph.
                    if let Some(path) = std::env::var_os("SNOWLARK_PROFILE_FOLDED") {
                        let _ = std::fs::write(path, s.folded_text());
                    }
                    s.report(50, per_sample)
                })
                .unwrap_or_default();
            let bindings = self
                .engine
                .profile
                .as_ref()
                .map(|p| p.report(60))
                .unwrap_or_default();
            eprint!("{js}{native}{bindings}");
        }
        rt::release_all(&mut self.isolate);
        // (V8's tasks still waiting are destroyed while the isolate lives)
        self.v8_tasks.close(&self.isolate);
    }
}

/// Gives `isolate`'s foreground tasks to a queue that the page's loop runs, woken by each task.
fn v8_task_queue(
    engine: &Engine,
    isolate: &v8::Isolate,
) -> std::sync::Arc<snowlark_js::platform::TaskQueue> {
    let waker = engine.waker();
    snowlark_js::platform::TaskQueue::register(isolate, move || waker.wake())
}

/// The browser family a chosen user agent claims, which decides whether `navigator.userAgentData`
/// and `navigator.oscpu` exist; `None` for the engine's own identity (everything installed, as it
/// always was) and without a network client.
fn ua_family(engine: &Engine) -> Option<rt::UaFamily> {
    use snowlark_net::ua::Family;
    let ua = engine.hooks.borrow().net.as_ref()?.user_agent();
    ua.strict.then(|| match ua.profile().family {
        Family::Gecko => rt::UaFamily::Gecko,
        Family::Chromium => rt::UaFamily::Chromium,
        Family::WebKit => rt::UaFamily::WebKit,
        Family::Other => rt::UaFamily::Other,
    })
}

impl Page {
    /// A page whose window shows an initial `about:blank` document.
    pub fn new(options: PageOptions) -> Page {
        snowlark_js::init();
        let engine = Engine::new();
        {
            let mut h = engine.hooks.borrow_mut();
            // (Resource Timing sees every fetch of the page through its client)
            h.net = options.net.map(|n| engine.observe_resource_timing(n));
            h.console.output = options.console;
            h.viewport = options.viewport;
            h.input = options.input;
            h.resource_override = options.resource_override;
        }
        engine.set_content_filter(options.content_filter);
        engine.set_password_forms(options.password_forms);
        if let Some(service) = options.web_storage {
            engine.set_web_storage(service);
        }
        if let Some(hub) = options.broadcast {
            engine.set_broadcast_hub(hub);
        }
        if let Some(idb) = options.idb {
            engine.set_idb_backend(idb);
        }
        if let Some(caches) = options.caches {
            engine.set_cache_backend(caches);
        }
        {
            let mut st = engine.render.borrow_mut();
            st.setup = options.render;
            st.viewport =
                snowlark_paint::Size::new(options.viewport.0 as f32, options.viewport.1 as f32);
        }
        let doc = {
            let mut dom = engine.dom_mut();
            let doc = dom.create_html_document(None);
            // (HTML "create a new browsing context and document": allow declarative shadow roots)
            dom.set_allow_declarative_shadow_roots(doc, true);
            // The initial about:blank document has no doctype.
            if let Some(dt) = dom.document_doctype(doc) {
                dom.remove_node(dt);
            }
            doc
        };
        engine.doc_state(doc, |s| s.has_browsing_context = true);
        let window = engine.create_window(doc);
        engine.dom_mut().set_document_url(doc, "about:blank");
        engine.render_attach(doc);
        let params = match options.heap_limit {
            Some(max) => v8::CreateParams::default().heap_limits(0, max),
            None => v8::CreateParams::default(),
        };
        let mut isolate = v8::Isolate::new(params);
        let v8_tasks = v8_task_queue(&engine, &isolate);
        let heap_near_limit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        snowlark_js::hooks::watch_heap_limit(&mut isolate, heap_near_limit.clone());
        snowlark_js::hooks::count_gcs(&mut isolate);
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        snowlark_js::hooks::set_resolve_hook(&mut isolate, crate::script::resolve_hook);
        snowlark_js::hooks::set_import_meta_hook(&mut isolate, crate::script::import_meta_hook);
        snowlark_js::hooks::set_rejection_hook(&mut isolate, crate::script::rejection_hook);
        snowlark_js::hooks::set_dynamic_import_hook(&mut isolate, crate::script::dynamic_import);
        snowlark_js::hooks::set_codegen_hook(&mut isolate, crate::csp::codegen_hook);
        snowlark_js::hooks::set_wasm_codegen_hook(&mut isolate, crate::csp::wasm_hook);
        snowlark_js::hooks::set_wasm_streaming_hook(
            &mut isolate,
            crate::engine::fetch::wasm_streaming_hook,
        );
        // Profiling counts every binding call, so getters take the slow path (Fast API calls
        // bypass the generated callbacks).
        let options = rt::InstallOptions {
            fast_api: engine.profile.is_none(),
            ua_family: ua_family(&engine),
            touch_events: engine.hooks.borrow().input.touch(),
            ..Default::default()
        };
        if let Some(p) = &engine.profile {
            rt::prof::install(&mut isolate, p.bindings.clone());
        }
        let cpu = engine
            .profile
            .as_ref()
            .map(|_| snowlark_js::cpu_profile::CpuProfiler::start(&mut isolate, 100));
        // SNOWLARK_PROFILE_NATIVE=0 leaves the native sampler out of SNOWLARK_PROFILE=1;
        // SNOWLARK_PROFILE_NATIVE=only runs it alone, with nothing else of profiling on: no V8 CPU
        // profiler (which routes every API call through its own thunk), no binding counters, Fast
        // API calls as usual. The way to see what a call costs as pages run it.
        let native_env = std::env::var("SNOWLARK_PROFILE_NATIVE").ok();
        let native = match &engine.profile {
            Some(p) if native_env.as_deref() != Some("0") => {
                snowlark_js::native_profile::NativeSampler::start(NATIVE_INTERVAL, p.marks.clone())
            }
            None if native_env.as_deref() == Some("only") => {
                snowlark_js::native_profile::NativeSampler::start(
                    NATIVE_INTERVAL,
                    engine.sample_marks.clone().unwrap_or_default(),
                )
            }
            _ => None,
        };
        {
            v8::scope!(let scope, &mut isolate);
            let bootstrap = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, bootstrap);
            snowlark_bindings::generated::init_isolate(scope, engine.clone(), options);
            crate::engine::gc::install(scope);
            let realm = crate::engine::frames::create_realm(scope, window, "about:blank");
            engine.set_top_realm(realm);
            engine.publish_window_document(scope, window);
        }
        Page {
            entered: None,
            engine,
            isolate,
            heap_near_limit,
            cpu,
            native,
            v8_tasks,
            v8_tasks_ran_at: Instant::now(),
        }
    }

    /// Whether V8 neared the heap limit (the page should be given up).
    pub fn heap_limit_reached(&self) -> bool {
        self.heap_near_limit
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Memory pressure: V8 collects all it can, then the engine frees what died (wrappers, detached
    /// subtrees, objects). Returns false while script is running.
    pub fn collect_garbage(&mut self) -> bool {
        self.with(|e, cx| {
            if e.script_running() || !e.gc_pass(cx.scope()) {
                return false;
            }
            cx.scope().low_memory_notification();
            e.collect_garbage(cx)
        })
    }

    /// `SNOWLARK_GC_STATS`'s report now, on stderr (tests: why a removed frame stays).
    pub fn gc_report(&mut self) -> bool {
        self.with(|e, cx| e.gc_report_now(cx))
    }

    /// The `SNOWLARK_GC_STATS` report lines now, returned (a harness's report). None while some
    /// state is borrowed.
    pub fn gc_report_text(&mut self) -> Option<Vec<String>> {
        self.with(|e, cx| e.gc_report_text(cx))
    }

    /// Writes a V8 heap snapshot to `path` (for tools/heap-retainers).
    pub fn write_heap_snapshot(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        self.with(|e, cx| e.write_heap_snapshot(cx.scope(), path))
    }

    /// The removed frames and replaced Windows not freed yet, by document URL.
    pub fn frames_alive(&mut self) -> std::collections::BTreeMap<String, usize> {
        self.engine.frames_alive().unwrap_or_default()
    }

    /// What the page's own data takes, from counters V8 and the DOM keep (nothing is walked).
    pub fn memory(&mut self) -> PageMemory {
        let h = self.isolate.get_heap_statistics();
        PageMemory {
            js_heap: h.total_physical_size(),
            js_external: h.external_memory(),
            dom: self.engine.dom().arena_bytes(),
        }
    }

    /// V8's used heap size in bytes.
    pub fn heap_used(&mut self) -> usize {
        self.isolate.get_heap_statistics().used_heap_size()
    }

    /// A dedicated worker's page (on the worker's thread): its global scope is the top-level global.
    pub(crate) fn new_worker(init: crate::worker::WorkerInit) -> Page {
        snowlark_js::init();
        let engine = Engine::new();
        {
            let mut h = engine.hooks.borrow_mut();
            h.net = init.net.clone();
            h.console.output = init.console;
            if init.console == crate::engine::window::ConsoleOutput::Tagged {
                h.console.devtools = Some(Default::default());
            }
        }
        let doc = {
            let mut dom = engine.dom_mut();
            let doc = dom.create_document(snowlark_dom::DocumentKind::Html);
            dom.set_document_url(doc, init.url.as_str());
            doc
        };
        engine.doc_state(doc, |s| s.has_browsing_context = true);
        engine.set_broadcast_hub(init.broadcast.clone());
        engine.set_idb_backend(init.idb.clone());
        engine.set_cache_backend(init.caches.clone());
        let global = engine.create_worker_global(doc, &init.name, init.url.clone());
        engine.event_loop.borrow_mut().waker = init.waker.clone();
        *engine.worker_scope.borrow_mut() = Some(crate::worker::WorkerScope {
            to_parent: init.to_parent,
            parent_waker: init.parent_waker,
            inbox: init.inbox,
            closing: false,
            terminated: false,
            creator: init.creator,
            blob_url_entry: init.blob_url_entry,
            module: init.module,
            load_failed: false,
            owner_secure: init.owner_secure,
            held: init.module.map(|_| Vec::new()),
        });
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        // (after the worker's own waker is in place)
        let v8_tasks = v8_task_queue(&engine, &isolate);
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        snowlark_js::hooks::set_resolve_hook(&mut isolate, crate::script::resolve_hook);
        snowlark_js::hooks::set_import_meta_hook(&mut isolate, crate::script::import_meta_hook);
        snowlark_js::hooks::set_rejection_hook(&mut isolate, crate::script::rejection_hook);
        snowlark_js::hooks::set_dynamic_import_hook(&mut isolate, crate::script::dynamic_import);
        snowlark_js::hooks::set_codegen_hook(&mut isolate, crate::csp::codegen_hook);
        snowlark_js::hooks::set_wasm_codegen_hook(&mut isolate, crate::csp::wasm_hook);
        snowlark_js::hooks::set_wasm_streaming_hook(
            &mut isolate,
            crate::engine::fetch::wasm_streaming_hook,
        );
        {
            v8::scope!(let scope, &mut isolate);
            let bootstrap = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, bootstrap);
            snowlark_bindings::generated::init_isolate(
                scope,
                engine.clone(),
                rt::InstallOptions {
                    worker: true,
                    ua_family: ua_family(&engine),
                    ..Default::default()
                },
            );
            crate::engine::gc::install(scope);
            let origin = engine.origin_of_document(doc);
            let realm = crate::engine::frames::create_realm_for(
                scope,
                global,
                &origin,
                snowlark_bindings::generated::GlobalKind::DedicatedWorker,
                None,
            );
            // (a worker that is not a secure context has none of its global's `[SecureContext]`
            // constructs)
            use snowlark_bindings::generated::WindowOrWorkerGlobalScopeImpl;
            if !WindowOrWorkerGlobalScopeImpl::is_secure_context(&*engine, global) {
                let context = v8::Local::new(scope, &realm);
                let scope = &mut v8::ContextScope::new(scope, context);
                snowlark_bindings::generated::hide_secure_context(
                    scope,
                    snowlark_bindings::generated::GlobalKind::DedicatedWorker,
                );
            }
            engine.set_top_realm(realm);
        }
        Page {
            entered: None,
            engine,
            isolate,
            heap_near_limit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cpu: None,
            native: None,
            v8_tasks,
            v8_tasks_ran_at: Instant::now(),
        }
    }

    /// Whether V8 records each error's call frames when the error is made, for the developer
    /// tools' line of an uncaught exception or rejection: on while the tools are open (it costs
    /// every error made, so it is off otherwise). Pages observe nothing of it (`stack` is
    /// unchanged).
    pub fn capture_error_stacks(&mut self, on: bool) {
        // (as many frames as a developer tools front end asks for by default)
        self.isolate
            .set_capture_stack_trace_for_uncaught_exceptions(on, 200);
    }

    pub fn engine(&self) -> &Rc<Engine> {
        &self.engine
    }

    /// Runs `f` with the engine and an execution context in the top-level window's realm.
    pub fn with<R>(&mut self, f: impl FnOnce(&Engine, &mut Cx) -> R) -> R {
        let engine = self.engine.clone();
        // (what runs in here is one task or less: animation clocks may move again (Blink's
        // `AnimationClock::NotifyTaskStart`); see `Engine::document_time`)
        engine.animations.borrow_mut().task += 1;
        let realm = match engine.entry_realm() {
            Some(r) => {
                if self.entered.as_ref() != Some(&r) {
                    self.entered = Some(r.clone());
                }
                r
            }
            // (its tasks are of discarded documents, which run nothing)
            None => self.entered.clone().expect("the top-level realm"),
        };
        v8::scope_with_context!(scope, &mut self.isolate, &realm);
        let mut cx = Cx::new(scope);
        f(&engine, &mut cx)
    }

    /// Evaluates a classic script in the window, returning its completion value as a string.
    pub fn eval(&mut self, source: &str, name: &str) -> Result<String, String> {
        self.with(|e, cx| e.eval_to_string(cx, source, name))
    }

    /// Replaces the document with one parsed from `html` (scripts run), as if loaded from `url`.
    pub fn load_html(&mut self, html: &str, url: &url::Url) {
        self.with(|e, cx| {
            e.load_html(cx, html, url);
        });
    }

    /// Shows the error page of a navigation of `url` that failed (or that the embedder refuses).
    pub fn show_error_page(&mut self, url: &url::Url, failure: crate::error_page::Failure) {
        self.with(|e, cx| e.show_error_page(cx, crate::engine::frames::TOP, url, failure));
    }

    /// Starts navigating to `url`.
    pub fn navigate(&mut self, url: url::Url) {
        self.engine.navigate(url);
    }

    /// Commits a top-level navigation the browser began ([`Engine::commit_navigation`]).
    pub fn commit_navigation(
        &mut self,
        params: crate::loader::CommitParams,
        stream: Option<snowlark_net::ResponseStream>,
    ) {
        self.with(|e, cx| e.commit_navigation(cx, params, stream));
    }

    /// Traverses to the top-level document's own session history entry `index` (`popstate`,
    /// `hashchange`), which the browser's session history reached ([`Engine::go_in_document`]).
    pub fn go_in_document(&mut self, index: usize) {
        self.engine.go_in_document(index);
    }

    /// Reloads the document at `url`: the server is asked whether what the HTTP cache holds is
    /// still good, however fresh it is.
    pub fn reload(&mut self, url: url::Url) {
        self.engine.reload(url);
    }

    /// One turn of the event loop: network events, one task, the microtask checkpoint, a rendering
    /// opportunity. Returns whether anything ran.
    pub fn turn(&mut self) -> bool {
        // V8's foreground tasks run when the page has nothing ready (below), or here once they
        // waited too long.
        let pumped = self.v8_tasks_ran_at.elapsed() >= V8_TASKS_MAX_WAIT && self.run_v8_tasks();
        self.with(|e, cx| {
            let mut ran = e.poll_network(cx) || pumped;
            ran |= e.poll_media(cx);
            if let Some(task) = e.take_task() {
                let start = e.now();
                (task.steps)(e, cx);
                e.note_task_ran(start);
                ran = true;
            }
            if ran && !e.script_running() {
                e.microtask_checkpoint(cx);
            }
            ran |= e.render_pump();
            // (a held pointer move whose frame never came)
            if e.held_move_deadline().is_some_and(|at| e.now() >= at) {
                ran |= e.dispatch_held_move(cx);
            }
            if e.frame_due() {
                // A rendering opportunity (HTML §8.1.7.3 "update the rendering").
                e.update_the_rendering(cx);
                ran = true;
            }
            // (requestIdleCallback's idle period, when the loop has nothing else to do)
            ran |= e.start_idle_period();
            e.advance_loading(cx);
            if !e.cssom.borrow().fonts_ready.is_empty() {
                e.update_fonts_ready(cx);
            }
            if e.font_waiters_pending() {
                e.update_font_waiters(cx);
            }
            // HTML §8.1.7.3: a microtask checkpoint whenever the JavaScript execution context
            // stack empties, also after what ran outside a task (rendering steps, promises the
            // engine resolved: `document.fonts.ready`, loading steps); without it their reactions
            // would wait for the next task, which an otherwise idle loop never runs.
            if !e.script_running() {
                e.microtask_checkpoint(cx);
            }
            // (and while a due collection waits for the ready tasks, which it does only so long,
            // whether or not they run)
            if (ran || e.collection_deferred()) && !e.script_running() {
                e.collect_garbage_after_task(cx);
            }
            if !e.script_running() && !e.has_ready_task() && !e.frame_due() {
                // The loop waits after this turn: its idle time, until the next timer or frame.
                let free = e.next_wakeup().map_or(Duration::MAX, |at| {
                    at.saturating_duration_since(Instant::now())
                });
                e.collect_garbage_when_idle(cx, free);
            }
            ran
        });
        // V8's foreground tasks are low priority: its garbage collector's jobs (a scavenge before the
        // young generation fills, incremental marking steps, the memory reducer) run once the page has
        // no task ready and no rendering update due, before the loop waits, not ahead of the page's
        // next task. A task that would have run first is not delayed by them: Speedometer's
        // `setTimeout` that ends a step's measured time, for one.
        if !self.engine.has_ready_task()
            && !self.engine.frame_due()
            && self.run_v8_tasks()
            && !self.engine.script_running()
        {
            // (a WebAssembly compilation's promise resolved)
            self.with(|e, cx| e.microtask_checkpoint(cx));
        }
        let ran = self.with(|e, _| e.has_ready_task());
        for (id, request) in self.engine.take_navigation_requests() {
            self.engine.navigate_request(id, request);
        }
        ran
    }

    /// Runs V8's foreground tasks for this isolate until none is left. Returns whether any ran.
    fn run_v8_tasks(&mut self) -> bool {
        // (in a loop nested in script, the tasks V8 posted as non-nestable wait)
        let ran = self.v8_tasks.run(self.engine.script_running());
        self.v8_tasks_ran_at = Instant::now();
        ran
    }

    /// Runs the event loop until `done` says so or `deadline` passes. Returns whether `done` held.
    pub fn run_until(
        &mut self,
        deadline: Instant,
        mut done: impl FnMut(&mut Page) -> bool,
    ) -> bool {
        loop {
            if done(self) {
                return true;
            }
            let ran = self.turn();
            if done(self) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            if ran {
                continue;
            }
            if !self.engine.has_pending_work() && !self.engine.has_ready_task() {
                return done(self);
            }
            let wake = self
                .engine
                .next_wakeup()
                .unwrap_or(now + Duration::from_millis(50));
            let wake = wake.min(deadline).min(now + Duration::from_millis(50));
            if wake > now {
                self.engine.waker().wait_until(wake);
            }
        }
    }

    /// Runs the event loop until nothing is left to do (no tasks, timers, frames or fetches), or until
    /// `timeout`. Returns whether it settled.
    pub fn run_until_settled(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        // (a collection a busy loop left for the idle time is work left too: the loop's next turn
        // with nothing ready runs it)
        self.run_until(deadline, |p| {
            !p.engine.has_pending_work()
                && !p.engine.has_ready_task()
                && !p.with(|e, cx| e.idle_collection_due(cx))
        })
    }

    /// Runs event loop turns while work is ready, for at most `budget` (an embedder's slice of its
    /// thread). Returns whether anything ran.
    pub fn run_for(&mut self, budget: Duration) -> bool {
        let start = Instant::now();
        let mut any = false;
        loop {
            let ran = self.turn();
            any |= ran;
            if !ran || start.elapsed() >= budget {
                // A due frame or timer runs in its own slice.
                if !ran && (self.engine.frame_due() || self.engine.has_ready_task()) {
                    any |= self.turn();
                }
                return any;
            }
        }
    }

    /// Whether the top-level document has finished loading: a navigation committed a document and
    /// its `load` event has fired (`document.readyState` is "complete").
    pub fn load_complete(&self) -> bool {
        let Some(doc) = self.engine.document() else {
            return false;
        };
        if self.engine.dom().document_url(doc) == "about:blank" {
            return false;
        }
        self.engine.doc_state(doc, |s| {
            s.ready_state == snowlark_bindings::generated::DocumentReadyState::Complete
        })
    }

    /// A rendering update now (HTML "update the rendering"), whatever the frame timing; returns
    /// whether the rendering changed.
    pub fn update_the_rendering(&mut self) -> bool {
        self.with(|e, cx| e.update_the_rendering(cx))
    }

    /// The console messages so far: (level, text).
    pub fn console_messages(&self) -> Vec<(&'static str, String)> {
        self.engine.hooks.borrow().console.messages.clone()
    }
}

/// Stops the script running on a page from another thread (a watchdog).
#[derive(Clone)]
pub struct Terminator(v8::IsolateHandle);

impl Terminator {
    /// Terminates the running script and any script that starts after it until the page resumes.
    pub fn terminate(&self) {
        self.0.terminate_execution();
    }

    /// Lets scripts run again after [`Terminator::terminate`] (scripts that start from now on run).
    pub fn resume(&self) {
        self.0.cancel_terminate_execution();
    }
}

impl Page {
    /// A handle that can stop this page's script from another thread.
    pub fn terminator(&self) -> Terminator {
        Terminator(self.isolate.thread_safe_handle())
    }

    /// The value of a global property of the window, if it is a string (read without running script
    /// beyond the property's getter).
    pub fn global_string(&mut self, name: &str) -> Option<String> {
        self.with(|_, cx| {
            let scope = cx.scope();
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, name)?;
            let v = global.get(scope, key.into())?;
            v.is_string().then(|| v.to_rust_string_lossy(scope))
        })
    }
}
