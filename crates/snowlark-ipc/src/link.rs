// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A link: a pipe between two children, made by the browser, which hands one end to each
//! (`docs/process-isolation.md` M7 stage 4). Neither child holds a handle to the other's process, so
//! nothing that names a handle or a section crosses a link: such things still go through the browser.
//! Bytes too large for a frame go in the link's memory instead, a ring for each direction that the
//! browser makes with the pipe and maps into both children.
//!
//! A ring is a section with two cells and the bytes. The writer copies a message's bytes in at its
//! write position and raises `written`; the reader copies them out, once, at its own read position,
//! and raises `read`. Each side's cell is its word: a reader takes nothing past `written` and never
//! more than the ring holds, a writer never more than `read` leaves free, and a cell that breaks those
//! rules ends the link. A message larger than the free space is sent before its bytes are all in, and
//! the rest follows as the reader makes room.
//!
//! The ends: [`HostEnd`] hears from a less privileged process ([`FromClient`]: its values arrive as
//! [`Untrusted`]), [`ClientEnd`] from a more privileged one ([`FromHost`]). Either may break a rule;
//! the link then ends, and whoever owns the end tells the browser, which decides who dies.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::bulk::{Bulk, INLINE_MAX};
use crate::connection::{Connection, Core};
use crate::error::Error;
use crate::section::{Section, MAX_SECTION};
use crate::untrusted::{FromChild, ProcessPolicy, Trust, Untrusted, Validate};
use crate::win::handle::{Peer, RemoteHandle};
use crate::win::io::{Link as PipeTransport, PipeLink};
use crate::win::{build_id, pipe};
use crate::wire::Wire;

/// Where a ring's `written` cell is.
const WRITTEN: usize = 0;
/// Where its `read` cell is (a cache line away from the writer's).
const READ: usize = 64;
/// Where its bytes start.
pub const RING_HEADER: usize = 128;

/// How long a side spins before it sleeps while it waits on the other.
const SPINS: u32 = 64;

/// What a child needs to join a link: its end of the pipe and the link's rings, as values in its
/// own handle table. The browser sends it inside a message of its own.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkEnd {
    pipe: RemoteHandle,
    /// The ring this end writes.
    out: Option<RingHandle>,
    /// The ring this end reads.
    into: Option<RingHandle>,
}

