// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dedicated workers (HTML §10): each `new Worker(url)` runs on a thread of its own with its own isolate,
//! engine and event loop; its global object is a `DedicatedWorkerGlobalScope`. Messages cross between
//! the threads as V8 structured serializations (bytes) over channels, and each side wakes the other's
//! event loop.
//!
//! Inside the worker, the global scope stands where a Window stands in a page: it is the top-level
//! "window" of the worker engine's browsing context list, with a document that is never exposed but
//! carries the worker's URL (the base URL and client of the worker's fetches) and origin.

use std::sync::{mpsc, Arc};

use snowlark_bindings::generated::{
    AbstractWorkerImpl, DedicatedWorkerGlobalScopeImpl, MessageEventTargetImpl,
    StructuredSerializeOptions, TrustedScriptURLOrString, WorkerGlobalScopeImpl, WorkerImpl,
    WorkerLocationImpl, WorkerNavigatorImpl, WorkerOptions,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsObject, JsValue};
use snowlark_bindings::InterfaceId;

use crate::engine::events::EventExt;
use crate::engine::{exception, Engine, Object};
use crate::event_loop::{TaskSource, Waker};

/// From the page to its worker.
pub enum ToWorker {
    Message(Vec<u8>),
    Terminate,
}

/// From a worker to its page.
pub enum FromWorker {
    Message(Vec<u8>),
    /// An uncaught exception the worker's global did not handle (HTML §10.2.5 "runtime script errors").
    Error {
        message: String,
        filename: String,
        lineno: u32,
        colno: u32,
    },
    /// The script could not be fetched or did not compile: a plain `error` event.
    LoadFailed,
    Closed,
}

/// The data of a worker's global object.
pub struct WorkerGlobalData {
    pub name: String,
    pub url: url::Url,
}

/// The parent side's `Worker` object.
pub struct WorkerObject {
    pub tx: mpsc::Sender<ToWorker>,
    pub waker: Arc<Waker>,
    pub terminated: bool,
}

/// A worker whose messages the parent's event loop polls.
pub struct WorkerLink {
    pub worker: Handle,
    pub rx: mpsc::Receiver<FromWorker>,
}

/// The worker side's link to its parent.
pub struct WorkerScope {
    pub to_parent: mpsc::Sender<FromWorker>,
    pub parent_waker: Arc<Waker>,
    pub inbox: mpsc::Receiver<ToWorker>,
    pub closing: bool,
    pub terminated: bool,
    /// The creating document's request client: the main script's fetch is the creator's, and a
    /// `blob:` or `data:` worker takes its CSP list.
    pub creator: Option<Arc<snowlark_net::Client>>,
    /// The script URL's blob URL entry, held by the creator when it parsed the URL
    /// ([`snowlark_net::Request::blob_url_entry`]).
    pub blob_url_entry: Option<u64>,
    /// A module worker (`type: "module"`): the credentials mode of its module fetches.
    pub module: Option<snowlark_net::CredentialsMode>,
    /// Its main script failed (the parent was told): the thread ends without `Closed`.
    pub load_failed: bool,
    /// The parent's messages that came before the main script ran: HTML "run a worker" enables
    /// the port message queue only after running it (a module worker's script runs when its graph
    /// has loaded, after the event loop started).
    pub held: Option<Vec<Vec<u8>>>,
}

/// What a worker thread starts from.
pub struct WorkerInit {
    pub net: Option<snowlark_net::NetClient>,
    pub console: crate::engine::window::ConsoleOutput,
    pub url: url::Url,
    pub name: String,
    pub waker: Arc<Waker>,
    pub parent_waker: Arc<Waker>,
    pub to_parent: mpsc::Sender<FromWorker>,
    pub inbox: mpsc::Receiver<ToWorker>,
    /// The creating document's request client.
    pub creator: Option<Arc<snowlark_net::Client>>,
    /// The script URL's blob URL entry ([`WorkerScope::blob_url_entry`]).
    pub blob_url_entry: Option<u64>,
    /// A module worker's credentials mode ([`WorkerScope::module`]).
    pub module: Option<snowlark_net::CredentialsMode>,
    /// The BroadcastChannel registry of the creator's event loop.
    pub broadcast: crate::BroadcastHub,
    /// The IndexedDB backend of the creator's event loop.
    pub idb: crate::IdbBackend,
}

