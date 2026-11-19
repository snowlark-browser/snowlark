// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The network across processes: the browser's [`NetBackend`] makes every fetch and WebSocket
//! connection, and a renderer's calls reach it through its pipe (`docs/process-isolation.md` section 7).
//!
//! A fetch or a connection is named by an id of the renderer's. The browser starts it with the tab's
//! request filter (the renderer has none: the filter, its rules and its log stay in the browser), and
//! a pump thread sends its events as they come: a body in pieces of at most
//! [`snowlark_ipc::INLINE_MAX`], always in the message, so that the pipe's backlog holds back a renderer
//! that stops reading, and the pump waiting on it holds back the fetch. A renderer that drops a
//! stream early cancels the fetch, and one that ends cancels them all; one that drops a connection
//! closes it. A keepalive fetch is not cancelled: it runs on to its response ([`NetBackend::start`]).
//!
//! Requests, blob origins and WebSocket parameters cross with each end's [`OriginTable`], so an
//! opaque origin stays one origin. What a request says about its client (origin, activation,
//! navigation) is still the renderer's word: M5 takes it from the frame token.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use snowlark_ipc::{wire, Bulk, Error, Replier, INLINE_MAX};
use snowlark_net::blob::BlobData;
use snowlark_net::cookies::CookieContext;
use snowlark_net::ua::UserAgent;
use snowlark_net::websocket::{WebSocketConnection, WsCommand, WsEvent, WsEventSender, WsParams};
use snowlark_net::wire::{with_origins, OriginTable};

use super::site_lock;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use snowlark_net::{
    BlockReason, FetchEvent, FetchSender, NetBackend, Observer, Origin, Request, RequestFilter,
    ResponseStream, Url,
};

use super::{answer, ask, unpack, BrowserPort, RendererPort, CALL_WAIT};

/// Renderer to browser: the calls of [`NetBackend`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// A [`Request`], packed with origins. Its events come as [`Down::Event`] with this id.
    Fetch {
        id: u64,
        request: Bulk,
    },
    /// A call with a [`Request`]: the answer is whether a stream follows, as for `Fetch`.
    Preload {
        id: u64,
        request: Bulk,
    },
    /// The renderer dropped the stream `id` before its last event.
    Cancel {
        id: u64,
    },
    /// M5: the renderer takes the stream the browser handed it as `id` ([`NetHost::hand_over`]):
    /// its further events come as [`Down::Event`] with that id.
    Adopt {
        id: u64,
    },
    /// Calls.
    PreloadCounts,
    IsPreloaded(Bulk),
    UserAgent,
    Trace,
    DocumentCookie(CookieContext),
    SetDocumentCookie {
        value: String,
        context: CookieContext,
    },
    /// An `(Origin, BlobData)`, packed with origins; the answer is the `blob:` URL.
    RegisterBlob(Bulk),
    /// A `(Url, Origin)`, packed with origins.
    RevokeBlob(Bulk),
    /// A hold on the blob URL entry the URL names ([`snowlark_net::NetClient::hold_blob_url`]); the
    /// answer is the token, or none.
    HoldBlobUrl(Url),
    /// The hold `token` is done with.
    ReleaseBlobUrl(u64),
    Preconnect(Url),
    DnsPrefetch {
        host: String,
        port: u16,
    },
    /// [`WsParams`], packed with origins. Its events come as [`Down::Socket`] with this id.
    WebSocket {
        id: u64,
        params: Bulk,
    },
    /// A [`WsCommand`], encoded, as bulk.
    SocketCommand {
        id: u64,
        command: Bulk,
    },
    /// The renderer dropped the connection `id`.
    SocketDropped {
        id: u64,
    },
}

/// One event of a fetch.
#[derive(Debug, Serialize, Deserialize)]
pub enum WireEvent {
    /// A piece of the body, at most [`INLINE_MAX`] bytes.
    Body(Vec<u8>),
    /// Any other [`FetchEvent`], encoded, as bulk.
    Other(Bulk),
}

/// The ids of streams the browser hands over start here, above any a renderer takes (its own go
/// up from 0; one that reaches this many is a lie the browser refuses before then).
pub const HANDED_FROM: u64 = 1 << 62;

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    Event {
        id: u64,
        event: WireEvent,
    },
    /// A [`WsEvent`], encoded, as bulk.
    Socket {
        id: u64,
        event: Bulk,
    },
}

