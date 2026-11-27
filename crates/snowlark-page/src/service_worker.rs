// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Service Workers in an event loop (W3C Service Workers §3 `ServiceWorker`,
//! `ServiceWorkerRegistration`, §3.4 `ServiceWorkerContainer`, §4 `ServiceWorkerGlobalScope`,
//! `ExtendableEvent`), over the registrations in `snowlark-sw`.
//!
//! An event loop opens one connection to the registry per storage key it has globals of. Calls go
//! out on it; answers and the registry's notices (Update Registration State, Update Worker State,
//! `updatefound`) come back in order on one queue, and each is handled in a task of the DOM
//! manipulation task source, as §8 queues them. A page's event loop can also run its key's
//! service workers: the registry sends it [`Notice::Start`] with the script, and the worker runs
//! on a thread of its own ([`crate::worker`]'s machinery, with a `ServiceWorkerGlobalScope` as its
//! global), which opens a connection of its own for its lifecycle events.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};

use snowlark_bindings::generated::{
    ClientsImpl, ExtendableEventImpl, ExtendableEventInit, GetNotificationOptions,
    NotificationOptions, RegistrationOptions, ServiceWorkerContainerImpl,
    ServiceWorkerGlobalScopeImpl, ServiceWorkerImpl, ServiceWorkerRegistrationImpl,
    ServiceWorkerState, ServiceWorkerUpdateViaCache, StructuredSerializeOptions,
    TrustedScriptURLOrString,
};
use snowlark_bindings::rt::{
    self, Cx, DomStr, Error, ExceptionName, Frozen, Handle, JsObject, JsPromise, JsValue,
};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;
use snowlark_sw::registry::{
    Answer, Call, CallId, ClientId, ConnId, JobError, Lifecycle, Notice, RegistrationId,
    RegistrationInfo, Slot, UpdateViaCache, VersionId, VersionInfo, WorkerState, WorkerType,
};
use snowlark_sw::ServiceWorkerBackend;

use crate::engine::events::EventExt;
use crate::engine::{Engine, Object};
use crate::event_loop::{TaskSource, Waker};

/// A registration object's data.
pub struct RegistrationData {
    pub global: Handle,
    pub id: RegistrationId,
    pub scope: String,
    pub update_via_cache: UpdateViaCache,
    pub installing: Option<Handle>,
    pub waiting: Option<Handle>,
    pub active: Option<Handle>,
}

/// A ServiceWorker object's data.
pub struct WorkerData {
    pub global: Handle,
    pub id: VersionId,
    pub script_url: String,
    pub state: WorkerState,
}

/// An ExtendableEvent's extend lifetime promises (§4.4): how many are pending, whether one was
/// rejected, and for a lifecycle event the version to report its outcome for.
#[derive(Default)]
pub struct Extensions {
    pub pending: Cell<usize>,
    pub rejected: Cell<bool>,
    pub report: Cell<Option<(VersionId, Lifecycle)>>,
}

/// What a service worker's thread starts from (with [`crate::worker::WorkerInit`]).
pub struct SwStart {
    pub backend: ServiceWorkerBackend,
    pub version: VersionInfo,
    pub registration: RegistrationInfo,
    pub source: Vec<u8>,
}

/// What a call's answer settles.
struct Pending {
    global: Handle,
    promise: JsPromise,
    kind: PendingKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Registration,
    Ready,
    Lookup,
    Lookups,
    Unregister,
    Done,
}

/// An event loop's service worker state.
#[derive(Default)]
pub struct SwState {
    backend: Option<ServiceWorkerBackend>,
    conns: HashMap<String, ConnId>,
    inbox: Arc<Mutex<VecDeque<Notice>>>,
    next_call: CallId,
    pending: HashMap<CallId, Pending>,
    registrations: HashMap<(Handle, RegistrationId), Handle>,
    workers: HashMap<(Handle, VersionId), Handle>,
    /// The service workers this event loop runs, by version: the thread's inbox and waker.
    running: HashMap<VersionId, (mpsc::Sender<crate::worker::ToWorker>, Arc<Waker>)>,
    /// On a service worker's thread: its version and registration as it started.
    own: Option<(VersionInfo, RegistrationInfo)>,
    /// When the last notice was taken (a `ready` answer may follow a state change closely).
    last_notice: Option<std::time::Instant>,
    /// The service worker clients told to the registry: each window's document, with its number
    /// and storage key.
    clients: HashMap<NodeId, (ClientId, String)>,
    next_client: ClientId,
    /// The clients' active service workers (`navigator.serviceWorker.controller`).
    controllers: HashMap<NodeId, VersionInfo>,
}

