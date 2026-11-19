// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The loopback: real encoding and decoding in one process, deterministically.
//!
//! Two forms, for the two kinds of seam:
//!
//! - [`pair`]: a [`crate::Connection`] pair with no pipe under it, for code written against
//!   connections (M2's services). Frames, hello, calls and sections all run.
//! - [`Sender`] and [`channel`]: `std::sync::mpsc`, except that with `SNOWLARK_IPC=loopback` every
//!   value goes through the wire format inside `send`, on the sender's thread, so order and timing
//!   stay what they are. For the channels that a process boundary will replace (tab commands and
//!   events), which can then be exercised long before there is a second process. Off, it costs a
//!   branch.
//!
//! What this does not cover: a value's type crossing is checked; timing, ordering between pipes,
//! and validation are not. `docs/process-isolation.md` section 12 lists the rest.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SendError};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::connection::{BrowserEnd, ChildEnd, Connection, Core, Flow, Transport};
use crate::error::Error;
use crate::frame::{self, FrameReader, Kind};
use crate::untrusted::{ProcessKind, ProcessPolicy};
use crate::win::build_id;
use crate::win::handle::Peer;
use crate::wire::{self, Wire};

/// Whether `SNOWLARK_IPC=loopback` is set (read once).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("SNOWLARK_IPC").is_ok_and(|v| v == "loopback"))
}

static LARGEST: AtomicU64 = AtomicU64::new(0);
static CROSSED: AtomicU64 = AtomicU64::new(0);

/// How many messages [`through`] (and so [`Sender::send`]) has put through the wire format.
pub fn crossed() -> u64 {
    CROSSED.load(Ordering::Relaxed)
}

/// The largest message that has gone through [`pass`] so far, in bytes.
pub fn largest() -> u64 {
    LARGEST.load(Ordering::Relaxed)
}

/// `value` as the far end of a pipe would get it: framed, cut out of the stream and decoded.
pub fn pass<T: Serialize + DeserializeOwned>(value: &T) -> Result<T, Error> {
    let bytes = wire::encode(value)?;
    LARGEST.fetch_max(bytes.len() as u64, Ordering::Relaxed);
    let mut reader = FrameReader::new();
    reader.push(&frame::encode(Kind::Message, 0, &bytes)?);
    let frame = reader.next_frame()?.ok_or(Error::BadFrame("no frame"))?;
    wire::decode(&frame.payload)
}

/// `std::sync::mpsc::Sender`, through the wire format when the loopback is on. One made by
/// [`Sender::to`] hands each value to a function instead.
pub struct Sender<T> {
    inner: Inner<T>,
}

enum Inner<T> {
    Channel(mpsc::Sender<T>),
    Call(Arc<dyn Fn(T) -> Result<(), T> + Send + Sync>),
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender {
            inner: match &self.inner {
                Inner::Channel(tx) => Inner::Channel(tx.clone()),
                Inner::Call(f) => Inner::Call(f.clone()),
            },
        }
    }
}

impl<T> Sender<T> {
    /// A sender that hands each value to `send` on the sending thread, with no thread or queue
    /// between: one that gives the value back is a closed channel's end.
    pub fn to(send: impl Fn(T) -> Result<(), T> + Send + Sync + 'static) -> Sender<T> {
        Sender {
            inner: Inner::Call(Arc::new(send)),
        }
    }
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sender")
    }
}

/// `value` through the wire format when the loopback is on, and as it is otherwise: for a message
/// that travels inside something that cannot cross as a whole.
pub fn through<T: Serialize + DeserializeOwned>(value: T) -> T {
    if !enabled() {
        return value;
    }
    CROSSED.fetch_add(1, Ordering::Relaxed);
    match pass(&value) {
        Ok(back) => back,
        // (a message that could not cross is the finding this mode exists for; the program goes on
        // with the value it has, so one run reports them all)
        Err(e) => {
            eprintln!(
                "[ipc] loopback: {} does not cross: {e}",
                std::any::type_name::<T>()
            );
            value
        }
    }
}

impl<T: Serialize + DeserializeOwned> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        match &self.inner {
            Inner::Channel(tx) => tx.send(through(value)),
            Inner::Call(f) => f(through(value)).map_err(SendError),
        }
    }
}

impl<T> From<mpsc::Sender<T>> for Sender<T> {
    fn from(inner: mpsc::Sender<T>) -> Sender<T> {
        Sender {
            inner: Inner::Channel(inner),
        }
    }
}

/// `std::sync::mpsc::channel`, with a [`Sender`].
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::channel();
    (tx.into(), rx)
}

/// One direction of a [`pair`]: hands frames to the peer's core on the sender's thread.
struct Direct {
    peer: Weak<Core>,
    reader: Mutex<FrameReader>,
}

impl Transport for Direct {
    fn write(&self, bytes: Vec<u8>) -> Result<(), Error> {
        let peer = self.peer.upgrade().ok_or(Error::Closed)?;
        let mut reader = self.reader.lock().unwrap();
        reader.push(&bytes);
        while let Some(frame) = reader.next_frame()? {
            // (there is no pipe to stop reading: the queue is the sender's own memory here)
            if peer.deliver(frame) == Flow::Stop {
                return Err(Error::Closed);
            }
        }
        Ok(())
    }

    fn resume(&self) {}

    fn shut(&self) {
        if let Some(peer) = self.peer.upgrade() {
            peer.closed();
        }
    }

    fn flushed(&self) -> bool {
        true
    }
}

/// A browser's end and a child's.
pub type Ends<Down, Up> = (BrowserEnd<Down, Up>, ChildEnd<Up, Down>);

/// Two connected ends in this process.
pub fn pair<Down: Wire, Up: Wire>(quota: u64) -> Result<Ends<Down, Up>, Error> {
    let build = build_id();
    let browser = Core::new(true, build, Peer::Local, quota);
    let child = Core::new(false, build, Peer::Local, 0);
    let direct = |peer: &Arc<Core>| {
        Arc::new(Direct {
            peer: Arc::downgrade(peer),
            reader: Mutex::new(FrameReader::new()),
        })
    };
    browser.attach(direct(&child));
    child.attach(direct(&browser));
    browser.say_hello()?;
    child.say_hello()?;
    let policy = ProcessPolicy {
        process: 0,
        kind: ProcessKind::Renderer,
        lock: Default::default(),
    };
    Ok((
        Connection::new(browser, Some(policy)),
        Connection::new(child, None),
    ))
}
