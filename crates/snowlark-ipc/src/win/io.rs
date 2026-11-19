// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The IO thread: one completion port per process, overlapped reads and writes on every pipe.
//!
//! An [`Op`] is one operation in the kernel's hands. It is boxed and leaked when issued and rebuilt
//! exactly once, on this thread when its packet comes, or on the spot if the call failed without
//! starting (then no packet comes). So its `OVERLAPPED` and buffer outlive the operation whatever
//! happens to the connection, and it holds the pipe open through an `Arc` until then.
//!
//! The handle closes when the link is shut and the last operation is back.

use std::collections::VecDeque;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::System::IO::*;

use crate::connection::{Core, Flow, Transport};
use crate::error::Error;
use crate::frame::FrameReader;

const READ_BUFFER: usize = 64 << 10;
/// Bytes queued for a peer that is not reading, past which `send` fails.
const MAX_BACKLOG: usize = 64 << 20;

struct Port(HANDLE);
// SAFETY: a completion port handle may be used from any thread.
unsafe impl Send for Port {}
// SAFETY: as above.
unsafe impl Sync for Port {}

fn port() -> Result<&'static Port, Error> {
    static PORT: OnceLock<Port> = OnceLock::new();
    if let Some(p) = PORT.get() {
        return Ok(p);
    }
    // SAFETY: makes a new port: no file yet, one thread's worth of concurrency.
    let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1)? };
    let mut created = false;
    let p = PORT.get_or_init(|| {
        created = true;
        Port(handle)
    });
    if created {
        std::thread::Builder::new()
            .name("snowlark ipc".into())
            .spawn(move || run(p))
            .map_err(|_| Error::Closed)?;
    } else {
        // SAFETY: the port this thread lost the race with is its own to close.
        let _ = unsafe { CloseHandle(handle) };
    }
    Ok(p)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Read,
    Write,
}

#[repr(C)]
struct Op {
    /// First, so that the kernel's pointer to it is a pointer to the `Op`.
    overlapped: OVERLAPPED,
    kind: OpKind,
    link: Arc<PipeLink>,
    buffer: Vec<u8>,
    /// How much of `buffer` a write has sent.
    done: usize,
}

fn run(port: &'static Port) {
    loop {
        let (mut bytes, mut key) = (0u32, 0usize);
        let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
        // SAFETY: out pointers to locals on a port that lives as long as the process.
        let result = unsafe {
            GetQueuedCompletionStatus(port.0, &mut bytes, &mut key, &mut overlapped, INFINITE)
        };
        if overlapped.is_null() {
            continue;
        }
        // SAFETY: every OVERLAPPED on this port is the first field of an Op leaked by `issue`, and
        // the kernel hands each back exactly once.
        let op = unsafe { Box::from_raw(overlapped as *mut Op) };
        let link = op.link.clone();
        match op.kind {
            OpKind::Read => link.read_done(op, bytes as usize, result.is_ok()),
            OpKind::Write => link.write_done(*op, bytes as usize, result.is_ok()),
        }
        link.op_returned();
    }
}

#[derive(Default)]
struct Writes {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    in_flight: bool,
}

/// One pipe on the completion port.
pub(crate) struct PipeLink {
    raw: HANDLE,
    owned: Mutex<Option<OwnedHandle>>,
    core: Weak<Core>,
    /// Whether a read is in the kernel. Also what orders issuing a read against `shut`.
    reading: Mutex<bool>,
    writes: Mutex<Writes>,
    reader: Mutex<FrameReader>,
    closed: AtomicBool,
    ops: AtomicUsize,
}

// SAFETY: `raw` is the handle in `owned`, usable from any thread; it is only used while `closed` is
// false, under the locks `shut` takes, and it closes only when no operation is in the kernel.
unsafe impl Send for PipeLink {}
// SAFETY: as above.
unsafe impl Sync for PipeLink {}

impl PipeLink {
    /// Puts `handle` on the port. Reading starts with [`PipeLink::start`].
    pub(crate) fn new(handle: OwnedHandle, core: &Arc<Core>) -> Result<Arc<PipeLink>, Error> {
        let raw = HANDLE(handle.as_raw_handle());
        // SAFETY: an open overlapped handle joins the port for the rest of its life.
        unsafe { CreateIoCompletionPort(raw, Some(port()?.0), 0, 0)? };
        Ok(Arc::new(PipeLink {
            raw,
            owned: Mutex::new(Some(handle)),
            core: Arc::downgrade(core),
            reading: Mutex::new(false),
            writes: Mutex::new(Writes::default()),
            reader: Mutex::new(FrameReader::new()),
            closed: AtomicBool::new(false),
            ops: AtomicUsize::new(0),
        }))
    }

    pub(crate) fn start(self: &Arc<Self>) {
        self.resume_read(Vec::new());
    }