impl Drop for SwState {
    /// The event loop is gone: the service workers it ran end, and its connections close, which
    /// is a client unload for each of its clients (§8 Handle Service Worker Client Unload).
    fn drop(&mut self) {
        for (tx, waker) in std::mem::take(&mut self.running).into_values() {
            let _ = tx.send(crate::worker::ToWorker::Terminate);
            waker.wake();
        }
        if let Some(backend) = &self.backend {
            for conn in std::mem::take(&mut self.conns).into_values() {
                backend.disconnect(conn);
            }
        }
    }
}

fn job_error(e: JobError) -> Error {
    let name = match e.name.as_str() {
        "TypeError" => return Error::Type(e.message.into()),
        "SecurityError" => ExceptionName::SecurityError,
        "InvalidStateError" => ExceptionName::InvalidStateError,
        "NotFoundError" => ExceptionName::NotFoundError,
        "AbortError" => ExceptionName::AbortError,
        _ => ExceptionName::InvalidStateError,
    };
    Error::Dom(name, e.message.into())
}

fn state_value(s: WorkerState) -> ServiceWorkerState {
    match s {
        WorkerState::Parsed => ServiceWorkerState::Parsed,
        WorkerState::Installing => ServiceWorkerState::Installing,
        WorkerState::Installed => ServiceWorkerState::Installed,
        WorkerState::Activating => ServiceWorkerState::Activating,
        WorkerState::Activated => ServiceWorkerState::Activated,
        WorkerState::Redundant => ServiceWorkerState::Redundant,
    }
}

impl Engine {
    /// Uses `backend` for this event loop's service worker registrations (the embedder's, shared
    /// by its pages). Must come before the first use.
    pub fn set_service_worker_backend(&self, backend: ServiceWorkerBackend) {
        self.sw.borrow_mut().backend = Some(backend);
    }

    /// The registrations of this event loop: the embedder's, or a registry of this page's own
    /// that fetches through the page's network.
    pub(crate) fn sw_backend(&self) -> ServiceWorkerBackend {
        if let Some(b) = self.sw.borrow().backend.clone() {
            return b;
        }
        let net = self
            .hooks
            .borrow()
            .net
            .as_ref()
            .map(|n| n.backend().clone());
        let b =
            ServiceWorkerBackend::new(Arc::new(snowlark_sw::LocalServiceWorkers::new(net, None)));
        self.sw.borrow_mut().backend = Some(b.clone());
        b
    }

    /// The connection of this event loop for `key`, opened on first use.
    fn sw_conn(&self, key: &str) -> ConnId {
        if let Some(c) = self.sw.borrow().conns.get(key) {
            return *c;
        }
        let backend = self.sw_backend();
        let inbox = self.sw.borrow().inbox.clone();
        let waker = self.waker();
        let sink = Arc::new(move |n: Notice| {
            inbox.lock().expect("the notice queue").push_back(n);
            waker.wake();
        });
        // (a service worker's own event loop does not run others)
        let runner = self.sw.borrow().own.is_none();
        let conn = backend.connect(key.to_owned(), runner, sink);
        self.sw.borrow_mut().conns.insert(key.to_owned(), conn);
        // (the registry learns this loop's clients of the key before its first call)
        self.sw_sync_clients();
        conn
    }

    /// Tells the registry which service worker clients this event loop has, for each key it is
    /// connected for: each browsing context's active document with an HTTP(S) URL. A document
    /// that is gone is a client unload (§8 Handle Service Worker Client Unload).
    pub(crate) fn sw_sync_clients(&self) {
        let (backend, conns) = {
            let s = self.sw.borrow();
            if s.own.is_some() || s.conns.is_empty() {
                return;
            }
            let Some(b) = s.backend.clone() else { return };
            (b, s.conns.clone())
        };
        let contexts: Vec<(Handle, NodeId)> = {
            let f = self.frames.borrow();
            f.list
                .iter()
                .filter(|b| !b.discarded)
                .map(|b| (b.window, b.document))
                .collect()
        };
        let mut now: HashMap<NodeId, (String, url::Url, bool)> = HashMap::new();
        for (window, doc) in contexts {
            if !self.dom().is_alive(doc) || self.associated_document(window) != Some(doc) {
                continue;
            }
            let Ok(url) = url::Url::parse(self.dom().document_url(doc)) else {
                continue;
            };
            if !matches!(url.scheme(), "http" | "https") {
                continue;
            }
            let Some(key) = self.storage_key(window) else {
                continue;
            };
            if !conns.contains_key(&key) {
                continue;
            }
            let secure =
                snowlark_bindings::generated::WindowOrWorkerGlobalScopeImpl::is_secure_context(
                    self, window,
                );
            now.insert(doc, (key, url, secure));
        }
        let mut calls: Vec<(ConnId, Call)> = Vec::new();
        {
            let mut s = self.sw.borrow_mut();
            let gone: Vec<NodeId> = s
                .clients
                .keys()
                .filter(|d| !now.contains_key(d))
                .copied()
                .collect();
            for doc in gone {
                let (client, key) = s.clients.remove(&doc).expect("the client");
                s.controllers.remove(&doc);
                if let Some(conn) = conns.get(&key) {
                    calls.push((*conn, Call::ClientGone { client }));
                }
            }
            for (doc, (key, url, secure)) in now {
                if s.clients.contains_key(&doc) {
                    continue;
                }
                s.next_client += 1;
                let client = s.next_client;
                s.clients.insert(doc, (client, key.clone()));
                calls.push((
                    conns[&key],
                    Call::Client {
                        client,
                        url,
                        secure,
                    },
                ));
            }
        }
        for (conn, call) in calls {
            backend.call(conn, 0, call);
        }
    }

