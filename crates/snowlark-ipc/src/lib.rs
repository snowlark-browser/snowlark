// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snowlark's process boundary (`docs/process-isolation.md` section 7): a duplex channel per child
//! (an overlapped pipe on Windows, a Unix stream socket on Linux), shared sections the browser
//! creates, handles only the browser passes, framing, typed connections with `send` and `call`,
//! process launch with an explicit handle list and limits, a build id check at connect, and a
//! loopback that runs all of it in one process.
//!
//! The rules it enforces:
//!
//! - Decoding is total, and everything from a child arrives as [`Untrusted`].
//! - A child that breaks a rule dies ([`Connection::bad_message`]).
//! - A child's channel stops being read while its queue is full.
//! - A `call` fails at once when the peer dies, so nothing waits on a dead process.
//!
//! `unsafe` is in `win/`, `posix/` and `section.rs`, each block with its reason. The rest is safe
//! code, and `frame.rs`, `wire.rs` and `connection.rs`, which see hostile bytes, have none.

mod bulk;
mod connection;
mod error;
pub mod frame;
pub mod link;
pub mod loopback;
#[cfg(unix)]
pub mod posix;
mod section;
mod untrusted;
#[cfg(windows)]
pub mod win;
pub mod wire;

/// The operating system's half, by the same names on every platform.
#[cfg(unix)]
pub(crate) use posix as os;
#[cfg(windows)]
pub(crate) use win as os;

pub use bulk::{Bulk, INLINE_MAX};
pub use connection::{
    BrowserEnd, ChildEnd, Connection, Received, Replier, SharedSection, MAX_QUEUED,
    MAX_QUEUED_BYTES,
};
pub use error::Error;
pub use link::{connect_link, ClientEnd, FromClient, FromHost, HostEnd, LinkEnd, LinkTrust};
pub use loopback::{channel, Sender};
pub use os::build_id;
pub use os::crash_dump::{install_crash_handler, KEPT_DUMPS};
pub use os::handle::RemoteHandle;
pub use os::launch::{
    connect_as, connect_to_browser, own_usage, thread_cpu, JobLimits, Launch, Process, Spawned,
    Usage,
};
pub use section::{Section, MAX_SECTION};
pub use untrusted::{
    BadMessage, FromBrowser, FromChild, ProcessKind, ProcessPolicy, SiteLock, Trust, Untrusted,
    Validate,
};
pub use wire::{BuildId, Wire};
