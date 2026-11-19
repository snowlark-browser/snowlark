// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Subtree filters: per element, a 256-bit Bloom filter of the class names and attribute local names
//! of the element and its descendants (not shadow trees, which have roots of their own), so that a
//! selector query can skip a subtree that cannot hold an element with the names its rightmost compound
//! needs. They live in [`Dom::filters`], beside the node store, by slot.
//!
//! Not observable by pages: a filter only ever lets a query skip a subtree in which no element could
//! match. Bits are added when a subtree gains a name (an attribute appended, a class changed, a node
//! inserted) and never cleared, so a removal leaves a stale bit, which makes a query walk a subtree it
//! could have skipped, never skip one it must walk. Names are hashed ASCII-lowercased, which covers the
//! quirks-mode class match and the HTML attribute name match. Blink does the same with 32 bits and two
//! bits per name (`Element::attribute_or_class_bloom_`, `SelectorQuery`'s `CouldMatchFilter`); on a
//! page of 7,141 elements and 201 class names those fill near the root, and a simulation had a class
//! query visit a median of 713 elements against 6 with 256 bits and three per name; 128 bits left
//! some names (`data-id` there) at 20 times their floor for some hash seeds.
//!
//! An HTML `slot` element also has [`SLOT_ELEMENT`] in its filter from its creation, so whether a tree
//! can hold a slot is a look at its root's children: a shadow tree without one has no slot assignment
//! to compute ([`Dom::may_hold_slots`]).
//!
//! Invariant (checked by [`Dom::check_subtree_filters`] in tests): an element's filter holds the bits
//! of each of its attributes and of each child element's filter, and a `slot` element's holds
//! [`SLOT_ELEMENT`].

use crate::dom::Dom;
use crate::node::{Attr, NodeData, NodeId};
use html5ever::{local_name, ns, LocalName};

/// A subtree filter: 256 bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Filter([u64; 4]);

impl Filter {
    /// No names.
    pub const EMPTY: Filter = Filter([0; 4]);

    #[inline(always)]
    pub fn is_empty(self) -> bool {
        self == Filter::EMPTY
    }

    #[inline(always)]
    fn bit(i: u32) -> Filter {
        let mut f = Filter::EMPTY;
        f.0[(i >> 6) as usize & 3] = 1 << (i & 63);
        f
    }
}

impl std::ops::BitOr for Filter {
    type Output = Filter;
    #[inline(always)]
    fn bitor(self, o: Filter) -> Filter {
        Filter([
            self.0[0] | o.0[0],
            self.0[1] | o.0[1],
            self.0[2] | o.0[2],
            self.0[3] | o.0[3],
        ])
    }
}

impl std::ops::BitOrAssign for Filter {
    #[inline(always)]
    fn bitor_assign(&mut self, o: Filter) {
        *self = *self | o;
    }
}

/// The bits every HTML `slot` element adds to its filter (and so to its ancestors'): three fixed bits,
/// which a name's bits may share (then a tree without slots is walked for them, as before).
pub const SLOT_ELEMENT: Filter = Filter([1 << 7, 1 << 29, 1 << 51, 0]);

/// The filter a new node starts with: [`SLOT_ELEMENT`] for an HTML `slot` element, else none.
pub(crate) fn initial_filter(data: &NodeData) -> Filter {
    match data {
        NodeData::Element(e)
            if e.name().ns == ns!(html) && e.name().local == local_name!("slot") =>
        {
            SLOT_ELEMENT
        }
        _ => Filter::EMPTY,
    }
}

const CLASS_SEED: u32 = 0x811c_9dc5;
const ATTRIBUTE_SEED: u32 = 0x811c_9dc5 ^ 0x5bd1_e995;

/// The filter bits of a class name.
#[inline]
pub fn class_bits(name: &str) -> Filter {
    bits(name, CLASS_SEED)
}

/// The filter bits of an attribute local name.
#[inline]
pub fn attribute_bits(local_name: &str) -> Filter {
    bits(local_name, ATTRIBUTE_SEED)
}