    /// Sends `call` for `global`; its answer settles `promise` as `kind` says.
    fn sw_call(
        &self,
        cx: &mut Cx,
        global: Handle,
        promise: &JsPromise,
        kind: PendingKind,
        call: impl FnOnce(&Engine, &mut Cx) -> rt::Result<Call>,
    ) {
        let Some(key) = self.storage_key(global) else {
            promise.reject(
                cx,
                Error::Dom(
                    ExceptionName::SecurityError,
                    "Service workers are not available in an opaque origin".into(),
                ),
            );
            return;
        };
        let call = match call(self, cx) {
            Ok(c) => c,
            Err(e) => {
                promise.reject(cx, e);
                return;
            }
        };
        let conn = self.sw_conn(&key);
        let id = {
            let mut s = self.sw.borrow_mut();
            s.next_call += 1;
            let id = s.next_call;
            s.pending.insert(
                id,
                Pending {
                    global,
                    promise: promise.clone(),
                    kind,
                },
            );
            id
        };
        self.sw_backend().call(conn, id, call);
    }

    /// Whether a call waits for its answer or notices wait to be taken (`ready`, which may never
    /// settle, does not keep the loop busy).
    pub(crate) fn service_workers_pending(&self) -> bool {
        let s = self.sw.borrow();
        let recent = s
            .last_notice
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1));
        if s.pending
            .values()
            .any(|p| p.kind != PendingKind::Ready || recent)
            || !s.inbox.lock().expect("the notice queue").is_empty()
        {
            return true;
        }
        // (a worker this loop sees on its way through installing or activating: more state
        // changes follow, each bounded by the registry's event timeout)
        s.workers.values().any(|h| {
            self.with_worker(*h, |w| {
                matches!(
                    w.state,
                    WorkerState::Parsed | WorkerState::Installing | WorkerState::Activating
                )
            }) == Some(true)
        })
    }

    /// Takes the registry's notices and queues a task for each, in order. Returns whether any came.
    pub(crate) fn poll_service_workers(&self, _cx: &mut Cx) -> bool {
        let notices: Vec<Notice> = {
            let s = self.sw.borrow();
            let mut q = s.inbox.lock().expect("the notice queue");
            q.drain(..).collect()
        };
        let any = !notices.is_empty();
        if any {
            self.sw.borrow_mut().last_notice = Some(std::time::Instant::now());
        }
        for n in notices {
            match n {
                // (to the runner: at once, not in a task of a page)
                Notice::Start {
                    version,
                    registration,
                    source,
                    worker_type,
                } => self.start_service_worker(version, registration, source, worker_type),
                Notice::Stop { version } => {
                    if let Some((tx, waker)) = self.sw.borrow_mut().running.remove(&version) {
                        let _ = tx.send(crate::worker::ToWorker::Terminate);
                        waker.wake();
                    }
                }
                n => self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    e.handle_notice(cx, n)
                }),
            }
        }
        any
    }

    fn handle_notice(&self, cx: &mut Cx, n: Notice) {
        match n {
            Notice::Answer { call, answer } => {
                let Some(p) = self.sw.borrow_mut().pending.remove(&call) else {
                    return;
                };
                self.settle_sw_answer(cx, p, answer);
            }
            Notice::Slot {
                registration,
                slot,
                version,
            } => {
                let objects: Vec<(Handle, Handle)> = self
                    .sw
                    .borrow()
                    .registrations
                    .iter()
                    .filter(|((_, id), _)| *id == registration)
                    .map(|((g, _), h)| (*g, *h))
                    .collect();
                for (global, h) in objects {
                    let w = version
                        .as_ref()
                        .map(|v| self.service_worker_object(global, v));
                    if let Object::ServiceWorkerRegistration(r) = &mut *self.obj_mut(h) {
                        match slot {
                            Slot::Installing => r.installing = w,
                            Slot::Waiting => r.waiting = w,
                            Slot::Active => r.active = w,
                        }
                    }
                }
            }
            Notice::Controller { client, version } => {
                let doc = {
                    let mut s = self.sw.borrow_mut();
                    let Some(doc) = s
                        .clients
                        .iter()
                        .find(|(_, (c, _))| *c == client)
                        .map(|(d, _)| *d)
                    else {
                        return;
                    };
                    match version {
                        Some(v) => s.controllers.insert(doc, v),
                        None => s.controllers.remove(&doc),
                    };
                    doc
                };
                // §8 Notify Controller Change: `controllerchange` at the client's container (one
                // made later has no listener to tell)
                let Some(window) = self.window_of_document(doc) else {
                    return;
                };
                let container = self
                    .frames
                    .borrow()
                    .singletons
                    .get(&(window, "serviceWorker"))
                    .copied();
                if let Some(c) = container {
                    self.fire_event(cx, c, "controllerchange", false, false);
                }
            }
            Notice::State { version, state } => {
                for v in self.sw.borrow_mut().controllers.values_mut() {
                    if v.id == version {
                        v.state = state;
                    }
                }
                let objects: Vec<Handle> = self
                    .sw
                    .borrow()
                    .workers
                    .iter()
                    .filter(|((_, id), _)| *id == version)
                    .map(|(_, h)| *h)
                    .collect();
                for h in objects {
                    if let Object::ServiceWorker(w) = &mut *self.obj_mut(h) {
                        w.state = state;
                    }
                    self.fire_event(cx, h, "statechange", false, false);
                }
            }
            Notice::UpdateFound { registration } => {
                let objects: Vec<Handle> = self
                    .sw
                    .borrow()
                    .registrations
                    .iter()
                    .filter(|((_, id), _)| *id == registration)
                    .map(|(_, h)| *h)
                    .collect();
                for h in objects {
                    self.fire_event(cx, h, "updatefound", false, false);
                }
            }
            Notice::Dispatch { version, event } => self.dispatch_lifecycle(cx, version, event),
            Notice::Start { .. } | Notice::Stop { .. } => {}
        }
    }

    fn settle_sw_answer(&self, cx: &mut Cx, p: Pending, answer: Answer) {
        match answer {
            Answer::Registration(Ok(info)) => {
                let h = self.registration_object(p.global, &info);
                p.promise.resolve(cx, h);
            }
            Answer::Registration(Err(e)) | Answer::Unregistered(Err(e)) => {
                p.promise.reject(cx, job_error(e))
            }
            Answer::Lookup(found) => match found {
                Some(info) => {
                    let h = self.registration_object(p.global, &info);
                    p.promise.resolve(cx, h);
                }
                None => p.promise.resolve(cx, ()),
            },
            Answer::Lookups(all) => {
                let list: Vec<Handle> = all
                    .iter()
                    .map(|info| self.registration_object(p.global, info))
                    .collect();
                p.promise.resolve(cx, Frozen(list));
            }
            Answer::Unregistered(Ok(b)) => p.promise.resolve(cx, b),
            Answer::Claimed(Ok(())) | Answer::Done => p.promise.resolve(cx, ()),
            Answer::Claimed(Err(e)) => p.promise.reject(cx, job_error(e)),
        }
        let _ = p.kind;
    }

    /// §3.2 "get the service worker registration object": one per registration in a global.
    fn registration_object(&self, global: Handle, info: &RegistrationInfo) -> Handle {
        if let Some(h) = self.sw.borrow().registrations.get(&(global, info.id)) {
            return *h;
        }
        let slot =
            |v: &Option<VersionInfo>| v.as_ref().map(|v| self.service_worker_object(global, v));
        let (installing, waiting, active) = (
            slot(&info.installing),
            slot(&info.waiting),
            slot(&info.active),
        );
        let h = self.alloc(Object::ServiceWorkerRegistration(Box::new(
            RegistrationData {
                global,
                id: info.id,
                scope: info.scope.to_string(),
                update_via_cache: info.update_via_cache,
                installing,
                waiting,
                active,
            },
        )));
        self.sw
            .borrow_mut()
            .registrations
            .insert((global, info.id), h);
        h
    }

    /// §3.1 "get the service worker object": one per service worker in a global.
    fn service_worker_object(&self, global: Handle, v: &VersionInfo) -> Handle {
        if let Some(h) = self.sw.borrow().workers.get(&(global, v.id)) {
            return *h;
        }
        let h = self.alloc(Object::ServiceWorker(Box::new(WorkerData {
            global,
            id: v.id,
            script_url: v.script_url.to_string(),
            state: v.state,
        })));
        self.sw.borrow_mut().workers.insert((global, v.id), h);
        h
    }

    /// `navigator.serviceWorker`: one container per global.
    pub(crate) fn service_worker_container(&self, global: Handle) -> Handle {
        self.window_singleton(global, "serviceWorker", || Object::ServiceWorkerContainer {
            global,
            ready: None,
        })
    }

    fn container_global(&self, h: Handle) -> Handle {
        match &*self.obj(h) {
            Object::ServiceWorkerContainer { global, .. } => *global,
            _ => h,
        }
    }

    fn with_registration<R>(&self, h: Handle, f: impl FnOnce(&RegistrationData) -> R) -> Option<R> {
        match &*self.obj(h) {
            Object::ServiceWorkerRegistration(r) => Some(f(r)),
            _ => None,
        }
    }

    fn with_worker<R>(&self, h: Handle, f: impl FnOnce(&WorkerData) -> R) -> Option<R> {
        match &*self.obj(h) {
            Object::ServiceWorker(w) => Some(f(w)),
            _ => None,
        }
    }

    /// The job client of a global: its document's request client.
    fn sw_client(&self, global: Handle) -> rt::Result<snowlark_net::Client> {
        let doc = self
            .associated_document(global)
            .ok_or_else(|| Error::Type("The global has no document".into()))?;
        Ok((*self.request_client(doc)).clone())
    }

    /// The URL a global's matching uses: its document's (a window's creation URL), or its own.
    fn sw_client_url(&self, global: Handle) -> Option<url::Url> {
        let doc = self.associated_document(global)?;
        url::Url::parse(self.dom().document_url(doc)).ok()
    }

    // ----- running a service worker ----------------------------------------------------------------

    /// The runner's part of [`Notice::Start`]: the version's thread, with its script.
    fn start_service_worker(
        &self,
        version: VersionInfo,
        registration: RegistrationInfo,
        source: Vec<u8>,
        worker_type: WorkerType,
    ) {
        let _ = worker_type;
        let (to_worker, inbox) = mpsc::channel();
        let (to_parent, _from_worker) = mpsc::channel();
        let waker = Arc::new(Waker::default());
        let id = version.id;
        let init = crate::worker::WorkerInit {
            owner_secure: true,
            net: self.hooks.borrow().net.clone(),
            console: if std::env::var("SNOWLARK_CONSOLE").is_ok_and(|v| v == "tagged") {
                crate::engine::window::ConsoleOutput::Tagged
            } else {
                self.hooks.borrow().console.output
            },
            url: version.script_url.clone(),
            name: String::new(),
            waker: waker.clone(),
            parent_waker: self.waker(),
            broadcast: self.broadcast_hub(),
            idb: self.idb_backend_for_worker(),
            caches: self.cache_backend(),
            webgl: self.gl.borrow().provider.clone(),
            sw_backend: None,
            to_parent,
            inbox,
            creator: None,
            blob_url_entry: None,
            module: None,
            service_worker: Some(SwStart {
                backend: self.sw_backend(),
                version,
                registration,
                source,
            }),
        };
        let started = std::thread::Builder::new()
            .name("snowlark-service-worker".into())
            .stack_size(8 << 20)
            .spawn(move || crate::worker::worker_main(init));
        if started.is_ok() {
            self.sw.borrow_mut().running.insert(id, (to_worker, waker));
        }
    }

    /// On a service worker's thread: this event loop's own version and registration.
    pub(crate) fn set_service_worker_own(&self, start: &SwStart) {
        let mut s = self.sw.borrow_mut();
        s.backend = Some(start.backend.clone());
        s.own = Some((start.version.clone(), start.registration.clone()));
    }

    /// Sends a call from the service worker's own connection with no promise.
    fn sw_send_own(&self, call: Call) {
        let Some(global) = self.window() else { return };
        let Some(key) = self.storage_key(global) else {
            return;
        };
        let conn = self.sw_conn(&key);
        self.sw_backend().call(conn, 0, call);
    }

    /// §8 Run Service Worker: runs the script it was started with and tells the registry whether
    /// it ran (a script that throws or does not compile fails the job).
    pub(crate) fn run_service_worker_script(&self, cx: &mut Cx, source: &[u8]) -> bool {
        let Some((version, _)) = self.sw.borrow().own.clone() else {
            return false;
        };
        let text = String::from_utf8_lossy(source).into_owned();
        // (the connection first, so the registry's notices about this version reach this loop
        // in order with its events)
        self.sw_send_own_connect();
        // (its `registration` and `serviceWorker` objects exist from the start, so the notices
        // that follow keep them current)
        if let Some(global) = self.window() {
            ServiceWorkerGlobalScopeImpl::registration(self, global);
            ServiceWorkerGlobalScopeImpl::service_worker(self, global);
        }
        let url = version.script_url.as_str();
        let referrer = self.referrer_id(url, &Default::default());
        // (an exception is reported at the global; the script then failed)
        let ok = self
            .run_classic_with(cx, &text, url, 0, referrer, false, None, None)
            .is_some();
        self.microtask_checkpoint(cx);
        self.sw_send_own(Call::Evaluated {
            version: version.id,
            ok,
        });
        ok
    }

    fn sw_send_own_connect(&self) {
        if let Some(global) = self.window() {
            if let Some(key) = self.storage_key(global) {
                self.sw_conn(&key);
            }
        }
    }

    /// Fires `install` or `activate` at the global and reports when its extensions settle (§8
    /// Install and Activate, "dispatch ... wait for ... extend lifetime promises").
    fn dispatch_lifecycle(&self, cx: &mut Cx, version: VersionId, event: Lifecycle) {
        let Some(global) = self.window() else { return };
        let ext = Rc::new(Extensions::default());
        ext.report.set(Some((version, event)));
        let (iface, name) = match event {
            Lifecycle::Install => (InterfaceId::ExtendableEvent, "install"),
            Lifecycle::Activate => (InterfaceId::ExtendableEvent, "activate"),
        };
        let ev = self.trusted_event(iface, name, false, false, EventExt::Extendable(ext.clone()));
        self.dispatch(cx, ev, global, false);
        self.microtask_checkpoint(cx);
        if ext.pending.get() == 0 {
            self.report_extended(&ext);
        }
    }

    /// The lifecycle event's extensions all settled: its outcome goes to the registry.
    fn report_extended(&self, ext: &Extensions) {
        if let Some((version, event)) = ext.report.take() {
            self.sw_send_own(Call::EventDone {
                version,
                event,
                ok: !ext.rejected.get(),
            });
        }
    }

    fn extensions(&self, event: Handle) -> Option<Rc<Extensions>> {
        self.ev(event, |e| match &e.ext {
            EventExt::Extendable(x) => Some(x.clone()),
            _ => None,
        })
    }
}

