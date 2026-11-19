// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How the network's types cross a process boundary (`docs/process-isolation.md`, M4 step 2): the
//! types derive `serde`, and this module holds what plain data cannot carry by itself.
//!
//! - **Origins.** An opaque origin is an identity of this process, equal only to itself. Each end of
//!   a connection keeps an [`OriginTable`], and an opaque origin crosses as an id in it: the same
//!   origin always gets the same id, and the same id always comes back as the same origin, so a
//!   `blob:` URL a sandboxed document made is still its own the next time it asks. Encoding and
//!   decoding run inside [`with_origins`]; outside it an opaque origin does not cross.
//! - **Instants.** A point in time crosses as its age when sent, and is that long before the moment
//!   it arrives: every time in a message moves by the time the message took, which keeps their
//!   order and the distances between them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::{Host, Origin};

/// The most opaque origins one peer may name; past it, a message does not decode.
pub const MAX_OPAQUE: usize = 4096;

/// One end's opaque origins and their ids.
pub struct OriginTable {
    /// Ids this end makes are odd at one end and even at the other, so the two never collide.
    side: u64,
    next: u64,
    ids: HashMap<Origin, u64>,
    origins: HashMap<u64, Origin>,
    /// Origins made for ids the peer named.
    named: usize,
}

impl OriginTable {
    /// The table of a renderer's end.
    pub fn renderer() -> OriginTable {
        OriginTable::new(1)
    }

    /// The table of the browser's end of one renderer's connection.
    pub fn browser() -> OriginTable {
        OriginTable::new(0)
    }

    fn new(side: u64) -> OriginTable {
        OriginTable {
            side,
            next: 0,
            ids: HashMap::new(),
            origins: HashMap::new(),
            named: 0,
        }
    }

    fn id_of(&mut self, origin: &Origin) -> u64 {
        if let Some(id) = self.ids.get(origin) {
            return *id;
        }
        let id = self.next * 2 + self.side;
        self.next += 1;
        self.ids.insert(origin.clone(), id);
        self.origins.insert(id, origin.clone());
        id
    }

    fn origin_of(&mut self, id: u64) -> Option<Origin> {
        if let Some(origin) = self.origins.get(&id) {
            return Some(origin.clone());
        }
        // (an id of this end's own it never made is a lie)
        if id % 2 == self.side || self.named >= MAX_OPAQUE {
            return None;
        }
        self.named += 1;
        let origin = Origin::new_opaque();
        self.ids.insert(origin.clone(), id);
        self.origins.insert(id, origin.clone());
        Some(origin)
    }

    /// How many opaque origins it knows.
    pub fn len(&self) -> usize {
        self.origins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.origins.is_empty()
    }
}

thread_local! {
    static TABLE: RefCell<Option<OriginTable>> = const { RefCell::new(None) };
}

/// Runs `f`, an encode or a decode, with `table` for the opaque origins it meets.
pub fn with_origins<R>(table: &mut OriginTable, f: impl FnOnce() -> R) -> R {
    struct Restore<'a>(&'a mut OriginTable);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            if let Some(t) = TABLE.with(|cell| cell.borrow_mut().take()) {
                *self.0 = t;
            }
        }
    }
    let taken = std::mem::replace(table, OriginTable::new(table.side));
    TABLE.with(|cell| *cell.borrow_mut() = Some(taken));
    let _restore = Restore(table);
    f()
}

fn table<R>(f: impl FnOnce(&mut OriginTable) -> R) -> Option<R> {
    TABLE.with(|cell| cell.borrow_mut().as_mut().map(f))
}

#[derive(Serialize, Deserialize)]
enum WireHost {
    Domain(String),
    V4([u8; 4]),
    V6([u8; 16]),
}

#[derive(Serialize, Deserialize)]
enum WireOrigin {
    Tuple(String, WireHost, u16),
    Opaque(u64),
}

