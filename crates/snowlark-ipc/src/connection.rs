// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A connection: `send`, `call` and `recv` over a [`Transport`], the same code for a real pipe and
//! for the loopback. No `unsafe` here: everything the operating system does is behind the transport
//! and [`crate::win`].
//!
//! Frames come in on the IO thread and wait as bytes in a bounded queue. Decoding happens in `recv`,
//! on the caller's thread, so a slow or hostile message never holds up the IO thread. When the queue
//! is full the transport stops reading: the peer blocks on its own pipe and browser memory stays
//! bounded (section 7's back-pressure).

use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::frame::{self, Frame, Kind};
use crate::link::LinkMemory;
use crate::os::handle::{Peer, RemoteHandle};
use crate::section::{Broker, Section};
use crate::untrusted::{FromBrowser, FromChild, ProcessPolicy, Trust, Untrusted, Validate};
use crate::wire::{self, BuildId, Control, ControlReply, Hello, Wire, MAGIC};

/// Queued frames past which the transport stops reading.
pub const MAX_QUEUED: usize = 4096;
/// Queued bytes past which the transport stops reading.
pub const MAX_QUEUED_BYTES: usize = 16 << 20;

/// Moves frames. Implemented by the pipe and by the loopback.
pub(crate) trait Transport: Send + Sync {
    fn write(&self, frame: Vec<u8>) -> Result<(), Error>;
    /// Reading may go on: the queue has room again.
    fn resume(&self);
    /// Stops everything. Further writes fail.
    fn shut(&self);
    /// Whether everything written has left this process.
    fn flushed(&self) -> bool;
    /// Writes a [`Kind::Handle`] frame with a copy of `fd` beside it.
    #[cfg(unix)]
    fn write_handle(&self, frame: Vec<u8>, fd: std::os::fd::OwnedFd) -> Result<(), Error> {
        let _ = (frame, fd);
        Err(Error::Refused)
    }
}

enum Inbound {
    Message(Vec<u8>),
    Call(u64, Vec<u8>),
}

struct State {
    queue: VecDeque<Inbound>,
    bytes: usize,
    /// The transport was told to stop reading.
    paused: bool,
    closed: bool,
    /// Why the peer was found to break a rule, if it was (the first time).
    broke: Option<String>,
    /// The peer said hello, and is the same build.
    hello: Option<Result<u32, Error>>,
    waiting: HashMap<u64, SyncSender<Result<Vec<u8>, Error>>>,
}

/// What the transport does after handing over a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flow {
    Go,
    /// The queue is full: stop reading until [`Transport::resume`].
    Pause,
    /// The stream is finished.
    Stop,
}

pub(crate) struct Core {
    state: Mutex<State>,
    changed: Condvar,
    transport: OnceLock<Arc<dyn Transport>>,
    next_id: AtomicU64,
    build: BuildId,
    browser: bool,
    pub(crate) peer: Peer,
    /// The browser's end: the sections made for this child.
    pub(crate) broker: Option<Mutex<Broker>>,
    /// A link's end: its rings.
    pub(crate) link: Option<LinkMemory>,
    /// The browser's end: descriptors the child gave, by the values it chose.
    #[cfg(unix)]
    received: Mutex<HashMap<u64, std::os::fd::OwnedFd>>,
}

/// Descriptors a child may have given the browser and not had taken, past which it breaks the rules.
#[cfg(unix)]
const MAX_GIVEN: usize = 256;

impl Core {
    pub(crate) fn new(browser: bool, build: BuildId, peer: Peer, quota: u64) -> Arc<Core> {
        Core::make(browser, build, peer, quota, None)
    }

    /// One end of a link between two children ([`crate::link`]).
    pub(crate) fn linked(build: BuildId, memory: LinkMemory) -> Arc<Core> {
        Core::make(false, build, Peer::Link, 0, Some(memory))
    }

