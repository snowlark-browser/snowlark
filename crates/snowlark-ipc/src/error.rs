// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What can go wrong at the boundary.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The other side is gone (its pipe broke, or this end closed).
    Closed,
    /// No answer in time.
    Timeout,
    /// A message is over the 1 MiB cap: such data goes by section.
    TooBig(usize),
    /// Too much is queued for a peer that is not reading.
    Backlog,
    /// The stream broke the framing rules.
    BadFrame(&'static str),
    /// A message that did not decode, or that the protocol does not allow here.
    BadMessage(String),
    /// The other end is another build: messages have no versions, so the two cannot talk.
    BuildMismatch,
    /// The browser refused a section (over the quota, or a size it does not allow).
    Refused,
    /// The operating system said no.
    #[cfg(windows)]
    Os(windows::core::Error),
    /// The operating system said no.
    #[cfg(unix)]
    Os(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Closed => write!(f, "the connection is closed"),
            Error::Timeout => write!(f, "timed out"),
            Error::TooBig(n) => write!(f, "a message of {n} bytes is over the cap"),
            Error::Backlog => write!(f, "too much is queued for the peer"),
            Error::BadFrame(why) => write!(f, "bad frame: {why}"),
            Error::BadMessage(why) => write!(f, "bad message: {why}"),
            Error::BuildMismatch => write!(f, "the other process is another build"),
            Error::Refused => write!(f, "refused"),
            Error::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(windows)]
impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Error {
        Error::Os(e)
    }
}