impl LinkEnd {
    /// Closes this end's handles without joining the link: the other end's join or reads then end.
    /// Only with a [`LinkEnd`] from the browser, as [`connect_link`].
    pub fn close(self) {
        drop(self.pipe.take());
        for ring in [self.out, self.into].into_iter().flatten() {
            drop(ring.handle.take());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RingHandle {
    handle: RemoteHandle,
    size: u64,
}

/// Hears from a less privileged process: what comes is [`Untrusted`].
pub struct FromClient;

impl Trust for FromClient {
    type Wrap<T> = Untrusted<T>;
    fn wrap<T>(value: T) -> Untrusted<T> {
        Untrusted::new(value)
    }
    const BROWSER: bool = false;
}

/// Hears from a more privileged process, which this one trusts.
pub struct FromHost;

impl Trust for FromHost {
    type Wrap<T> = T;
    fn wrap<T>(value: T) -> T {
        value
    }
    const BROWSER: bool = false;
}

/// The end of a link that serves the other (the GPU process's end of a renderer's link).
pub type HostEnd<Out, In> = Connection<Out, In, FromClient>;
/// The end of a link that is served (a renderer's end of its link to the GPU process).
pub type ClientEnd<Out, In> = Connection<Out, In, FromHost>;

/// A link's memory as one end has it.
pub(crate) struct LinkMemory {
    out: Option<Mutex<Writer>>,
    into: Option<Mutex<Reader>>,
}

struct Ring {
    section: Section,
    /// The bytes it holds.
    cap: u64,
}

impl Ring {
    fn map(handle: RingHandle) -> Result<Ring, Error> {
        if handle.size <= RING_HEADER as u64 || handle.size > MAX_SECTION {
            return Err(Error::Refused);
        }
        Ok(Ring {
            section: Section::from_remote(handle.handle, handle.size)?,
            cap: handle.size - RING_HEADER as u64,
        })
    }

    /// The bytes at stream position `at`, wrapping.
    fn read(&self, at: u64, len: usize, out: &mut Vec<u8>) -> bool {
        let start = (at % self.cap) as usize;
        let first = len.min(self.cap as usize - start);
        let head = self.section.read_once(RING_HEADER + start, first);
        let tail = self.section.read_once(RING_HEADER, len - first);
        match (head, tail) {
            (Some(head), Some(tail)) => {
                out.extend_from_slice(&head);
                out.extend_from_slice(&tail);
                true
            }
            _ => false,
        }
    }

    fn write(&self, at: u64, bytes: &[u8]) -> bool {
        let start = (at % self.cap) as usize;
        let first = bytes.len().min(self.cap as usize - start);
        self.section.write(RING_HEADER + start, &bytes[..first])
            && self.section.write(RING_HEADER, &bytes[first..])
    }

    fn cell(&self, offset: usize) -> u64 {
        self.section.load_u64(offset).unwrap_or(0)
    }
}

struct Writer {
    ring: Ring,
    /// How far this end has written: what its `written` cell says.
    written: u64,
}

struct Reader {
    ring: Ring,
    /// How far this end has read: what its `read` cell says.
    read: u64,
}

/// Waits a little longer each time it is called: spins first, then sleeps.
struct Backoff {
    spins: u32,
    end: Instant,
}

impl Backoff {
    fn new(timeout: Duration) -> Backoff {
        Backoff {
            spins: 0,
            end: Instant::now() + timeout,
        }
    }

    fn wait(&mut self) -> Result<(), Error> {
        if Instant::now() >= self.end {
            return Err(Error::Timeout);
        }
        self.spins += 1;
        if self.spins < SPINS {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
}

impl<O: Wire, I: Wire> Connection<O, I, FromChild> {
    /// Makes a link between this child and `other`: a pipe, and a ring for each direction of
    /// `rings[0]` bytes (this child to `other`) and `rings[1]` (`other` to this child), none for 0.
    /// Returns what each child needs to join it ([`connect_link`]), this child's first; each goes to
    /// its child in a message of the caller's. The browser keeps nothing of the link.
    pub fn link<O2: Wire, I2: Wire>(
        &self,
        other: &Connection<O2, I2, FromChild>,
        rings: [u64; 2],
    ) -> Result<[LinkEnd; 2], Error> {
        let (server, client) = pipe::pair()?;
        let mut made = [None, None];
        for (slot, &cap) in made.iter_mut().zip(&rings) {
            if cap == 0 {
                continue;
            }
            let size = cap.checked_add(RING_HEADER as u64).ok_or(Error::Refused)?;
            if size > MAX_SECTION {
                return Err(Error::Refused);
            }
            *slot = Some(Section::create(size as usize)?);
        }
        let give = |peer: &Peer, section: &Option<Section>| -> Result<_, Error> {
            section
                .as_ref()
                .map(|s| {
                    Ok(RingHandle {
                        handle: RemoteHandle::from_value(peer.duplicate_section(s)?),
                        size: s.size() as u64,
                    })
                })
                .transpose()
        };
        let mine = &self.core.peer;
        let theirs = &other.core.peer;
        // (the browser's own handles close as `made` and the pipe's ends drop)
        Ok([
            LinkEnd {
                pipe: mine.duplicate(&server)?,
                out: give(mine, &made[0])?,
                into: give(mine, &made[1])?,
            },
            LinkEnd {
                pipe: theirs.duplicate(&client)?,
                out: give(theirs, &made[1])?,
                into: give(theirs, &made[0])?,
            },
        ])
    }
}

/// Joins a link with the end the browser gave this process ([`Connection::link`]): says hello on it
/// and waits for the other end's, at most `timeout`. `Tr` is [`FromClient`] at the end that serves,
/// [`FromHost`] at the end that is served. Call it only with a [`LinkEnd`] from the browser: its
/// values name handles in this process's table, which this takes.
pub fn connect_link<Out: Wire, In: Wire, Tr: LinkTrust>(
    end: LinkEnd,
    timeout: Duration,
) -> Result<Connection<Out, In, Tr>, Error> {
    // (each handle is taken before anything can fail, so none is left open)
    let pipe = end.pipe.take();
    let out = end.out.map(Ring::map);
    let into = end.into.map(Ring::map);
    let memory = LinkMemory {
        out: out
            .transpose()?
            .map(|ring| Mutex::new(Writer { ring, written: 0 })),
        into: into
            .transpose()?
            .map(|ring| Mutex::new(Reader { ring, read: 0 })),
    };
    let core = Core::linked(build_id(), memory);
    let link = PipeLink::new(pipe, &core)?;
    core.attach(Arc::new(PipeTransport(link.clone())));
    link.start();
    core.say_hello()?;
    Ok(Connection::new(core, None).established(timeout)?.0)
}

/// The two kinds of link end.
pub trait LinkTrust: Trust + sealed::Sealed {}
impl LinkTrust for FromClient {}
impl LinkTrust for FromHost {}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::FromClient {}
    impl Sealed for super::FromHost {}
}

impl<Out: Wire, In: Wire, Tr: LinkTrust> Connection<Out, In, Tr> {
    /// Sends the message `make` builds around `bytes`: in the message if they are small, else in
    /// this end's ring. Bytes the ring has no room for are written after the message, as the other
    /// end makes room, waiting at most `timeout` each time. Messages sent so keep their order.
    pub fn send_bulk(
        &self,
        bytes: &[u8],
        make: impl FnOnce(Bulk) -> Out,
        timeout: Duration,
    ) -> Result<(), Error> {
        if bytes.len() <= INLINE_MAX {
            return self.send(&make(Bulk::Inline(bytes.to_vec())));
        }
        let memory = self.core.link.as_ref().ok_or(Error::Refused)?;
        let mut w = memory
            .out
            .as_ref()
            .ok_or(Error::Refused)?
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pos = w.written;
        let mut done = self.fill(&mut w, bytes)?;
        self.send(&make(Bulk::Ring {
            pos,
            len: bytes.len() as u64,
        }))?;
        let mut backoff = Backoff::new(timeout);
        while done < bytes.len() {
            let more = self.fill(&mut w, &bytes[done..])?;
            if more == 0 {
                if self.core.is_closed() {
                    return Err(Error::Closed);
                }
                backoff.wait()?;
            } else {
                done += more;
                backoff = Backoff::new(timeout);
            }
        }
        Ok(())
    }

    /// Writes what the ring has room for of `bytes`: how much.
    fn fill(&self, w: &mut Writer, bytes: &[u8]) -> Result<usize, Error> {
        let read = w.ring.cell(READ);
        let used = w.written.checked_sub(read).filter(|&u| u <= w.ring.cap);
        let Some(used) = used else {
            return Err(self.broken(format!("a ring read to {read}, written to {}", w.written)));
        };
        let n = bytes.len().min((w.ring.cap - used) as usize);
        if n == 0 {
            return Ok(0);
        }
        if !w.ring.write(w.written, &bytes[..n]) {
            return Err(Error::Refused);
        }
        w.written += n as u64;
        w.ring.section.store_u64(WRITTEN, w.written);
        Ok(n)
    }

    /// The bytes of a bulk from the other end, copied once. Open each message's bulk in the order
    /// the messages came: one never opened is skipped. A bulk that breaks a rule ends the link: an
    /// inline one over [`INLINE_MAX`], a section or a handle (only the browser gives those), ring
    /// bytes behind what was read, or a `written` cell that runs back or past the ring. Bytes that
    /// do not come within `timeout` of the last end it too.
    pub fn open_bulk(&self, bulk: Bulk, timeout: Duration) -> Result<Vec<u8>, Error> {
        let (pos, len) = match bulk {
            Bulk::Inline(bytes) if bytes.len() <= INLINE_MAX => return Ok(bytes),
            Bulk::Inline(bytes) => return Err(self.broken(format!("{} bytes inline", bytes.len()))),
            Bulk::Shared { .. } | Bulk::Given { .. } => {
                return Err(self.broken("a section on a link".into()))
            }
            Bulk::Ring { pos, len } => (pos, len),
        };
        let reader = self.core.link.as_ref().and_then(|m| m.into.as_ref());
        let Some(reader) = reader else {
            return Err(self.broken("ring bytes on a link with no ring this way".into()));
        };
        let mut r = reader.lock().unwrap_or_else(|e| e.into_inner());
        if pos < r.read {
            return Err(self.broken(format!("ring bytes at {pos}, read to {}", r.read)));
        }
        let end = pos.checked_add(len);
        if end.is_none_or(|end| end > u64::MAX / 2) {
            return Err(self.broken(format!("ring bytes at {pos}, {len} of them")));
        }
        // (memory grows with the bytes that come, not with what the writer says will)
        let mut out = Vec::with_capacity(len.min(r.ring.cap) as usize);
        let mut backoff = Backoff::new(timeout);
        loop {
            let written = r.ring.cell(WRITTEN);
            let held = written.checked_sub(r.read).filter(|&h| h <= r.ring.cap);
            let Some(held) = held else {
                return Err(self.broken(format!("a ring written to {written}, read to {}", r.read)));
            };
            let at = r.read;
            let skip = pos.saturating_sub(at).min(held);
            let wanted = len - out.len() as u64;
            let take = (held - skip).min(wanted);
            if skip + take > 0 {
                if take > 0 && !r.ring.read(at + skip, take as usize, &mut out) {
                    return Err(Error::Refused);
                }
                r.read = at + skip + take;
                r.ring.section.store_u64(READ, r.read);
                backoff = Backoff::new(timeout);
            }
            if r.read >= pos && out.len() as u64 == len {
                return Ok(out);
            }
            if skip + take == 0 {
                if self.core.is_closed() {
                    return Err(Error::Closed);
                }
                if backoff.wait().is_err() {
                    return Err(self.broken(format!("ring bytes at {pos} never came")));
                }
            }
        }
    }

    /// The other end broke a rule: the link ends.
    fn broken(&self, why: String) -> Error {
        self.core.bad_message(&why);
        Error::BadMessage(why)
    }
}

impl<Out: Wire, In: Wire> Connection<Out, In, FromClient> {
    /// Validates a value from the other end, as `policy` says it may send. On failure the link
    /// ends, and the caller tells the browser.
    pub fn check<T: Validate>(
        &self,
        value: Untrusted<T>,
        policy: &ProcessPolicy,
    ) -> Result<T, Error> {
        value.validate(policy).map_err(|bad| self.broken(bad.0))
    }

    /// The other end broke a rule the caller found: the link ends.
    pub fn bad_message(&self, reason: &str) {
        self.core.bad_message(reason);
    }
}
