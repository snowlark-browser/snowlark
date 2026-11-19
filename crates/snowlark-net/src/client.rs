// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The network thread and the channel-based client page threads use.
//!
//! Page threads are synchronous. [`NetClient::fetch`] hands a [`Request`] to the network thread and
//! returns a [`ResponseStream`] at once; the page polls it ([`ResponseStream::try_next`]) from its event
//! loop, optionally woken by a callback, or blocks on it ([`ResponseStream::next_blocking`]). Dropping the
//! stream cancels the fetch.

use std::sync::Arc;
use std::thread::JoinHandle;

use bytes::{Bytes, BytesMut};
use tokio::sync::{mpsc, oneshot};

use crate::blob::{BlobData, BlobStore};
use crate::config::NetConfig;
use crate::cookies::{CookieContext, CookieStore};
use crate::cors::PreflightCache;
use crate::csp::Violation;
use crate::fetch::{NetState, Sink};
use crate::filter::RequestFilter;
use crate::hsts::HstsStore;
use crate::observe::FetchObserver;
use crate::request::Request;
use crate::response::{NetError, NetErrorKind, Response, ResponseHead};
use crate::transport::Transport;

/// One step of a fetch, in order: any number of `Violation`s, then either `Error`, or `Head`, any number
/// of `Body` chunks (already content-decoded) and `Done` (or `Error` if the body failed midway).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum FetchEvent {
    /// A CSP violation the request caused (report-only or enforced), for the document to report.
    Violation(Violation),
    Head(ResponseHead),
    Body(Bytes),
    Done,
    Error(NetError),
}

/// A callback that sees a fetch's events on the network thread ([`NetClient::fetch_observed`]).
pub type Observer = Arc<dyn Fn(&FetchEvent) + Send + Sync>;

/// The events of one fetch.
pub struct ResponseStream {
    rx: Rx,
    /// Cancels the fetch when dropped (not for a preload's stream: the preload goes on for its
    /// consumer).
    _cancel: Option<oneshot::Sender<()>>,
    /// A backend of another process: tells it the page dropped the stream before the last event.
    on_cancel: Option<Box<dyn FnOnce() + Send>>,
    finished: bool,
    /// The client's observer and this fetch's id with it.
    observer: Option<(Arc<dyn FetchObserver>, u64)>,
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some((o, id)) = &self.observer {
            o.canceled(*id);
        }
        if let Some(cancel) = self.on_cancel.take() {
            cancel();
        }
    }
}

/// The network side of a stream made by [`ResponseStream::remote`]: a [`NetBackend`] of another
/// process feeds it the events it receives for the fetch, in [`FetchEvent`]'s order.
pub struct FetchSender {
    tx: mpsc::UnboundedSender<FetchEvent>,
}

impl FetchSender {
    /// Queues the next event. False once the page has dropped the stream: the fetch should stop.
    /// It never waits, and does not wake the page: the backend runs the fetch's `notify` after it.
    pub fn send(&self, event: FetchEvent) -> bool {
        self.tx.send(event).is_ok()
    }

    /// Whether the page has dropped the stream.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

enum Rx {
    Bounded(mpsc::Receiver<FetchEvent>),
    Unbounded(mpsc::UnboundedReceiver<FetchEvent>),
}

impl Rx {
    fn try_recv(&mut self) -> Result<FetchEvent, mpsc::error::TryRecvError> {
        match self {
            Rx::Bounded(r) => r.try_recv(),
            Rx::Unbounded(r) => r.try_recv(),
        }
    }

    fn blocking_recv(&mut self) -> Option<FetchEvent> {
        match self {
            Rx::Bounded(r) => r.blocking_recv(),
            Rx::Unbounded(r) => r.blocking_recv(),
        }
    }

