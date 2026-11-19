// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Keys (IndexedDB §2.4), key ranges (§2.9) and key paths (§2.5): the types, the comparison that
//! orders every record, and the byte encoding the on-disk log uses.

use std::cmp::Ordering;
use std::ops::Bound;

use serde::{Deserialize, Serialize};

/// A key (§2.4). Strings are 16-bit code units: keys compare by code unit, and a string key can hold
/// lone surrogates.
#[derive(Clone, Debug)]
pub enum Key {
    Number(f64),
    Date(f64),
    String(Vec<u16>),
    Binary(Vec<u8>),
    Array(Vec<Key>),
}

impl Key {
    /// A string key from Rust text.
    pub fn string(s: &str) -> Key {
        Key::String(s.encode_utf16().collect())
    }

    /// The rank of a key's type in the comparison: number < date < string < binary < array.
    fn rank(&self) -> u8 {
        match self {
            Key::Number(_) => 0,
            Key::Date(_) => 1,
            Key::String(_) => 2,
            Key::Binary(_) => 3,
            Key::Array(_) => 4,
        }
    }

    /// Whether this is an array key.
    pub fn is_array(&self) -> bool {
        matches!(self, Key::Array(_))
    }
}

/// §2.4 "compare two keys".
pub fn compare(a: &Key, b: &Key) -> Ordering {
    let (ta, tb) = (a.rank(), b.rank());
    if ta != tb {
        return ta.cmp(&tb);
    }
    match (a, b) {
        (Key::Number(x), Key::Number(y)) | (Key::Date(x), Key::Date(y)) => {
            // (keys never hold NaN; -0 and +0 are equal)
            x.partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (Key::String(x), Key::String(y)) => x.cmp(y),
        (Key::Binary(x), Key::Binary(y)) => x.cmp(y),
        (Key::Array(x), Key::Array(y)) => {
            for (i, j) in x.iter().zip(y.iter()) {
                let c = compare(i, j);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        _ => unreachable!("same rank, same type"),
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Key) -> bool {
        compare(self, other) == Ordering::Equal
    }
}

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Key) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Key) -> Ordering {
        compare(self, other)
    }
}

/// A key range (§2.9): a lower and an upper bound, each null or a key, open or closed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyRange {
    pub lower: Option<Key>,
    pub upper: Option<Key>,
    pub lower_open: bool,
    pub upper_open: bool,
}

impl KeyRange {
    /// The unbounded key range.
    pub fn unbounded() -> KeyRange {
        KeyRange::default()
    }

    /// A key range containing only `key`.
    pub fn only(key: Key) -> KeyRange {
        KeyRange {
            lower: Some(key.clone()),
            upper: Some(key),
            lower_open: false,
            upper_open: false,
        }
    }

    /// Whether `key` is in the range.
    pub fn contains(&self, key: &Key) -> bool {
        let lower_ok = match &self.lower {
            None => true,
            Some(l) => match compare(l, key) {
                Ordering::Less => true,
                Ordering::Equal => !self.lower_open,
                Ordering::Greater => false,
            },
        };
        let upper_ok = match &self.upper {
            None => true,
            Some(u) => match compare(u, key) {
                Ordering::Greater => true,
                Ordering::Equal => !self.upper_open,
                Ordering::Less => false,
            },
        };
        lower_ok && upper_ok
    }

    /// Whether the range is a single key (`only`), and that key.
    pub fn single(&self) -> Option<&Key> {
        match (&self.lower, &self.upper) {
            (Some(l), Some(u)) if !self.lower_open && !self.upper_open && l == u => Some(l),
            _ => None,
        }
    }

    /// The range as `BTreeMap::range` bounds, or `None` when no key can be in it (bounds that
    /// `BTreeMap::range` would refuse: equal and excluded, or crossed).
    pub fn bounds(&self) -> Option<(Bound<&Key>, Bound<&Key>)> {
        let lo = match &self.lower {
            None => Bound::Unbounded,
            Some(k) if self.lower_open => Bound::Excluded(k),
            Some(k) => Bound::Included(k),
        };
        let hi = match &self.upper {
            None => Bound::Unbounded,
            Some(k) if self.upper_open => Bound::Excluded(k),
            Some(k) => Bound::Included(k),
        };
        if let (Some(l), Some(u)) = (&self.lower, &self.upper) {
            match compare(l, u) {
                Ordering::Greater => return None,
                Ordering::Equal if self.lower_open || self.upper_open => return None,
                _ => {}
            }
        }
        Some((lo, hi))
    }
}

