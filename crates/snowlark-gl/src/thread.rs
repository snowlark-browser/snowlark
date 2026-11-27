// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The process's WebGL thread, and [`Service`], a page's way to it.
//!
//! ANGLE opens one display per process (per kind of device), and every context on it shares that
//! display's Direct3D 11 device and its immediate context, which must not be used from several
//! threads at once: contexts driven from several page threads together are corrupted, fail to be
//! made, or wait forever on the device. So one thread owns every context of the process, as a GPU
//! process's main thread owns every client's, and each page's [`Service`] sends it the protocol's
//! messages: batches of commands without waiting, queries and readbacks waiting for the answer.
//! Contexts are renumbered process-wide on the way, so pages keep numbering theirs from 1.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;

use crate::protocol::*;
use crate::service::Device;

enum Request {
    /// The device of the kind, opened if it is not yet.
    Open(bool, Sender<Answer>),
    Create {
        warp: bool,
        id: ContextId,
        attributes: Attributes,
        width: u32,
        height: u32,
        answer: Sender<Answer>,
    },
    Resize(ContextId, u32, u32),
    Destroy(ContextId),
    Submit(Vec<(ContextId, Command)>),
    Query(ContextId, Query, Sender<Answer>),
    Present(ContextId, Sender<Answer>),
    #[cfg(windows)]
    ShareInto(ContextId, Option<SharedTargets>, Sender<Answer>),
    PresentShared(ContextId, Sender<Answer>),
    ReadPresented(ContextId, Sender<Answer>),
}

enum Answer {
    Opened(Result<(), String>),
    Created(Result<Created, String>),
    Reply(Reply),
    Readback(Option<Readback>),
    #[cfg(windows)]
    Shared(bool),
    PresentedShared(SharedPresent),
}

/// What the thread owns: the devices it opened (graphics card, WARP) and which one each context is
/// on.
struct Host {
    devices: Vec<(bool, Device)>,
    owner: HashMap<ContextId, usize>,
    /// The device whose context is current on the thread.
    last: Option<usize>,
}

impl Host {
    fn open(&mut self, warp: bool) -> Result<usize, String> {
        if let Some(i) = self.devices.iter().position(|(w, _)| *w == warp) {
            return Ok(i);
        }
        self.devices.push((warp, Device::new(warp)?));
        Ok(self.devices.len() - 1)
    }

    /// The device `i`, knowing whether its context is still the thread's current one.
    fn device(&mut self, i: usize) -> &mut Device {
        let d = &mut self.devices[i].1;
        if self.last != Some(i) {
            d.forget_current();
            self.last = Some(i);
        }
        d
    }

    fn of(&mut self, id: ContextId) -> Option<&mut Device> {
        let i = *self.owner.get(&id)?;
        Some(self.device(i))
    }

    fn handle(&mut self, r: Request) {
        // (a page that went away no longer waits for its answer: nothing to do then)
        match r {
            Request::Open(warp, answer) => {
                let _ = answer.send(Answer::Opened(self.open(warp).map(|_| ())));
            }
            Request::Create {
                warp,
                id,
                attributes,
                width,
                height,
                answer,
            } => {
                let made = self.open(warp).and_then(|i| {
                    let made = self
                        .device(i)
                        .create_context(id, &attributes, width, height);
                    if made.is_ok() {
                        self.owner.insert(id, i);
                    }
                    made
                });
                let _ = answer.send(Answer::Created(made));
            }
            Request::Resize(id, width, height) => {
                if let Some(d) = self.of(id) {
                    d.resize(id, width, height);
                }
            }
            Request::Destroy(id) => {
                if let Some(d) = self.of(id) {
                    d.destroy(id);
                }
                self.owner.remove(&id);
            }
            Request::Submit(batch) => {
                // (one page's batch: its contexts are all on one device)
                let Some(&(first, _)) = batch.first() else {
                    return;
                };
                if let Some(d) = self.of(first) {
                    d.submit(batch);
                }
            }
            Request::Query(id, q, answer) => {
                let reply = match self.of(id) {
                    Some(d) => d.query(id, &q),
                    None => Reply::None,
                };
                let _ = answer.send(Answer::Reply(reply));
            }
            Request::Present(id, answer) => {
                let rb = self.of(id).and_then(|d| d.present(id));
                let _ = answer.send(Answer::Readback(rb));
            }
            #[cfg(windows)]
            Request::ShareInto(id, targets, answer) => {
                let ok = self.of(id).is_some_and(|d| d.share_into(id, targets));
                let _ = answer.send(Answer::Shared(ok));
            }
            Request::PresentShared(id, answer) => {
                let p = self
                    .of(id)
                    .map_or(SharedPresent::Unavailable, |d| d.present_shared(id));
                let _ = answer.send(Answer::PresentedShared(p));
            }
            Request::ReadPresented(id, answer) => {
                let rb = self.of(id).and_then(|d| d.read_presented(id));
                let _ = answer.send(Answer::Readback(rb));
            }
        }
    }
}