/// The worker thread: its page, the main script, then the event loop until terminated or closed.
fn worker_main(init: WorkerInit) {
    let to_parent = init.to_parent.clone();
    let parent_waker = init.parent_waker.clone();
    let mut page = crate::page::Page::new_worker(init);
    let ok = page.with(|e, cx| e.run_worker_main_script(cx));
    if !ok {
        let _ = to_parent.send(FromWorker::LoadFailed);
        parent_waker.wake();
        return;
    }
    loop {
        let done = page.with(|e, cx| e.poll_worker_inbox(cx));
        if done {
            break;
        }
        let ran = page.turn();
        if page.engine().worker_closing() {
            break;
        }
        if !ran {
            let now = std::time::Instant::now();
            let wake = page
                .engine()
                .next_wakeup()
                .unwrap_or(now + std::time::Duration::from_millis(50))
                .min(now + std::time::Duration::from_millis(50));
            if wake > now {
                page.engine().waker().wait_until(wake);
            }
        }
    }
    if !page.engine().worker_load_failed_already() {
        let _ = to_parent.send(FromWorker::Closed);
    }
    parent_waker.wake();
}

fn type_error(m: impl Into<String>) -> rt::Error {
    rt::Error::Type(m.into().into())
}

impl Engine {
    /// The worker side: a module worker's main script graph failed (HTML "run a worker" onComplete):
    /// the parent fires `error` at the Worker object, and the worker stops.
    pub(crate) fn worker_load_failed(&self) {
        let mut ws = self.worker_scope.borrow_mut();
        let Some(ws) = ws.as_mut() else {
            return;
        };
        if !ws.load_failed {
            ws.load_failed = true;
            ws.terminated = true;
            let _ = ws.to_parent.send(FromWorker::LoadFailed);
            ws.parent_waker.wake();
        }
    }

    fn worker_load_failed_already(&self) -> bool {
        self.worker_scope
            .borrow()
            .as_ref()
            .is_some_and(|w| w.load_failed)
    }

    /// The worker side: whether `close()` ran or the parent terminated the worker.
    pub(crate) fn worker_closing(&self) -> bool {
        self.worker_scope
            .borrow()
            .as_ref()
            .is_some_and(|w| w.closing || w.terminated)
    }

    /// The worker side: takes the parent's messages. Returns whether the worker must stop.
    pub(crate) fn poll_worker_inbox(&self, _cx: &mut Cx) -> bool {
        loop {
            let msg = {
                let ws = self.worker_scope.borrow();
                let Some(ws) = ws.as_ref() else {
                    return false;
                };
                match ws.inbox.try_recv() {
                    Ok(m) => m,
                    Err(mpsc::TryRecvError::Empty) => return false,
                    Err(mpsc::TryRecvError::Disconnected) => return true,
                }
            };
            match msg {
                ToWorker::Terminate => {
                    if let Some(ws) = self.worker_scope.borrow_mut().as_mut() {
                        ws.terminated = true;
                    }
                    return true;
                }
                ToWorker::Message(bytes) => {
                    if let Some(held) = self
                        .worker_scope
                        .borrow_mut()
                        .as_mut()
                        .and_then(|w| w.held.as_mut())
                    {
                        held.push(bytes);
                        continue;
                    }
                    self.queue_worker_message(bytes);
                }
            }
        }
    }