/// A key path (§2.5): a string or a list of strings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyPath {
    String(String),
    Sequence(Vec<String>),
}

impl KeyPath {
    /// Whether this is a valid key path (§2.5).
    pub fn is_valid(&self) -> bool {
        match self {
            KeyPath::String(s) => is_valid_key_path_string(s),
            KeyPath::Sequence(v) => !v.is_empty() && v.iter().all(|s| is_valid_key_path_string(s)),
        }
    }
}

/// §2.5: the empty string, an identifier, or identifiers separated by periods.
pub fn is_valid_key_path_string(s: &str) -> bool {
    s.is_empty() || s.split('.').all(is_identifier_name)
}

/// ECMAScript's IdentifierName (without escape sequences): an identifier start then identifier
/// parts.
fn is_identifier_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if is_id_start(c) => chars.all(is_id_part),
        _ => false,
    }
}

fn is_id_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

fn is_id_part(c: char) -> bool {
    is_id_start(c)
        || c.is_alphanumeric()
        || c == '\u{200C}'
        || c == '\u{200D}'
        // (combining marks and connector punctuation: ID_Continue's Mn, Mc, Pc)
        || matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}' | '\u{203F}' | '\u{2040}'
            | '\u{2054}' | '\u{FE33}' | '\u{FE34}' | '\u{FE4D}'..='\u{FE4F}' | '\u{FF3F}')
}

// ----- the byte encoding -------------------------------------------------------------------------------

pub(crate) const TAG_NUMBER: u8 = 1;
pub(crate) const TAG_DATE: u8 = 2;
pub(crate) const TAG_STRING: u8 = 3;
pub(crate) const TAG_BINARY: u8 = 4;
pub(crate) const TAG_ARRAY: u8 = 5;

/// The deepest an array key read from disk may nest. (Too deep for a thread with a 1 MiB stack:
/// see [`crate::wire::WIRE_KEY_DEPTH`].)
pub const MAX_KEY_DEPTH: u32 = 10_000;

/// Appends a key's encoding to `out`.
pub fn encode_key(key: &Key, out: &mut Vec<u8>) {
    match key {
        Key::Number(n) => {
            out.push(TAG_NUMBER);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Key::Date(n) => {
            out.push(TAG_DATE);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Key::String(s) => {
            out.push(TAG_STRING);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            for u in s {
                out.extend_from_slice(&u.to_le_bytes());
            }
        }
        Key::Binary(b) => {
            out.push(TAG_BINARY);
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        Key::Array(items) => {
            out.push(TAG_ARRAY);
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for k in items {
                encode_key(k, out);
            }
        }
    }
}

/// A reader over encoded bytes.
pub struct Reader<'a> {
    pub bytes: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    pub fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    pub fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }

    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub fn key(&mut self) -> Option<Key> {
        self.key_at_depth(0)
    }

    fn key_at_depth(&mut self, depth: u32) -> Option<Key> {
        if depth > MAX_KEY_DEPTH {
            return None;
        }
        Some(match self.u8()? {
            TAG_NUMBER => Key::Number(self.f64()?),
            TAG_DATE => Key::Date(self.f64()?),
            TAG_STRING => {
                let n = self.u32()? as usize;
                let raw = self.take(n.checked_mul(2)?)?;
                Key::String(
                    raw.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .collect(),
                )
            }
            TAG_BINARY => Key::Binary(self.bytes()?.to_vec()),
            TAG_ARRAY => {
                let n = self.u32()? as usize;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(self.key_at_depth(depth + 1)?);
                }
                Key::Array(items)
            }
            _ => return None,
        })
    }
}