fn to_wire(origin: &Origin) -> Option<WireOrigin> {
    Some(match origin {
        Origin::Tuple(scheme, host, port) => {
            let host = match host {
                Host::Domain(d) => WireHost::Domain(d.clone()),
                Host::Ipv4(a) => WireHost::V4(a.octets()),
                Host::Ipv6(a) => WireHost::V6(a.octets()),
            };
            WireOrigin::Tuple(scheme.clone(), host, *port)
        }
        Origin::Opaque(_) => WireOrigin::Opaque(table(|t| t.id_of(origin))?),
    })
}

fn from_wire(origin: WireOrigin) -> Result<Origin, &'static str> {
    match origin {
        WireOrigin::Tuple(scheme, host, port) => {
            let host = match host {
                WireHost::Domain(d) => {
                    // (only a host as the URL parser leaves it: one that parses to itself)
                    match Host::parse(&d) {
                        Ok(Host::Domain(p)) if p == d => Host::Domain(d),
                        _ => return Err("a host that is not one"),
                    }
                }
                WireHost::V4(a) => Host::Ipv4(Ipv4Addr::from(a)),
                WireHost::V6(a) => Host::Ipv6(Ipv6Addr::from(a)),
            };
            if scheme.is_empty()
                || !scheme
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b"+-.0123456789".contains(&b))
            {
                return Err("a scheme that is not one");
            }
            Ok(Origin::Tuple(scheme, host, port))
        }
        WireOrigin::Opaque(id) => table(|t| t.origin_of(id))
            .flatten()
            .ok_or("an opaque origin it may not name"),
    }
}

/// `#[serde(with = "crate::wire::origin")]`.
pub mod origin {
    use super::*;

    pub fn serialize<S: Serializer>(origin: &Origin, s: S) -> Result<S::Ok, S::Error> {
        to_wire(origin)
            .ok_or_else(|| S::Error::custom("an opaque origin outside an origin table"))?
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Origin, D::Error> {
        from_wire(WireOrigin::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// `#[serde(with = "crate::wire::option_origin")]`.
pub mod option_origin {
    use super::*;

    pub fn serialize<S: Serializer>(origin: &Option<Origin>, s: S) -> Result<S::Ok, S::Error> {
        match origin {
            Some(o) => Some(
                to_wire(o).ok_or_else(|| S::Error::custom("an opaque origin outside a table"))?,
            )
            .serialize(s),
            None => None::<WireOrigin>.serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Origin>, D::Error> {
        Option::<WireOrigin>::deserialize(d)?
            .map(|o| from_wire(o).map_err(D::Error::custom))
            .transpose()
    }
}

/// `#[serde(with = "crate::wire::option_origins")]`.
pub mod option_origins {
    use super::*;

    pub fn serialize<S: Serializer>(
        origins: &Option<Vec<Origin>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        origins
            .as_ref()
            .map(|list| {
                list.iter()
                    .map(|o| {
                        to_wire(o)
                            .ok_or_else(|| S::Error::custom("an opaque origin outside a table"))
                    })
                    .collect::<Result<Vec<WireOrigin>, _>>()
            })
            .transpose()?
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<Origin>>, D::Error> {
        Option::<Vec<WireOrigin>>::deserialize(d)?
            .map(|list| {
                list.into_iter()
                    .map(|o| from_wire(o).map_err(D::Error::custom))
                    .collect()
            })
            .transpose()
    }
}

/// `#[serde(with = "crate::wire::instant")]`, for an `Option<Instant>`.
pub mod instant {
    use super::*;

    pub fn serialize<S: Serializer>(at: &Option<Instant>, s: S) -> Result<S::Ok, S::Error> {
        at.map(|at| Instant::now().saturating_duration_since(at).as_nanos() as u64)
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Instant>, D::Error> {
        let age = Option::<u64>::deserialize(d)?;
        Ok(age.map(|age| {
            let now = Instant::now();
            // (a time before this process's clock can say becomes the earliest it can)
            now.checked_sub(Duration::from_nanos(age)).unwrap_or(now)
        }))
    }
}

#[cfg(test)]
mod tests;
