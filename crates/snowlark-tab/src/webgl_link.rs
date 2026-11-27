// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL across processes (`docs/process-isolation.md` §6, "WebGL"): a renderer's contexts run in the
//! GPU process, and the renderer holds no graphics device.
//!
//! The page crate validates and encodes WebGL's calls into `snowlark_gl`'s protocol, as in one
//! process; here that protocol crosses a link between the renderer and the GPU process
//! (`snowlark_ipc::link`: a pipe and a ring of shared memory each way, made by the browser). Commands
//! that answer nothing go without waiting; a query, a new context and a present wait for their answer,
//! which is the only thing the GPU process sends. Batches, uploads and readbacks travel in the rings.
//!
//! [`WebGlChannel`] is the renderer's end, shared by all its pages and workers; each of them has a
//! [`RemoteGl`], a `snowlark_gl::Backend` with contexts of its own. [`serve`] is the GPU process's end:
//! one thread per link, driving the process's WebGL thread (`snowlark_gl::Service`). A link that ends
//! (the GPU process gone) loses every context on it: each then reads as reset, and the next context
//! asks for a new link.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
pub use snowlark_gl as gl;
pub use snowlark_gl::in_gpu_process;
use snowlark_gl::{Attributes, Backend, Command, ContextId, Created, Query, Readback, Reply};
use snowlark_ipc::{
    wire, BadMessage, Bulk, ClientEnd, Error, HostEnd, LinkEnd, ProcessKind, ProcessPolicy,
    Received, Validate,
};

/// The link's ring from the renderer to the GPU process (batches and uploads), and back (readbacks).
/// A message larger than its ring is streamed through it.
pub const RING_UP: u64 = 8 << 20;
pub const RING_DOWN: u64 = 8 << 20;
/// The most bytes one request may carry (an upload of a whole 16384 × 16384 RGBA texture).
pub const MAX_REQUEST: u64 = 1 << 30;
/// The widest and tallest drawing buffer the GPU process makes.
pub const MAX_SIDE: u32 = 16384;
/// The most contexts one link may have at once.
pub const MAX_CONTEXTS: usize = 1024;
/// How long either end waits for the other's hello.
pub const CONNECT_WAIT: Duration = Duration::from_secs(10);
/// How long the renderer waits for an answer (a shader compile or a `finish` may be slow) before it
/// counts the GPU process as gone.
const ANSWER_WAIT: Duration = Duration::from_secs(30);
/// How long a message's bytes may take to come through a ring.
const BYTES_WAIT: Duration = Duration::from_secs(10);
/// `GL_UNKNOWN_CONTEXT_RESET`: what a context on a link that ended reads as.
const UNKNOWN_CONTEXT_RESET: i32 = 0x8255;

/// Renderer to GPU process: an encoded [`Request`], in the message or in the ring.
#[derive(Debug, Serialize, Deserialize)]
pub enum GlUp {
    /// A request that answers nothing.
    Send(Bulk),
    /// A request whose [`GlDown::Answer`] the renderer waits for.
    Ask(Bulk),
}

/// GPU process to renderer: the encoded [`Answer`] to the oldest [`GlUp::Ask`] not answered yet.
#[derive(Debug, Serialize, Deserialize)]
pub enum GlDown {
    Answer(Bulk),
}

/// What the renderer asks of its contexts (`snowlark_gl::Backend`'s calls), by the link's context
/// ids.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Create {
        id: ContextId,
        attributes: Attributes,
        width: u32,
        height: u32,
    },
    Resize {
        id: ContextId,
        width: u32,
        height: u32,
    },
    Destroy(ContextId),
    Submit(Vec<(ContextId, Command)>),
    Query(ContextId, Query),
    Present(ContextId),
}