/// `value`, encoded with `origins`, as bulk for the browser.
pub(crate) fn pack_with<T: Serialize>(
    port: &RendererPort,
    origins: &Mutex<OriginTable>,
    value: &T,
) -> Result<Bulk, Error> {
    let mut table = origins.lock().unwrap_or_else(|e| e.into_inner());
    let bytes = with_origins(&mut table, || wire::to_bytes(value))?;
    drop(table);
    port.bulk(bytes, CALL_WAIT)
}

/// What the renderer packed with origins. One that does not decode kills the renderer.
fn unpack_with<T: DeserializeOwned>(
    port: &BrowserPort,
    origins: &Mutex<OriginTable>,
    bulk: Bulk,
) -> Result<T, Error> {
    let bytes = port.open_bulk(bulk)?;
    let mut table = origins.lock().unwrap_or_else(|e| e.into_inner());
    with_origins(&mut table, || wire::decode(&bytes))
        .inspect_err(|e| port.bad_message(&e.to_string()))
}

/// A fetch of the renderer's, as it waits for events.
struct Fetch {
    sender: FetchSender,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    observer: Option<Observer>,
}

/// A WebSocket connection of the renderer's, as it waits for events.
struct Socket {
    sender: WsEventSender,
    notify: Arc<dyn Fn() + Send + Sync>,
}

/// What a renderer has open, by its ids.
#[derive(Default)]
struct Waiting {
    fetches: HashMap<u64, Fetch>,
    sockets: HashMap<u64, Socket>,
}