    fn make(
        browser: bool,
        build: BuildId,
        peer: Peer,
        quota: u64,
        link: Option<LinkMemory>,
    ) -> Arc<Core> {
        Arc::new(Core {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                bytes: 0,
                paused: false,
                closed: false,
                broke: None,
                hello: None,
                waiting: HashMap::new(),
            }),
            changed: Condvar::new(),
            transport: OnceLock::new(),
            next_id: AtomicU64::new(1),
            build,
            browser,
            peer,
            broker: browser.then(|| Mutex::new(Broker::new(quota))),
            link,
            #[cfg(unix)]
            received: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn attach(&self, transport: Arc<dyn Transport>) {
        let _ = self.transport.set(transport);
    }

    fn write(&self, frame: Vec<u8>) -> Result<(), Error> {
        if self.state.lock().unwrap().closed {
            return Err(Error::Closed);
        }
        self.transport.get().ok_or(Error::Closed)?.write(frame)
    }

    pub(crate) fn say_hello(&self) -> Result<(), Error> {
        let hello = Hello {
            magic: MAGIC,
            build: self.build,
            process: std::process::id(),
        };
        self.write(frame::encode(Kind::Hello, 0, &wire::encode(&hello)?)?)
    }

    /// Takes a frame from the transport (the IO thread).
    pub(crate) fn deliver(&self, frame: Frame) -> Flow {
        match self.route(frame) {
            Ok(flow) => flow,
            Err(why) => {
                self.bad_message(&why);
                Flow::Stop
            }
        }
    }

    fn route(&self, frame: Frame) -> Result<Flow, String> {
        let mut s = self.state.lock().unwrap();
        if s.hello.is_none() {
            if frame.kind != Kind::Hello {
                return Err(format!("{:?} before hello", frame.kind));
            }
            let hello: Hello = wire::decode(&frame.payload).map_err(|e| format!("hello: {e}"))?;
            if hello.magic != MAGIC {
                return Err("hello: wrong magic".into());
            }
            s.hello = Some(if hello.build == self.build {
                Ok(hello.process)
            } else {
                Err(Error::BuildMismatch)
            });
            self.changed.notify_all();
            return Ok(Flow::Go);
        }
        match frame.kind {
            Kind::Hello => Err("a second hello".into()),
            Kind::Handle => Err("a handle frame with no descriptor".into()),
            Kind::Message | Kind::Call => {
                s.bytes += frame.payload.len();
                s.queue.push_back(match frame.kind {
                    Kind::Call => Inbound::Call(frame.id, frame.payload),
                    _ => Inbound::Message(frame.payload),
                });
                self.changed.notify_one();
                if s.queue.len() >= MAX_QUEUED || s.bytes >= MAX_QUEUED_BYTES {
                    s.paused = true;
                    return Ok(Flow::Pause);
                }
                Ok(Flow::Go)
            }
            Kind::Control | Kind::ControlReply if self.link.is_some() => {
                Err(format!("{:?} on a link", frame.kind))
            }
            Kind::Reply | Kind::ControlReply => {
                if frame.kind == Kind::ControlReply && self.browser {
                    return Err("a control reply sent to the browser".into());
                }
                // (a reply may come after its caller timed out; a reply to no call never can)
                if frame.id == 0 || frame.id >= self.next_id.load(Ordering::Acquire) {
                    return Err(format!("a reply to call {} never made", frame.id));
                }
                if let Some(tx) = s.waiting.remove(&frame.id) {
                    let _ = tx.send(Ok(frame.payload));
                }
                Ok(Flow::Go)
            }
            Kind::Control => {
                drop(s);
                if !self.browser {
                    return Err("control from the browser".into());
                }
                self.serve_control(frame)?;
                Ok(Flow::Go)
            }
        }
    }

    fn serve_control(&self, frame: Frame) -> Result<(), String> {
        let control: Control = wire::decode(&frame.payload).map_err(|e| format!("control: {e}"))?;
        let broker = self.broker.as_ref().ok_or("no broker")?;
        let reply = match control {
            Control::AllocShared { size } => {
                let mut broker = broker.lock().unwrap();
                match broker.alloc(size) {
                    Some((id, section)) => match self.duplicate_section(&section) {
                        Ok(handle) => ControlReply::Allocated {
                            id,
                            handle,
                            size: section.size() as u64,
                        },
                        Err(_) => {
                            broker.free(id);
                            ControlReply::Refused
                        }
                    },
                    None => ControlReply::Refused,
                }
            }
            Control::FreeShared { id } => {
                if !broker.lock().unwrap().free(id) {
                    return Err(format!("freeing section {id}, which it does not hold"));
                }
                ControlReply::Freed
            }
        };
        let payload = wire::encode(&reply).map_err(|e| e.to_string())?;
        let bytes = frame::encode(Kind::ControlReply, frame.id, &payload);
        // (the peer may be gone by now, which the transport reports on its own)
        if let Ok(bytes) = bytes {
            let _ = self.write(bytes);
        }
        Ok(())
    }

    /// A copy of `section` for the peer, which may map it to read and write but not resize it: its
    /// value there.
    pub(crate) fn duplicate_section(&self, section: &Section) -> Result<u64, Error> {
        #[cfg(windows)]
        return self.peer.duplicate_section(section);
        #[cfg(unix)]
        return self.duplicate(section.handle()).map(|h| h.value());
    }

    /// A copy of `handle` for the peer, a child or this process: its value there.
    #[cfg(windows)]
    pub(crate) fn duplicate(
        &self,
        handle: &impl std::os::windows::io::AsHandle,
    ) -> Result<RemoteHandle, Error> {
        self.peer.duplicate(handle)
    }

    /// A copy of `fd` for the peer, a child or this process: its value there.
    #[cfg(unix)]
    pub(crate) fn duplicate(&self, fd: &impl std::os::fd::AsFd) -> Result<RemoteHandle, Error> {
        match self.peer {
            Peer::Child(_) | Peer::Local => crate::posix::handle::give(self, fd.as_fd()),
            Peer::Browser | Peer::Link => Err(Error::Refused),
        }
    }

    /// Sends a [`Kind::Handle`] frame with `fd` beside it, kept as `value` at the other end.
    #[cfg(unix)]
    pub(crate) fn write_handle(&self, value: u64, fd: std::os::fd::OwnedFd) -> Result<(), Error> {
        if self.state.lock().unwrap().closed {
            return Err(Error::Closed);
        }
        let bytes = frame::encode(Kind::Handle, value, &[])?;
        self.transport
            .get()
            .ok_or(Error::Closed)?
            .write_handle(bytes, fd)
    }

    /// Takes a descriptor from the transport (the IO thread), kept under `value`: at the browser's
    /// end with this child's connection, at a child's in its process's table.
    #[cfg(unix)]
    pub(crate) fn deliver_handle(&self, value: u64, fd: std::os::fd::OwnedFd) -> Flow {
        let why = if self.state.lock().unwrap().hello.is_none() {
            Some("a handle before hello")
        } else if self.link.is_some() {
            Some("a handle on a link")
        } else if self.browser {
            let mut received = self.received.lock().unwrap();
            if received.len() >= MAX_GIVEN {
                Some("too many handles given")
            } else if received.insert(value, fd).is_some() {
                Some("a handle value given twice")
            } else {
                None
            }
        } else {
            crate::posix::handle::keep(value, fd);
            None
        };
        match why {
            Some(why) => {
                self.bad_message(why);
                Flow::Stop
            }
            None => Flow::Go,
        }
    }

    /// The descriptor the child gave under `value` ([`Core::deliver_handle`]), taken.
    #[cfg(unix)]
    pub(crate) fn take_received(&self, value: u64) -> Option<std::os::fd::OwnedFd> {
        self.received.lock().unwrap().remove(&value)
    }

    /// The stream ended, whatever the reason: everything waiting hears it at once.
    pub(crate) fn closed(&self) {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        for (_, tx) in s.waiting.drain() {
            let _ = tx.send(Err(Error::Closed));
        }
        self.changed.notify_all();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.state.lock().unwrap().paused
    }

    /// The peer broke the rules. At the browser's end it dies, with no attempt to recover.
    pub(crate) fn bad_message(&self, reason: &str) {
        eprintln!("[ipc] bad message from {}: {reason}", self.peer.describe());
        self.state
            .lock()
            .unwrap()
            .broke
            .get_or_insert_with(|| reason.to_string());
        if self.browser {
            self.peer.kill();
        }
        if let Some(t) = self.transport.get() {
            t.shut();
        }
        self.closed();
    }

    fn wait_hello(&self, timeout: Duration) -> Result<u32, Error> {
        let end = Instant::now() + timeout;
        let mut s = self.state.lock().unwrap();
        loop {
            match &s.hello {
                Some(Ok(pid)) => return Ok(*pid),
                Some(Err(_)) => return Err(Error::BuildMismatch),
                None if s.closed => return Err(Error::Closed),
                None => {}
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            s = self.changed.wait_timeout(s, left).unwrap().0;
        }
    }

    /// The next inbound, waiting at most `timeout`.
    fn pop(&self, timeout: Option<Duration>) -> Result<Inbound, Error> {
        let end = timeout.map(|t| Instant::now() + t);
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some(inbound) = s.queue.pop_front() {
                s.bytes -= match &inbound {
                    Inbound::Message(b) | Inbound::Call(_, b) => b.len(),
                };
                // (with hysteresis: not a read per message at the limit)
                let resume =
                    s.paused && s.queue.len() < MAX_QUEUED / 2 && s.bytes < MAX_QUEUED_BYTES / 2;
                if resume {
                    s.paused = false;
                }
                drop(s);
                if resume {
                    if let Some(t) = self.transport.get() {
                        t.resume();
                    }
                }
                return Ok(inbound);
            }
            if s.closed {
                return Err(Error::Closed);
            }
            s = match end {
                None => self.changed.wait(s).unwrap(),
                Some(end) => {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Error::Timeout);
                    }
                    self.changed.wait_timeout(s, left).unwrap().0
                }
            };
        }
    }

    fn call_raw(&self, kind: Kind, payload: &[u8], timeout: Duration) -> Result<Vec<u8>, Error> {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        let (tx, rx) = sync_channel(1);
        {
            let mut s = self.state.lock().unwrap();
            if s.closed {
                return Err(Error::Closed);
            }
            s.waiting.insert(id, tx);
        }
        let sent = frame::encode(kind, id, payload).and_then(|bytes| self.write(bytes));
        if let Err(e) = sent {
            self.state.lock().unwrap().waiting.remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(reply) => reply,
            Err(RecvTimeoutError::Timeout) => {
                self.state.lock().unwrap().waiting.remove(&id);
                Err(Error::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(Error::Closed),
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Some(t) = self.transport.get() {
            t.shut();
        }
    }
}

/// What came off a connection.
pub enum Received<In, Tr: Trust> {
    Message(Tr::Wrap<In>),
    /// Its sender waits for the [`Replier`] to answer.
    Call(Tr::Wrap<In>, Replier),
}

/// Answers one call.
pub struct Replier {
    core: Arc<Core>,
    id: u64,
}

impl Replier {
    pub fn reply<R: Wire>(self, value: &R) -> Result<(), Error> {
        let bytes = frame::encode(Kind::Reply, self.id, &wire::encode(value)?)?;
        self.core.write(bytes)
    }
}

/// One end of a connection: it sends `Out` and hears `In`. `Tr` says who the peer is:
/// [`FromChild`] hands out [`Untrusted`] values, [`FromBrowser`] plain ones. Clones share the
/// connection, so any thread may `send` and `call`; take messages from one thread.
pub struct Connection<Out, In, Tr: Trust> {
    pub(crate) core: Arc<Core>,
    policy: Option<Arc<ProcessPolicy>>,
    _types: PhantomData<fn(Out) -> (In, Tr)>,
}

/// The browser's end of a connection to a child.
pub type BrowserEnd<Out, In> = Connection<Out, In, FromChild>;
/// A child's end of its connection to the browser.
pub type ChildEnd<Out, In> = Connection<Out, In, FromBrowser>;

impl<Out, In, Tr: Trust> Clone for Connection<Out, In, Tr> {
    fn clone(&self) -> Self {
        Connection {
            core: self.core.clone(),
            policy: self.policy.clone(),
            _types: PhantomData,
        }
    }
}

impl<Out: Wire, In: Wire, Tr: Trust> Connection<Out, In, Tr> {
    pub(crate) fn new(core: Arc<Core>, policy: Option<ProcessPolicy>) -> Self {
        Connection {
            core,
            policy: policy.map(Arc::new),
            _types: PhantomData,
        }
    }

    /// Waits until the peer has said hello and is this build. Returns its process id.
    pub(crate) fn established(self, timeout: Duration) -> Result<(Self, u32), Error> {
        let pid = self.core.wait_hello(timeout)?;
        Ok((self, pid))
    }

    /// Sends a message nobody waits on.
    pub fn send(&self, message: &Out) -> Result<(), Error> {
        self.core
            .write(frame::encode(Kind::Message, 0, &wire::encode(message)?)?)
    }

    /// Sends a message and waits for the answer. Fails at once, with [`Error::Closed`], when the
    /// peer dies: nothing ever waits on a dead process.
    pub fn call<R: Wire>(&self, message: &Out, timeout: Duration) -> Result<Tr::Wrap<R>, Error> {
        let reply = self
            .core
            .call_raw(Kind::Call, &wire::encode(message)?, timeout)?;
        Ok(Tr::wrap(self.decode(&reply)?))
    }

    fn decode<T: Wire>(&self, bytes: &[u8]) -> Result<T, Error> {
        wire::decode(bytes).inspect_err(|e| {
            // (a link ends too: neither end parses past a message it cannot read)
            if Tr::BROWSER || self.core.link.is_some() {
                self.core.bad_message(&e.to_string());
            }
        })
    }

    fn received(&self, inbound: Inbound) -> Result<Received<In, Tr>, Error> {
        Ok(match inbound {
            Inbound::Message(bytes) => Received::Message(Tr::wrap(self.decode(&bytes)?)),
            Inbound::Call(id, bytes) => Received::Call(
                Tr::wrap(self.decode(&bytes)?),
                Replier {
                    core: self.core.clone(),
                    id,
                },
            ),
        })
    }

    /// The next message, waiting for it. At the browser's end a message that does not decode kills
    /// the sender. [`Error::Closed`] once the peer is gone and everything it sent has been taken.
    pub fn recv(&self) -> Result<Received<In, Tr>, Error> {
        self.received(self.core.pop(None)?)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Received<In, Tr>, Error> {
        self.received(self.core.pop(Some(timeout))?)
    }

    /// Waits until everything sent has left this process, at most `timeout`. A process about to exit
    /// calls this: what is still queued dies with it.
    pub fn flush(&self, timeout: Duration) -> Result<(), Error> {
        let end = Instant::now() + timeout;
        let transport = self.core.transport.get().ok_or(Error::Closed)?;
        while !transport.flushed() {
            if Instant::now() >= end {
                return Err(Error::Timeout);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// Puts bytes on the wire as they are (for tests of what a peer may do).
    #[doc(hidden)]
    pub fn write_raw(&self, bytes: Vec<u8>) -> Result<(), Error> {
        self.core.write(bytes)
    }

    /// How many messages wait to be taken.
    #[doc(hidden)]
    pub fn queued(&self) -> usize {
        self.core.state.lock().unwrap().queue.len()
    }

    /// Why this end ended for the peer breaking a rule, if it did: on a link, where the end that
    /// saw it tells the browser (a read ends with [`Error::Closed`] either way).
    pub fn broke(&self) -> Option<String> {
        self.core.state.lock().unwrap().broke.clone()
    }

    /// Closes this end. The peer's reads end, and calls in flight here fail.
    pub fn close(&self) {
        if let Some(t) = self.core.transport.get() {
            t.shut();
        }
        self.core.closed();
    }
}

impl<Out: Wire, In: Wire> Connection<Out, In, FromChild> {
    /// The process on the other end, as the browser has it.
    pub fn policy(&self) -> &ProcessPolicy {
        self.policy.as_ref().expect("a browser end has a policy")
    }

    /// The sender broke the rules: it is logged and dies, with no attempt to recover.
    pub fn bad_message(&self, reason: &str) {
        self.core.bad_message(reason);
    }

    /// Validates a value from this child. On failure the child dies.
    pub fn check<T: Validate>(&self, value: Untrusted<T>) -> Result<T, Error> {
        value.validate(self.policy()).map_err(|bad| {
            self.bad_message(&bad.0);
            Error::BadMessage(bad.0)
        })
    }

    /// Gives the child a handle: only the browser duplicates, into a child, never the other way.
    /// The value travels in a message of the caller's, and the child takes it with
    /// [`Connection::take_handle`].
    #[cfg(windows)]
    pub fn duplicate_to_child(
        &self,
        handle: &impl std::os::windows::io::AsHandle,
    ) -> Result<RemoteHandle, Error> {
        self.core.duplicate(handle)
    }

    /// Gives the child a copy of a descriptor: only the browser gives, to a child, never the other
    /// way but from the GPU process. The value travels in a message of the caller's, and the child
    /// takes it with [`Connection::take_handle`].
    #[cfg(unix)]
    pub fn duplicate_to_child(&self, fd: &impl std::os::fd::AsFd) -> Result<RemoteHandle, Error> {
        self.core.duplicate(fd)
    }

    /// Takes a handle the GPU process gave ([`Connection::give_handle`]) out of its table into the
    /// browser's: it is closed there. Only a GPU process may give handles (its device's shared
    /// textures, which the browser hands to renderers): from any other child this is refused. The
    /// value can name only a handle of the child's own, never one of the browser's.
    #[cfg(windows)]
    pub fn take_from_child(
        &self,
        handle: RemoteHandle,
    ) -> Result<std::os::windows::io::OwnedHandle, Error> {
        // (the loopback's child is this process: nothing crosses)
        let local = matches!(self.core.peer, Peer::Local);
        if self.policy().kind != crate::ProcessKind::Gpu && !local {
            return Err(Error::Refused);
        }
        self.core.peer.take_out(handle)
    }

    /// Takes a descriptor the GPU process gave ([`Connection::give_handle`]). From any other child
    /// this is refused. The value names only a descriptor this child gave on this connection.
    #[cfg(unix)]
    pub fn take_from_child(&self, handle: RemoteHandle) -> Result<std::os::fd::OwnedFd, Error> {
        // (the loopback's child is this process: what it gave is in this process's table)
        if matches!(self.core.peer, Peer::Local) {
            return handle.take_checked();
        }
        if self.policy().kind != crate::ProcessKind::Gpu {
            return Err(Error::Refused);
        }
        self.core
            .take_received(handle.value())
            .ok_or(Error::Refused)
    }

    /// Gives the child a section the browser made and keeps: it can map it to read and write, not
    /// extend it. The child maps it with [`Connection::map_section`]. What the child writes there is
    /// its word ([`Section::read_once`], [`Section::load_u64`]).
    pub fn share_section(&self, section: &Section) -> Result<RemoteHandle, Error> {
        Ok(RemoteHandle::from_value(
            self.core.duplicate_section(section)?,
        ))
    }

    /// A section this child asked for, by the id it holds.
    pub fn section(&self, id: u64) -> Option<Arc<Section>> {
        self.core.broker.as_ref()?.lock().unwrap().get(id)
    }

    /// The section bytes held for this child now, against its quota.
    pub fn shared_bytes(&self) -> u64 {
        self.core
            .broker
            .as_ref()
            .map_or(0, |b| b.lock().unwrap().used())
    }
}

/// A section the browser made for this process, freed when it is dropped.
pub struct SharedSection {
    section: Section,
    id: u64,
    core: Arc<Core>,
    /// False once handed over: the browser frees it then, so this end must not.
    owned: bool,
}

impl SharedSection {
    /// The id the browser knows it by.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Closes this end's mapping and leaves the section to the browser, which frees it when it reads
    /// the message that names it ([`crate::Bulk`]). Returns its id.
    pub fn hand_over(mut self) -> u64 {
        self.owned = false;
        self.id
    }
}

impl std::ops::Deref for SharedSection {
    type Target = Section;
    fn deref(&self) -> &Section {
        &self.section
    }
}

impl Drop for SharedSection {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        let free = wire::encode(&Control::FreeShared { id: self.id });
        if let Ok(bytes) = free {
            let _ = self
                .core
                .call_raw(Kind::Control, &bytes, Duration::from_secs(1));
        }
    }
}

impl<Out: Wire, In: Wire> Connection<Out, In, FromBrowser> {
    /// Takes a handle the browser put in this process's table. There is no way to do this at the
    /// browser's end, whatever a message type says: a child's word never names a browser handle.
    #[cfg(windows)]
    pub fn take_handle(&self, handle: RemoteHandle) -> std::os::windows::io::OwnedHandle {
        handle.take()
    }

    /// Takes a descriptor the browser gave this process. A value that names none fails.
    #[cfg(unix)]
    pub fn take_handle(&self, handle: RemoteHandle) -> Result<std::os::fd::OwnedFd, Error> {
        handle.take_checked()
    }

    /// Leaves `handle` in this process's table for the browser to take out
    /// ([`Connection::take_from_child`]), which closes it here. The value travels in a message of
    /// the caller's; one the browser never takes stays open here.
    #[cfg(windows)]
    pub fn give_handle(&self, handle: std::os::windows::io::OwnedHandle) -> RemoteHandle {
        RemoteHandle::give(handle)
    }

    /// Gives the browser `fd`, for it to take with [`Connection::take_from_child`]; it is closed
    /// here. The value travels in a message of the caller's.
    #[cfg(unix)]
    pub fn give_handle(&self, fd: std::os::fd::OwnedFd) -> Result<RemoteHandle, Error> {
        match self.core.peer {
            Peer::Browser | Peer::Local => {
                crate::posix::handle::give(&self.core, std::os::fd::AsFd::as_fd(&fd))
            }
            Peer::Child(_) | Peer::Link => Err(Error::Refused),
        }
    }

    /// Maps a section of `size` bytes the browser shared with [`Connection::share_section`].
    pub fn map_section(&self, handle: RemoteHandle, size: u64) -> Result<Section, Error> {
        Section::from_remote(handle, size)
    }

    /// `AllocShared`: asks the browser for a section of at least `size` bytes, counted against this
    /// process's quota. The browser creates it and duplicates it here.
    pub fn alloc_shared(&self, size: u64, timeout: Duration) -> Result<SharedSection, Error> {
        let bytes = wire::encode(&Control::AllocShared { size })?;
        let reply = self.core.call_raw(Kind::Control, &bytes, timeout)?;
        match wire::decode(&reply)? {
            ControlReply::Allocated { id, handle, size } => Ok(SharedSection {
                section: Section::from_remote(RemoteHandle::from_value(handle), size)?,
                id,
                core: self.core.clone(),
                owned: true,
            }),
            _ => Err(Error::Refused),
        }
    }
}
