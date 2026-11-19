// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs "canonical space sequences" and "canonicalize whitespace": a run of spaces
//! around a position becomes the alternation of spaces and no-break spaces that renders as that
//! many spaces.

use snowlark_bindings::rt::Cx;
use snowlark_dom::NodeId;

use crate::engine::Engine;

const NBSP: char = '\u{a0}';

/// "The canonical space sequence" of length `n`.
pub(super) fn canonical_space_sequence(
    mut n: u32,
    non_breaking_start: bool,
    non_breaking_end: bool,
) -> Vec<char> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![if !non_breaking_start && !non_breaking_end {
            ' '
        } else {
            NBSP
        }];
    }
    let mut buffer = Vec::new();
    let pair = if non_breaking_start {
        [NBSP, ' ']
    } else {
        [' ', NBSP]
    };
    while n > 3 {
        buffer.extend_from_slice(&pair);
        n -= 2;
    }
    let tail: &[char] = if n == 3 {
        match (non_breaking_start, non_breaking_end) {
            (false, false) => &[' ', NBSP, ' '],
            (true, false) => &[NBSP, NBSP, ' '],
            (false, true) => &[' ', NBSP, NBSP],
            (true, true) => &[NBSP, ' ', NBSP],
        }
    } else {
        match (non_breaking_start, non_breaking_end) {
            (false, false) | (true, false) => &[NBSP, ' '],
            (false, true) => &[' ', NBSP],
            (true, true) => &[NBSP, NBSP],
        }
    };
    buffer.extend_from_slice(tail);
    buffer
}

impl Engine {
    /// The UTF-16 code unit of a Text node at `i`.
    pub(super) fn ec_unit(&self, t: NodeId, i: u32) -> Option<u16> {
        let dom = self.dom();
        if !dom.is_text(t) {
            return None;
        }
        dom.character_data(t)?.encode_utf16().nth(i as usize)
    }

    fn ec_space_at(&self, t: NodeId, i: u32) -> bool {
        matches!(self.ec_unit(t, i), Some(0x20) | Some(0xa0))
    }

    /// "Canonicalize whitespace" at (`node`, `offset`).
    pub(super) fn ec_canonicalize_whitespace(
        &self,
        cx: &mut Cx,
        node: NodeId,
        offset: u32,
        fix_collapsed_space: bool,
    ) {
        if !self.ec_editable_or_host(node) {
            return;
        }
        let (mut start_node, mut start_offset) = (node, offset);
        loop {
            let before = self
                .ec_child_before(start_node, start_offset)
                .filter(|&b| self.ec_same_host(start_node, Some(b)));
            if let Some(b) = before {
                start_node = b;
                start_offset = self.ec_len(start_node);
            } else if start_offset == 0
                && !self.ec_follows_line_break(start_node)
                && self.ec_same_host(start_node, self.ec_parent(start_node))
            {
                start_offset = self.ec_index(start_node);
                start_node = self.ec_parent(start_node).unwrap();
            } else if self.dom().is_text(start_node)
                && start_offset != 0
                && !self.ec_pre(start_node)
                && self.ec_space_at(start_node, start_offset - 1)
            {
                start_offset -= 1;
            } else {
                break;
            }
        }
        let (mut end_node, mut end_offset) = (start_node, start_offset);
        let mut length = 0u32;
        let mut collapse_spaces = start_offset == 0 && self.ec_follows_line_break(start_node);
        loop {
            let at = self
                .ec_child(end_node, end_offset)
                .filter(|&c| self.ec_same_host(end_node, Some(c)));
            if let Some(c) = at {
                end_node = c;
                end_offset = 0;
            } else if end_offset == self.ec_len(end_node)
                && !self.ec_precedes_line_break(end_node)
                && self.ec_same_host(end_node, self.ec_parent(end_node))
            {
                end_offset = 1 + self.ec_index(end_node);
                end_node = self.ec_parent(end_node).unwrap();
            } else if self.dom().is_text(end_node)
                && end_offset != self.ec_len(end_node)
                && !self.ec_pre(end_node)
                && self.ec_space_at(end_node, end_offset)
            {
                let is_space = self.ec_unit(end_node, end_offset) == Some(0x20);
                if fix_collapsed_space && collapse_spaces && is_space {
                    self.ec_delete_data(cx, end_node, end_offset, 1);
                    continue;
                }
                collapse_spaces = is_space;
                end_offset += 1;
                length += 1;
            } else {
                break;
            }
        }
        if fix_collapsed_space {
            while self.ec_bp_cmp((start_node, start_offset), (end_node, end_offset))
                == std::cmp::Ordering::Less
            {
                let before = self
                    .ec_child_before(end_node, end_offset)
                    .filter(|&b| self.ec_same_host(end_node, Some(b)));
                if let Some(b) = before {
                    end_node = b;
                    end_offset = self.ec_len(end_node);
                } else if end_offset == 0 && self.ec_same_host(end_node, self.ec_parent(end_node)) {
                    end_offset = self.ec_index(end_node);
                    end_node = self.ec_parent(end_node).unwrap();
                } else if self.dom().is_text(end_node)
                    && !self.ec_pre(end_node)
                    && end_offset == self.ec_len(end_node)
                    && end_offset > 0
                    && self.ec_unit(end_node, end_offset - 1) == Some(0x20)
                    && self.ec_precedes_line_break(end_node)
                {
                    end_offset -= 1;
                    length = length.saturating_sub(1);
                    self.ec_delete_data(cx, end_node, end_offset, 1);
                } else {
                    break;
                }
            }
        }
        let mut replacement = canonical_space_sequence(
            length,
            start_offset == 0 && self.ec_follows_line_break(start_node),
            end_offset == self.ec_len(end_node) && self.ec_precedes_line_break(end_node),
        )
        .into_iter();
        while self.ec_bp_cmp((start_node, start_offset), (end_node, end_offset))
            == std::cmp::Ordering::Less
        {
            if let Some(c) = self.ec_child(start_node, start_offset) {
                start_node = c;
                start_offset = 0;
            } else if !self.dom().is_text(start_node) || start_offset == self.ec_len(start_node) {
                let Some(p) = self.ec_parent(start_node) else {
                    break;
                };
                start_offset = 1 + self.ec_index(start_node);
                start_node = p;
            } else {
                let Some(element) = replacement.next() else {
                    break;
                };
                if self.ec_unit(start_node, start_offset) != Some(element as u16) {
                    let mut buf = [0u8; 4];
                    self.ec_insert_data(
                        cx,
                        start_node,
                        start_offset,
                        element.encode_utf8(&mut buf),
                    );
                    self.ec_delete_data(cx, start_node, start_offset + 1, 1);
                }
                start_offset += 1;
            }
        }
    }
}
