// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text access: `CharacterData` (DOM §4.10), `Text` (§4.11), `textContent` / `nodeValue` (§4.4),
//! `normalize()`.
//!
//! DOM strings are sequences of UTF-16 code units; Snowlark stores them as UTF-8 `String`s and converts
//! offsets at the boundary. An offset that splits a surrogate pair can't be represented in UTF-8: the half
//! that remains becomes U+FFFD. Every other case is exact.

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::node::{NodeData, NodeId};
use crate::records::{MutationKind, Reaction};

/// Length of `s` in UTF-16 code units.
#[inline]
pub fn utf16_len(s: &str) -> usize {
    if s.is_ascii() {
        return s.len();
    }
    s.bytes()
        .map(|b| usize::from((b & 0xC0) != 0x80) + usize::from(b >= 0xF0))
        .sum()
}

/// Byte offset of UTF-16 offset `units` in `s`: `Ok` on a character boundary, `Err(byte offset of the
/// character)` when `units` falls inside a surrogate pair, and `Ok(s.len())` past the end.
pub fn utf16_to_byte_offset(s: &str, units: usize) -> Result<usize, usize> {
    if s.is_ascii() {
        return Ok(units.min(s.len()));
    }
    let mut count = 0;
    for (i, c) in s.char_indices() {
        if count == units {
            return Ok(i);
        }
        let w = c.len_utf16();
        if count + w > units {
            return Err(i);
        }
        count += w;
    }
    Ok(s.len())
}

/// The code units `[offset, offset + count)` of `s` (clamped to its end), as a `String`.
pub fn utf16_substring(s: &str, offset: usize, count: usize) -> String {
    let end = offset.saturating_add(count);
    match (
        utf16_to_byte_offset(s, offset),
        utf16_to_byte_offset(s, end),
    ) {
        (Ok(a), Ok(b)) => s[a..b].to_owned(),
        _ => {
            let units: Vec<u16> = s.encode_utf16().collect();
            let end = end.min(units.len());
            String::from_utf16_lossy(&units[offset.min(end)..end])
        }
    }
}

/// `s` with code units `[offset, offset + count)` replaced by `data`.
pub fn utf16_splice(s: &mut String, offset: usize, count: usize, data: &str) {
    let end = offset.saturating_add(count);
    match (
        utf16_to_byte_offset(s, offset),
        utf16_to_byte_offset(s, end),
    ) {
        (Ok(a), Ok(b)) => s.replace_range(a..b, data),
        _ => {
            let mut units: Vec<u16> = s.encode_utf16().collect();
            let end = end.min(units.len());
            units.splice(offset.min(end)..end, data.encode_utf16());
            *s = String::from_utf16_lossy(&units);
        }
    }
}

impl Dom {
    /// The data of a CharacterData node (Text, CDATASection, Comment, ProcessingInstruction).
    #[inline]
    pub fn character_data(&self, id: NodeId) -> Option<&str> {
        match &self.n(id).data {
            NodeData::Text(s) | NodeData::CDataSection(s) | NodeData::Comment(s) => Some(s),
            NodeData::ProcessingInstruction { data, .. } => Some(data),
            _ => None,
        }
    }

    fn character_data_mut(&mut self, id: NodeId) -> &mut String {
        match &mut self.n_mut(id).data {
            NodeData::Text(s) | NodeData::CDataSection(s) | NodeData::Comment(s) => s.make_mut(),
            NodeData::ProcessingInstruction { data, .. } => data,
            _ => panic!("not a CharacterData node"),
        }
    }

    /// Appends `data` to the node's data (without measuring it); short data stays in the node.
    fn append_character_data(&mut self, id: NodeId, data: &str) {
        match &mut self.n_mut(id).data {
            NodeData::Text(s) | NodeData::CDataSection(s) | NodeData::Comment(s) => {
                let end = s.len();
                s.splice(end..end, data)
            }
            NodeData::ProcessingInstruction { data: d, .. } => d.push_str(data),
            _ => panic!("not a CharacterData node"),
        }
    }

    /// Replaces the code units `[offset, offset + count)` of the node's data (clamped to its end)
    /// with `data`, as [`utf16_splice`]; short data stays in the node.
    fn splice_character_data(&mut self, id: NodeId, offset: usize, count: usize, data: &str) {
        let s = self.character_data(id).expect("not CharacterData");
        let end = offset.saturating_add(count);
        let (Ok(a), Ok(b)) = (
            utf16_to_byte_offset(s, offset),
            utf16_to_byte_offset(s, end),
        ) else {
            // (an offset inside a surrogate pair)
            utf16_splice(self.character_data_mut(id), offset, count, data);
            return;
        };
        match &mut self.n_mut(id).data {
            NodeData::Text(s) | NodeData::CDataSection(s) | NodeData::Comment(s) => {
                s.splice(a..b, data)
            }
            NodeData::ProcessingInstruction { data: d, .. } => d.replace_range(a..b, data),
            _ => unreachable!("CharacterData checked above"),
        }
    }

