// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A transaction's record reads and writes as messages (IndexedDB §2.8, "asynchronously execute a
//! request"): an event loop sends a [`Request`] and goes on; the backend runs the requests of a
//! transaction in the order they were sent and posts each one's [`Answer`] to the loop
//! ([`Msg::Answer`]), where it arrives as a task. The loop fires the request's event from that task,
//! so it never waits for the backend, and a backend in another process costs no round trip per
//! request.

use serde::{Deserialize, Serialize};

use crate::backend::{LocalIdb, LoopId, Msg, Refused, WriteError};
use crate::db::{Change, Direction, Record, Seek, Source};
use crate::key::{Key, KeyRange};
use crate::wire::WireFound;

/// A read or write in a transaction. Each is the call of [`LocalIdb`] of the same name.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// [`LocalIdb::apply`]: `add` fails over a record with the same key.
    Apply {
        change: Change,
        add: bool,
    },
    First {
        source: Source,
        range: KeyRange,
    },
    GetAll {
        source: Source,
        range: KeyRange,
        direction: Direction,
        count: u32,
    },
    Count {
        source: Source,
        range: KeyRange,
    },
    CursorAdvance {
        source: Source,
        direction: Direction,
        range: KeyRange,
        seek: Seek,
        count: u32,
    },
    /// [`LocalIdb::put_generated`]: a record of a store with a key generator, given no key.
    Generate {
        store: u64,
        record: Record,
        fill: Vec<(u64, Fill)>,
        add: bool,
    },
}

/// An index key of a record whose key the backend generates, and which depends on that key: the
/// index's key path is the store's key path, so its key is the generated key; or it is a sequence
/// with the store's key path in it, so its key is an array whose `None` items are the generated key
/// (the value has no key at that path until the key is injected into it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Fill {
    Key,
    Array(Vec<Option<Key>>),
}

impl Fill {
    /// The index key, with `key` the generated key.
    pub fn with(self, key: &Key) -> Key {
        match self {
            Fill::Key => key.clone(),
            Fill::Array(items) => Key::Array(
                items
                    .into_iter()
                    .map(|k| k.unwrap_or_else(|| key.clone()))
                    .collect(),
            ),
        }
    }
}

/// What a [`Request`] answers, by kind.
#[derive(Debug, Serialize, Deserialize)]
pub enum Answer {
    Applied(Result<(), WriteError>),
    /// `First` and `CursorAdvance`.
    Found(Result<Option<WireFound>, Refused>),
    All(Result<Vec<WireFound>, Refused>),
    Count(Result<u64, Refused>),
    /// `Generate`: the key generated.
    Generated(Result<u64, WriteError>),
}

impl LocalIdb {
    /// Runs a request in transaction `txn` now.
    pub fn run(&self, txn: u64, request: Request) -> Answer {
        match request {
            Request::Apply { change, add } => Answer::Applied(self.apply(txn, change, add)),
            Request::First { source, range } => Answer::Found(
                self.first(txn, source, &range)
                    .map(|f| f.map(WireFound::from)),
            ),
            Request::GetAll {
                source,
                range,
                direction,
                count,
            } => Answer::All(
                self.get_all(txn, source, &range, direction, count)
                    .map(|v| v.into_iter().map(WireFound::from).collect()),
            ),
            Request::Count { source, range } => Answer::Count(self.count(txn, source, &range)),
            Request::CursorAdvance {
                source,
                direction,
                range,
                seek,
                count,
            } => Answer::Found(
                self.cursor_advance(txn, source, direction, &range, seek, count)
                    .map(|f| f.map(WireFound::from)),
            ),
            Request::Generate {
                store,
                record,
                fill,
                add,
            } => Answer::Generated(self.put_generated(txn, store, record, fill, add)),
        }
    }

    /// Runs a request in transaction `txn` and posts its answer to `loop_` as request `seq`'s.
    /// Requests run in the order of the calls, as a transaction's must (§2.8).
    pub fn request(&self, loop_: LoopId, txn: u64, seq: u64, request: Request) {
        let answer = self.run(txn, request);
        self.post(loop_, Msg::Answer { txn, seq, answer });
    }
}
