// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The IO thread: one epoll instance per process, level-triggered, every socket non-blocking.
//!
//! A write goes out on the caller's thread when nothing is queued before it; what the socket has no
//! room for waits in the link's queue, and the IO thread sends it when epoll says the socket is
//! writable. Reads happen on the IO thread only. A link whose connection's queue is full is taken
//! off epoll's read interest, so the peer blocks on its own socket (section 7's back-pressure), and
//! put back when the queue has room: level-triggered, epoll then reports what waited.
//!
//! Descriptors travel as ancillary data (`SCM_RIGHTS`) on the first byte of their
//! [`Kind::Handle`] frame, so one always arrives no later than its frame: received descriptors wait
//! in order, and each handle frame takes the oldest.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::connection::{Core, Flow, Transport};
use crate::error::Error;
use crate::frame::{FrameReader, Kind};

const READ_BUFFER: usize = 64 << 10;
/// Bytes queued for a peer that is not reading, past which `send` fails.
const MAX_BACKLOG: usize = 64 << 20;
/// Descriptors one read may bring: more is the peer breaking the rules.
const FDS_PER_READ: usize = 16;
/// Descriptors received ahead of their handle frames, past which the peer breaks the rules.
const MAX_PENDING_FDS: usize = 64;

struct Io {
    epoll: OwnedFd,
    links: Mutex<HashMap<u64, Arc<PipeLink>>>,
    next: AtomicU64,
}

fn io() -> Result<&'static Io, Error> {
    static IO: OnceLock<Io> = OnceLock::new();
    if let Some(io) = IO.get() {
        return Ok(io);
    }
    // SAFETY: makes a new epoll instance; the descriptor is owned at once.
    let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if fd < 0 {
        return Err(Error::Os(std::io::Error::last_os_error()));
    }
    // SAFETY: a new descriptor of ours.
    let epoll = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut created = false;
    let io = IO.get_or_init(|| {
        created = true;
        Io {
            epoll,
            links: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    });
    if created {
        std::thread::Builder::new()
            .name("snowlark ipc".into())
            .spawn(move || run(io))
            .map_err(|_| Error::Closed)?;
    }
    Ok(io)
}