/// Appends a string (length-prefixed UTF-8).
pub fn encode_str(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Appends a key path (or none).
pub fn encode_key_path(p: Option<&KeyPath>, out: &mut Vec<u8>) {
    match p {
        None => out.push(0),
        Some(KeyPath::String(s)) => {
            out.push(1);
            encode_str(s, out);
        }
        Some(KeyPath::Sequence(v)) => {
            out.push(2);
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for s in v {
                encode_str(s, out);
            }
        }
    }
}

impl Reader<'_> {
    pub fn key_path(&mut self) -> Option<Option<KeyPath>> {
        Some(match self.u8()? {
            0 => None,
            1 => Some(KeyPath::String(self.string()?)),
            2 => {
                let n = self.u32()? as usize;
                let mut v = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    v.push(self.string()?);
                }
                Some(KeyPath::Sequence(v))
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Key {
        Key::string(x)
    }

    #[test]
    fn type_order() {
        let keys = [
            Key::Number(f64::NEG_INFINITY),
            Key::Number(-1.0),
            Key::Number(0.0),
            Key::Number(f64::INFINITY),
            Key::Date(-1.0),
            Key::Date(5.0),
            s(""),
            s("a"),
            s("b"),
            Key::Binary(vec![]),
            Key::Binary(vec![0]),
            Key::Binary(vec![255]),
            Key::Array(vec![]),
            Key::Array(vec![Key::Number(1.0)]),
            Key::Array(vec![Key::Number(1.0), Key::Number(0.0)]),
            Key::Array(vec![s("a")]),
        ];
        for (i, a) in keys.iter().enumerate() {
            for (j, b) in keys.iter().enumerate() {
                assert_eq!(compare(a, b), i.cmp(&j), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn zeros_and_code_units() {
        assert_eq!(Key::Number(0.0), Key::Number(-0.0));
        // Code unit order: U+FFFF sorts after a surrogate pair's lead unit.
        let astral = Key::String("\u{10000}".encode_utf16().collect());
        let bmp = Key::String(vec![0xFFFF]);
        assert!(astral < bmp);
        // Binary keys compare as unsigned bytes.
        assert!(Key::Binary(vec![0x7F]) < Key::Binary(vec![0x80]));
    }

    #[test]
    fn ranges() {
        let r = KeyRange {
            lower: Some(Key::Number(1.0)),
            upper: Some(Key::Number(5.0)),
            lower_open: true,
            upper_open: false,
        };
        assert!(!r.contains(&Key::Number(1.0)));
        assert!(r.contains(&Key::Number(1.5)));
        assert!(r.contains(&Key::Number(5.0)));
        assert!(!r.contains(&s("a")));
        assert!(KeyRange::unbounded().contains(&s("x")));
        let empty = KeyRange {
            lower: Some(Key::Number(1.0)),
            upper: Some(Key::Number(1.0)),
            lower_open: true,
            upper_open: false,
        };
        assert!(empty.bounds().is_none());
        assert_eq!(
            KeyRange::only(Key::Number(2.0)).single(),
            Some(&Key::Number(2.0))
        );
    }

    #[test]
    fn key_paths() {
        assert!(is_valid_key_path_string(""));
        assert!(is_valid_key_path_string("a"));
        assert!(is_valid_key_path_string("a.b.c"));
        assert!(is_valid_key_path_string("$x._y"));
        assert!(is_valid_key_path_string("\u{00E9}t\u{00E9}"));
        assert!(!is_valid_key_path_string("a b"));
        assert!(!is_valid_key_path_string(".a"));
        assert!(!is_valid_key_path_string("a."));
        assert!(!is_valid_key_path_string("a..b"));
        assert!(!is_valid_key_path_string("1a"));
        assert!(!is_valid_key_path_string("+"));
        assert!(!KeyPath::Sequence(vec![]).is_valid());
        assert!(KeyPath::Sequence(vec!["a".into(), "b.c".into()]).is_valid());
        assert!(!KeyPath::Sequence(vec!["a b".into()]).is_valid());
    }

    #[test]
    fn encoding_round_trips() {
        let keys = [
            Key::Number(-0.5),
            Key::Date(1e12),
            Key::String(vec![0xD800, 0x41]),
            Key::Binary(vec![1, 2, 3]),
            Key::Array(vec![s("x"), Key::Array(vec![Key::Number(1.0)])]),
        ];
        let mut buf = Vec::new();
        for k in &keys {
            encode_key(k, &mut buf);
        }
        let mut r = Reader::new(&buf);
        for k in &keys {
            assert_eq!(&r.key().unwrap(), k);
        }
        assert!(r.at_end());
        let mut buf = Vec::new();
        let p = KeyPath::Sequence(vec!["a".into(), "b".into()]);
        encode_key_path(Some(&p), &mut buf);
        assert_eq!(Reader::new(&buf).key_path().unwrap(), Some(p));
    }
}