    async fn recv(&mut self) -> Option<FetchEvent> {
        match self {
            Rx::Bounded(r) => r.recv().await,
            Rx::Unbounded(r) => r.recv().await,
        }
    }
}

impl ResponseStream {
    /// A stream for a [`NetBackend`] other than this process's network thread, fed through the
    /// [`FetchSender`]. The queue is unbounded, since one reader takes every fetch's events off the
    /// wire and must never wait on one page. `on_cancel` runs, once, if the page drops the stream
    /// before taking its last event; a sender dropped before the last event ends the stream with an
    /// `Aborted` error.
    pub fn remote(on_cancel: impl FnOnce() + Send + 'static) -> (FetchSender, ResponseStream) {
        let (tx, rx) = mpsc::unbounded_channel();
        let stream = ResponseStream {
            rx: Rx::Unbounded(rx),
            _cancel: None,
            on_cancel: Some(Box::new(on_cancel)),
            finished: false,
            observer: None,
        };
        (FetchSender { tx }, stream)
    }

    fn track(&mut self, event: Option<FetchEvent>) -> Option<FetchEvent> {
        match &event {
            Some(FetchEvent::Done) | Some(FetchEvent::Error(_)) | None => self.finished = true,
            _ => {}
        }
        if let (Some((o, id)), Some(e)) = (&self.observer, &event) {
            o.event(*id, e);
        }
        event
    }

    /// The next event if one is ready.
    pub fn try_next(&mut self) -> Option<FetchEvent> {
        if self.finished {
            return None;
        }
        match self.rx.try_recv() {
            Ok(e) => self.track(Some(e)),
            Err(mpsc::error::TryRecvError::Empty) => None,
            Err(mpsc::error::TryRecvError::Disconnected) => self.track(Some(FetchEvent::Error(
                NetError::new(NetErrorKind::Aborted, "the network thread stopped"),
            ))),
        }
    }

    /// Wait for the next event; `None` once the fetch has finished. Must not be called on the network
    /// thread (or inside any async runtime).
    pub fn next_blocking(&mut self) -> Option<FetchEvent> {
        if self.finished {
            return None;
        }
        match self.rx.blocking_recv() {
            Some(e) => self.track(Some(e)),
            None => self.track(Some(FetchEvent::Error(NetError::new(
                NetErrorKind::Aborted,
                "the network thread stopped",
            )))),
        }
    }

    /// The next event, awaited (on the network's runtime); `None` once the fetch has finished.
    pub async fn next_async(&mut self) -> Option<FetchEvent> {
        if self.finished {
            return None;
        }
        match self.rx.recv().await {
            Some(e) => self.track(Some(e)),
            None => self.track(Some(FetchEvent::Error(NetError::new(
                NetErrorKind::Aborted,
                "the network thread stopped",
            )))),
        }
    }

    /// Whether the last event (Done or Error) has been taken.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Block until the whole response is in, collecting violations on the way.
    pub fn collect(mut self) -> Collected {
        let mut violations = Vec::new();
        let mut head = None;
        let mut body = BytesMut::new();
        while let Some(event) = self.next_blocking() {
            match event {
                FetchEvent::Violation(v) => violations.push(v),
                FetchEvent::Head(h) => head = Some(h),
                FetchEvent::Body(b) => body.extend_from_slice(&b),
                FetchEvent::Done => {
                    let head = head.take().expect("Done always follows Head");
                    let has_body = !body.is_empty()
                        || !(head.response_type == crate::ResponseType::OpaqueRedirect
                            || matches!(head.status, 101 | 103 | 204 | 205 | 304));
                    return Collected {
                        result: Ok(Response {
                            head,
                            body: has_body.then(|| body.freeze()),
                        }),
                        violations,
                    };
                }
                FetchEvent::Error(e) => {
                    return Collected {
                        result: Err(e),
                        violations,
                    }
                }
            }
        }
        Collected {
            result: Err(NetError::new(
                NetErrorKind::Aborted,
                "the fetch ended without a response",
            )),
            violations,
        }
    }

    /// Block until the whole response is in.
    pub fn read_all(self) -> Result<Response, NetError> {
        self.collect().result
    }

