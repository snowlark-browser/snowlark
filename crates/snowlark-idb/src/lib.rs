// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Indexed Database API 3.0 (<https://w3c.github.io/IndexedDB/>) storage for Snowlark, free of V8 and
//! the DOM: what the spec runs "in parallel".
//!
//! - [`key`]: keys and their order, key ranges, key paths.
//! - [`db`]: a database's object stores, indexes and records; changes and their undo entries; the
//!   reads and cursor steps of §6.
//! - [`backend`]: the backend a profile's event loops share: connection queues, `versionchange` and
//!   `blocked`, transaction scheduling, commits.
//! - [`request`]: a transaction's reads and writes as messages, answered through the loop's inbox.
//! - [`service`]: the backend as an event loop sees it ([`IdbService`]), and the [`Backend`] handle.
//! - [`wire`]: how keys, values and changes cross to another process, and what is checked there.
//! - [`disk`]: the durable form (one append-only log file per database) and the thread that writes it.
//!
//! The JavaScript side (the IDB* interfaces, key conversion, structured clone of values, events)
//! is `snowlark-page`'s.

pub mod backend;
pub mod db;
pub mod disk;
pub mod key;
pub mod request;
pub mod service;
pub mod wire;

pub use backend::{
    lock, quota_for_volume, DbRef, Durability, Failure, Inbox, LocalIdb, LoopId, Mode, Msg,
    Refused, WriteError, QUOTA,
};
pub use db::{Change, Database, Direction, Found, Index, Record, Seek, Source, Store, Undo, Value};
pub use key::{compare, Key, KeyPath, KeyRange};
pub use request::{Answer, Fill, Request};
pub use service::{Backend, IdbService};
