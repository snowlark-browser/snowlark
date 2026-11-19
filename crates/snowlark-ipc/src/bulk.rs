// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bytes too large for a pipe frame (`docs/process-isolation.md` section 7, "Bulk"): a message carries
//! a [`Bulk`], which holds small bytes itself and names a shared section for the rest.
//!
//! The privileged side still makes every section. A child that sends bulk asks for a section
//! (`AllocShared`), writes it and hands it over: the browser takes it out of the child's quota as it
//! reads it, so a section is never freed twice and never outlives its message by the child's word. The
//! browser sends bulk in a section it makes and duplicates into the child, which maps it, copies it
//! and closes it. Either reader copies once, before anything parses the bytes.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::connection::Connection;
use crate::error::Error;
use crate::section::Section;
use crate::untrusted::{FromBrowser, FromChild};
use crate::win::handle::RemoteHandle;
use crate::wire::Wire;

/// The most a [`Bulk`] holds in the message itself. A message may carry a few of them and still fit a
/// frame; anything larger goes in a section.
pub const INLINE_MAX: usize = 64 << 10;

/// Bytes that travel with a message.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bulk {
    /// At most [`INLINE_MAX`] bytes, in the message.
    Inline(Vec<u8>),
    /// Child to browser: a section the child asked for and handed over, by the browser's id.
    Shared { id: u64, len: u64 },
    /// Browser to child: a section the browser made, as a handle value in the child's table.
    Given { handle: u64, size: u64, len: u64 },
    /// On a link between two children: `len` bytes in the sender's ring, from stream position
    /// `pos` ([`crate::link`]).
    Ring { pos: u64, len: u64 },
}

impl Bulk {
    /// The number of bytes it stands for, by the sender's word.
    pub fn len(&self) -> u64 {
        match self {
            Bulk::Inline(b) => b.len() as u64,
            Bulk::Shared { len, .. } | Bulk::Given { len, .. } | Bulk::Ring { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<Out: Wire, In: Wire> Connection<Out, In, FromBrowser> {
    /// Packs `bytes` to send to the browser: in the message if small, else in a section asked for
    /// now, which the browser takes over when it opens the bulk.
    pub fn bulk(&self, bytes: Vec<u8>, timeout: Duration) -> Result<Bulk, Error> {
        if bytes.len() <= INLINE_MAX {
            return Ok(Bulk::Inline(bytes));
        }
        let section = self.alloc_shared(bytes.len() as u64, timeout)?;
        if !section.write(0, &bytes) {
            return Err(Error::Refused);
        }
        Ok(Bulk::Shared {
            id: section.hand_over(),
            len: bytes.len() as u64,
        })
    }

    /// The bytes of a bulk from the browser. A section it names is mapped, copied and closed.
    pub fn open_bulk(&self, bulk: Bulk) -> Result<Vec<u8>, Error> {
        match bulk {
            Bulk::Inline(bytes) => Ok(bytes),
            Bulk::Given { handle, size, len } => {
                let section = Section::from_remote(RemoteHandle::from_value(handle), size)?;
                section.read_once(0, len as usize).ok_or(Error::Refused)
            }
            Bulk::Shared { .. } => Err(Error::BadMessage(
                "a section of the child's own from the browser".into(),
            )),
            Bulk::Ring { .. } => Err(Error::BadMessage("a link's ring from the browser".into())),
        }
    }
}

impl<Out: Wire, In: Wire> Connection<Out, In, FromChild> {
    /// Packs `bytes` to send to the child: in the message if small, else in a new section given to
    /// it. The browser's own copy of the section closes here; the child's handle keeps it alive.
    pub fn bulk(&self, bytes: &[u8]) -> Result<Bulk, Error> {
        if bytes.len() <= INLINE_MAX {
            return Ok(Bulk::Inline(bytes.to_vec()));
        }
        let section = Section::create(bytes.len())?;
        if !section.write(0, bytes) {
            return Err(Error::Refused);
        }
        let handle = self.core.peer.duplicate_section(&section)?;
        Ok(Bulk::Given {
            handle,
            size: section.size() as u64,
            len: bytes.len() as u64,
        })
    }

    /// The bytes of a bulk from the child. A section it names leaves the child's quota as it is read.
    /// A bulk that breaks a rule kills the child: an inline one over [`INLINE_MAX`], a section it does
    /// not hold, a length past the section's end, or a handle (only the browser gives handles).
    pub fn open_bulk(&self, bulk: Bulk) -> Result<Vec<u8>, Error> {
        let bad = |why: String| {
            self.bad_message(&why);
            Error::BadMessage(why)
        };
        match bulk {
            Bulk::Inline(bytes) if bytes.len() <= INLINE_MAX => Ok(bytes),
            Bulk::Inline(bytes) => Err(bad(format!("{} bytes inline", bytes.len()))),
            Bulk::Shared { id, len } => {
                let taken = self
                    .core
                    .broker
                    .as_ref()
                    .and_then(|b| b.lock().unwrap().take(id));
                let Some(section) = taken else {
                    return Err(bad(format!("bulk in section {id}, which it does not hold")));
                };
                match usize::try_from(len)
                    .ok()
                    .and_then(|len| section.read_once(0, len))
                {
                    Some(bytes) => Ok(bytes),
                    None => Err(bad(format!(
                        "bulk of {len} bytes in a section of {}",
                        section.size()
                    ))),
                }
            }
            Bulk::Given { .. } => Err(bad("a handle from a child".into())),
            Bulk::Ring { .. } => Err(bad("a link's ring on the browser's pipe".into())),
        }
    }

    /// Hands a bulk from this child on to the child at `to` without reading it: a section it names
    /// leaves this child's quota and is given to `to`, which maps and copies it as it would one the
    /// browser made ([`Connection::open_bulk`]). The rules are [`Connection::open_bulk`]'s, and one
    /// broken kills this child. (A renderer's raster jobs reach the GPU process so.)
    pub fn pass_bulk<O2: Wire, I2: Wire>(
        &self,
        bulk: Bulk,
        to: &Connection<O2, I2, FromChild>,
    ) -> Result<Bulk, Error> {
        let bad = |why: String| {
            self.bad_message(&why);
            Error::BadMessage(why)
        };
        match bulk {
            Bulk::Inline(bytes) if bytes.len() <= INLINE_MAX => Ok(Bulk::Inline(bytes)),
            Bulk::Inline(bytes) => Err(bad(format!("{} bytes inline", bytes.len()))),
            Bulk::Shared { id, len } => {
                let taken = self
                    .core
                    .broker
                    .as_ref()
                    .and_then(|b| b.lock().unwrap().take(id));
                let Some(section) = taken else {
                    return Err(bad(format!("bulk in section {id}, which it does not hold")));
                };
                if len > section.size() as u64 {
                    return Err(bad(format!(
                        "bulk of {len} bytes in a section of {}",
                        section.size()
                    )));
                }
                // (the browser's handle closes as `section` drops: the other child's keeps it)
                let handle = to.core.peer.duplicate_section(&section)?;
                Ok(Bulk::Given {
                    handle,
                    size: section.size() as u64,
                    len,
                })
            }
            Bulk::Given { .. } => Err(bad("a handle from a child".into())),
            Bulk::Ring { .. } => Err(bad("a link's ring on the browser's pipe".into())),
        }
    }
}