    /// Hands an operation to the kernel. False if it did not start.
    fn issue(self: &Arc<Self>, kind: OpKind, buffer: Vec<u8>, done: usize) -> bool {
        self.ops.fetch_add(1, Ordering::AcqRel);
        let op = Box::into_raw(Box::new(Op {
            overlapped: OVERLAPPED::default(),
            kind,
            link: self.clone(),
            buffer,
            done,
        }));
        // SAFETY: `op` is a live allocation that nothing else touches until its packet comes. The
        // buffer's heap memory does not move: the Vec is not touched meanwhile. On a call that fails
        // without starting no packet will come, so the box is rebuilt here instead.
        unsafe {
            let overlapped = std::ptr::addr_of_mut!((*op).overlapped);
            let buffer = &mut (*op).buffer;
            let result = match kind {
                OpKind::Read => ReadFile(self.raw, Some(buffer), None, Some(overlapped)),
                OpKind::Write => WriteFile(self.raw, Some(&buffer[done..]), None, Some(overlapped)),
            };
            match result {
                Ok(()) => true,
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => true,
                Err(_) => {
                    drop(Box::from_raw(op));
                    self.op_returned();
                    false
                }
            }
        }
    }

    fn resume_read(self: &Arc<Self>, buffer: Vec<u8>) {
        let mut reading = self.reading.lock().unwrap();
        if *reading || self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut buffer = buffer;
        buffer.resize(READ_BUFFER, 0);
        *reading = true;
        if !self.issue(OpKind::Read, buffer, 0) {
            *reading = false;
            drop(reading);
            self.end();
        }
    }

    fn read_done(self: &Arc<Self>, op: Box<Op>, bytes: usize, ok: bool) {
        *self.reading.lock().unwrap() = false;
        if !ok {
            self.end();
            return;
        }
        let Some(core) = self.core.upgrade() else {
            self.end();
            return;
        };
        let mut pause = false;
        {
            let mut reader = self.reader.lock().unwrap();
            reader.push(&op.buffer[..bytes]);
            loop {
                match reader.next_frame() {
                    Ok(Some(frame)) => match core.deliver(frame) {
                        Flow::Go => {}
                        // (what is already here still goes in: at most one read's worth)
                        Flow::Pause => pause = true,
                        Flow::Stop => return,
                    },
                    Ok(None) => break,
                    Err(e) => {
                        core.bad_message(&e.to_string());
                        return;
                    }
                }
            }
        }
        // A receiver may have made room since: `is_paused` is checked after `reading` went false, so
        // either it sees that, or the receiver's `resume` comes after and starts the read itself.
        if !pause || !core.is_paused() {
            let mut op = op;
            self.resume_read(std::mem::take(&mut op.buffer));
        }
    }

    fn write_done(self: &Arc<Self>, op: Op, bytes: usize, ok: bool) {
        let mut writes = self.writes.lock().unwrap();
        writes.in_flight = false;
        if !ok {
            drop(writes);
            self.end();
            return;
        }
        let Op { buffer, done, .. } = op;
        let done = done + bytes;
        // (after `stop` nothing is issued: on a peer that does not read, a write would never come
        // back, and the handle closes only when the last operation has)
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if done < buffer.len() {
            writes.in_flight = true;
            if !self.issue(OpKind::Write, buffer, done) {
                writes.in_flight = false;
                drop(writes);
                self.end();
            }
            return;
        }
        writes.bytes -= buffer.len();
        if !self.next_write(&mut writes) {
            drop(writes);
            self.end();
        }
    }

    fn next_write(self: &Arc<Self>, writes: &mut Writes) -> bool {
        let Some(frame) = writes.queue.pop_front() else {
            return true;
        };
        writes.in_flight = true;
        let started = self.issue(OpKind::Write, frame, 0);
        writes.in_flight = started;
        started
    }

    /// The stream is over: everything stops, and the connection hears it.
    fn end(self: &Arc<Self>) {
        self.stop();
        if let Some(core) = self.core.upgrade() {
            core.closed();
        }
    }

    fn stop(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // (a barrier: any issue that saw `closed` false is finished and counted after these)
        drop(self.reading.lock().unwrap());
        self.writes.lock().unwrap().queue.clear();
        // SAFETY: the handle is open, since operations hold it and this runs at most once before
        // the last of them returns. A failure means nothing was pending.
        let _ = unsafe { CancelIoEx(self.raw, None) };
        self.close_if_idle();
    }

    fn op_returned(&self) {
        self.ops.fetch_sub(1, Ordering::AcqRel);
        self.close_if_idle();
    }

    fn close_if_idle(&self) {
        if self.closed.load(Ordering::Acquire) && self.ops.load(Ordering::Acquire) == 0 {
            drop(self.owned.lock().unwrap().take());
        }
    }
}

/// `Transport` needs an `Arc` to issue with, so the connection holds this.
pub(crate) struct Link(pub(crate) Arc<PipeLink>);

impl Transport for Link {
    fn write(&self, frame: Vec<u8>) -> Result<(), Error> {
        let link = &self.0;
        let mut writes = link.writes.lock().unwrap();
        if link.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if writes.bytes + frame.len() > MAX_BACKLOG {
            return Err(Error::Backlog);
        }
        writes.bytes += frame.len();
        writes.queue.push_back(frame);
        if !writes.in_flight && !link.next_write(&mut writes) {
            drop(writes);
            link.end();
            return Err(Error::Closed);
        }
        Ok(())
    }

    fn resume(&self) {
        self.0.resume_read(Vec::new());
    }

    fn shut(&self) {
        self.0.stop();
    }

    fn flushed(&self) -> bool {
        let writes = self.0.writes.lock().unwrap();
        !writes.in_flight && writes.queue.is_empty()
    }
}