/// Three bits (k = 3) from an FNV-1a hash of `name`, ASCII-lowercased, through Murmur3's finalizer,
/// whose output gives three 8-bit indices.
#[inline]
fn bits(name: &str, seed: u32) -> Filter {
    let mut h = seed;
    for &b in name.as_bytes() {
        h = hash_byte(h, b);
    }
    finish(h)
}

/// One byte of [`bits`]' FNV-1a hash.
#[inline(always)]
fn hash_byte(h: u32, b: u8) -> u32 {
    (h ^ b.to_ascii_lowercase() as u32).wrapping_mul(0x0100_0193)
}

/// [`bits`]' finalizer and bit selection.
#[inline(always)]
fn finish(mut h: u32) -> Filter {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    Filter::bit(h & 255) | Filter::bit((h >> 8) & 255) | Filter::bit((h >> 16) & 255)
}

/// The filter bits of attribute local names, by interned name: each name's bits are computed once
/// and found again from the name's own hash (direct-mapped, [`NameBits::SIZE`] entries), so an
/// attribute appended costs no hashing of its name.
#[derive(Debug)]
pub(crate) struct NameBits(Box<[Option<(LocalName, Filter)>; NameBits::SIZE]>);

impl NameBits {
    const SIZE: usize = 64;

    pub(crate) fn new() -> NameBits {
        NameBits(Box::new(std::array::from_fn(|_| None)))
    }

    /// [`attribute_bits`] of `name`.
    #[inline]
    pub(crate) fn get(&mut self, name: &LocalName) -> Filter {
        // (a short name's hash is its bytes as they are packed in the atom: mixed before use)
        let h = name.get_hash();
        let i = ((h ^ (h >> 32)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58) as usize;
        let slot = &mut self.0[i % Self::SIZE];
        if let Some((n, f)) = slot {
            if n == name {
                return *f;
            }
        }
        let f = attribute_bits(name);
        *slot = Some((name.clone(), f));
        f
    }
}

/// Whether a node with filter `filter` could hold every name of `needed`.
#[inline(always)]
pub fn could_match(filter: Filter, needed: Filter) -> bool {
    (filter.0[0] & needed.0[0]) == needed.0[0]
        && (filter.0[1] & needed.0[1]) == needed.0[1]
        && (filter.0[2] & needed.0[2]) == needed.0[2]
        && (filter.0[3] & needed.0[3]) == needed.0[3]
}

/// The bits of each token of a class attribute's value ([`class_bits`] of each, hashed as the
/// value is scanned: no byte of a multi-byte UTF-8 sequence is ASCII whitespace, so splitting the
/// bytes splits the characters).
fn class_value_bits(value: &str) -> Filter {
    let mut f = Filter::EMPTY;
    let mut h = CLASS_SEED;
    let mut in_token = false;
    for &b in value.as_bytes() {
        if b.is_ascii_whitespace() {
            if in_token {
                f |= finish(h);
                h = CLASS_SEED;
                in_token = false;
            }
        } else {
            h = hash_byte(h, b);
            in_token = true;
        }
    }
    if in_token {
        f |= finish(h);
    }
    f
}

fn is_class(attr: &Attr) -> bool {
    attr.name.ns == ns!() && attr.name.local == local_name!("class")
}

/// The bits an attribute adds to its element's filter.
pub(crate) fn bits_of_attribute(attr: &Attr) -> Filter {
    attribute_bits(&attr.name.local) | bits_of_changed_value(attr)
}

/// The bits a changed value adds: only a class attribute's value is in the filter.
pub(crate) fn bits_of_changed_value(attr: &Attr) -> Filter {
    if is_class(attr) {
        class_value_bits(&attr.value)
    } else {
        Filter::EMPTY
    }
}

impl Dom {
    /// The subtree filter of `node`: see the module documentation. Only elements have bits (a node of
    /// another kind under a query root has no children); for others it is empty.
    #[inline(always)]
    pub fn subtree_filter(&self, node: NodeId) -> Filter {
        debug_assert!(self.is_alive(node));
        self.filters[node.index as usize]
    }