/// The renderer's [`NetBackend`].
pub struct RemoteNet {
    port: RendererPort,
    /// The next id, held while a fetch, connection or preload is opened: the browser takes ids only
    /// in increasing order, and the page's thread and media players' open them at once.
    next: Mutex<u64>,
    waiting: Arc<Mutex<Waiting>>,
    origins: Mutex<OriginTable>,
    trace: OnceLock<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl RemoteNet {
    pub fn new(port: RendererPort) -> RemoteNet {
        RemoteNet {
            port,
            next: Mutex::new(0),
            waiting: Arc::default(),
            origins: Mutex::new(OriginTable::renderer()),
            trace: OnceLock::new(),
        }
    }

    fn send(&self, message: Up) {
        let _ = self.port.send(&super::Up::Net(message));
    }

    fn ask<A: DeserializeOwned>(&self, message: Up) -> Option<A> {
        ask(&self.port, &super::Up::Net(message))
    }

    /// Opens something under the next id: `open` sends its first message, so that no message of a
    /// later id reaches the pipe before it.
    fn open<R>(&self, open: impl FnOnce(u64) -> R) -> R {
        let mut next = lock(&self.next);
        let id = *next;
        *next += 1;
        open(id)
    }

    /// [`NetBackend::websocket`]'s connection under `id`.
    fn open_websocket(
        &self,
        id: u64,
        params: Bulk,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> WebSocketConnection {
        let commands = self.port.clone();
        let (port, waiting) = (self.port.clone(), self.waiting.clone());
        let (sender, connection) = WebSocketConnection::remote(
            move |command: WsCommand| {
                let packed = wire::to_bytes(&command).and_then(|b| commands.bulk(b, CALL_WAIT));
                if let Ok(command) = packed {
                    let _ = commands.send(&super::Up::Net(Up::SocketCommand { id, command }));
                }
            },
            move || {
                lock(&waiting).sockets.remove(&id);
                let _ = port.send(&super::Up::Net(Up::SocketDropped { id }));
            },
        );
        lock(&self.waiting)
            .sockets
            .insert(id, Socket { sender, notify });
        self.send(Up::WebSocket { id, params });
        connection
    }

    /// A stream for the fetch `id`, which the browser feeds.
    fn stream(
        &self,
        id: u64,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        observer: Option<Observer>,
    ) -> ResponseStream {
        let (port, waiting) = (self.port.clone(), self.waiting.clone());
        let (sender, stream) = ResponseStream::remote(move || {
            lock(&waiting).fetches.remove(&id);
            let _ = port.send(&super::Up::Net(Up::Cancel { id }));
        });
        let fetch = Fetch {
            sender,
            notify,
            observer,
        };
        lock(&self.waiting).fetches.insert(id, fetch);
        stream
    }

    pub(super) fn dispatch(&self, message: Down) {
        match message {
            Down::Event { id, event } => self.fetch_event(id, event),
            Down::Socket { id, event } => self.socket_event(id, event),
        }
    }

    fn fetch_event(&self, id: u64, event: WireEvent) {
        let event = match event {
            WireEvent::Body(bytes) => FetchEvent::Body(bytes.into()),
            WireEvent::Other(bulk) => {
                let event = self
                    .port
                    .open_bulk(bulk)
                    .and_then(|bytes| wire::decode::<FetchEvent>(&bytes));
                let Ok(event) = event else {
                    return;
                };
                event
            }
        };
        let last = matches!(event, FetchEvent::Done | FetchEvent::Error(_));
        let mut waiting = lock(&self.waiting);
        let Some(fetch) = waiting.fetches.get(&id) else {
            return;
        };
        if let Some(observer) = &fetch.observer {
            observer(&event);
        }
        fetch.sender.send(event);
        let notify = fetch.notify.clone();
        if last {
            waiting.fetches.remove(&id);
        }
        drop(waiting);
        if let Some(notify) = notify {
            notify();
        }
    }

    fn socket_event(&self, id: u64, event: Bulk) {
        let event = self
            .port
            .open_bulk(event)
            .and_then(|bytes| wire::decode::<WsEvent>(&bytes));
        let Ok(event) = event else {
            return;
        };
        let last = matches!(event, WsEvent::Closed { .. });
        let mut waiting = lock(&self.waiting);
        let Some(socket) = waiting.sockets.get(&id) else {
            return;
        };
        socket.sender.send(event);
        let notify = socket.notify.clone();
        if last {
            waiting.sockets.remove(&id);
        }
        drop(waiting);
        notify();
    }

    /// How many fetches and connections wait for events.
    pub fn waiting(&self) -> (usize, usize) {
        let w = lock(&self.waiting);
        (w.fetches.len(), w.sockets.len())
    }
}

/// A stream that has failed already: the browser is gone, or the request does not encode.
fn failed(why: &str) -> ResponseStream {
    let (sender, stream) = ResponseStream::remote(|| {});
    sender.send(FetchEvent::Error(snowlark_net::NetError::new(
        snowlark_net::NetErrorKind::Aborted,
        why,
    )));
    stream
}

impl NetBackend for RemoteNet {
    fn start(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        observer: Option<Observer>,
        // (the browser applies the tab's own)
        _filter: Option<Arc<dyn RequestFilter>>,
    ) -> ResponseStream {
        let Ok(request) = pack_with(&self.port, &self.origins, &request) else {
            return failed("the request does not cross");
        };
        self.open(|id| {
            let stream = self.stream(id, notify, observer);
            self.send(Up::Fetch { id, request });
            stream
        })
    }

    fn websocket(
        &self,
        params: WsParams,
        _filter: Option<Arc<dyn RequestFilter>>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> WebSocketConnection {
        let Ok(params) = pack_with(&self.port, &self.origins, &params) else {
            return WebSocketConnection::failed();
        };
        self.open(|id| self.open_websocket(id, params, notify))
    }

    fn preload(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        _filter: Option<Arc<dyn RequestFilter>>,
    ) -> Option<ResponseStream> {
        let request = pack_with(&self.port, &self.origins, &request).ok()?;
        self.open(|id| {
            // (made before asking, so no event can come before it is there)
            let stream = self.stream(id, notify, None);
            if self.ask::<bool>(Up::Preload { id, request }) == Some(true) {
                return Some(stream);
            }
            // (no stream follows; dropping it cancels an id the browser never kept, which it
            // ignores)
            drop(stream);
            None
        })
    }

    fn preload_counts(&self) -> (usize, usize) {
        self.ask::<(u64, u64)>(Up::PreloadCounts)
            .map_or((0, 0), |(a, b)| (a as usize, b as usize))
    }

    fn is_preloaded(&self, request: &Request) -> bool {
        pack_with(&self.port, &self.origins, request)
            .ok()
            .and_then(|r| self.ask(Up::IsPreloaded(r)))
            .unwrap_or(false)
    }

    fn preconnect(&self, url: &Url) {
        self.send(Up::Preconnect(url.clone()));
    }

    fn dns_prefetch(&self, host: &str, port: u16) {
        self.send(Up::DnsPrefetch {
            host: host.to_owned(),
            port,
        });
    }

    fn user_agent(&self) -> Arc<UserAgent> {
        Arc::new(self.ask(Up::UserAgent).unwrap_or_default())
    }

    fn trace(&self) -> bool {
        *self
            .trace
            .get_or_init(|| self.ask(Up::Trace).unwrap_or(false))
    }

    fn profile_dir(&self) -> Option<std::path::PathBuf> {
        // (a renderer touches no files)
        None
    }

    fn document_cookie(&self, context: &CookieContext) -> String {
        self.ask(Up::DocumentCookie(context.clone()))
            .unwrap_or_default()
    }

    fn set_document_cookie(&self, value: &str, context: &CookieContext) {
        self.send(Up::SetDocumentCookie {
            value: value.to_owned(),
            context: context.clone(),
        });
    }

    fn register_blob(&self, origin: &Origin, data: BlobData) -> String {
        let pair = (OriginRef(origin), data);
        pack_with(&self.port, &self.origins, &pair)
            .ok()
            .and_then(|b| self.ask(Up::RegisterBlob(b)))
            .unwrap_or_default()
    }

    fn revoke_blob(&self, url: &Url, origin: &Origin) {
        if let Ok(b) = pack_with(&self.port, &self.origins, &(url, OriginRef(origin))) {
            self.send(Up::RevokeBlob(b));
        }
    }

    fn hold_blob_url(&self, url: &Url) -> Option<u64> {
        self.ask::<Option<u64>>(Up::HoldBlobUrl(url.clone()))
            .flatten()
    }

    fn release_blob_url(&self, token: u64) {
        self.send(Up::ReleaseBlobUrl(token));
    }

    /// The stream the browser handed over as `id`: its events come down from the browser once
    /// it hears the renderer took it. (An id the browser never handed over is one it kills the
    /// renderer for; a page never names one.)
    fn adopt(
        &self,
        id: u64,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<ResponseStream> {
        if id < HANDED_FROM {
            return None;
        }
        let stream = self.stream(id, notify, None);
        self.send(Up::Adopt { id });
        Some(stream)
    }
}

/// An origin, encoded through the origin table.
struct OriginRef<'a>(&'a Origin);

impl Serialize for OriginRef<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        snowlark_net::wire::origin::serialize(self.0, s)
    }
}

/// An origin, decoded through the origin table.
struct OriginOwned(Origin);

impl<'de> Deserialize<'de> for OriginOwned {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        snowlark_net::wire::origin::deserialize(d).map(OriginOwned)
    }
}