    /// The worker side: the main script ran; the messages held until now are queued, in order.
    pub(crate) fn enable_worker_messages(&self) {
        let held = self
            .worker_scope
            .borrow_mut()
            .as_mut()
            .and_then(|w| w.held.take());
        for bytes in held.into_iter().flatten() {
            self.queue_worker_message(bytes);
        }
    }

    /// Queues a `message` (or `messageerror`) event of the parent's at the global scope.
    fn queue_worker_message(&self, bytes: Vec<u8>) {
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let Some(global) = e.window() else { return };
            match e.deserialize_value(cx, &bytes) {
                Ok(data) => e.fire_port_message(cx, global, Some(data), "message"),
                Err(()) => e.fire_port_message(cx, global, None, "messageerror"),
            }
        });
    }

    /// Fires a `message` (or `messageerror`) event with no source and no origin.
    fn fire_port_message(&self, cx: &mut Cx, target: Handle, data: Option<JsValue>, type_: &str) {
        let ev = self.trusted_event(
            InterfaceId::MessageEvent,
            type_,
            false,
            false,
            EventExt::Message {
                data: data.map(crate::engine::held::Held::value),
                origin: String::new(),
                last_event_id: String::new(),
                source: None,
                ports: Vec::new(),
            },
        );
        self.dispatch(cx, ev, target, false);
    }

    /// The worker side: an uncaught error the global did not handle goes to the parent.
    pub(crate) fn worker_report_error(
        &self,
        message: &str,
        filename: &str,
        lineno: u32,
        colno: u32,
    ) -> bool {
        let ws = self.worker_scope.borrow();
        let Some(ws) = ws.as_ref() else {
            return false;
        };
        let _ = ws.to_parent.send(FromWorker::Error {
            message: message.to_owned(),
            filename: filename.to_owned(),
            lineno,
            colno,
        });
        ws.parent_waker.wake();
        true
    }

    /// Fetches a worker script synchronously (the main script or `importScripts`): the source, the
    /// final URL and the response head.
    fn fetch_worker_script(
        &self,
        url: &url::Url,
        destination: snowlark_net::Destination,
    ) -> Option<(String, String, snowlark_net::ResponseHead)> {
        let net = self.hooks.borrow().net.clone()?;
        let doc = self.document()?;
        let mut hold = None;
        let mut stream = if destination == snowlark_net::Destination::Worker {
            // The main script is the creator's fetch (its policy was checked, and its violations
            // reported, by the creator before the thread started; redirects are checked here).
            let (creator, blob_url_entry) = {
                let scope = self.worker_scope.borrow();
                let w = scope.as_ref();
                (
                    w.and_then(|w| w.creator.clone()),
                    w.and_then(|w| w.blob_url_entry),
                )
            };
            let client = creator.unwrap_or_else(|| self.request_client(doc));
            let mut req = snowlark_net::Request::subresource(url.clone(), destination, client);
            req.mode = snowlark_net::Mode::SameOrigin;
            req.blob_url_entry = blob_url_entry;
            hold = blob_url_entry;
            net.fetch(req)
        } else {
            let req = snowlark_net::Request::subresource(
                url.clone(),
                destination,
                self.request_client(doc),
            );
            self.policy_fetch(doc, req, None, None)?
        };
        let mut head = None;
        let mut body = Vec::new();
        loop {
            match stream.next_blocking() {
                Some(snowlark_net::FetchEvent::Head(h)) => head = Some(h),
                Some(snowlark_net::FetchEvent::Body(b)) => body.extend_from_slice(&b),
                Some(snowlark_net::FetchEvent::Violation(_)) => {}
                Some(snowlark_net::FetchEvent::Done) => break,
                Some(snowlark_net::FetchEvent::Error(_)) | None => return None,
            }
        }
        if let Some(token) = hold {
            net.release_blob_url(token);
        }
        let head = head?;
        if !head.ok() && head.status != 0 {
            return None;
        }
        let final_url = head.url().cloned().unwrap_or_else(|| url.clone());
        // HTML "fetch a classic worker script": a JavaScript MIME type is required of an HTTP(S)
        // response only, so a `blob:` or `data:` main script runs whatever its type says; "fetch a
        // classic worker-imported script" (`importScripts`) requires it of every response.
        let mime = snowlark_net::mime::extract_mime_type(&head.headers)
            .map(|m| m.essence())
            .unwrap_or_default();
        let checked = destination != snowlark_net::Destination::Worker
            || matches!(final_url.scheme(), "http" | "https");
        if checked && !snowlark_net::mime::is_javascript_essence(&mime) {
            return None;
        }
        let (text, _, _) = encoding_rs::UTF_8.decode(&body);
        Some((text.into_owned(), final_url.to_string(), head))
    }

    /// Runs the worker's main script. Returns false when it could not be fetched.
    pub(crate) fn run_worker_main_script(&self, cx: &mut Cx) -> bool {
        let Some(global) = self.window() else {
            return false;
        };
        let url = match &*self.obj(global) {
            Object::WorkerGlobal(g) => g.url.clone(),
            _ => return false,
        };
        // A module worker: its module script graph, fetched with the worker's options; it runs (or
        // fails the worker) when the graph completes, on this thread's event loop.
        let module = self.worker_scope.borrow().as_ref().and_then(|w| w.module);
        if let Some(credentials) = module {
            let key = crate::script::module_key(self.module_map_id(cx), url.as_str(), "javascript");
            self.modules.borrow_mut().options.insert(
                key,
                crate::script::ModuleOptions {
                    worker: Some(crate::script::WorkerModuleFetch {
                        credentials,
                        top_level: true,
                    }),
                    ..Default::default()
                },
            );
            let done = crate::script::GraphDone::WorkerMain;
            self.load_module_graph_of_type(cx, &url, "javascript", done);
            return true;
        }
        let Some((source, final_url, head)) =
            self.fetch_worker_script(&url, snowlark_net::Destination::Worker)
        else {
            return false;
        };
        // The worker's policy container (HTML "run a worker" step "initialize worker global scope's
        // policy container"): the response's, or for `blob:` and `data:` the creator's.
        if let (Some(doc), Ok(u)) = (self.document(), url::Url::parse(&final_url)) {
            if matches!(u.scheme(), "blob" | "data") {
                let creator = self
                    .worker_scope
                    .borrow()
                    .as_ref()
                    .and_then(|w| w.creator.as_ref().and_then(|c| c.csp.clone()));
                if creator.is_some() {
                    self.set_csp_list(cx, doc, creator);
                }
            } else {
                self.document_response_policy(cx, doc, &u, &head, None);
            }
        }
        // (its base URL is its response's URL, its fetch options the default ones)
        let referrer = self.referrer_id(&final_url, &Default::default());
        self.run_classic_with(cx, &source, &final_url, 0, referrer, false, None, None);
        self.microtask_checkpoint(cx);
        true
    }

    /// A classic script that rethrows what it throws (for `importScripts`).
    fn run_classic_rethrowing(&self, cx: &mut Cx, source: &str, url: &str) -> rt::Result<()> {
        let referrer = self.referrer_id(url, &Default::default());
        self.enter_script();
        let r = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let ok = (|| {
                let code = v8::String::new(tc, source)?;
                let origin = crate::script::script_origin(tc, url, referrer)?;
                let script = v8::Script::compile(tc, code, Some(&origin))?;
                script.run(tc)
            })();
            if ok.is_none() && tc.has_caught() {
                tc.rethrow();
                Err(rt::Error::Pending)
            } else {
                Ok(())
            }
        };
        self.leave_script(cx);
        r
    }

    // ----- the parent side ---------------------------------------------------------------------------

    /// Takes the workers' messages and queues their events. Returns whether anything arrived.
    pub(crate) fn poll_workers(&self, _cx: &mut Cx) -> bool {
        let mut progressed = false;
        let mut i = 0;
        loop {
            let (worker, msg) = {
                let links = self.workers.borrow();
                let Some(link) = links.get(i) else { break };
                match link.rx.try_recv() {
                    Ok(m) => (link.worker, Some(m)),
                    Err(mpsc::TryRecvError::Empty) => (link.worker, None),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        (link.worker, Some(FromWorker::Closed))
                    }
                }
            };
            let Some(msg) = msg else {
                i += 1;
                continue;
            };
            progressed = true;
            let terminated = match &*self.obj(worker) {
                Object::Worker(w) => w.terminated,
                _ => true,
            };
            match msg {
                FromWorker::Closed => {
                    self.workers.borrow_mut().remove(i);
                    continue;
                }
                _ if terminated => {}
                FromWorker::Message(bytes) => {
                    self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                        match e.deserialize_value(cx, &bytes) {
                            Ok(data) => e.fire_port_message(cx, worker, Some(data), "message"),
                            Err(()) => e.fire_port_message(cx, worker, None, "messageerror"),
                        }
                    });
                }
                FromWorker::Error {
                    message,
                    filename,
                    lineno,
                    colno,
                } => {
                    self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                        let ev = e.trusted_event(
                            InterfaceId::ErrorEvent,
                            "error",
                            false,
                            true,
                            EventExt::Error {
                                message: message.clone(),
                                filename: filename.clone(),
                                lineno,
                                colno,
                                error: None,
                            },
                        );
                        if e.dispatch(cx, ev, worker, false) {
                            e.console_error(&format!(
                                "Uncaught (in worker) {message} ({filename}:{lineno}:{colno})"
                            ));
                        }
                    });
                }
                FromWorker::LoadFailed => {
                    self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                        e.fire_event(cx, worker, "error", false, false);
                    });
                }
            }
        }
        progressed
    }

    fn worker_object<R>(&self, h: Handle, f: impl FnOnce(&mut WorkerObject) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::Worker(w) => Some(f(w)),
            _ => None,
        }
    }

    /// StructuredSerialize into bytes (HTML §2.7.3), for another isolate.
    pub(crate) fn serialize_value(&self, cx: &mut Cx, value: &JsValue) -> rt::Result<Vec<u8>> {
        self.serialize_to_bytes(cx, value)
    }
}