    /// The DOM "length" of a node (DOM §4.2): 0 for doctypes and Attrs, UTF-16 length of the data for
    /// CharacterData, the number of children otherwise.
    pub fn length(&self, id: NodeId) -> usize {
        match &self.n(id).data {
            NodeData::DocumentType(_) | NodeData::Attr(_) => 0,
            _ => match self.character_data(id) {
                Some(s) => utf16_len(s),
                None => self.child_count(id),
            },
        }
    }

    /// `CharacterData.substringData(offset, count)`.
    pub fn substring_data(&self, id: NodeId, offset: usize, count: usize) -> DomResult<String> {
        let data = self.character_data(id).expect("not CharacterData");
        if offset > utf16_len(data) {
            return Err(DomError::IndexSize);
        }
        Ok(utf16_substring(data, offset, count))
    }

    /// DOM §4.10 "replace data" (`replaceData`; `appendData`, `insertData` and `deleteData` are special
    /// cases, see below).
    pub fn replace_data(
        &mut self,
        id: NodeId,
        offset: usize,
        count: usize,
        data: &str,
    ) -> DomResult<()> {
        // 1-3.
        let length = utf16_len(self.character_data(id).expect("not CharacterData"));
        if offset > length {
            return Err(DomError::IndexSize);
        }
        let count = count.min(length - offset);
        // 4.
        self.queue_value_record(MutationKind::CharacterData, id, None, |d| {
            d.character_data(id).map(str::to_owned)
        });
        // 5-6.
        if offset == length {
            self.append_character_data(id, data);
        } else {
            self.splice_character_data(id, offset, count, data);
        }
        // 8-11. (step 7 has no observable effect here: ranges' offsets are code units, as the data's
        // are; the layer above moves its live ranges)
        if offset < length {
            let added = utf16_len(data) as u32;
            self.react(|| Reaction::DataReplaced {
                node: id,
                offset: offset as u32,
                count: count as u32,
                added,
            });
        }
        self.data_replaced(id);
        Ok(())
    }

    /// Step 12 of "replace data", after the data changed.
    fn data_replaced(&mut self, id: NodeId) {
        // 12.
        if let Some(parent) = self.parent(id) {
            self.react(|| Reaction::ChildrenChanged { parent });
        }
        self.bump(id);
    }

    /// `CharacterData.appendData(data)`, and the HTML and XML parsers' "insert a character" into an
    /// adjacent Text node: "replace data" with offset and count the node's length and 0. The offset is
    /// always valid and nothing is replaced, so the data is never measured: the cost is that of `data`
    /// alone (the parsers append text runs one by one to the same node, so measuring it each time made
    /// a long text node quadratic in its length).
    pub fn append_data(&mut self, id: NodeId, data: &str) {
        // 4.
        self.queue_value_record(MutationKind::CharacterData, id, None, |d| {
            d.character_data(id).map(str::to_owned)
        });
        // 5-6.
        self.append_character_data(id, data);
        self.data_replaced(id);
    }

    /// `CharacterData.insertData(offset, data)`.
    pub fn insert_data(&mut self, id: NodeId, offset: usize, data: &str) -> DomResult<()> {
        self.replace_data(id, offset, 0, data)
    }

    /// `CharacterData.deleteData(offset, count)`.
    pub fn delete_data(&mut self, id: NodeId, offset: usize, count: usize) -> DomResult<()> {
        self.replace_data(id, offset, count, "")
    }

    /// `CharacterData.data` setter: replace data with offset 0 and count the length.
    pub fn set_data(&mut self, id: NodeId, data: &str) {
        let len = self.length(id);
        self.replace_data(id, 0, len, data).expect("offset 0");
    }

    /// `Text.splitText(offset)` (DOM §4.11 "split a Text node").
    pub fn split_text(&mut self, id: NodeId, offset: usize) -> DomResult<NodeId> {
        // 1-2.
        let length = self.length(id);
        if offset > length {
            return Err(DomError::IndexSize);
        }
        // 3-5.
        let count = length - offset;
        let new_data = self.substring_data(id, offset, count)?;
        let doc = self.node_document(id);
        let new_node = self.create_text_node(doc, &new_data);
        // 6-7.
        if let Some(parent) = self.parent(id) {
            let next = self.next_sibling(id);
            self.insert(new_node, parent, next, false);
            // 7.2-7.5 (the layer above moves its live ranges)
            if self.reactions_enabled {
                let index = self.index(id) as u32;
                self.react(|| Reaction::TextSplit {
                    node: id,
                    new_node,
                    offset: offset as u32,
                    parent,
                    index,
                });
            }
        }
        // 8.
        self.replace_data(id, offset, count, "")?;
        Ok(new_node)
    }

    /// `Text.wholeText`: the data of the contiguous Text nodes of `id`, in tree order.
    pub fn whole_text(&self, id: NodeId) -> String {
        let mut first = id;
        while let Some(p) = self.previous_sibling(first).filter(|&p| self.is_text(p)) {
            first = p;
        }
        let mut out = String::new();
        let mut cur = Some(first);
        while let Some(c) = cur.filter(|&c| self.is_text(c)) {
            out.push_str(self.character_data(c).unwrap_or(""));
            cur = self.next_sibling(c);
        }
        out
    }

