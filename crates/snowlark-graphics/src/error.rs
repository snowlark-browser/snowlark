// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The error of the decoders and other fallible work here.

use std::fmt;

/// Why decoding or another operation here failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a format decoded here: the platform's decoders may know it.
    Unsupported,
    /// Broken data, or an argument out of range.
    Invalid,
    /// Too large to decode or to allocate (past [`crate::image::MAX_PIXELS`]).
    TooLarge,
    /// A platform call failed, with its message.
    Platform(String),
}

/// A result with [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Unsupported => f.write_str("unsupported format"),
            Error::Invalid => f.write_str("invalid data"),
            Error::TooLarge => f.write_str("too large"),
            Error::Platform(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}