    /// Stop the fetch (dropping the stream does the same).
    pub fn cancel(self) {}
}

/// A finished fetch.
#[derive(Debug)]
pub struct Collected {
    pub result: Result<Response, NetError>,
    pub violations: Vec<Violation>,
}

/// The handle page threads keep: cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct NetClient {
    backend: Arc<dyn NetBackend>,
    observer: Option<Arc<dyn FetchObserver>>,
    /// Content blocking's filter for this client's requests ([`crate::filter`]).
    filter: Option<Arc<dyn RequestFilter>>,
}

impl std::fmt::Debug for NetClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetClient").finish_non_exhaustive()
    }
}

/// How many events a fetch may have waiting before the network side waits for the page to read them.
const STREAM_CAPACITY: usize = 64;

impl NetClient {
    /// A client of `backend`: what a renderer process makes of its pipe to the browser.
    /// The backend its calls go to: what the browser serves a renderer's network calls from.
    pub fn backend(&self) -> &Arc<dyn NetBackend> {
        &self.backend
    }

    pub fn with_backend(backend: Arc<dyn NetBackend>) -> NetClient {
        NetClient {
            backend,
            observer: None,
            filter: None,
        }
    }

    /// Start a fetch.
    pub fn fetch(&self, request: Request) -> ResponseStream {
        self.start(request, None)
    }