    /// DOM "descendant text content": the data of all Text descendants, in tree order.
    pub fn descendant_text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        for d in self.descendants(id) {
            if let NodeData::Text(s) | NodeData::CDataSection(s) = &self.n(d).data {
                out.push_str(s);
            }
        }
        out
    }

    /// DOM "child text content": the data of the Text children, in tree order.
    pub fn child_text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        for c in self.children(id) {
            if let NodeData::Text(s) | NodeData::CDataSection(s) = &self.n(c).data {
                out.push_str(s);
            }
        }
        out
    }

    /// `Node.textContent` getter (DOM "get text content").
    pub fn text_content(&self, id: NodeId) -> Option<String> {
        match &self.n(id).data {
            NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) | NodeData::Element(_) => {
                Some(self.descendant_text_content(id))
            }
            NodeData::Attr(_) => Some(self.attr_value(id).to_owned()),
            NodeData::Text(_)
            | NodeData::CDataSection(_)
            | NodeData::Comment(_)
            | NodeData::ProcessingInstruction { .. } => self.character_data(id).map(str::to_owned),
            _ => None,
        }
    }

    /// DOM "string replace all" with `string` within `parent`.
    pub fn string_replace_all(&mut self, string: &str, parent: NodeId) {
        let node = (!string.is_empty()).then(|| {
            let doc = self.node_document(parent);
            self.create_text_node(doc, string)
        });
        self.replace_all(node, parent);
    }

    /// `Node.textContent` setter (DOM "set text content"); null is the empty string.
    pub fn set_text_content(&mut self, id: NodeId, value: &str) {
        match &self.n(id).data {
            NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) | NodeData::Element(_) => {
                self.string_replace_all(value, id)
            }
            NodeData::Attr(_) => self.set_attr_value(id, value),
            NodeData::Text(_)
            | NodeData::CDataSection(_)
            | NodeData::Comment(_)
            | NodeData::ProcessingInstruction { .. } => self.set_data(id, value),
            _ => {}
        }
    }

    /// `Node.nodeValue` getter.
    pub fn node_value(&self, id: NodeId) -> Option<&str> {
        match &self.n(id).data {
            NodeData::Attr(_) => Some(self.attr_value(id)),
            _ => self.character_data(id),
        }
    }

    /// `Node.nodeValue` setter; null is the empty string.
    pub fn set_node_value(&mut self, id: NodeId, value: &str) {
        match &self.n(id).data {
            NodeData::Attr(_) => self.set_attr_value(id, value),
            _ if self.is_character_data(id) => self.set_data(id, value),
            _ => {}
        }
    }

    /// `Node.normalize()` (DOM §4.4).
    pub fn normalize(&mut self, id: NodeId) {
        let mut cur = self.first_child(id);
        while let Some(node) = cur {
            if !self.is_exclusive_text(node) {
                cur = self.next_in_tree_order(node, Some(id));
                continue;
            }
            // 1-2. Empty Text nodes are removed.
            let length = self.length(node);
            if length == 0 {
                cur = self.next_in_tree_order(node, Some(id));
                self.remove(node, false);
                continue;
            }
            // 3. The data of the contiguous exclusive Text nodes after it.
            let mut data = String::new();
            let mut following = Vec::new();
            let mut sib = self.next_sibling(node);
            while let Some(s) = sib.filter(|&s| self.is_exclusive_text(s)) {
                data.push_str(self.character_data(s).unwrap_or(""));
                following.push(s);
                sib = self.next_sibling(s);
            }
            // 4.
            if !following.is_empty() {
                self.replace_data(node, length, 0, &data)
                    .expect("offset is the length");
                // 5-6. (the layer above moves its live ranges)
                if self.reactions_enabled {
                    let mut at = length as u32;
                    for &s in &following {
                        let (parent, index) = (self.parent(s), self.index(s) as u32);
                        let len = self.length(s) as u32;
                        if let Some(parent) = parent {
                            self.react(|| Reaction::TextMerged {
                                from: s,
                                into: node,
                                at,
                                parent,
                                index,
                            });
                        }
                        at += len;
                    }
                }
                // 7.
                for s in following {
                    self.remove(s, false);
                }
            }
            cur = self.next_in_tree_order(node, Some(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("a\u{e9}\u{1F600}"), 4);
        assert_eq!(utf16_to_byte_offset("a\u{1F600}b", 1), Ok(1));
        assert_eq!(utf16_to_byte_offset("a\u{1F600}b", 2), Err(1));
        assert_eq!(utf16_to_byte_offset("a\u{1F600}b", 3), Ok(5));
        assert_eq!(utf16_substring("a\u{1F600}b", 1, 2), "\u{1F600}");
        assert_eq!(utf16_substring("a\u{1F600}b", 1, 1), "\u{FFFD}");
        let mut s = String::from("h\u{e9}llo");
        utf16_splice(&mut s, 1, 1, "e");
        assert_eq!(s, "hello");
        let mut s = String::from("\u{1F600}");
        utf16_splice(&mut s, 1, 1, "");
        assert_eq!(s, "\u{FFFD}");
    }
}