impl ServiceWorkerContainerImpl for Engine {
    /// §3.4.1 `controller`: the service worker object of the client's active service worker.
    /// (Until fetch interception exists, a controlled client's requests go to the network, as
    /// when the worker does not respond.)
    fn controller(&self, this: Handle) -> Option<Handle> {
        let global = self.container_global(this);
        let doc = self.associated_document(global)?;
        let v = self.sw.borrow().controllers.get(&doc).cloned()?;
        Some(self.service_worker_object(global, &v))
    }

    /// §3.4.2 `ready`: settled with the registration matching the client once it has an active
    /// worker. [SameObject] in effect: one promise per container.
    fn ready(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        if let Object::ServiceWorkerContainer { ready: Some(p), .. } = &*self.obj(this) {
            return Ok(p.clone());
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if let Object::ServiceWorkerContainer { ready, .. } = &mut *self.obj_mut(this) {
            *ready = Some(p.clone());
        }
        let global = self.container_global(this);
        let Some(client_url) = self.sw_client_url(global) else {
            return Ok(p);
        };
        self.sw_call(cx, global, &p, PendingKind::Ready, move |_, _| {
            Ok(Call::Ready { client_url })
        });
        Ok(p)
    }

    /// §3.4.3 `register(scriptURL, options)` and "Start Register".
    fn register(
        &self,
        cx: &mut Cx,
        this: Handle,
        script_url: TrustedScriptURLOrString,
        options: RegistrationOptions,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let global = self.container_global(this);
        // Trusted Types: "Get Trusted Type compliant string" for the sink (a violation throws).
        let s = match self.associated_document(global) {
            Some(doc) => match self.tt_compliant(
                cx,
                script_url.into(),
                crate::engine::trusted_types::TtKind::ScriptUrl,
                doc,
                "ServiceWorkerContainer register",
            ) {
                Ok(s) => s,
                Err(e) => {
                    p.reject(cx, e);
                    return Ok(p);
                }
            },
            None => match script_url {
                TrustedScriptURLOrString::String(s) => s,
                TrustedScriptURLOrString::TrustedScriptURL(h) => match &*self.obj(h) {
                    Object::TrustedScriptUrl(s) => s.clone(),
                    _ => String::new(),
                },
            },
        };
        let base = match self.current_base_url(cx) {
            Ok(b) => b,
            Err(e) => {
                p.reject(cx, e);
                return Ok(p);
            }
        };
        self.sw_call(cx, global, &p, PendingKind::Registration, move |e, _| {
            let script_url = base
                .join(&s)
                .map_err(|_| Error::Type("The script URL is not a valid URL".into()))?;
            let scope = match &options.scope {
                Some(scope) => base
                    .join(scope)
                    .map_err(|_| Error::Type("The scope is not a valid URL".into()))?,
                None => script_url
                    .join("./")
                    .map_err(|_| Error::Type("The scope is not a valid URL".into()))?,
            };
            if options.type_ == snowlark_bindings::generated::WorkerType::Module {
                return Err(Error::Type(
                    "Module service workers are not supported yet".into(),
                ));
            }
            Ok(Call::Register {
                client: e.sw_client(global)?,
                script_url,
                scope,
                update_via_cache: match options.update_via_cache {
                    ServiceWorkerUpdateViaCache::Imports => UpdateViaCache::Imports,
                    ServiceWorkerUpdateViaCache::All => UpdateViaCache::All,
                    ServiceWorkerUpdateViaCache::None => UpdateViaCache::None,
                },
                worker_type: WorkerType::Classic,
            })
        });
        Ok(p)
    }

    /// §3.4.4 `getRegistration(clientURL)`.
    fn get_registration(
        &self,
        cx: &mut Cx,
        this: Handle,
        client_url: &str,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let global = self.container_global(this);
        let base = match self.current_base_url(cx) {
            Ok(b) => b,
            Err(e) => {
                p.reject(cx, e);
                return Ok(p);
            }
        };
        let client_url = client_url.to_owned();
        self.sw_call(cx, global, &p, PendingKind::Lookup, move |e, _| {
            let url = base
                .join(&client_url)
                .map_err(|_| Error::Type("The client URL is not a valid URL".into()))?;
            let own = e.sw_client_url(global).map(|u| u.origin());
            if own.as_ref() != Some(&url.origin()) {
                return Err(Error::Dom(
                    ExceptionName::SecurityError,
                    "The client URL's origin is not the page's".into(),
                ));
            }
            Ok(Call::GetRegistration { client_url: url })
        });
        Ok(p)
    }

    /// §3.4.5 `getRegistrations()`.
    fn get_registrations(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let global = self.container_global(this);
        self.sw_call(cx, global, &p, PendingKind::Lookups, |_, _| {
            Ok(Call::GetRegistrations)
        });
        Ok(p)
    }

    fn start_messages(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
}

impl ServiceWorkerRegistrationImpl for Engine {
    fn installing(&self, this: Handle) -> Option<Handle> {
        self.with_registration(this, |r| r.installing).flatten()
    }
    fn waiting(&self, this: Handle) -> Option<Handle> {
        self.with_registration(this, |r| r.waiting).flatten()
    }
    fn active(&self, this: Handle) -> Option<Handle> {
        self.with_registration(this, |r| r.active).flatten()
    }
    fn scope(&self, this: Handle) -> DomStr<'_> {
        self.with_registration(this, |r| r.scope.clone())
            .unwrap_or_default()
            .into()
    }
    fn update_via_cache(&self, this: Handle) -> ServiceWorkerUpdateViaCache {
        match self.with_registration(this, |r| r.update_via_cache) {
            Some(UpdateViaCache::All) => ServiceWorkerUpdateViaCache::All,
            Some(UpdateViaCache::None) => ServiceWorkerUpdateViaCache::None,
            _ => ServiceWorkerUpdateViaCache::Imports,
        }
    }

    /// §3.2.7 `update()`.
    fn update(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let Some((global, id)) = self.with_registration(this, |r| (r.global, r.id)) else {
            return Ok(p);
        };
        // (a service worker whose own state is "installing" may not update its registration)
        if let Some((v, _)) = self.sw.borrow().own.as_ref() {
            let installing = self
                .sw
                .borrow()
                .workers
                .get(&(global, v.id))
                .and_then(|h| self.with_worker(*h, |w| w.state))
                == Some(WorkerState::Installing);
            if installing {
                p.reject(
                    cx,
                    Error::Dom(
                        ExceptionName::InvalidStateError,
                        "An installing service worker cannot update its registration".into(),
                    ),
                );
                return Ok(p);
            }
        }
        self.sw_call(cx, global, &p, PendingKind::Registration, move |e, _| {
            Ok(Call::Update {
                client: e.sw_client(global)?,
                registration: id,
            })
        });
        Ok(p)
    }

    /// §3.2.8 `unregister()`.
    fn unregister(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let Some((global, id)) = self.with_registration(this, |r| (r.global, r.id)) else {
            return Ok(p);
        };
        self.sw_call(cx, global, &p, PendingKind::Unregister, move |e, _| {
            Ok(Call::Unregister {
                client: e.sw_client(global)?,
                registration: id,
            })
        });
        Ok(p)
    }

    /// Notifications API `showNotification()`: notification permission is never granted.
    fn show_notification(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _title: &str,
        _options: NotificationOptions,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.reject(
            cx,
            Error::Type("No notification permission has been granted".into()),
        );
        Ok(p)
    }

    fn get_notifications(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _filter: GetNotificationOptions,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.resolve(cx, Frozen(Vec::<Handle>::new()));
        Ok(p)
    }
}

impl ServiceWorkerImpl for Engine {
    fn script_url(&self, this: Handle) -> DomStr<'_> {
        self.with_worker(this, |w| w.script_url.clone())
            .unwrap_or_default()
            .into()
    }
    fn state(&self, this: Handle) -> ServiceWorkerState {
        state_value(
            self.with_worker(this, |w| w.state)
                .unwrap_or(WorkerState::Redundant),
        )
    }
    fn post_message_message_transfer(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _message: JsValue,
        _transfer: Vec<JsObject>,
    ) -> rt::Result<()> {
        // (messages to a service worker come with clients and messaging, not yet)
        Ok(())
    }
    fn post_message_message_options(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _message: JsValue,
        _options: StructuredSerializeOptions,
    ) -> rt::Result<()> {
        Ok(())
    }
}