/// What the browser has open for a renderer, by the renderer's ids.
#[derive(Default)]
struct Open {
    fetches: HashMap<u64, ResponseStream>,
    /// Fetches that navigate a child navigable: the site of the document that comes back is
    /// hosted by the renderer (`SiteLock::host`).
    navigations: HashSet<u64>,
    /// Streams handed over to the renderer and not adopted yet ([`NetHost::hand_over`]).
    handed: HashMap<u64, ResponseStream>,
    sockets: HashMap<u64, WebSocketConnection>,
}

/// What the pump looks at.
#[derive(Clone, Copy)]
enum Ready {
    Fetch(u64),
    Socket(u64),
}

/// The browser's end of one renderer's network calls.
pub struct NetHost {
    port: BrowserPort,
    backend: Arc<dyn NetBackend>,
    filter: Option<Arc<dyn RequestFilter>>,
    site_lock: site_lock::Mode,
    origins: Mutex<OriginTable>,
    open: Arc<Mutex<Open>>,
    /// To the pump: a fetch or a connection has events.
    ready: Mutex<mpsc::Sender<Ready>>,
    /// The next id a fetch or a connection may have: a renderer's ids only go up, so one used twice
    /// is a lie however soon the first finished.
    next: AtomicU64,
    /// The next id for a stream the browser hands over ([`HANDED_FROM`] and up).
    next_handed: AtomicU64,
}

impl NetHost {
    pub fn new(
        port: BrowserPort,
        backend: Arc<dyn NetBackend>,
        filter: Option<Arc<dyn RequestFilter>>,
        site_lock: site_lock::Mode,
    ) -> NetHost {
        let (ready, woken) = mpsc::channel();
        let open: Arc<Mutex<Open>> = Arc::default();
        {
            let (port, open) = (port.clone(), open.clone());
            let _ = std::thread::Builder::new()
                .name("snowlark-net-pump".into())
                .spawn(move || pump(&port, &open, &woken));
        }
        NetHost {
            port,
            backend,
            filter,
            site_lock,
            origins: Mutex::new(OriginTable::browser()),
            open,
            ready: Mutex::new(ready),
            next: AtomicU64::new(0),
            next_handed: AtomicU64::new(HANDED_FROM),
        }
    }