fn run(io: &'static Io) {
    // SAFETY: epoll_event is plain data; all zeroes is a valid value.
    let mut events = [unsafe { std::mem::zeroed::<libc::epoll_event>() }; 64];
    loop {
        // SAFETY: the buffer holds `events.len()` entries, which the call fills.
        let n = unsafe {
            libc::epoll_wait(
                io.epoll.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                -1,
            )
        };
        if n < 0 {
            // (EINTR; nothing else can happen to a valid epoll descriptor and buffer)
            continue;
        }
        for event in &events[..n as usize] {
            let (bits, key) = (event.events, event.u64);
            let Some(link) = io.links.lock().unwrap().get(&key).cloned() else {
                continue;
            };
            let hung = bits & (libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0;
            if bits & libc::EPOLLIN as u32 != 0 || (hung && !link.paused.load(Ordering::Acquire)) {
                link.read_ready();
            }
            if bits & libc::EPOLLOUT as u32 != 0 || hung {
                link.write_ready();
            }
        }
    }
}

struct ReadSide {
    frames: FrameReader,
    /// Descriptors that came ahead of their handle frames.
    fds: VecDeque<OwnedFd>,
    buffer: Vec<u8>,
}

struct Out {
    bytes: Vec<u8>,
    /// How much of `bytes` has gone.
    done: usize,
    /// Goes with the first byte that is sent.
    fd: Option<OwnedFd>,
}

#[derive(Default)]
struct Writes {
    queue: VecDeque<Out>,
    bytes: usize,
}

/// One socket on the IO thread.
pub(crate) struct PipeLink {
    fd: OwnedFd,
    key: u64,
    core: Weak<Core>,
    /// The connection's queue is full: not reading.
    paused: AtomicBool,
    closed: AtomicBool,
    /// Whether anything waits in `writes` (kept beside it, so interest is set without its lock).
    pending: AtomicBool,
    reader: Mutex<ReadSide>,
    writes: Mutex<Writes>,
    /// The events epoll watches for this socket now, if it is on epoll.
    interest: Mutex<Option<u32>>,
}

impl PipeLink {
    /// Takes `fd` for the IO thread. Reading starts with [`PipeLink::start`].
    pub(crate) fn new(fd: OwnedFd, core: &Arc<Core>) -> Result<Arc<PipeLink>, Error> {
        let io = io()?;
        set_nonblocking(fd.as_raw_fd())?;
        let key = io.next.fetch_add(1, Ordering::Relaxed);
        let link = Arc::new(PipeLink {
            fd,
            key,
            core: Arc::downgrade(core),
            paused: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            reader: Mutex::new(ReadSide {
                frames: FrameReader::new(),
                fds: VecDeque::new(),
                buffer: vec![0; READ_BUFFER],
            }),
            writes: Mutex::new(Writes::default()),
            interest: Mutex::new(None),
        });
        io.links.lock().unwrap().insert(key, link.clone());
        Ok(link)
    }

    pub(crate) fn start(&self) {
        self.update();
    }

    /// Makes epoll watch what this socket needs now: reads unless paused, writes while any wait.
    fn update(&self) {
        let Ok(io) = io() else { return };
        let mut interest = self.interest.lock().unwrap();
        let closed = self.closed.load(Ordering::Acquire);
        let mut want = 0u32;
        if !closed && !self.paused.load(Ordering::Acquire) {
            want |= libc::EPOLLIN as u32;
        }
        if !closed && self.pending.load(Ordering::Acquire) {
            want |= libc::EPOLLOUT as u32;
        }
        if *interest == Some(want) || (interest.is_none() && want == 0) {
            return;
        }
        let op = match (*interest, want) {
            (Some(_), 0) => libc::EPOLL_CTL_DEL,
            (Some(_), _) => libc::EPOLL_CTL_MOD,
            (None, _) => libc::EPOLL_CTL_ADD,
        };
        let mut event = libc::epoll_event {
            events: want,
            u64: self.key,
        };
        // SAFETY: the socket is open (it closes only when the link is dropped, after `stop` took it
        // off epoll), and `event` is a local the call reads.
        let r =
            unsafe { libc::epoll_ctl(io.epoll.as_raw_fd(), op, self.fd.as_raw_fd(), &mut event) };
        if r == 0 {
            *interest = (want != 0).then_some(want);
        }
    }

    fn read_ready(self: &Arc<Self>) {
        if self.paused.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
            return;
        }
        let Some(core) = self.core.upgrade() else {
            self.end();
            return;
        };
        let mut pause = false;
        {
            let mut r = self.reader.lock().unwrap();
            let ReadSide {
                frames,
                fds,
                buffer,
            } = &mut *r;
            let mut received = Vec::new();
            let n = match recv(self.fd.as_raw_fd(), buffer, &mut received) {
                Ok((0, _)) => {
                    drop(r);
                    self.end();
                    return;
                }
                Ok((n, truncated)) => {
                    if truncated {
                        drop(r);
                        core.bad_message("more descriptors than one read takes");
                        return;
                    }
                    n
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return
                }
                Err(_) => {
                    drop(r);
                    self.end();
                    return;
                }
            };
            fds.extend(received);
            if fds.len() > MAX_PENDING_FDS {
                drop(r);
                core.bad_message("descriptors with no handle frames");
                return;
            }
            frames.push(&buffer[..n]);
            loop {
                let flow = match frames.next_frame() {
                    Ok(Some(frame)) if frame.kind == Kind::Handle => {
                        let Some(fd) = fds.pop_front() else {
                            drop(r);
                            core.bad_message("a handle frame with no descriptor");
                            return;
                        };
                        if !frame.payload.is_empty() {
                            drop(r);
                            core.bad_message("a handle frame with bytes");
                            return;
                        }
                        core.deliver_handle(frame.id, fd)
                    }
                    Ok(Some(frame)) => core.deliver(frame),
                    Ok(None) => break,
                    Err(e) => {
                        drop(r);
                        core.bad_message(&e.to_string());
                        return;
                    }
                };
                match flow {
                    Flow::Go => {}
                    // (what is already here still goes in: at most one read's worth)
                    Flow::Pause => pause = true,
                    Flow::Stop => return,
                }
            }
        }
        if pause {
            self.paused.store(true, Ordering::Release);
            self.update();
            // A receiver may have made room since: `is_paused` is read after `paused` was set, so
            // either it sees that, or the receiver's `resume` comes after and clears it itself.
            if !core.is_paused() {
                self.paused.store(false, Ordering::Release);
                self.update();
            }
        }
    }

    fn write_ready(self: &Arc<Self>) {
        let mut w = self.writes.lock().unwrap();
        let ok = self.flush(&mut w);
        self.pending.store(!w.queue.is_empty(), Ordering::Release);
        drop(w);
        if ok {
            self.update();
        } else {
            self.end();
        }
    }

    /// Sends what the socket takes of the queue. False when the stream is broken.
    fn flush(&self, w: &mut Writes) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return true;
        }
        while let Some(out) = w.queue.front_mut() {
            match send(
                self.fd.as_raw_fd(),
                &out.bytes[out.done..],
                out.fd.as_ref().map(|f| f.as_fd()),
            ) {
                Ok(n) => {
                    out.fd = None;
                    out.done += n;
                    if out.done == out.bytes.len() {
                        w.bytes -= out.bytes.len();
                        w.queue.pop_front();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
        true
    }

    fn queue(self: &Arc<Self>, bytes: Vec<u8>, fd: Option<OwnedFd>) -> Result<(), Error> {
        let mut w = self.writes.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if w.bytes + bytes.len() > MAX_BACKLOG {
            return Err(Error::Backlog);
        }
        w.bytes += bytes.len();
        w.queue.push_back(Out { bytes, done: 0, fd });
        let ok = w.queue.len() > 1 || self.flush(&mut w);
        self.pending.store(!w.queue.is_empty(), Ordering::Release);
        drop(w);
        if !ok {
            self.end();
            return Err(Error::Closed);
        }
        self.update();
        Ok(())
    }

    /// The stream is over: everything stops, and the connection hears it.
    fn end(&self) {
        self.stop();
        if let Some(core) = self.core.upgrade() {
            core.closed();
        }
    }

    fn stop(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut w = self.writes.lock().unwrap();
            w.queue.clear();
            w.bytes = 0;
        }
        self.pending.store(false, Ordering::Release);
        // SAFETY: an open socket of ours. The peer's reads end; so do ours.
        unsafe { libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_RDWR) };
        self.update();
        if let Ok(io) = io() {
            io.links.lock().unwrap().remove(&self.key);
        }
    }
}

