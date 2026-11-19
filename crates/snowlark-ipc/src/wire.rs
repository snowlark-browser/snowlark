// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The message format: `postcard` for every interface, and the transport's own messages.
//!
//! Decoding is total (a bad message is an `Err`, never a panic), allocates in proportion to the bytes
//! present, and takes the whole payload: trailing bytes are as bad as missing ones.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::frame::MAX_PAYLOAD;

/// What a message type must be to cross: plain data with `serde`, and no recursion in its shape
/// (nothing here limits depth, so a recursive type would let a sender overflow the reader's stack).
pub trait Wire: Serialize + DeserializeOwned + Send + 'static {}
impl<T: Serialize + DeserializeOwned + Send + 'static> Wire for T {}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let bytes = postcard::to_allocvec(value)
        .map_err(|e| Error::BadMessage(format!("does not encode: {e}")))?;
    if bytes.len() > MAX_PAYLOAD {
        return Err(Error::TooBig(bytes.len()));
    }
    Ok(bytes)
}

/// Encodes with no cap: for bytes that travel as [`crate::Bulk`] rather than in a frame.
pub fn to_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    postcard::to_allocvec(value).map_err(|e| Error::BadMessage(format!("does not encode: {e}")))
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    match postcard::take_from_bytes(bytes) {
        Ok((value, [])) => Ok(value),
        Ok(_) => Err(Error::BadMessage("trailing bytes".into())),
        Err(e) => Err(Error::BadMessage(format!("does not decode: {e}"))),
    }
}

/// A build's identity. One binary runs at both ends, so messages carry no versions; this is how a
/// stale child left over an update is told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildId(pub [u8; 16]);

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub magic: u32,
    pub build: BuildId,
    pub process: u32,
}

pub(crate) const MAGIC: u32 = 0x4f53_4d31;

/// Child to browser. The child never duplicates a handle and never creates a section it shares.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Control {
    AllocShared { size: u64 },
    FreeShared { id: u64 },
}

/// Browser to child, answering [`Control`]. `handle` is a value in the child's own handle table.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ControlReply {
    Allocated { id: u64, handle: u64, size: u64 },
    Refused,
    Freed,
}

/// Runs the transport's own decoders on `bytes` (for the fuzz target: they are private).
#[doc(hidden)]
pub fn fuzz_transport_messages(bytes: &[u8]) {
    let _ = decode::<Hello>(bytes);
    let _ = decode::<Control>(bytes);
    let _ = decode::<ControlReply>(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Sample {
        Text(String),
        Numbers(Vec<u32>),
        Pair { a: Option<u64>, b: bool },
    }

    #[test]
    fn values_round_trip() {
        for v in [
            Sample::Text("héllo".into()),
            Sample::Numbers(vec![1, 2, 300_000]),
            Sample::Pair {
                a: Some(u64::MAX),
                b: true,
            },
        ] {
            assert_eq!(decode::<Sample>(&encode(&v).unwrap()).unwrap(), v);
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = encode(&Sample::Numbers(vec![1])).unwrap();
        bytes.push(0);
        assert!(decode::<Sample>(&bytes).is_err());
    }

    #[test]
    fn every_truncation_is_an_error() {
        let bytes = encode(&Sample::Text("a string".into())).unwrap();
        for n in 0..bytes.len() {
            assert!(decode::<Sample>(&bytes[..n]).is_err(), "{n} bytes");
        }
    }

    #[test]
    fn a_huge_claimed_length_does_not_allocate_it() {
        // A variant tag, then a length of about 2^63 in postcard's varint, and no elements.
        let bytes = [1, 255, 255, 255, 255, 255, 255, 255, 255, 127];
        assert!(decode::<Sample>(&bytes).is_err());
    }

    #[test]
    fn over_the_cap_does_not_encode() {
        let v = Sample::Text("x".repeat(MAX_PAYLOAD + 1));
        assert!(matches!(encode(&v), Err(Error::TooBig(_))));
    }
}
