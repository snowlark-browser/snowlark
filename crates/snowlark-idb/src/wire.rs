// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How IndexedDB's types cross a process boundary (`docs/process-isolation.md`, M4 step 2): they
//! derive `serde`, and this module holds what a derive cannot carry or check.
//!
//! - **Keys** cross as their byte encoding ([`crate::key::encode_key`]), so their `serde` shape is
//!   not recursive (the IPC format sets no depth limit), and decode here with the checks the disk's
//!   reader does without, since the disk holds only what this process wrote: no NaN (keys never
//!   hold one, and [`crate::key::compare`] orders NaN equal to every number, which breaks every
//!   `BTreeMap` of records), at most [`WIRE_KEY_DEPTH`] nested arrays, and no more memory reserved
//!   than the bytes present can fill. A key nested deeper does not encode either, so the sender's
//!   call fails there instead of killing it.
//! - **Values** cross as bytes ([`value`]); a read's answer as [`WireFound`].
//! - **Changes**: [`check_change`] refuses what the backend would apply wrongly: an id it cannot
//!   count past, a key path no page could have made.

use std::fmt;
use std::sync::Arc;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::db::{Change, Found, Value};
use crate::key::{
    encode_key, Key, KeyPath, Reader, TAG_ARRAY, TAG_BINARY, TAG_DATE, TAG_NUMBER, TAG_STRING,
};

/// The deepest an array key may nest to cross. Reading, encoding, cloning, comparing and dropping a
/// key each recurse once per level: measured in release, every one of them overflows a 1 MiB stack
/// at 10,000 levels (the disk's [`crate::key::MAX_KEY_DEPTH`]) and none at 4,000, so this leaves
/// half of a 1 MiB stack for what calls them.
pub const WIRE_KEY_DEPTH: u32 = 2_000;

/// The fewest bytes one encoded key takes (a tag and a length).
const MIN_KEY_BYTES: usize = 5;

/// How deep `key` nests (without recursing: it may be too deep to).
fn depth(key: &Key) -> u32 {
    let mut deepest = 0;
    let mut stack = vec![(key, 0)];
    while let Some((k, d)) = stack.pop() {
        deepest = deepest.max(d);
        if let Key::Array(items) = k {
            stack.extend(items.iter().map(|i| (i, d + 1)));
        }
    }
    deepest
}

impl Serialize for Key {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.is_array() && depth(self) > WIRE_KEY_DEPTH {
            return Err(serde::ser::Error::custom("a key nested too deep to cross"));
        }
        let mut out = Vec::new();
        encode_key(self, &mut out);
        s.serialize_bytes(&out)
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Key, D::Error> {
        d.deserialize_bytes(KeyVisitor)
    }
}

struct KeyVisitor;

impl Visitor<'_> for KeyVisitor {
    type Value = Key;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an encoded key")
    }

    fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Key, E> {
        decode_key(bytes).map_err(E::custom)
    }
}

/// A key from bytes another process sent: the whole of `bytes`, checked.
pub fn decode_key(bytes: &[u8]) -> Result<Key, &'static str> {
    let mut r = Reader::new(bytes);
    let key = untrusted_key(&mut r, 0)?;
    if !r.at_end() {
        return Err("bytes after a key");
    }
    Ok(key)
}

fn number(r: &mut Reader<'_>) -> Result<f64, &'static str> {
    match r.f64() {
        Some(n) if n.is_nan() => Err("a key that is NaN"),
        Some(n) => Ok(n),
        None => Err("a key cut short"),
    }
}

fn untrusted_key(r: &mut Reader<'_>, depth: u32) -> Result<Key, &'static str> {
    const SHORT: &str = "a key cut short";
    if depth > WIRE_KEY_DEPTH {
        return Err("a key nested too deep");
    }
    Ok(match r.u8().ok_or(SHORT)? {
        TAG_NUMBER => Key::Number(number(r)?),
        TAG_DATE => Key::Date(number(r)?),
        TAG_STRING => {
            let n = r.u32().ok_or(SHORT)? as usize;
            let raw = n.checked_mul(2).and_then(|n| r.take(n)).ok_or(SHORT)?;
            Key::String(
                raw.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect(),
            )
        }
        TAG_BINARY => Key::Binary(r.bytes().ok_or(SHORT)?.to_vec()),
        TAG_ARRAY => {
            let n = r.u32().ok_or(SHORT)? as usize;
            // (what is left bounds how many items can follow, so it bounds what is reserved)
            let left = r.bytes.len().saturating_sub(r.pos);
            let mut items = Vec::with_capacity(n.min(left / MIN_KEY_BYTES));
            for _ in 0..n {
                items.push(untrusted_key(r, depth + 1)?);
            }
            Key::Array(items)
        }
        _ => return Err("not a key"),
    })
}

/// A record's value as bytes: `#[serde(with = "crate::wire::value")]`.
pub mod value {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Value, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
        d.deserialize_byte_buf(ValueVisitor)
    }

    struct ValueVisitor;

    impl Visitor<'_> for ValueVisitor {
        type Value = Value;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a value's bytes")
        }

        fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Value, E> {
            Ok(Arc::from(bytes))
        }
    }
}