fn set_nonblocking(fd: RawFd) -> Result<(), Error> {
    // SAFETY: reads and sets the status flags of an open descriptor of ours.
    let r = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            -1
        } else {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK)
        }
    };
    if r < 0 {
        return Err(Error::Os(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Room for one `SCM_RIGHTS` message of [`FDS_PER_READ`] descriptors.
const CONTROL_WORDS: usize = 16;

/// One read: the bytes into `buffer`, the descriptors that came with them into `fds` (closing on
/// exec). Returns the count and whether descriptors were cut off for want of room.
fn recv(fd: RawFd, buffer: &mut [u8], fds: &mut Vec<OwnedFd>) -> std::io::Result<(usize, bool)> {
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    let mut control = [0u64; CONTROL_WORDS];
    // SAFETY: msghdr is plain data; all zeroes is a valid empty header.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    // SAFETY: a pure size computation.
    let space = unsafe { libc::CMSG_SPACE((FDS_PER_READ * size_of::<libc::c_int>()) as u32) };
    debug_assert!(space as usize <= size_of_val(&control));
    msg.msg_controllen = space as _;
    // SAFETY: `msg` points at the buffer and the control space above, both live for the call.
    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: walks the control messages the kernel wrote into `control`, within the length it
    // set; each SCM_RIGHTS payload is an array of descriptors now open in this process, which are
    // owned at once, so none leaks.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const libc::c_int;
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / size_of::<libc::c_int>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok((n as usize, msg.msg_flags & libc::MSG_CTRUNC != 0))
}

/// One write of `bytes`, with `with` as `SCM_RIGHTS` on its first byte. Never raises SIGPIPE.
fn send(fd: RawFd, bytes: &[u8], with: Option<BorrowedFd<'_>>) -> std::io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    let mut control = [0u64; 4];
    // SAFETY: msghdr is plain data; all zeroes is a valid empty header.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(with) = with {
        // SAFETY: the control space holds one header and one descriptor (24 of its 32 bytes, 8-byte
        // aligned); the header is written through the pointer the macros give into it.
        unsafe {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as _;
            (libc::CMSG_DATA(c) as *mut libc::c_int).write_unaligned(with.as_raw_fd());
        }
    }
    // SAFETY: `msg` points at the bytes and the control space, both live for the call; the kernel
    // only reads them.
    let n = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// `Transport` for a socket.
pub(crate) struct Link(pub(crate) Arc<PipeLink>);

impl Transport for Link {
    fn write(&self, frame: Vec<u8>) -> Result<(), Error> {
        self.0.queue(frame, None)
    }

    fn write_handle(&self, frame: Vec<u8>, fd: OwnedFd) -> Result<(), Error> {
        self.0.queue(frame, Some(fd))
    }

    fn resume(&self) {
        self.0.paused.store(false, Ordering::Release);
        self.0.update();
    }

    fn shut(&self) {
        self.0.stop();
    }

    fn flushed(&self) -> bool {
        self.0.writes.lock().unwrap().queue.is_empty()
    }
}
