// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The wrapper cache: handle to wrapper, read on every call that returns a platform object.
//!
//! A node's handle carries its index in the DOM's node store, so node wrappers sit in pages by that
//! index and a read is two bounds checks and one compare. Everything else, and a node whose index still
//! holds the entry of an earlier generation, goes in a hash map.
//!
//! **An entry never moves** between its insertion and its removal: V8 writes to a weak wrapper's handle
//! when its object dies (`snowlark_js::wrapper_ref`). Pages are allocated whole and never resized, and
//! the map's entries are boxed. Replacing or removing an entry drops it where it is.

use super::gc::Wrapper;
use super::HandleHash;
use std::collections::HashMap;

/// Above this node index a wrapper goes in the map, so that a stray handle cannot grow the table
/// without limit.
const MAX_INDEX: usize = 1 << 28;
/// Entries per page.
const PAGE: usize = 1024;

type Page<W> = Box<[Option<(u64, W)>]>;

pub(crate) struct WrapperTable<W = Wrapper> {
    /// By node index, with the full handle.
    pages: Vec<Option<Page<W>>>,
    rest: HashMap<u64, Box<W>, HandleHash>,
}

impl<W> Default for WrapperTable<W> {
    fn default() -> Self {
        WrapperTable {
            pages: Vec::new(),
            rest: HashMap::default(),
        }
    }
}

/// The index a node's handle has in the pages; other handles have a zero high half.
#[inline(always)]
fn index(handle: u64) -> Option<usize> {
    (handle >> 32 != 0).then_some(handle as u32 as usize)
}

impl<W> WrapperTable<W> {
    #[inline(always)]
    fn slot(&self, i: usize) -> Option<&Option<(u64, W)>> {
        self.pages.get(i / PAGE)?.as_ref().map(|p| &p[i % PAGE])
    }

    #[inline]
    pub(crate) fn get(&self, handle: &u64) -> Option<&W> {
        if let Some(Some((key, wrapper))) = index(*handle).and_then(|i| self.slot(i)) {
            if key == handle {
                return Some(wrapper);
            }
        }
        if self.rest.is_empty() {
            return None;
        }
        self.rest.get(handle).map(|w| &**w)
    }

    /// Caches `wrapper` for `handle`, dropping the entry it replaces where it is.
    pub(crate) fn insert(&mut self, handle: u64, wrapper: W) {
        if let Some(i) = index(handle).filter(|&i| i < MAX_INDEX) {
            let p = i / PAGE;
            if p >= self.pages.len() {
                self.pages.resize_with(p + 1, || None);
            }
            let page = self.pages[p].get_or_insert_with(|| (0..PAGE).map(|_| None).collect());
            match &mut page[i % PAGE] {
                Some((key, old)) if *key == handle => {
                    *old = wrapper;
                    return;
                }
                Some(_) => {}
                empty => {
                    *empty = Some((handle, wrapper));
                    return;
                }
            }
        }
        match self.rest.get_mut(&handle) {
            Some(old) => **old = wrapper,
            None => {
                self.rest.insert(handle, Box::new(wrapper));
            }
        }
    }

    /// Drops `handle`'s entry where it is; false when it had none.
    pub(crate) fn remove(&mut self, handle: &u64) -> bool {
        if let Some(i) = index(*handle) {
            if let Some(Some(page)) = self.pages.get_mut(i / PAGE) {
                let slot = &mut page[i % PAGE];
                if slot.as_ref().is_some_and(|(key, _)| key == handle) {
                    *slot = None;
                    return true;
                }
            }
        }
        self.rest.remove(handle).is_some()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&u64, &W)> {
        self.pages
            .iter()
            .flatten()
            .flat_map(|page| page.iter().flatten())
            .map(|(key, wrapper)| (key, wrapper))
            .chain(self.rest.iter().map(|(key, wrapper)| (key, &**wrapper)))
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &W> {
        self.iter().map(|(_, wrapper)| wrapper)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node's handle: its generation in the high half.
    fn node(generation: u64, index: u64) -> u64 {
        generation << 32 | index
    }

    #[test]
    fn a_node_and_an_object_with_the_same_low_half_are_different_entries() {
        let mut t = WrapperTable::default();
        t.insert(node(1, 5), "node");
        t.insert(5, "object");
        assert_eq!(t.get(&node(1, 5)), Some(&"node"));
        assert_eq!(t.get(&5), Some(&"object"));
    }

    #[test]
    fn an_index_holds_one_generation_and_the_map_the_others() {
        let mut t = WrapperTable::default();
        t.insert(node(1, 5), "old");
        // (an index reused while the earlier generation's entry is still there)
        t.insert(node(2, 5), "new");
        assert_eq!(t.get(&node(1, 5)), Some(&"old"));
        assert_eq!(t.get(&node(2, 5)), Some(&"new"));
        assert_eq!(t.get(&node(3, 5)), None);
        assert!(t.remove(&node(1, 5)));
        assert_eq!(t.get(&node(1, 5)), None);
        assert_eq!(t.get(&node(2, 5)), Some(&"new"));
        assert!(t.remove(&node(2, 5)));
        assert!(!t.remove(&node(2, 5)));
        assert_eq!(t.iter().count(), 0);
    }

    #[test]
    fn a_new_generation_does_not_read_as_the_old_one() {
        let mut t = WrapperTable::default();
        t.insert(node(1, 5), "old");
        assert_eq!(t.get(&node(2, 5)), None);
        assert!(!t.remove(&node(2, 5)));
        assert_eq!(t.get(&node(1, 5)), Some(&"old"));
    }

    #[test]
    fn insert_replaces_the_entry() {
        let mut t = WrapperTable::default();
        t.insert(node(1, 2), 1);
        t.insert(node(1, 2), 2);
        assert_eq!(t.get(&node(1, 2)), Some(&2));
        t.insert(node(1, 2), 3);
        t.insert(9, 4);
        t.insert(9, 7);
        assert_eq!(t.get(&node(2, 2)), None);
        let mut all: Vec<(u64, i32)> = t.iter().map(|(&k, &v)| (k, v)).collect();
        all.sort();
        assert_eq!(all, vec![(9, 7), (node(1, 2), 3)]);
        assert_eq!(t.values().count(), 2);
    }

    #[test]
    fn an_index_past_the_limit_goes_in_the_map() {
        let mut t = WrapperTable::default();
        let h = node(1, MAX_INDEX as u64);
        t.insert(h, "far");
        assert_eq!(t.get(&h), Some(&"far"));
        assert_eq!(t.pages.len(), 0);
        assert!(t.remove(&h));
    }

    #[test]
    fn entries_stay_where_they_are_as_the_table_grows() {
        let mut t = WrapperTable::default();
        t.insert(node(1, 3), 1u32);
        t.insert(77, 2u32);
        let a = t.get(&node(1, 3)).unwrap() as *const u32;
        let b = t.get(&77).unwrap() as *const u32;
        for i in 0..(PAGE as u64 * 8) {
            t.insert(node(1, i + 10), 0);
            t.insert(1000 + i, 0);
        }
        assert_eq!(t.get(&node(1, 3)).unwrap() as *const u32, a);
        assert_eq!(t.get(&77).unwrap() as *const u32, b);
    }
}