/// A record a read found ([`Found`]), as it crosses.
#[derive(Debug, Serialize, Deserialize)]
pub struct WireFound(pub Key, pub Key, #[serde(with = "value")] pub Value);

impl From<Found> for WireFound {
    fn from((key, primary, value): Found) -> WireFound {
        WireFound(key, primary, value)
    }
}

impl From<WireFound> for Found {
    fn from(WireFound(key, primary, value): WireFound) -> Found {
        (key, primary, value)
    }
}

/// Whether a change another process sent keeps what the backend relies on: a store or index id it
/// can count past (`next_id` is the id plus one), and the key paths of a new store or index valid
/// (§2.5), as every page checks before it makes one; other pages of the origin evaluate them.
/// Keys were checked as they decoded; what the transaction may change, the backend checks.
pub fn check_change(change: &Change) -> Result<(), String> {
    let path_ok = |p: &KeyPath| p.is_valid();
    match change {
        Change::CreateStore { id, key_path, .. } => {
            if *id == u64::MAX {
                return Err(format!("an object store id of {id}"));
            }
            if key_path.as_ref().is_some_and(|p| !path_ok(p)) {
                return Err(format!("an object store key path of {key_path:?}"));
            }
        }
        Change::CreateIndex { id, key_path, .. } => {
            if *id == u64::MAX {
                return Err(format!("an index id of {id}"));
            }
            if !path_ok(key_path) {
                return Err(format!("an index key path of {key_path:?}"));
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Record;
    use crate::key::KeyRange;

    fn encode<T: Serialize>(value: &T) -> Vec<u8> {
        postcard::to_allocvec(value).unwrap()
    }

    fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, postcard::Error> {
        postcard::from_bytes(bytes)
    }

    fn nested(depth: u32) -> Key {
        let mut k = Key::Number(1.0);
        for _ in 0..depth {
            k = Key::Array(vec![k]);
        }
        k
    }

    #[test]
    fn keys_values_and_changes_round_trip() {
        let keys = [
            Key::Number(-0.5),
            Key::Number(f64::INFINITY),
            Key::Date(1e12),
            Key::String(vec![0xD800, 0x41]),
            Key::Binary(vec![1, 2, 3]),
            Key::Array(vec![Key::string("x"), Key::Array(vec![])]),
        ];
        for k in &keys {
            assert_eq!(&decode::<Key>(&encode(k)).unwrap(), k);
        }
        let value: Value = (0..200_000u32).map(|i| i as u8).collect();
        let change = Change::Put {
            store: 3,
            key: Key::string("a"),
            record: Record {
                value: value.clone(),
                index_keys: vec![(4, keys.to_vec())],
            },
        };
        let bytes = encode(&change);
        // (the value crosses as its bytes, not one item per byte)
        assert!(bytes.len() < value.len() + 200);
        let Change::Put { store, key, record } = decode::<Change>(&bytes).unwrap() else {
            panic!("a put");
        };
        assert_eq!((store, key), (3, Key::string("a")));
        assert_eq!(record.value, value);
        assert_eq!(record.index_keys, vec![(4, keys.to_vec())]);
        let range = KeyRange {
            lower: Some(Key::Number(1.0)),
            upper: None,
            lower_open: true,
            upper_open: false,
        };
        assert_eq!(decode::<KeyRange>(&encode(&range)).unwrap(), range);
    }

    #[test]
    fn a_key_that_breaks_an_invariant_does_not_decode() {
        // NaN, as a number, a date, and inside an array.
        for k in [
            Key::Number(f64::NAN),
            Key::Date(f64::NAN),
            Key::Array(vec![Key::Number(1.0), Key::Number(-f64::NAN)]),
        ] {
            assert!(decode::<Key>(&encode(&k)).is_err(), "{k:?}");
        }
        // As deep as may cross, and no deeper: the sender does not encode one deeper either.
        assert!(decode::<Key>(&encode(&nested(WIRE_KEY_DEPTH))).is_ok());
        let mut deeper = Vec::new();
        encode_key(&nested(WIRE_KEY_DEPTH + 1), &mut deeper);
        assert!(decode_key(&deeper).is_err());
        assert!(postcard::to_allocvec(&nested(WIRE_KEY_DEPTH + 1)).is_err());
        // Truncated, with bytes after it, an unknown tag.
        let mut bytes = Vec::new();
        encode_key(&Key::string("abc"), &mut bytes);
        assert!(decode_key(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode_key(&longer).is_err());
        assert!(decode_key(&[9]).is_err());
        // An array that claims four billion items reserves room for what is there, then fails.
        let mut claim = vec![TAG_ARRAY];
        claim.extend_from_slice(&u32::MAX.to_le_bytes());
        claim.extend(std::iter::repeat_n(TAG_ARRAY, 40));
        assert!(decode_key(&claim).is_err());
    }

    #[test]
    fn a_change_the_backend_cannot_count_past_is_refused() {
        let store = |id, key_path| Change::CreateStore {
            id,
            name: "s".into(),
            key_path,
            auto_increment: false,
        };
        let index = |id, key_path| Change::CreateIndex {
            store: 1,
            id,
            name: "i".into(),
            key_path,
            unique: false,
            multi_entry: false,
        };
        let good = KeyPath::String("a.b".into());
        let bad = KeyPath::String("a..b".into());
        assert!(check_change(&store(5, Some(good.clone()))).is_ok());
        assert!(check_change(&store(5, None)).is_ok());
        assert!(check_change(&index(5, good.clone())).is_ok());
        assert!(check_change(&store(u64::MAX, None)).is_err());
        assert!(check_change(&index(u64::MAX, good)).is_err());
        assert!(check_change(&store(5, Some(bad.clone()))).is_err());
        assert!(check_change(&index(5, bad)).is_err());
        assert!(check_change(&index(5, KeyPath::Sequence(vec![]))).is_err());
    }
}