    /// Adds `bits` to the filters of `node` and its element ancestors (stopping at the first that has
    /// them all, whose ancestors have them too, or at a node that is not an element).
    pub(crate) fn add_filter_bits(&mut self, node: NodeId, bits: Filter) {
        if bits.is_empty() {
            return;
        }
        let mut cur = Some(node);
        while let Some(id) = cur {
            let n = self.n(id);
            if !matches!(n.data, NodeData::Element(_)) {
                return;
            }
            let parent = n.parent;
            let f = &mut self.filters[id.index as usize];
            if could_match(*f, bits) {
                return;
            }
            *f |= bits;
            cur = parent;
        }
    }

    /// Whether the tree under `node` (an element and its descendants, or the children of a document,
    /// fragment or shadow root, not including shadow trees inside) could hold an HTML `slot` element.
    /// False only when it holds none; true may also be a filter bit shared with a name, or left by a
    /// removed slot.
    pub fn may_hold_slots(&self, node: NodeId) -> bool {
        match self.n(node).data {
            NodeData::Element(_) => could_match(self.subtree_filter(node), SLOT_ELEMENT),
            NodeData::Document(_) | NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) => {
                self.children(node)
                    .any(|c| could_match(self.subtree_filter(c), SLOT_ELEMENT))
            }
            _ => false,
        }
    }

    /// Panics where an element's filter lacks the bits of one of its attributes or children
    /// (computed here without [`NameBits`]), or a `slot` element's lacks [`SLOT_ELEMENT`].
    #[doc(hidden)]
    pub fn check_subtree_filters(&self, root: NodeId) {
        for id in self.inclusive_descendants(root) {
            let Some(e) = self.element(id) else {
                continue;
            };
            let f = self.subtree_filter(id);
            if e.name().ns == ns!(html) && e.name().local == local_name!("slot") {
                assert!(could_match(f, SLOT_ELEMENT), "slot {id:?} lacks its bits");
            }
            for a in e.attrs() {
                assert!(
                    could_match(f, bits_of_attribute(a)),
                    "{id:?} lacks the bits of attribute {:?}",
                    a.name.local
                );
            }
            for c in self.children(id) {
                assert!(
                    could_match(f, self.subtree_filter(c)),
                    "{id:?} lacks the bits of child {c:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache gives each name the bits [`attribute_bits`] computes: static, short (packed in the
    /// atom) and long interned names, names that differ only in case, and more names than it has
    /// entries, asked for twice over in another order.
    #[test]
    fn cached_name_bits_are_the_names_bits() {
        let mut names: Vec<String> = ["class", "id", "href", "data-id", "viewBox", "viewbox", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        names.extend((0..300).map(|i| format!("data-attribute-{i}")));
        names.extend((0..300).map(|i| format!("d{i}")));
        let mut cache = NameBits::new();
        for round in 0..2 {
            let order: Vec<&String> = if round == 0 {
                names.iter().collect()
            } else {
                names.iter().rev().collect()
            };
            for n in order {
                let atom = LocalName::from(n.as_str());
                assert_eq!(cache.get(&atom), attribute_bits(n), "{n}");
                assert_eq!(cache.get(&atom), attribute_bits(n), "{n} again");
            }
        }
    }

    /// A class value's bits are its tokens' [`class_bits`], however the tokens are separated.
    #[test]
    fn class_value_bits_are_the_tokens_bits() {
        for value in [
            "",
            " ",
            "a",
            "a b",
            "  todo-list\tview\n\x0cmain\r",
            "édition  ünïcode\u{a0}nbsp",
            "UPPER lower",
            "x y z w v u t s r q p o n m l k j i",
        ] {
            let want = value
                .split(|c: char| c.is_ascii_whitespace())
                .filter(|t| !t.is_empty())
                .fold(Filter::EMPTY, |acc, t| acc | class_bits(t));
            assert_eq!(class_value_bits(value), want, "{value:?}");
        }
    }
}