impl ServiceWorkerGlobalScopeImpl for Engine {
    fn clients(&self, this: Handle) -> Handle {
        self.window_singleton(this, "clients", || Object::Clients { global: this })
    }

    fn registration(&self, this: Handle) -> Handle {
        let info = self.sw.borrow().own.as_ref().map(|o| o.1.clone());
        match info {
            Some(info) => self.registration_object(this, &info),
            None => this,
        }
    }

    fn service_worker(&self, this: Handle) -> Handle {
        let info = self.sw.borrow().own.as_ref().map(|o| o.0.clone());
        match info {
            Some(info) => self.service_worker_object(this, &info),
            None => this,
        }
    }

    /// §4.1.4 `skipWaiting()`.
    fn skip_waiting(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let Some(version) = self.sw.borrow().own.as_ref().map(|o| o.0.id) else {
            p.resolve(cx, ());
            return Ok(p);
        };
        self.sw_call(cx, this, &p, PendingKind::Done, move |_, _| {
            Ok(Call::SkipWaiting { version })
        });
        Ok(p)
    }
}

impl ClientsImpl for Engine {
    fn get(&self, cx: &mut Cx, _this: Handle, _id: &str) -> rt::Result<JsPromise> {
        // (no client is known before clients and messaging exist)
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.resolve(cx, ());
        Ok(p)
    }