impl Request {
    /// Whether the renderer waits for an answer to it.
    fn asks(&self) -> bool {
        matches!(
            self,
            Request::Create { .. } | Request::Query(..) | Request::Present(_)
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Answer {
    Created(Result<Created, String>),
    Reply(Reply),
    Readback(Option<Readback>),
}

impl Validate for GlUp {
    fn validate(self, _policy: &ProcessPolicy) -> Result<GlUp, BadMessage> {
        let (GlUp::Send(b) | GlUp::Ask(b)) = &self;
        match b {
            Bulk::Inline(_) | Bulk::Ring { .. } if b.len() <= MAX_REQUEST => Ok(self),
            Bulk::Inline(_) | Bulk::Ring { .. } => {
                Err(BadMessage("a WebGL request over its cap".into()))
            }
            Bulk::Shared { .. } | Bulk::Given { .. } => {
                Err(BadMessage("a section on a WebGL link".into()))
            }
        }
    }
}

/// The renderer's end of its link.
pub type GlClient = ClientEnd<GlUp, GlDown>;
/// The GPU process's end of a renderer's link.
pub type GlServer = HostEnd<GlDown, GlUp>;

/// Where a renderer gets its link to the GPU process: the browser makes it ([`crate::services`]'s
/// `webgl`); `None` when there is no GPU process to link to.
pub trait EstablishLink: Send + Sync {
    fn establish(&self) -> Option<LinkEnd>;
}

/// The renderer's end of its link to the GPU process, joined on first use and again after the link
/// ended; shared by every page and worker of the process.
pub struct WebGlChannel {
    establish: Arc<dyn EstablishLink>,
    link: Mutex<Option<Arc<Linked>>>,
    /// Context ids on the link, process-wide, so the pages' contexts never meet.
    next: AtomicU32,
}

/// One link, once joined: a request and its answer are sent and read under `turn`, so answers come
/// to whoever asked.
struct Linked {
    link: GlClient,
    turn: Mutex<()>,
}

impl WebGlChannel {
    pub fn new(establish: Arc<dyn EstablishLink>) -> Arc<WebGlChannel> {
        Arc::new(WebGlChannel {
            establish,
            link: Mutex::new(None),
            next: AtomicU32::new(1),
        })
    }

    /// The link, joined now if there is none or the last one ended.
    fn linked(&self) -> Result<Arc<Linked>, String> {
        let mut held = self.link.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = held.as_ref().filter(|l| !l.link.is_closed()) {
            return Ok(l.clone());
        }
        *held = None;
        let end = self
            .establish
            .establish()
            .ok_or("no GPU process to run WebGL in")?;
        let link =
            snowlark_ipc::connect_link::<GlUp, GlDown, snowlark_ipc::FromHost>(end, CONNECT_WAIT)
                .map_err(|e| format!("the link to the GPU process: {e}"))?;
        let l = Arc::new(Linked {
            link,
            turn: Mutex::new(()),
        });
        *held = Some(l.clone());
        Ok(l)
    }

    /// Joins the link now (a renderer asks at its start, so its first context does not wait).
    pub fn prepare(&self) -> bool {
        self.linked().is_ok()
    }

    /// A backend for one page or worker.
    pub fn backend(self: &Arc<Self>) -> Result<RemoteGl, String> {
        let link = self.linked()?;
        Ok(RemoteGl {
            channel: self.clone(),
            link,
            ids: HashMap::new(),
        })
    }
}

impl Linked {
    fn turn(&self) -> MutexGuard<'_, ()> {
        self.turn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn put(&self, kind: fn(Bulk) -> GlUp, request: &Request) -> Result<(), Error> {
        let bytes = wire::to_bytes(request)?;
        self.link.send_bulk(&bytes, kind, BYTES_WAIT)
    }

    /// Sends a request that answers nothing (in order with the others).
    fn send(&self, request: &Request) {
        let _turn = self.turn();
        // (a link that ended answers nothing from now on: the contexts read as reset)
        let _ = self.put(GlUp::Send, request);
    }

    /// Sends a request and waits for its answer: `None` when the link ended.
    fn ask(&self, request: &Request) -> Option<Answer> {
        let _turn = self.turn();
        self.put(GlUp::Ask, request).ok()?;
        let bulk = match self.link.recv_timeout(ANSWER_WAIT) {
            Ok(Received::Message(GlDown::Answer(bulk))) => bulk,
            Ok(Received::Call(..)) | Err(_) => {
                // (an answer that never came: the link is done, whatever is still in it)
                self.link.close();
                return None;
            }
        };
        let answer = self
            .link
            .open_bulk(bulk, BYTES_WAIT)
            .and_then(|bytes| wire::decode(&bytes));
        match answer {
            Ok(a) => Some(a),
            Err(_) => {
                self.link.close();
                None
            }
        }
    }
}

/// A renderer's pages' WebGL in the GPU process: each page and worker gets a [RemoteGl] of the
/// process's one [WebGlChannel].
pub struct GpuWebGl(Arc<WebGlChannel>);

impl GpuWebGl {
    pub fn new(establish: Arc<dyn EstablishLink>) -> GpuWebGl {
        GpuWebGl(WebGlChannel::new(establish))
    }
}

impl snowlark_page::WebGlProvider for GpuWebGl {
    fn backend(&self) -> Result<Box<dyn Backend>, String> {
        Ok(Box::new(self.0.backend()?))
    }

    fn prepare(&self) {
        if !self.0.prepare() {
            eprintln!("[renderer] no link to the GPU process for WebGL yet");
        }
    }
}

/// A page's (or worker's) contexts in the GPU process.
pub struct RemoteGl {
    channel: Arc<WebGlChannel>,
    link: Arc<Linked>,
    /// The page's context ids to the link's.
    ids: HashMap<ContextId, ContextId>,
}

const GONE: &str = "the GPU process is gone";

impl Backend for RemoteGl {
    fn create_context(
        &mut self,
        id: ContextId,
        attributes: &Attributes,
        width: u32,
        height: u32,
    ) -> Result<Created, String> {
        self.destroy(id);
        if self.link.link.is_closed() {
            // (a new link for the new context; the old ones stay lost)
            self.link = self.channel.linked()?;
            self.ids.clear();
        }
        let global = self.channel.next.fetch_add(1, Ordering::Relaxed);
        let request = Request::Create {
            id: global,
            attributes: *attributes,
            width,
            height,
        };
        match self.link.ask(&request) {
            Some(Answer::Created(made)) => {
                if made.is_ok() {
                    self.ids.insert(id, global);
                }
                made
            }
            Some(_) => Err("the GPU process answered something else".into()),
            None => Err(GONE.into()),
        }
    }

    fn resize(&mut self, id: ContextId, width: u32, height: u32) {
        if let Some(&g) = self.ids.get(&id) {
            self.link.send(&Request::Resize {
                id: g,
                width,
                height,
            });
        }
    }

    fn destroy(&mut self, id: ContextId) {
        if let Some(g) = self.ids.remove(&id) {
            self.link.send(&Request::Destroy(g));
        }
    }

    fn submit(&mut self, mut batch: Vec<(ContextId, Command)>) {
        // (commands for a context the page does not have do nothing, as on the device)
        batch.retain_mut(|(id, _)| match self.ids.get(id) {
            Some(&g) => {
                *id = g;
                true
            }
            None => false,
        });
        if !batch.is_empty() {
            self.link.send(&Request::Submit(batch));
        }
    }

    fn query(&mut self, id: ContextId, query: &Query) -> Reply {
        let Some(&g) = self.ids.get(&id) else {
            return Reply::None;
        };
        match self.link.ask(&Request::Query(g, query.clone())) {
            Some(Answer::Reply(r)) => r,
            // A context whose GPU process is gone was reset (WebGL 1.0 §5.15.2: it is lost).
            None if *query == Query::ResetStatus => Reply::Int(UNKNOWN_CONTEXT_RESET),
            _ => Reply::None,
        }
    }

    fn present(&mut self, id: ContextId) -> Option<Readback> {
        let &g = self.ids.get(&id)?;
        match self.link.ask(&Request::Present(g))? {
            Answer::Readback(rb) => rb,
            _ => None,
        }
    }
}

impl Drop for RemoteGl {
    fn drop(&mut self) {
        for (_, g) in self.ids.drain() {
            self.link.send(&Request::Destroy(g));
        }
    }
}

/// A backend on this process's WebGL thread, as the GPU process runs a link's contexts on (its device,
/// as `SNOWLARK_WEBGL` asks: `snowlark_gl::Service::serving`).
pub fn gpu_backend() -> Result<Box<dyn Backend>, String> {
    snowlark_gl::Service::serving().map(|s| Box::new(s) as Box<dyn Backend>)
}

/// What the GPU process checks a renderer's link against: a renderer may send only [`GlUp`].
fn policy() -> ProcessPolicy {
    ProcessPolicy {
        process: 0,
        kind: ProcessKind::Renderer,
        lock: Default::default(),
    }
}

/// The contexts one link has made, by its ids.
#[derive(Default)]
struct Served {
    contexts: HashSet<ContextId>,
}

impl Served {
    /// Checks `request` against what the link has; an answer in place of running it, when it
    /// is refused as an honest page could cause (too many contexts, a buffer too large).
    fn refuse(&self, request: &Request) -> Option<Answer> {
        match request {
            Request::Create {
                id, width, height, ..
            } => {
                if *width > MAX_SIDE || *height > MAX_SIDE {
                    Some(Answer::Created(Err(format!(
                        "a {width}x{height} drawing buffer is larger than the GPU process makes"
                    ))))
                } else if !self.contexts.contains(id) && self.contexts.len() >= MAX_CONTEXTS {
                    Some(Answer::Created(Err("too many WebGL contexts".into())))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Serves a renderer's link in the GPU process until it ends: each request runs on a backend
/// `make` makes at the first context (the process's WebGL thread), each asking one is answered. A
/// request that breaks the protocol ends the link, and `refused` is told why.
pub fn serve(
    link: GlServer,
    make: impl Fn() -> Result<Box<dyn Backend>, String>,
    refused: impl Fn(&str),
) {
    let policy = policy();
    let mut backend: Option<Box<dyn Backend>> = None;
    let mut served = Served::default();
    let broke = |why: String| {
        link.bad_message(&why);
        refused(&why);
    };
    loop {
        let message = match link.recv() {
            Ok(Received::Message(m)) => m,
            Ok(Received::Call(..)) => {
                broke("a call on a WebGL link".into());
                break;
            }
            Err(_) => {
                if let Some(why) = link.broke() {
                    refused(&why);
                }
                break;
            }
        };
        let Ok(message) = link.check(message, &policy) else {
            if let Some(why) = link.broke() {
                refused(&why);
            }
            break;
        };
        let (asks, bulk) = match message {
            GlUp::Send(b) => (false, b),
            GlUp::Ask(b) => (true, b),
        };
        let bytes = match link.open_bulk(bulk, BYTES_WAIT) {
            Ok(b) => b,
            Err(Error::Closed) => break,
            Err(e) => {
                refused(&e.to_string());
                break;
            }
        };
        let request: Request = match wire::decode(&bytes) {
            Ok(r) => r,
            Err(e) => {
                broke(format!("a WebGL request that does not decode: {e}"));
                break;
            }
        };
        if request.asks() != asks {
            broke("a WebGL request sent as the other kind".into());
            break;
        }
        let answer = match served.refuse(&request) {
            Some(a) => Some(a),
            None => run(request, &mut backend, &mut served, &make),
        };
        if let Some(answer) = answer {
            let sent = wire::to_bytes(&answer)
                .and_then(|bytes| link.send_bulk(&bytes, GlDown::Answer, BYTES_WAIT));
            if sent.is_err() {
                break;
            }
        }
    }
    // (the backend's drop destroys the link's contexts)
    link.close();
}

/// Runs one request: its answer, if it asks one.
fn run(
    request: Request,
    backend: &mut Option<Box<dyn Backend>>,
    served: &mut Served,
    make: &impl Fn() -> Result<Box<dyn Backend>, String>,
) -> Option<Answer> {
    if backend.is_none() {
        if let Request::Create { .. } = request {
            match make() {
                Ok(b) => *backend = Some(b),
                Err(e) => return Some(Answer::Created(Err(e))),
            }
        }
    }
    let Some(b) = backend.as_mut() else {
        // (no context was ever made: nothing to run it on)
        return match request {
            Request::Query(..) => Some(Answer::Reply(Reply::None)),
            Request::Present(_) => Some(Answer::Readback(None)),
            _ => None,
        };
    };
    match request {
        Request::Create {
            id,
            attributes,
            width,
            height,
        } => {
            let made = b.create_context(id, &attributes, width, height);
            if made.is_ok() {
                served.contexts.insert(id);
            } else {
                served.contexts.remove(&id);
            }
            Some(Answer::Created(made))
        }
        Request::Resize { id, width, height } => {
            if width <= MAX_SIDE && height <= MAX_SIDE {
                b.resize(id, width, height);
            }
            None
        }
        Request::Destroy(id) => {
            served.contexts.remove(&id);
            b.destroy(id);
            None
        }
        Request::Submit(batch) => {
            b.submit(batch);
            None
        }
        Request::Query(id, q) => Some(Answer::Reply(b.query(id, &q))),
        Request::Present(id) => Some(Answer::Readback(b.present(id))),
    }
}

#[cfg(test)]
#[path = "webgl_link_tests.rs"]
mod tests;
