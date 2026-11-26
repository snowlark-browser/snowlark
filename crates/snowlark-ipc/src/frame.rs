// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Framing: what travels on a pipe. A frame is a little-endian `u32` body length, then the body: a
//! kind byte, a little-endian `u64` call id (0 where it means nothing) and the payload.
//!
//! Nothing here trusts the bytes. [`FrameReader`] holds at most one frame plus what the last read
//! brought, whatever the length field claims, and a bad frame is an error, never a panic.

use crate::error::Error;

/// The most a frame's body may hold (`docs/process-isolation.md` section 7); more goes by section.
pub const MAX_BODY: usize = 1 << 20;
const LENGTH: usize = 4;
const PREFIX: usize = 9;
/// The most a frame's payload may hold.
pub const MAX_PAYLOAD: usize = MAX_BODY - PREFIX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The first frame each way: build id and process id.
    Hello,
    /// A message nobody waits on.
    Message,
    /// A message whose sender waits for a [`Kind::Reply`] with its id.
    Call,
    Reply,
    /// The transport's own business (sections), child to browser, with a [`Kind::ControlReply`].
    Control,
    ControlReply,
    /// A descriptor that came beside the frame's bytes (on Linux, where a handle cannot be put in
    /// another process's table): the receiver keeps it under the frame's id, the value a later
    /// message names it by. Empty payload.
    Handle,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Kind::Hello => 1,
            Kind::Message => 2,
            Kind::Call => 3,
            Kind::Reply => 4,
            Kind::Control => 5,
            Kind::ControlReply => 6,
            Kind::Handle => 7,
        }
    }

    fn from_byte(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Hello,
            2 => Kind::Message,
            3 => Kind::Call,
            4 => Kind::Reply,
            5 => Kind::Control,
            6 => Kind::ControlReply,
            7 => Kind::Handle,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub id: u64,
    pub payload: Vec<u8>,
}

/// A frame's bytes on the wire.
pub fn encode(kind: Kind, id: u64, payload: &[u8]) -> Result<Vec<u8>, Error> {
    if payload.len() > MAX_PAYLOAD {
        return Err(Error::TooBig(payload.len()));
    }
    let mut out = Vec::with_capacity(LENGTH + PREFIX + payload.len());
    out.extend_from_slice(&((PREFIX + payload.len()) as u32).to_le_bytes());
    out.push(kind.byte());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Cuts a byte stream into frames.
#[derive(Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
    /// Where the unread part of `buffer` starts.
    start: usize,
}

impl FrameReader {
    pub fn new() -> FrameReader {
        FrameReader::default()
    }

    /// Takes bytes off the pipe. Call [`FrameReader::next_frame`] until it says `None` before pushing again.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// The next whole frame, if one is here. After an error the stream is lost: the peer is lying.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, Error> {
        let unread = &self.buffer[self.start..];
        let Some(length) = unread.first_chunk::<LENGTH>() else {
            return Ok(None);
        };
        let length = u32::from_le_bytes(*length) as usize;
        if !(PREFIX..=MAX_BODY).contains(&length) {
            return Err(Error::BadFrame("length out of range"));
        }
        let Some(body) = unread.get(LENGTH..LENGTH + length) else {
            return Ok(None);
        };
        let kind = Kind::from_byte(body[0]).ok_or(Error::BadFrame("unknown kind"))?;
        let mut id = [0; 8];
        id.copy_from_slice(&body[1..PREFIX]);
        let frame = Frame {
            kind,
            id: u64::from_le_bytes(id),
            payload: body[PREFIX..].to_vec(),
        };
        self.start += LENGTH + length;
        Ok(Some(frame))
    }

    /// How many bytes wait for the rest of their frame.
    pub fn pending(&self) -> usize {
        self.buffer.len() - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_any_split() {
        let a = encode(Kind::Message, 0, b"first").unwrap();
        let b = encode(Kind::Call, 7, &[9; 300]).unwrap();
        let stream = [a, b].concat();
        for chunk in [1, 2, 3, 5, 13, 64, 4096] {
            let mut reader = FrameReader::new();
            let mut frames = Vec::new();
            for bytes in stream.chunks(chunk) {
                reader.push(bytes);
                while let Some(f) = reader.next_frame().unwrap() {
                    frames.push(f);
                }
            }
            assert_eq!(frames.len(), 2, "chunks of {chunk}");
            assert_eq!(frames[0].payload, b"first");
            assert_eq!((frames[1].kind, frames[1].id), (Kind::Call, 7));
            assert_eq!(frames[1].payload.len(), 300);
            assert_eq!(reader.pending(), 0);
        }
    }

    #[test]
    fn lengths_out_of_range_are_errors() {
        for length in [0u32, 8, MAX_BODY as u32 + 1, u32::MAX] {
            let mut reader = FrameReader::new();
            reader.push(&length.to_le_bytes());
            assert!(reader.next_frame().is_err(), "length {length}");
        }
    }

    #[test]
    fn a_claimed_length_allocates_nothing_until_the_bytes_come() {
        let mut reader = FrameReader::new();
        reader.push(&(MAX_BODY as u32).to_le_bytes());
        assert_eq!(reader.next_frame().unwrap(), None);
        assert_eq!(reader.pending(), LENGTH);
    }

    #[test]
    fn unknown_kinds_are_errors() {
        let mut bytes = encode(Kind::Message, 0, b"x").unwrap();
        bytes[LENGTH] = 200;
        let mut reader = FrameReader::new();
        reader.push(&bytes);
        assert!(reader.next_frame().is_err());
    }

    #[test]
    fn the_cap_is_on_the_payload_with_its_prefix() {
        assert!(encode(Kind::Message, 0, &vec![0; MAX_PAYLOAD]).is_ok());
        assert!(matches!(
            encode(Kind::Message, 0, &vec![0; MAX_PAYLOAD + 1]),
            Err(Error::TooBig(_))
        ));
    }
}