    /// Takes `id` for a new fetch or connection.
    fn take_id(&self, id: u64) -> Result<(), Error> {
        let next = self.next.load(Ordering::Relaxed);
        if id < next {
            return Err(self.bad(format!("id {id} used again (next is {next})")));
        }
        if id >= HANDED_FROM {
            return Err(self.bad(format!("id {id} is in the browser's range")));
        }
        self.next.store(id + 1, Ordering::Relaxed);
        Ok(())
    }

    /// M5: keeps `stream` (a navigation's response, whose head the browser took) for the renderer
    /// to adopt under the id returned ([`Up::Adopt`]; the renderer's `NetClient::adopt`). Its
    /// events are sent down once it is adopted.
    ///
    /// The stream was made with the window's wake-up (the tab's host hears of the head through
    /// it), which tells this host's pump nothing: the rest of the body would reach the renderer
    /// only as far as it had arrived when the pump last looked. So the stream is read on the
    /// network's runtime into one with the pump's wake-up (the backend's own hand-over and
    /// adoption), and that one is kept for the renderer.
    pub fn hand_over(&self, stream: ResponseStream) -> u64 {
        let id = self.next_handed.fetch_add(1, Ordering::Relaxed);
        let notify = self.notify(Ready::Fetch(id));
        let pumped = self
            .backend
            .hand_over(stream)
            .and_then(|inner| self.backend.adopt(inner, Some(notify)));
        let stream = match pumped {
            Some(s) => s,
            // (a backend with no hand-over of its own: the pump sees only what is in by then)
            None => {
                let (sender, stream) = ResponseStream::remote(|| {});
                sender.send(FetchEvent::Error(snowlark_net::NetError::new(
                    snowlark_net::NetErrorKind::Aborted,
                    "the browser's network cannot hand a response over",
                )));
                stream
            }
        };
        lock(&self.open).handed.insert(id, stream);
        id
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    fn notify(&self, what: Ready) -> Arc<dyn Fn() + Send + Sync> {
        let ready = Mutex::new(lock(&self.ready).clone());
        Arc::new(move || {
            let _ = lock(&ready).send(what);
        })
    }

    /// Has the pump look at `what` (events may have come before it was kept).
    fn look(&self, what: Ready) {
        let _ = lock(&self.ready).send(what);
    }

    /// Keeps `stream` as the fetch `id` and has the pump look at it.
    fn keep(&self, id: u64, stream: ResponseStream) {
        lock(&self.open).fetches.insert(id, stream);
        self.look(Ready::Fetch(id));
    }

    /// A request the renderer sent, checked against its site lock.
    fn request(&self, bulk: Bulk) -> Result<Request, Error> {
        let request: Request = unpack_with(&self.port, &self.origins, bulk)?;
        self.check(site_lock::request_violation(self.port.policy(), &request))?;
        Ok(request)
    }

    /// Why a request is refused, if it is: its URL's scheme, in this host's mode
    /// ([`site_lock::refuse`]). The renderer gets a network error, as a web page's `file:`
    /// request gets from the network itself.
    fn refused(&self, request: &Request) -> Option<String> {
        let why = site_lock::request_url_refused(self.port.policy(), request);
        if site_lock::refuse(&self.port, self.site_lock, why.clone()) {
            why
        } else {
            None
        }
    }

    /// Acts on a site-lock check ([`site_lock::check`] in this host's mode).
    fn check(&self, violation: Option<String>) -> Result<(), Error> {
        site_lock::check(&self.port, self.site_lock, violation)
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        let port = &self.port;
        match message {
            Up::Fetch { id, request } => {
                self.take_id(id)?;
                let request = self.request(request)?;
                if request.mode == snowlark_net::Mode::Navigate {
                    lock(&self.open).navigations.insert(id);
                }
                let notify = self.notify(Ready::Fetch(id));
                let stream = match self.refused(&request) {
                    Some(why) => {
                        let (sender, stream) = ResponseStream::remote(|| {});
                        sender.send(FetchEvent::Error(snowlark_net::NetError::blocked(
                            BlockReason::FileAccess,
                            why,
                        )));
                        stream
                    }
                    None => self
                        .backend
                        .start(request, Some(notify), None, self.filter.clone()),
                };
                self.keep(id, stream);
            }
            Up::Preload { id, request } => {
                self.take_id(id)?;
                let request = self.request(request)?;
                let notify = self.notify(Ready::Fetch(id));
                let stream = match self.refused(&request) {
                    Some(_) => None,
                    None => self
                        .backend
                        .preload(request, Some(notify), self.filter.clone()),
                };
                let follows = stream.is_some();
                if let Some(stream) = stream {
                    self.keep(id, stream);
                }
                answer(port, replier, &follows)?;
            }
            Up::Cancel { id } => {
                // (one that finished meanwhile is gone already)
                lock(&self.open).fetches.remove(&id);
            }
            Up::Adopt { id } => {
                let stream = lock(&self.open).handed.remove(&id);
                let Some(stream) = stream else {
                    return Err(self.bad(format!("adopted stream {id}, which it was not handed")));
                };
                self.keep(id, stream);
            }
            Up::PreloadCounts => {
                let (a, b) = self.backend.preload_counts();
                answer(port, replier, &(a as u64, b as u64))?;
            }
            Up::IsPreloaded(request) => {
                let request = self.request(request)?;
                answer(port, replier, &self.backend.is_preloaded(&request))?;
            }
            Up::UserAgent => answer(port, replier, &*self.backend.user_agent())?,
            Up::Trace => answer(port, replier, &self.backend.trace())?,
            Up::DocumentCookie(context) => {
                self.check(cookie_violation(port.policy(), &context))?;
                answer(port, replier, &self.backend.document_cookie(&context))?
            }
            Up::SetDocumentCookie { value, context } => {
                self.check(cookie_violation(port.policy(), &context))?;
                self.backend.set_document_cookie(&value, &context)
            }
            Up::RegisterBlob(b) => {
                let (OriginOwned(origin), data): (OriginOwned, BlobData) =
                    unpack_with(port, &self.origins, b)?;
                self.check(site_lock::origin_violation(
                    port.policy(),
                    "the blob's origin",
                    &origin,
                ))?;
                answer(port, replier, &self.backend.register_blob(&origin, data))?;
            }
            Up::RevokeBlob(b) => {
                let (url, OriginOwned(origin)): (Url, OriginOwned) =
                    unpack_with(port, &self.origins, b)?;
                self.check(site_lock::origin_violation(
                    port.policy(),
                    "the blob's origin",
                    &origin,
                ))?;
                self.backend.revoke_blob(&url, &origin);
            }
            Up::HoldBlobUrl(url) => {
                // (a blob URL's origin is its creator's: the one in the URL)
                self.check(site_lock::origin_violation(
                    port.policy(),
                    "the blob's origin",
                    &snowlark_net::origin::origin_of(&url, None),
                ))?;
                answer(port, replier, &self.backend.hold_blob_url(&url))?;
            }
            Up::ReleaseBlobUrl(token) => self.backend.release_blob_url(token),
            Up::Preconnect(url) => self.backend.preconnect(&url),
            Up::DnsPrefetch { host, port } => self.backend.dns_prefetch(&host, port),
            Up::WebSocket { id, params } => {
                self.take_id(id)?;
                let params: WsParams = unpack_with(port, &self.origins, params)?;
                self.check(site_lock::origin_violation(
                    port.policy(),
                    "the socket's client origin",
                    &params.client.origin,
                ))?;
                let notify = self.notify(Ready::Socket(id));
                let socket = self.backend.websocket(params, self.filter.clone(), notify);
                lock(&self.open).sockets.insert(id, socket);
                self.look(Ready::Socket(id));
            }
            Up::SocketCommand { id, command } => {
                let command: WsCommand = unpack(port, command)?;
                // (one closed meanwhile is gone already)
                if let Some(socket) = lock(&self.open).sockets.get(&id) {
                    socket.send(command);
                }
            }
            Up::SocketDropped { id } => {
                lock(&self.open).sockets.remove(&id);
            }
        }
        Ok(())
    }

    /// The renderer is gone: cancels its fetches and closes its connections, as Chromium's network
    /// service does when a renderer's `URLLoader` pipes break. Its keepalive fetches go on in the
    /// network until their response, at most 30 s ([`NetBackend::start`]: Chromium's
    /// `KeepAliveURLLoader::OnURLLoaderDisconnected`), with nobody to send their events to.
    pub(super) fn gone(&self) {
        let mut open = lock(&self.open);
        open.fetches.clear();
        open.handed.clear();
        open.sockets.clear();
    }

    /// How many of the renderer's fetches and connections are open.
    pub fn open(&self) -> (usize, usize) {
        let open = lock(&self.open);
        (open.fetches.len(), open.sockets.len())
    }
}

impl Drop for NetHost {
    /// Cancels what the renderer left open. The pump holds the same table, and each fetch's notify
    /// holds the pump's channel: emptying it is what lets both end.
    fn drop(&mut self) {
        self.gone();
    }
}

/// Sends `message`, waiting while the pipe's backlog is full. False once the renderer is gone.
pub(super) fn send_waiting(port: &BrowserPort, message: &super::Down) -> bool {
    loop {
        match port.send(message) {
            Ok(()) => return true,
            Err(Error::Backlog) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => return false,
        }
    }
}

/// The pump: for each fetch or connection that has events, takes them and sends them in order.
/// Ends with the host and the last fetch it started, or with the renderer.
fn pump(port: &BrowserPort, open: &Mutex<Open>, woken: &mpsc::Receiver<Ready>) {
    while let Ok(what) = woken.recv() {
        let going = match what {
            Ready::Fetch(id) => pump_fetch(port, open, id),
            Ready::Socket(id) => pump_socket(port, open, id),
        };
        if !going {
            return;
        }
    }
}

fn pump_fetch(port: &BrowserPort, open: &Mutex<Open>, id: u64) -> bool {
    let events: Vec<FetchEvent> = {
        let mut open = lock(open);
        let Some(stream) = open.fetches.get_mut(&id) else {
            return true;
        };
        let events: Vec<FetchEvent> = std::iter::from_fn(|| stream.try_next()).collect();
        let finished = stream.is_finished();
        // A child navigable's document from the browser's own network: its site is hosted here.
        if open.navigations.contains(&id) {
            if let Some(head) = events.iter().find_map(|e| match e {
                FetchEvent::Head(head) => Some(head),
                _ => None,
            }) {
                open.navigations.remove(&id);
                if let Some(site) = head.url().and_then(snowlark_net::origin::site_of_url) {
                    port.policy().lock.host(&site);
                }
            }
        }
        if finished {
            open.fetches.remove(&id);
            open.navigations.remove(&id);
        }
        events
    };
    for event in events {
        let wire_events: Vec<WireEvent> = match event {
            FetchEvent::Body(bytes) => bytes
                .chunks(INLINE_MAX)
                .map(|piece| WireEvent::Body(piece.to_vec()))
                .collect(),
            other => match wire::to_bytes(&other).and_then(|b| port.bulk(&b)) {
                Ok(bulk) => vec![WireEvent::Other(bulk)],
                Err(_) => continue,
            },
        };
        for event in wire_events {
            if !send_waiting(port, &super::Down::Net(Down::Event { id, event })) {
                return false;
            }
        }
    }
    true
}

/// What is wrong with a `document.cookie` context a renderer sent, if anything: its URL must be
/// of a site the renderer has, and it is never the HTTP API's.
fn cookie_violation(
    policy: &snowlark_ipc::ProcessPolicy,
    context: &CookieContext,
) -> Option<String> {
    if context.http_api {
        return Some("a cookie context of the HTTP API".into());
    }
    site_lock::url_violation(policy, "the document's URL", &context.url).or_else(|| {
        site_lock::origin_violation(
            policy,
            "the document's origin",
            &snowlark_net::origin::origin_of(&context.url, None),
        )
    })
}

fn pump_socket(port: &BrowserPort, open: &Mutex<Open>, id: u64) -> bool {
    let events: Vec<WsEvent> = {
        let mut open = lock(open);
        let Some(socket) = open.sockets.get_mut(&id) else {
            return true;
        };
        let events: Vec<WsEvent> = std::iter::from_fn(|| socket.try_next()).collect();
        if events.iter().any(|e| matches!(e, WsEvent::Closed { .. })) {
            open.sockets.remove(&id);
        }
        events
    };
    for event in events {
        let Ok(event) = wire::to_bytes(&event).and_then(|b| port.bulk(&b)) else {
            continue;
        };
        if !send_waiting(port, &super::Down::Net(Down::Socket { id, event })) {
            return false;
        }
    }
    true
}
