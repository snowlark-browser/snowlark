// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Descriptors between processes. No process can put a descriptor in another's table here, so a
//! descriptor travels on the connection itself: a [`crate::frame::Kind::Handle`] frame whose bytes
//! carry a value the sender chose and whose ancillary data (`SCM_RIGHTS`) carries a copy of the
//! descriptor. The receiver keeps it under that value until a message names it. The frame goes
//! before any message that names it, on the same stream, so it is always there first.
//!
//! What a child receives (from the browser, the only process that gives it any) goes in this
//! process's table, as a handle duplicated into it would on Windows. What the browser receives from
//! a child stays with that child's connection, so a child can only ever name its own.

use std::collections::HashMap;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::connection::Core;
use crate::error::Error;
use crate::posix::launch::Process;

/// A descriptor on its way to another process: the value it is kept under there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteHandle(u64);

impl RemoteHandle {
    pub(crate) fn from_value(value: u64) -> RemoteHandle {
        RemoteHandle(value)
    }

    pub(crate) fn value(self) -> u64 {
        self.0
    }

    /// Owns the descriptor this value names in this process's table ([`crate::ChildEnd::take_handle`]):
    /// a value that names none fails.
    pub(crate) fn take_checked(self) -> Result<OwnedFd, Error> {
        table()
            .lock()
            .unwrap()
            .remove(&self.0)
            .ok_or(Error::Refused)
    }
}

/// This process's table of descriptors received from the browser (or given in the loopback).
fn table() -> &'static Mutex<HashMap<u64, OwnedFd>> {
    static TABLE: OnceLock<Mutex<HashMap<u64, OwnedFd>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

/// Keeps a descriptor that came from the browser under the value it chose.
pub(crate) fn keep(value: u64, fd: OwnedFd) {
    table().lock().unwrap().insert(value, fd);
}

/// How many descriptors wait in this process's table, not taken yet.
#[doc(hidden)]
pub fn waiting() -> usize {
    table().lock().unwrap().len()
}

/// Gives the peer of `core` a copy of `fd`: in this process's table for the loopback, else on the
/// connection. The value names it there.
pub(crate) fn give(core: &Core, fd: BorrowedFd<'_>) -> Result<RemoteHandle, Error> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let copy = fd.try_clone_to_owned().map_err(Error::Os)?;
    let value = NEXT.fetch_add(1, Ordering::Relaxed);
    match core.peer {
        Peer::Local => keep(value, copy),
        Peer::Child(_) | Peer::Browser => core.write_handle(value, copy)?,
        Peer::Link => return Err(Error::Refused),
    }
    Ok(RemoteHandle(value))
}

/// Who is at the other end of a connection.
#[derive(Clone)]
pub(crate) enum Peer {
    /// A child of this process (the browser's end).
    Child(Arc<Process>),
    /// The browser (a child's end).
    Browser,
    /// This same process (the loopback).
    Local,
    /// The other end of a link between two children.
    Link,
}

impl Peer {
    pub(crate) fn describe(&self) -> String {
        match self {
            Peer::Child(p) => format!("process {}", p.pid()),
            Peer::Browser => "the browser".into(),
            Peer::Local => "the loopback".into(),
            Peer::Link => "the linked process".into(),
        }
    }

    pub(crate) fn kill(&self) {
        if let Peer::Child(p) = self {
            p.kill();
        }
    }
}