/// The thread, started by the first service of the process; it lives as long as the process.
fn thread() -> Result<&'static Sender<Request>, String> {
    static THREAD: OnceLock<Result<Sender<Request>, String>> = OnceLock::new();
    THREAD
        .get_or_init(|| {
            let (to, from) = channel::<Request>();
            std::thread::Builder::new()
                .name("webgl".into())
                .spawn(move || {
                    let mut host = Host {
                        devices: Vec::new(),
                        owner: HashMap::new(),
                        last: None,
                    };
                    for r in from {
                        host.handle(r);
                    }
                })
                .map_err(|e| format!("the WebGL thread did not start: {e}"))?;
            Ok(to)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Process-wide context ids, so contexts of different pages never meet on the thread.
static NEXT_CONTEXT: AtomicU32 = AtomicU32::new(1);

const GONE: &str = "the WebGL thread stopped";

/// A page's contexts on ANGLE, run on the process's WebGL thread.
pub struct Service {
    warp: bool,
    to: Sender<Request>,
    answer: Sender<Answer>,
    answers: Receiver<Answer>,
    /// The page's context ids to the thread's.
    ids: HashMap<ContextId, ContextId>,
}

impl Service {
    /// The service as `SNOWLARK_WEBGL` asks: `off` (no WebGL), `gpu-process` (this process runs
    /// none: its contexts are the GPU process's, [`crate::in_gpu_process`]), `warp` (Direct3D's
    /// software rasterizer), otherwise the graphics card.
    pub fn from_env() -> Result<Service, String> {
        if crate::in_gpu_process() {
            return Err("WebGL runs in the GPU process (SNOWLARK_WEBGL=gpu-process)".into());
        }
        Service::serving()
    }

    /// The service of the process that runs the contexts, as `SNOWLARK_WEBGL` asks: the GPU
    /// process's, where `gpu-process` means the graphics card.
    pub fn serving() -> Result<Service, String> {
        match std::env::var("SNOWLARK_WEBGL").as_deref() {
            Ok("off") => Err("WebGL is off (SNOWLARK_WEBGL=off)".into()),
            Ok("warp") => Service::new(true),
            _ => Service::new(false),
        }
    }

    /// ANGLE's display on the graphics card, or on WARP (Direct3D's software rasterizer) when
    /// `warp`; an error when it cannot be opened.
    pub fn new(warp: bool) -> Result<Service, String> {
        let (answer, answers) = channel();
        let mut s = Service {
            warp,
            to: thread()?.clone(),
            answer,
            answers,
            ids: HashMap::new(),
        };
        match s.ask(Request::Open(warp, s.answer.clone())) {
            Some(Answer::Opened(r)) => r.map(|()| s),
            _ => Err(GONE.into()),
        }
    }

    fn send(&self, r: Request) {
        // (when the thread is gone, every query answers nothing from then on)
        let _ = self.to.send(r);
    }

    fn ask(&mut self, r: Request) -> Option<Answer> {
        self.to.send(r).ok()?;
        self.answers.recv().ok()
    }
}

impl Backend for Service {
    fn create_context(
        &mut self,
        id: ContextId,
        attributes: &Attributes,
        width: u32,
        height: u32,
    ) -> Result<Created, String> {
        self.destroy(id);
        let global = NEXT_CONTEXT.fetch_add(1, Ordering::Relaxed);
        let r = Request::Create {
            warp: self.warp,
            id: global,
            attributes: *attributes,
            width,
            height,
            answer: self.answer.clone(),
        };
        match self.ask(r) {
            Some(Answer::Created(made)) => {
                if made.is_ok() {
                    self.ids.insert(id, global);
                }
                made
            }
            _ => Err(GONE.into()),
        }
    }

    fn resize(&mut self, id: ContextId, width: u32, height: u32) {
        if let Some(&g) = self.ids.get(&id) {
            self.send(Request::Resize(g, width, height));
        }
    }

    fn destroy(&mut self, id: ContextId) {
        if let Some(g) = self.ids.remove(&id) {
            self.send(Request::Destroy(g));
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
            self.send(Request::Submit(batch));
        }
    }

    fn query(&mut self, id: ContextId, query: &Query) -> Reply {
        let Some(&g) = self.ids.get(&id) else {
            return Reply::None;
        };
        match self.ask(Request::Query(g, query.clone(), self.answer.clone())) {
            Some(Answer::Reply(r)) => r,
            _ => Reply::None,
        }
    }

    fn present(&mut self, id: ContextId) -> Option<Readback> {
        let &g = self.ids.get(&id)?;
        match self.ask(Request::Present(g, self.answer.clone()))? {
            Answer::Readback(rb) => rb,
            _ => None,
        }
    }

    #[cfg(windows)]
    fn share_into(&mut self, id: ContextId, targets: Option<SharedTargets>) -> bool {
        let Some(&g) = self.ids.get(&id) else {
            return false;
        };
        matches!(
            self.ask(Request::ShareInto(g, targets, self.answer.clone())),
            Some(Answer::Shared(true))
        )
    }

    fn present_shared(&mut self, id: ContextId) -> SharedPresent {
        let Some(&g) = self.ids.get(&id) else {
            return SharedPresent::Unavailable;
        };
        match self.ask(Request::PresentShared(g, self.answer.clone())) {
            Some(Answer::PresentedShared(p)) => p,
            _ => SharedPresent::Unavailable,
        }
    }

    fn read_presented(&mut self, id: ContextId) -> Option<Readback> {
        let &g = self.ids.get(&id)?;
        match self.ask(Request::ReadPresented(g, self.answer.clone()))? {
            Answer::Readback(rb) => rb,
            _ => None,
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        for (_, g) in self.ids.drain() {
            let _ = self.to.send(Request::Destroy(g));
        }
    }
}
