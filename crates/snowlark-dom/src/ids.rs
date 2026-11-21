// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The id and name index of a document: for each `id` value, the connected elements that have it, and
//! for each `name` value, how many connected elements have it. Shadow trees are included (their elements
//! are connected), so a lookup filters by tree.
//!
//! `getElementById` (DOM §4.2.4) and the named properties of `Window` and `Document` (HTML §7.2.2.3,
//! §3.1.6) read it instead of walking the document; the named properties only ask whether a name can be
//! there at all, then walk as before when it can. Kept where the CONNECTED flag changes
//! ([`Dom::set_connected`]) and where a connected element's `id` or `name` changes.

use std::collections::HashMap;

use html5ever::{local_name, ns};
use rustc_hash::FxBuildHasher;
use smallvec::SmallVec;

use crate::dom::Dom;
use crate::node::NodeId;

#[derive(Clone, Debug, Default)]
pub(crate) struct NameIndex {
    /// Unordered: the first in tree order is found when asked.
    ids: HashMap<Box<str>, SmallVec<[NodeId; 1]>, FxBuildHasher>,
    names: HashMap<Box<str>, u32, FxBuildHasher>,
}

/// Which attribute an index entry is for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Id,
    Name,
}

impl NameIndex {
    fn add(&mut self, key: Key, value: &str, element: NodeId) {
        if value.is_empty() {
            return;
        }
        match key {
            Key::Id => match self.ids.get_mut(value) {
                Some(list) => list.push(element),
                None => {
                    self.ids.insert(value.into(), smallvec::smallvec![element]);
                }
            },
            Key::Name => match self.names.get_mut(value) {
                Some(n) => *n += 1,
                None => {
                    self.names.insert(value.into(), 1);
                }
            },
        }
    }

    fn remove(&mut self, key: Key, value: &str, element: NodeId) {
        if value.is_empty() {
            return;
        }
        match key {
            Key::Id => {
                if let Some(list) = self.ids.get_mut(value) {
                    if let Some(i) = list.iter().position(|&e| e == element) {
                        list.swap_remove(i);
                    }
                    if list.is_empty() {
                        self.ids.remove(value);
                    }
                }
            }
            Key::Name => {
                if let Some(n) = self.names.get_mut(value) {
                    *n -= 1;
                    if *n == 0 {
                        self.names.remove(value);
                    }
                }
            }
        }
    }
}

/// The index key of an attribute (in no namespace), if it has one.
pub(crate) fn key_of(ns: &html5ever::Namespace, local: &html5ever::LocalName) -> Option<Key> {
    if *ns != ns!() {
        return None;
    }
    match *local {
        local_name!("id") => Some(Key::Id),
        local_name!("name") => Some(Key::Name),
        _ => None,
    }
}

impl Dom {
    /// Adds a connected element's `id` and `name` to its document's index, or removes them.
    pub(crate) fn index_element(&mut self, element: NodeId, connected: bool) {
        let Some(doc) = self.n(element).document else {
            return;
        };
        if !self
            .attrs(element)
            .iter()
            .any(|a| key_of(&a.name.ns, &a.name.local).is_some())
        {
            return;
        }
        // (out of the document while the element's attributes are read)
        let mut index = std::mem::take(&mut self.doc_mut(doc).names);
        for a in self.attrs(element) {
            if let Some(key) = key_of(&a.name.ns, &a.name.local) {
                if connected {
                    index.add(key, &a.value, element);
                } else {
                    index.remove(key, &a.value, element);
                }
            }
        }
        self.doc_mut(doc).names = index;
    }

    /// After a connected element's `id` or `name` changed from `old` to `new`.
    pub(crate) fn reindex_attribute(
        &mut self,
        element: NodeId,
        key: Key,
        old: Option<&str>,
        new: Option<&str>,
    ) {
        if !self.is_connected(element) {
            return;
        }
        let Some(doc) = self.n(element).document else {
            return;
        };
        let index = &mut self.doc_mut(doc).names;
        if let Some(old) = old {
            index.remove(key, old, element);
        }
        if let Some(new) = new {
            index.add(key, new, element);
        }
    }

    /// DOM `getElementById` on a connected document or shadow root `root`: the first element in tree
    /// order in `root`'s tree whose ID is `id`. `None` when `root` is not connected (the caller walks).
    pub fn indexed_element_by_id(&self, root: NodeId, id: &str) -> Option<Option<NodeId>> {
        if !self.is_connected(root) {
            return None;
        }
        let doc = self.n(root).document?;
        let Some(list) = self.doc(doc).names.ids.get(id) else {
            return Some(None);
        };
        // (with no shadow root ever made, every connected element is in its document's tree)
        let in_tree =
            |&e: &NodeId| (root == doc && !self.has_shadow_roots()) || self.root(e) == root;
        let first = list
            .iter()
            .filter(|e| in_tree(e))
            .copied()
            .min_by(|&a, &b| self.tree_order(a, b));
        Some(first)
    }

    /// Whether a connected element of `doc` (or of a shadow tree in it) may have `value` as its `id` or
    /// `name`: false means none has.
    pub fn may_have_id_or_name(&self, doc: NodeId, value: &str) -> bool {
        let index = &self.doc(doc).names;
        index.ids.contains_key(value) || index.names.contains_key(value)
    }

    /// HTML "select the indicated part" for a URL equal to `doc`'s URL apart from its fragment
    /// `fragment` (steps 3-9): the first element in tree order whose ID is the fragment, else the first
    /// `a` element whose `name` is, tried with the fragment as it is and then percent-decoded; the
    /// top of the document for an empty fragment or (decoded) "top" in any case.
    pub fn indicated_part(&self, doc: NodeId, fragment: &str) -> IndicatedPart {
        if fragment.is_empty() {
            return IndicatedPart::TopOfDocument;
        }
        if let Some(el) = self.potential_indicated_element(doc, fragment) {
            return IndicatedPart::Element(el);
        }
        let decoded = percent_decode(fragment);
        if let Some(el) = self.potential_indicated_element(doc, &decoded) {
            return IndicatedPart::Element(el);
        }
        if decoded.eq_ignore_ascii_case("top") {
            return IndicatedPart::TopOfDocument;
        }
        IndicatedPart::None
    }

    /// HTML "find a potential indicated element": an element of `doc`'s tree with ID `fragment`, else
    /// the first `a` element in tree order with a `name` attribute of `fragment`.
    fn potential_indicated_element(&self, doc: NodeId, fragment: &str) -> Option<NodeId> {
        if let Some(Some(el)) = self.indexed_element_by_id(doc, fragment) {
            return Some(el);
        }
        if !self.doc(doc).names.names.contains_key(fragment) {
            return None;
        }
        let root = self.document_element(doc)?;
        self.descendants(root).find(|&n| {
            self.is_html_element_named(n, &local_name!("a"))
                && self.get_attribute(n, "name") == Some(fragment)
        })
    }
}

/// What a fragment indicates in a document ([`Dom::indicated_part`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndicatedPart {
    Element(NodeId),
    TopOfDocument,
    None,
}

/// URL "percent-decode" of a string, then "UTF-8 decode without BOM" (a BOM stays; invalid sequences
/// become U+FFFD).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