impl AbstractWorkerImpl for Engine {}

impl MessageEventTargetImpl for Engine {}

impl WorkerNavigatorImpl for Engine {
    fn permissions(&self, this: Handle) -> Handle {
        self.navigator_permissions(this)
    }
    fn media_capabilities(&self, this: Handle) -> Handle {
        Engine::media_capabilities(self, this)
    }
}

impl WorkerImpl for Engine {
    fn terminate(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.worker_object(this, |w| {
            if !w.terminated {
                w.terminated = true;
                let _ = w.tx.send(ToWorker::Terminate);
                w.waker.wake();
            }
        });
        Ok(())
    }
    fn post_message_message_transfer(
        &self,
        cx: &mut Cx,
        this: Handle,
        message: JsValue,
        _transfer: Vec<JsObject>,
    ) -> rt::Result<()> {
        let bytes = self.serialize_value(cx, &message)?;
        self.worker_object(this, |w| {
            if !w.terminated {
                let _ = w.tx.send(ToWorker::Message(bytes));
                w.waker.wake();
            }
        });
        Ok(())
    }
    fn post_message_message_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        message: JsValue,
        _options: StructuredSerializeOptions,
    ) -> rt::Result<()> {
        WorkerImpl::post_message_message_transfer(self, cx, this, message, Vec::new())
    }
    /// HTML §10.2.6.3 `new Worker(scriptURL, options)`.
    fn constructor(
        &self,
        cx: &mut Cx,
        script_url: TrustedScriptURLOrString,
        options: WorkerOptions,
    ) -> rt::Result<Handle> {
        let s = match script_url {
            TrustedScriptURLOrString::String(s) => s,
            TrustedScriptURLOrString::TrustedScriptURL(h) => match &*self.obj(h) {
                Object::TrustedScriptUrl(s) => s.clone(),
                _ => String::new(),
            },
        };
        let base = self.current_base_url(cx)?;
        let url = base
            .join(&s)
            .map_err(|_| exception(ExceptionName::SyntaxError, "Invalid worker script URL"))?;
        // A module worker's module fetches take its credentials mode (HTML "run a worker").
        let module =
            (options.type_ == snowlark_bindings::generated::WorkerType::Module).then(|| {
                use snowlark_bindings::generated::RequestCredentials as C;
                match options.credentials {
                    C::Omit => snowlark_net::CredentialsMode::Omit,
                    C::Include => snowlark_net::CredentialsMode::Include,
                    _ => snowlark_net::CredentialsMode::SameOrigin,
                }
            });
        // The URL's blob URL entry is resolved here, as parsing the URL does (URL Standard §4.4):
        // the script is fetched on the worker's thread, by when `revokeObjectURL` may have run.
        let blob_url_entry = self.hold_blob_url(&url);
        let (to_worker, inbox) = mpsc::channel();
        let (to_parent, rx) = mpsc::channel();
        let waker = Arc::new(Waker::default());
        // The creator's request policy: worker-src (child-src, script-src, default-src) decides
        // whether the script is fetched at all; a blocked worker gets its `error` event.
        let creator_doc = self.current_global_document(cx);
        let creator = creator_doc.map(|d| self.request_client(d));
        let blocked = match (creator_doc, &creator) {
            (Some(d), Some(c)) if c.csp.is_some() => {
                let mut req = snowlark_net::Request::subresource(
                    url.clone(),
                    snowlark_net::Destination::Worker,
                    c.clone(),
                );
                req.mode = snowlark_net::Mode::SameOrigin;
                self.request_blocked(cx, d, &req)
            }
            _ => false,
        };
        if blocked {
            let _ = to_parent.send(FromWorker::LoadFailed);
        } else {
            let init = WorkerInit {
                net: self.hooks.borrow().net.clone(),
                // `SNOWLARK_CONSOLE=tagged` (headless runs): the worker's messages as tagged lines, with
                // its realm, since its console has no sink of its own.
                console: if std::env::var("SNOWLARK_CONSOLE").is_ok_and(|v| v == "tagged") {
                    crate::engine::window::ConsoleOutput::Tagged
                } else {
                    self.hooks.borrow().console.output
                },
                url,
                name: options.name.clone(),
                waker: waker.clone(),
                parent_waker: self.waker(),
                broadcast: self.broadcast_hub(),
                idb: self.idb_backend_for_worker(),
                to_parent,
                inbox,
                creator,
                blob_url_entry,
                module,
            };
            std::thread::Builder::new()
                .name("snowlark-worker".into())
                .stack_size(8 << 20)
                .spawn(move || worker_main(init))
                .map_err(|_| exception(ExceptionName::OperationError, "Cannot start the worker"))?;
        }
        let h = self.alloc(Object::Worker(Box::new(WorkerObject {
            tx: to_worker,
            waker,
            terminated: false,
        })));
        self.workers.borrow_mut().push(WorkerLink { worker: h, rx });
        Ok(h)
    }
}