    /// Start a fetch; `notify` runs on the network thread after each event is queued (to wake the page's
    /// event loop).
    pub fn fetch_with_notify(
        &self,
        request: Request,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> ResponseStream {
        self.start(request, Some(notify))
    }

    /// Start a fetch whose events `observer` also sees, on the network thread, as each is produced
    /// (before the page can read it). A body consumer that must not depend on the page thread uses
    /// it: V8's streaming compiler parses a script on a thread of its own while the page thread may be
    /// busy, or waiting for V8. If the fetch is cancelled midway, the observer sees an `Aborted`
    /// error.
    pub fn fetch_observed(
        &self,
        request: Request,
        notify: Arc<dyn Fn() + Send + Sync>,
        observer: Observer,
    ) -> ResponseStream {
        self.start_with(request, Some(notify), Some(observer))
    }

    /// Fetch "establish a WebSocket connection" and then the connection (RFC 6455); `notify` runs on
    /// the network thread after each event is queued.
    pub fn websocket(
        &self,
        params: crate::websocket::WsParams,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> crate::websocket::WebSocketConnection {
        self.backend.websocket(params, self.filter.clone(), notify)
    }

    /// The same client, with every fetch started through it (and its clones) reported to
    /// `observer` (the developer tools' Network panel).
    pub fn observed(&self, observer: Arc<dyn FetchObserver>) -> NetClient {
        NetClient {
            observer: Some(observer),
            ..self.clone()
        }
    }

    /// The observer every fetch of this client is reported to, if any.
    pub fn observer(&self) -> Option<Arc<dyn FetchObserver>> {
        self.observer.clone()
    }

    /// The same client, with every request started through it (and its clones) checked by
    /// `filter` before it goes out ([`crate::filter`]); `None` removes the filter.
    pub fn filtered(&self, filter: Option<Arc<dyn RequestFilter>>) -> NetClient {
        NetClient {
            filter,
            ..self.clone()
        }
    }

    /// The client's request filter.
    pub fn filter(&self) -> Option<&Arc<dyn RequestFilter>> {
        self.filter.as_ref()
    }

    fn start(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> ResponseStream {
        self.start_with(request, notify, None)
    }

    fn start_with(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        observer: Option<Observer>,
    ) -> ResponseStream {
        let devtools = self
            .observer
            .as_ref()
            .map(|o| (o.clone(), o.started(&request)));
        let mut stream = self
            .backend
            .start(request, notify, observer, self.filter.clone());
        stream.observer = devtools;
        stream
    }

    /// Starts a preload of `request` (HTML "preload"): its response waits in the list of preloaded
    /// resources for the fetch that consumes it ([`crate::preload`]). The stream returned shows the
    /// preload's own progress (the `link` element fires `load` or `error` from it); dropping it does
    /// not stop the preload. A preload with the same key already there is not fetched again; its
    /// stream replays it. `None` when the request is not one a preload can serve (fetch it normally
    /// then).
    pub fn preload(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<ResponseStream> {
        self.backend.preload(request, notify, self.filter.clone())
    }

    /// [`NetClient::preload`] whose own fetch the client's observer sees, through the stream
    /// returned, as a fetch of its own: a `link` element's preload, which reports its own timing
    /// (HTML "preload"; the fetch that consumes it reports none).
    pub fn preload_observed(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<ResponseStream> {
        let observer = self
            .observer
            .as_ref()
            .map(|o| (o.clone(), o.started(&request)));
        let Some(mut stream) = self.backend.preload(request, notify, self.filter.clone()) else {
            if let Some((o, id)) = observer {
                o.canceled(id);
            }
            return None;
        };
        stream.observer = observer;
        Some(stream)
    }

    /// Preloads fetched and preloads a fetch consumed, so far.
    pub fn preload_counts(&self) -> (usize, usize) {
        self.backend.preload_counts()
    }

    /// The connection pools now, a line per origin with a request on it or waiting for it
    /// (connections in use of the per-host limit, requests waiting, idle connections): for a
    /// harness to say why a load stalled.
    pub fn pool_report(&self) -> Vec<String> {
        self.backend.pool_report()
    }

    /// Whether a preload for `request`'s preload key waits for its consumer.
    pub fn is_preloaded(&self, request: &Request) -> bool {
        self.backend.is_preloaded(request)
    }

    /// HTML `<link rel=preconnect>`: opens a connection to `url`'s origin (TCP, and TLS for https)
    /// and leaves it in the pool for the requests that follow. Best effort; errors are ignored.
    pub fn preconnect(&self, url: &url::Url) {
        self.backend.preconnect(url);
    }

    /// HTML `<link rel=dns-prefetch>`: resolves `host` ahead of use (the system resolver caches it).
    pub fn dns_prefetch(&self, host: &str, port: u16) {
        self.backend.dns_prefetch(host, port);
    }

    /// The user agent in force: the `User-Agent` header of every request this network thread makes
    /// from now on and what pages read of it.
    pub fn user_agent(&self) -> Arc<crate::ua::UserAgent> {
        self.backend.user_agent()
    }

    /// Whether fetches and scripts log what they do (`NetConfig::trace`).
    pub fn trace(&self) -> bool {
        self.backend.trace()
    }

    /// The profile's folder, for the V8 code cache; `None` keeps it in memory.
    pub fn profile_dir(&self) -> Option<std::path::PathBuf> {
        self.backend.profile_dir()
    }

    /// `document.cookie`'s getter: the cookies of `context` a script may read.
    pub fn document_cookie(&self, context: &CookieContext) -> String {
        self.backend.document_cookie(context)
    }

    /// `document.cookie`'s setter.
    pub fn set_document_cookie(&self, value: &str, context: &CookieContext) {
        self.backend.set_document_cookie(value, context);
    }

    /// Registers `data` under a new `blob:` URL of `origin`, which it returns.
    pub fn register_blob(&self, origin: &url::Origin, data: BlobData) -> String {
        self.backend.register_blob(origin, data)
    }

    /// Revokes a `blob:` URL of `origin`.
    pub fn revoke_blob(&self, url: &url::Url, origin: &url::Origin) {
        self.backend.revoke_blob(url, origin);
    }

    /// A hold on the blob URL entry `url` names now ([`crate::blob::BlobStore::hold`]), for a
    /// request of that URL ([`Request::blob_url_entry`]): what parsing the URL resolves. `None`
    /// for a URL that names no blob.
    pub fn hold_blob_url(&self, url: &url::Url) -> Option<u64> {
        if url.scheme() != "blob" {
            return None;
        }
        self.backend.hold_blob_url(url)
    }

    /// Releases a hold the request that carried it is done with.
    pub fn release_blob_url(&self, token: u64) {
        self.backend.release_blob_url(token);
    }

    /// See [`NetBackend::hand_over`].
    pub fn hand_over(&self, stream: ResponseStream) -> Option<u64> {
        self.backend.hand_over(stream)
    }

    /// See [`NetBackend::adopt`].
    pub fn adopt(
        &self,
        id: u64,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<ResponseStream> {
        self.backend.adopt(id, notify)
    }

    /// The network thread's own stores, for the browser half: a client whose backend lives in
    /// another process has none, and nothing in a page may ask for them.
    fn local(&self) -> &LocalNet {
        self.backend
            .local()
            .expect("the network's stores are the browser's")
    }

    pub fn cookies(&self) -> &Arc<CookieStore> {
        &self.local().state.cookies
    }

    pub fn hsts(&self) -> &Arc<HstsStore> {
        &self.local().state.hsts
    }

    pub fn blobs(&self) -> &Arc<BlobStore> {
        &self.local().state.blobs
    }

    pub fn config(&self) -> &NetConfig {
        &self.local().state.config
    }

    /// Changes the user agent for what starts from now on (a message from the browser; the value
    /// was sanitized where it was chosen, and is again where the header is built).
    pub fn set_user_agent(&self, ua: crate::ua::UserAgent) {
        *self
            .local()
            .state
            .user_agent
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(ua);
    }

    /// Empties the HTTP cache of the entries stored or used from `begin` (inclusive) to `end`
    /// (exclusive), in milliseconds since 1970.
    pub fn clear_http_cache(&self, begin: i64, end: i64) {
        self.local().state.http_cache.clear_between(begin, end);
    }

    /// Close idle pooled connections.
    pub fn close_idle_connections(&self) {
        self.local().state.transport.close_idle();
    }
}

/// What a client reaches the network through (`docs/process-isolation.md`, M2): [`LocalNet`], the
/// network thread of this process, or, in a renderer, the same calls as messages to the browser's.
/// A request's filter is the network side's: the browser attaches it for each tab.
pub trait NetBackend: Send + Sync {
    /// Starts a fetch. `notify` runs after each event is queued, and `observer` sees each as it is
    /// produced, on the network side.
    ///
    /// Dropping the stream cancels the fetch, except a keepalive one ([`Request::keepalive`]) that
    /// has not had its response head: that one goes on, redirects included, until its head or an
    /// error, or for [`NetConfig::keepalive_detached_timeout`] at most, and its events go nowhere.
    /// This is Chromium's `KeepAliveURLLoader` (`content/browser/loader/keep_alive_url_loader.cc`):
    /// `OnURLLoaderDisconnected` keeps the load going and starts the 30 s timer,
    /// `OnReceiveResponse` with no renderer connected deletes the loader, and
    /// `OnDisconnectedLoaderTimerFired` ends it. So a page that goes away, or a renderer that
    /// ends, does not cancel its beacons and keepalive fetches.
    fn start(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        observer: Option<Observer>,
        filter: Option<Arc<dyn RequestFilter>>,
    ) -> ResponseStream;

    /// See [`NetClient::websocket`].
    fn websocket(
        &self,
        params: crate::websocket::WsParams,
        filter: Option<Arc<dyn RequestFilter>>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> crate::websocket::WebSocketConnection;

    /// See [`NetClient::preload`].
    fn preload(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        filter: Option<Arc<dyn RequestFilter>>,
    ) -> Option<ResponseStream>;

    fn preload_counts(&self) -> (usize, usize);

    /// See [`NetClient::pool_report`]: nothing from a backend that does not see the connections.
    fn pool_report(&self) -> Vec<String> {
        Vec::new()
    }

    fn is_preloaded(&self, request: &Request) -> bool;

    fn preconnect(&self, url: &url::Url);

    fn dns_prefetch(&self, host: &str, port: u16);

    fn user_agent(&self) -> Arc<crate::ua::UserAgent>;

    fn trace(&self) -> bool;

    fn profile_dir(&self) -> Option<std::path::PathBuf>;

    fn document_cookie(&self, context: &CookieContext) -> String;

    fn set_document_cookie(&self, value: &str, context: &CookieContext);

    fn register_blob(&self, origin: &url::Origin, data: BlobData) -> String;

    fn revoke_blob(&self, url: &url::Url, origin: &url::Origin);

    /// See [`NetClient::hold_blob_url`].
    fn hold_blob_url(&self, url: &url::Url) -> Option<u64>;

    /// See [`NetClient::release_blob_url`].
    fn release_blob_url(&self, token: u64);

    /// Keeps `stream`, whose events so far the caller took (a navigation's response head: the
    /// browser decides on it), under an id another client of this backend adopts
    /// ([`NetBackend::adopt`]) to read the rest: how a response the browser fetched reaches the
    /// renderer that commits it (`docs/process-isolation.md`, M5; Chromium hands the
    /// `URLLoaderClientEndpoints` to the renderer in `NavigationRequest::CommitNavigation`).
    /// `None`: this backend hands nothing over (a renderer's).
    fn hand_over(&self, stream: ResponseStream) -> Option<u64> {
        drop(stream);
        None
    }

    /// The stream handed over as `id` ([`NetBackend::hand_over`]), with `notify` run after each
    /// of its further events; `None` when there is none (it was adopted already, or never handed
    /// over).
    fn adopt(
        &self,
        id: u64,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<ResponseStream> {
        let _ = (id, notify);
        None
    }

    /// This process's network thread, when that is what this is.
    fn local(&self) -> Option<&LocalNet> {
        None
    }
}

/// The network thread of this process.
pub struct LocalNet {
    handle: tokio::runtime::Handle,
    state: Arc<NetState>,
    /// Streams handed over and not adopted yet ([`NetBackend::hand_over`]).
    handed: std::sync::Mutex<std::collections::HashMap<u64, ResponseStream>>,
    next_handed: std::sync::atomic::AtomicU64,
}

impl LocalNet {
    fn new(handle: tokio::runtime::Handle, state: Arc<NetState>) -> LocalNet {
        LocalNet {
            handle,
            state,
            handed: Default::default(),
            next_handed: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

impl NetBackend for LocalNet {
    fn start(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        observer: Option<Observer>,
        filter: Option<Arc<dyn RequestFilter>>,
    ) -> ResponseStream {
        let (tx, rx) = mpsc::channel(STREAM_CAPACITY);
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let state = self.state.clone();
        let detached_timeout = request
            .keepalive
            .then_some(self.state.config.keepalive_detached_timeout);
        self.handle.spawn(async move {
            let sink = Sink {
                tx: Some(tx),
                notify,
                observer,
                filter,
                ended: Default::default(),
            };
            let fetch = state.fetch(request, sink);
            tokio::pin!(fetch);
            tokio::select! {
                _ = &mut fetch => return,
                _ = cancel_rx => {}
            }
            // The stream was dropped. A keepalive fetch runs on: with nobody to take its events,
            // sending the head fails and ends it (as `KeepAliveURLLoader::OnReceiveResponse`
            // deletes a loader whose renderer is gone), and the timer ends it otherwise.
            if let Some(timeout) = detached_timeout {
                let _ = tokio::time::timeout(timeout, fetch).await;
            }
        });
        ResponseStream {
            rx: Rx::Bounded(rx),
            _cancel: Some(cancel_tx),
            on_cancel: None,
            finished: false,
            observer: None,
        }
    }

    fn websocket(
        &self,
        params: crate::websocket::WsParams,
        filter: Option<Arc<dyn RequestFilter>>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> crate::websocket::WebSocketConnection {
        crate::websocket::start(&self.handle, self.state.clone(), filter, params, notify)
    }

    fn preload(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        filter: Option<Arc<dyn RequestFilter>>,
    ) -> Option<ResponseStream> {
        let (tx, rx) = mpsc::unbounded_channel();
        let watcher = crate::preload::Watcher { tx, notify };
        match self.state.preloads.register(&request, watcher)? {
            crate::preload::Registered::Existing => {}
            crate::preload::Registered::New(entry) => {
                let state = self.state.clone();
                self.handle.spawn(async move {
                    // At most a few preloads are on the network at once, so that the requests the
                    // page needs now (its own stylesheets and scripts, never held back) do not wait
                    // behind a page's long list of preloads for the connection's streams; a preload
                    // that a fetch consumed goes at once.
                    let slots = state.preloads.slots.clone();
                    let _slot = tokio::select! {
                        permit = slots.acquire_owned() => permit.ok(),
                        _ = entry.urgent.notified() => None,
                    };
                    let observer: Observer = Arc::new(move |e: &FetchEvent| entry.record(e));
                    let sink = Sink {
                        tx: None,
                        notify: None,
                        observer: Some(observer),
                        filter,
                        ended: Default::default(),
                    };
                    state.fetch_with(request, sink, false).await;
                });
            }
        }
        Some(ResponseStream {
            rx: Rx::Unbounded(rx),
            _cancel: None,
            on_cancel: None,
            finished: false,
            observer: None,
        })
    }

    fn preload_counts(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.state.preloads.started.load(Relaxed),
            self.state.preloads.consumed.load(Relaxed),
        )
    }

    fn is_preloaded(&self, request: &Request) -> bool {
        self.state.preloads.contains(request)
    }

    fn pool_report(&self) -> Vec<String> {
        self.state.transport.pool_report()
    }

    fn preconnect(&self, url: &url::Url) {
        let transport = self.state.transport.clone();
        let url = url.clone();
        self.handle.spawn(async move {
            transport.preconnect(&url).await;
        });
    }

    fn dns_prefetch(&self, host: &str, port: u16) {
        let target = crate::config::resolve_host(&self.state.config.host_rules, host).to_owned();
        self.handle.spawn(async move {
            let _ = tokio::net::lookup_host((target.as_str(), port)).await;
        });
    }

    fn user_agent(&self) -> Arc<crate::ua::UserAgent> {
        self.state.user_agent()
    }

    fn trace(&self) -> bool {
        self.state.config.trace
    }

    fn profile_dir(&self) -> Option<std::path::PathBuf> {
        self.state.config.profile_dir.clone()
    }

    fn document_cookie(&self, context: &CookieContext) -> String {
        self.state.cookies.cookie_string(context)
    }

    fn set_document_cookie(&self, value: &str, context: &CookieContext) {
        self.state.cookies.set_cookie(value, context);
    }

    fn register_blob(&self, origin: &url::Origin, data: BlobData) -> String {
        self.state.blobs.register(origin, data).to_string()
    }

    fn revoke_blob(&self, url: &url::Url, origin: &url::Origin) {
        self.state.blobs.revoke(url, origin);
    }

    fn hold_blob_url(&self, url: &url::Url) -> Option<u64> {
        self.state.blobs.hold(url)
    }

    fn release_blob_url(&self, token: u64) {
        self.state.blobs.release(token);
    }

    fn hand_over(&self, stream: ResponseStream) -> Option<u64> {
        let id = self
            .next_handed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.handed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, stream);
        Some(id)
    }

    /// The rest of the handed stream's events, pumped on the network's runtime into a stream of
    /// the adopter's with its `notify`: the stream handed over was made with the browser's wake,
    /// and the renderer's thread must be woken for what follows. Dropping the adopted stream
    /// drops the original, which cancels the fetch.
    fn adopt(
        &self,
        id: u64,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<ResponseStream> {
        let mut original = self
            .handed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)?;
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let (sender, stream) = ResponseStream::remote(move || {
            let _ = stop_tx.send(());
        });
        self.handle.spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return,
                    event = original.next_async() => {
                        let Some(event) = event else { return };
                        if !sender.send(event) {
                            return;
                        }
                        if let Some(notify) = &notify {
                            notify();
                        }
                    }
                }
            }
        });
        Some(stream)
    }

    fn local(&self) -> Option<&LocalNet> {
        Some(self)
    }
}

/// The network thread: a tokio runtime on a dedicated OS thread. Dropping it stops the thread after
/// saving cookies and HSTS hosts.
pub struct NetThread {
    client: NetClient,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl NetThread {
    /// Start the thread. Fails only when TLS can't be set up or the thread can't be created.
    pub fn start(config: NetConfig) -> Result<NetThread, NetError> {
        let config = Arc::new(config);
        let (cookies, hsts) = match &config.profile_dir {
            Some(dir) => (
                CookieStore::open(&dir.join("cookies.json")),
                HstsStore::open(&dir.join("hsts.json")),
            ),
            None => (CookieStore::in_memory(), HstsStore::in_memory()),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .thread_name("snowlark-net")
            .build()
            .map_err(|e| {
                NetError::new(
                    NetErrorKind::Io,
                    format!("can't start the network runtime: {e}"),
                )
            })?;
        let handle = runtime.handle().clone();
        let transport = {
            let _guard = runtime.enter();
            Transport::new(config.clone())?
        };
        let state = Arc::new(NetState {
            user_agent: std::sync::RwLock::new(Arc::new(crate::ua::UserAgent::chosen(
                &config.user_agent,
            ))),
            config: config.clone(),
            transport,
            cookies: Arc::new(cookies),
            hsts: Arc::new(hsts),
            blobs: Arc::new(BlobStore::default()),
            preflight: PreflightCache::default(),
            preloads: Default::default(),
            http_cache: Default::default(),
            detached: std::sync::OnceLock::new(),
            script_check: std::sync::OnceLock::new(),
            media_urls: Default::default(),
        });
        {
            let (weak, handle) = (Arc::downgrade(&state), handle.clone());
            let _ = state.detached.set(Box::new(move |request, sink| {
                if let Some(state) = weak.upgrade() {
                    handle.spawn(state.fetch(request, sink));
                }
            }));
        }
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let saver = state.clone();
        let thread = std::thread::Builder::new()
            .name("snowlark-net".into())
            // (async state machines are large in debug builds)
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                runtime.block_on(async move {
                    let mut ticker = tokio::time::interval(saver.config.save_interval);
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    let mut shutdown_rx = shutdown_rx;
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            _ = ticker.tick() => {
                                if saver.cookies.is_dirty() || saver.hsts.is_dirty() {
                                    let s = saver.clone();
                                    let _ = tokio::task::spawn_blocking(move || {
                                        let _ = s.cookies.flush();
                                        let _ = s.hsts.flush();
                                    })
                                    .await;
                                }
                            }
                        }
                    }
                    let _ = saver.cookies.flush();
                    let _ = saver.hsts.flush();
                });
                runtime.shutdown_background();
            })
            .map_err(|e| {
                NetError::new(
                    NetErrorKind::Io,
                    format!("can't start the network thread: {e}"),
                )
            })?;
        Ok(NetThread {
            client: NetClient {
                backend: Arc::new(LocalNet::new(handle, state)),
                observer: None,
                filter: None,
            },
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
    }

    pub fn client(&self) -> NetClient {
        self.client.clone()
    }

    /// Installs the script parser opaque-response blocking's last step needs
    /// ([`crate::orb::ScriptCheck`]). Without one, a cross-origin `no-cors` response that is
    /// neither labelled nor sniffed as a safe type is blocked rather than parsed. Once per thread;
    /// a second call is ignored.
    pub fn set_script_check(&self, check: Arc<dyn crate::orb::ScriptCheck>) {
        let _ = self.client.local().state.script_check.set(check);
    }
}

impl Drop for NetThread {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