    /// §4.3.4 `claim()`: the registry checks that this is the active worker and makes it the
    /// active service worker of the clients its registration matches.
    fn claim(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let global = match &*self.obj(this) {
            Object::Clients { global } => *global,
            _ => this,
        };
        let Some(version) = self.sw.borrow().own.as_ref().map(|o| o.0.id) else {
            p.reject(
                cx,
                Error::Dom(
                    ExceptionName::InvalidStateError,
                    "Only the active service worker can claim clients".into(),
                ),
            );
            return Ok(p);
        };
        self.sw_call(cx, global, &p, PendingKind::Done, move |_, _| {
            Ok(Call::Claim { version })
        });
        Ok(p)
    }
}

impl ExtendableEventImpl for Engine {
    /// §4.4.1 `waitUntil(f)`: add lifetime promise.
    fn wait_until(&self, cx: &mut Cx, this: Handle, f: JsObject) -> rt::Result<()> {
        let trusted = self.ev(this, |e| e.is_trusted);
        let dispatching = self.ev(this, |e| e.dispatching);
        let Some(ext) = self.extensions(this) else {
            return Ok(());
        };
        if !trusted {
            return Err(Error::Dom(
                ExceptionName::InvalidStateError,
                "The event was not dispatched by the browser".into(),
            ));
        }
        if !dispatching && ext.pending.get() == 0 {
            return Err(Error::Dom(
                ExceptionName::InvalidStateError,
                "The event's lifetime is over".into(),
            ));
        }
        ext.pending.set(ext.pending.get() + 1);
        let value = {
            let scope = cx.scope();
            let o = v8::Local::new(scope, f.global());
            JsValue::new(scope, o.into())
        };
        self.upon(
            cx,
            &value,
            Box::new(
                move |e: &Engine, cx: &mut Cx, outcome: Result<JsValue, JsValue>| {
                    if outcome.is_err() {
                        ext.rejected.set(true);
                    }
                    ext.pending.set(ext.pending.get() - 1);
                    if ext.pending.get() == 0 {
                        // (a microtask later, so promises added in reaction to this one count)
                        let ext = ext.clone();
                        let undefined = rt::undefined_value(cx.scope());
                        e.upon(
                            cx,
                            &undefined,
                            Box::new(move |e: &Engine, _: &mut Cx, _| {
                                if ext.pending.get() == 0 {
                                    e.report_extended(&ext);
                                }
                            }),
                        );
                    }
                },
            ),
        );
        Ok(())
    }

    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: ExtendableEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::ExtendableEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Extendable(Rc::new(Extensions::default())),
        ))
    }
}