impl WorkerGlobalScopeImpl for Engine {
    fn self_(&self, this: Handle) -> Handle {
        this
    }
    fn location(&self, this: Handle) -> Handle {
        let url = match &*self.obj(this) {
            Object::WorkerGlobal(g) => g.url.clone(),
            _ => url::Url::parse("about:blank").expect("about:blank"),
        };
        self.window_singleton(this, "location", || Object::WorkerLocation(url))
    }
    fn navigator(&self, this: Handle) -> Handle {
        self.window_singleton(this, "navigator", || Object::WorkerNavigator)
    }
    /// HTML §10.3.1 `importScripts(...urls)`: fetched and run in order, synchronously.
    fn import_scripts(
        &self,
        cx: &mut Cx,
        this: Handle,
        urls: Vec<TrustedScriptURLOrString>,
    ) -> rt::Result<()> {
        // 1: a module worker has no classic scripts to import.
        if self
            .worker_scope
            .borrow()
            .as_ref()
            .is_some_and(|w| w.module.is_some())
        {
            return Err(type_error(
                "importScripts() is not allowed in a module worker",
            ));
        }
        let base = match &*self.obj(this) {
            Object::WorkerGlobal(g) => g.url.clone(),
            _ => return Ok(()),
        };
        let mut parsed = Vec::new();
        for u in urls {
            let s = match u {
                TrustedScriptURLOrString::String(s) => s,
                TrustedScriptURLOrString::TrustedScriptURL(h) => match &*self.obj(h) {
                    Object::TrustedScriptUrl(s) => s.clone(),
                    _ => String::new(),
                },
            };
            parsed.push(
                base.join(&s)
                    .map_err(|_| exception(ExceptionName::SyntaxError, "Invalid script URL"))?,
            );
        }
        for url in parsed {
            let Some((source, final_url, _)) =
                self.fetch_worker_script(&url, snowlark_net::Destination::Script)
            else {
                return Err(exception(
                    ExceptionName::NetworkError,
                    "Failed to load the script",
                ));
            };
            self.run_classic_rethrowing(cx, &source, &final_url)?;
        }
        Ok(())
    }
}

impl DedicatedWorkerGlobalScopeImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(match &*self.obj(this) {
            Object::WorkerGlobal(g) => g.name.clone(),
            _ => String::new(),
        })
    }
    fn post_message_message_transfer(
        &self,
        cx: &mut Cx,
        _this: Handle,
        message: JsValue,
        _transfer: Vec<JsObject>,
    ) -> rt::Result<()> {
        let bytes = self.serialize_value(cx, &message)?;
        if let Some(ws) = self.worker_scope.borrow().as_ref() {
            let _ = ws.to_parent.send(FromWorker::Message(bytes));
            ws.parent_waker.wake();
        }
        Ok(())
    }
    fn post_message_message_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        message: JsValue,
        _options: StructuredSerializeOptions,
    ) -> rt::Result<()> {
        DedicatedWorkerGlobalScopeImpl::post_message_message_transfer(
            self,
            cx,
            this,
            message,
            Vec::new(),
        )
    }
    fn close(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        if let Some(ws) = self.worker_scope.borrow_mut().as_mut() {
            ws.closing = true;
        }
        Ok(())
    }
}

impl WorkerLocationImpl for Engine {
    fn href(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.worker_location(this).to_string())
    }
    fn origin(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.worker_location(this).origin().ascii_serialization())
    }
    fn protocol(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(format!("{}:", self.worker_location(this).scheme()))
    }
    fn host(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(crate::engine::url::host_with_port(
            &self.worker_location(this),
        ))
    }
    fn hostname(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.worker_location(this)
                .host_str()
                .unwrap_or("")
                .to_owned(),
        )
    }
    fn port(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.worker_location(this)
                .port()
                .map(|p| p.to_string())
                .unwrap_or_default(),
        )
    }
    fn pathname(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.worker_location(this).path().to_owned())
    }
    fn search(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.worker_location(this)
                .query()
                .filter(|q| !q.is_empty())
                .map(|q| format!("?{q}"))
                .unwrap_or_default(),
        )
    }
    fn hash(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.worker_location(this)
                .fragment()
                .filter(|f| !f.is_empty())
                .map(|f| format!("#{f}"))
                .unwrap_or_default(),
        )
    }
}

impl Engine {
    fn worker_location(&self, h: Handle) -> url::Url {
        match &*self.obj(h) {
            Object::WorkerLocation(u) => u.clone(),
            _ => url::Url::parse("about:blank").expect("about:blank"),
        }
    }
}
