// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Find in page: the rendered text of a fragment tree, and searching it.
//!
//! [`find_items`] walks the fragments once and lists the text the page shows in the order layout
//! built it: text fragments (a node and the byte range of its DOM text), with a break where a block
//! or an atomic inline begins or ends (a match never crosses one) and a space between the lines of one
//! block (a match may wrap). Nothing that is not rendered is listed: `display: none` and `<script>`
//! have no fragments, `visibility: hidden` text and generated content (`::before`, markers) are left
//! out. [`FindIndex::build`] reads the DOM text of those ranges into runs of characters with white
//! space collapsed as rendering collapses it, remembering for each character the DOM byte it came
//! from, so that a match is mapped back to byte ranges of text nodes: a match may span inline
//! elements. [`FindIndex::search`] finds the case-insensitive matches of a query in slices of bounded
//! time, so a long document does not hold its tab up.
//!
//! The highlights are painted from the node ranges ([`FindHighlights`]) by the same pass that paints
//! text, so they follow scroll containers, clips and transforms and touch no DOM.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use snowlark_dom::NodeId;
use snowlark_paint::Color;

use crate::fragment::{fragment_flags, Fragment, FragmentKind};
use crate::style::{PseudoElement, Style, Visibility};

/// The most characters of rendered text an index holds (a document with more is searched up to
/// there, [`FindIndex::truncated`]).
pub const MAX_CHARS: usize = 8_000_000;

/// Yellow, as Chrome paints every match.
pub const MATCH_COLOR: Color = Color::rgb(0xFF, 0xFF, 0x00);
/// Orange, as Chrome paints the active match.
pub const ACTIVE_COLOR: Color = Color::rgb(0xFF, 0x96, 0x32);

/// One piece of the rendered text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindItem {
    /// The part of a text node's DOM text (bytes) a text fragment shows.
    Text { node: NodeId, range: Range<u32> },
    /// The line of a block ends and another follows: a space unless one is there.
    Space,
    /// A block or an atomic inline begins or ends.
    Break,
}

/// The rendered text of a fragment tree, in layout order.
pub fn find_items<S: Style>(root: &Fragment<S>) -> Vec<FindItem> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn walk<S: Style>(f: &Fragment<S>, out: &mut Vec<FindItem>) {
    match &f.kind {
        FragmentKind::Text(t) => {
            if let Some(node) = t.node {
                if t.style.visibility() == Visibility::Visible && !t.text_range.is_empty() {
                    match out.last_mut() {
                        // (the next piece of the same text: one range)
                        Some(FindItem::Text { node: n, range })
                            if *n == node && range.end == t.text_range.start =>
                        {
                            range.end = t.text_range.end;
                        }
                        _ => out.push(FindItem::Text {
                            node,
                            range: t.text_range.clone(),
                        }),
                    }
                }
            }
        }
        FragmentKind::Line { .. } => {
            for c in &f.children {
                walk(&c.fragment, out);
            }
        }
        FragmentKind::Box(b) => {
            // Generated content is not part of the page's text.
            if matches!(
                b.pseudo,
                Some(
                    PseudoElement::Before
                        | PseudoElement::After
                        | PseudoElement::Marker
                        | PseudoElement::Backdrop
                        | PseudoElement::DetailsSummary
                )
            ) || b.flags & fragment_flags::LIST_MARKER != 0
                // CSS Contain 2 `content-visibility: hidden`: skipped contents are not found.
                || b.style.contents_skipped()
            {
                return;
            }
            let inline = b.flags & fragment_flags::INLINE_BOX != 0;
            if !inline {
                out.push(FindItem::Break);
            }
            let mut lines = 0;
            for c in &f.children {
                if matches!(c.fragment.kind, FragmentKind::Line { .. }) {
                    if lines > 0 {
                        out.push(FindItem::Space);
                    }
                    lines += 1;
                }
                walk(&c.fragment, out);
            }
            if !inline {
                out.push(FindItem::Break);
            }
        }
    }
}

/// A run of text a match can lie in.
#[derive(Debug, Default)]
struct Run {
    /// The characters, white space collapsed to single spaces.
    chars: Vec<char>,
    /// The DOM byte offset of each character in its text node (`u32::MAX`: no text node, a space
    /// between lines).
    dom: Vec<u32>,
    /// The text node of each stretch of characters: (first character, node).
    pieces: Vec<(u32, NodeId)>,
}

impl Run {
    fn push_space(&mut self, dom: u32) {
        if self.chars.last().is_some_and(|&c| c != ' ') {
            self.chars.push(' ');
            self.dom.push(dom);
        }
    }

    /// The node holding character `i`.
    fn node_of(&self, i: usize) -> Option<NodeId> {
        let k = self
            .pieces
            .partition_point(|&(first, _)| first as usize <= i);
        k.checked_sub(1).map(|k| self.pieces[k].1)
    }
}

/// The searchable text of a document.
#[derive(Debug, Default)]
pub struct FindIndex {
    runs: Vec<Run>,
    chars: usize,
    truncated: bool,
}

/// One match: characters `start..end` of run `run`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FindMatch {
    pub run: u32,
    pub start: u32,
    pub end: u32,
}

/// Where a search stands: it resumes from here.
#[derive(Clone, Debug)]
pub struct FindCursor {
    query: Vec<char>,
    run: usize,
    pos: usize,
}

fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

/// A query as the search compares it: case folded, white space collapsed to single spaces.
pub fn fold_query(query: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for c in query.chars() {
        if c.is_whitespace() {
            if out.last().is_some_and(|&p| p != ' ') {
                out.push(' ');
            }
        } else {
            out.push(fold(c));
        }
    }
    while out.last() == Some(&' ') {
        out.pop();
    }
    out
}

impl FindIndex {
    /// Reads the text of `items`: `text_of` gives a text node's DOM text.
    pub fn build<'a>(items: &[FindItem], text_of: impl Fn(NodeId) -> Option<&'a str>) -> FindIndex {
        let mut index = FindIndex::default();
        let mut run = Run::default();
        let finish = |index: &mut FindIndex, run: &mut Run| {
            while run.chars.last() == Some(&' ') {
                run.chars.pop();
                run.dom.pop();
            }
            if !run.chars.is_empty() {
                index.chars += run.chars.len();
                index.runs.push(std::mem::take(run));
            } else {
                *run = Run::default();
            }
        };
        for item in items {
            if index.chars + run.chars.len() >= MAX_CHARS {
                index.truncated = true;
                break;
            }
            match item {
                FindItem::Break => finish(&mut index, &mut run),
                FindItem::Space => run.push_space(u32::MAX),
                FindItem::Text { node, range } => {
                    let Some(text) = text_of(*node) else {
                        continue;
                    };
                    let (lo, hi) = (range.start as usize, (range.end as usize).min(text.len()));
                    if lo >= hi || !text.is_char_boundary(lo) || !text.is_char_boundary(hi) {
                        continue;
                    }
                    run.pieces.push((run.chars.len() as u32, *node));
                    for (i, c) in text[lo..hi].char_indices() {
                        let at = (lo + i) as u32;
                        if c.is_whitespace() {
                            if run.chars.last().is_some_and(|&p| p != ' ') {
                                run.chars.push(' ');
                                run.dom.push(at);
                            }
                        } else {
                            run.chars.push(c);
                            run.dom.push(at);
                        }
                    }
                }
            }
        }
        finish(&mut index, &mut run);
        index
    }

    /// Whether the document had more text than the index holds.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// How many characters of text the index holds.
    pub fn len(&self) -> usize {
        self.chars
    }

    pub fn is_empty(&self) -> bool {
        self.chars == 0
    }

    /// The text of run `i` (for tests and the selection text).
    pub fn run_text(&self, i: usize) -> String {
        self.runs[i].chars.iter().collect()
    }

    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// A search for `query` from the start (`None`: the query is empty).
    pub fn start(&self, query: &str) -> Option<FindCursor> {
        let query = fold_query(query);
        (!query.is_empty()).then_some(FindCursor {
            query,
            run: 0,
            pos: 0,
        })
    }

    /// Continues a search until `deadline` (checked every few thousand characters) or `max`
    /// matches are found, appending matches in document order; true when the whole index has been
    /// searched. Matches do not overlap.
    pub fn search(
        &self,
        cursor: &mut FindCursor,
        out: &mut Vec<FindMatch>,
        deadline: Option<Instant>,
        max: usize,
    ) -> bool {
        let q = &cursor.query;
        let mut since_check = 0usize;
        while cursor.run < self.runs.len() {
            let chars = &self.runs[cursor.run].chars;
            let n = chars.len();
            let m = q.len();
            while cursor.pos + m <= n {
                if out.len() >= max {
                    return false;
                }
                let p = cursor.pos;
                if fold(chars[p]) == q[0] && (1..m).all(|k| fold(chars[p + k]) == q[k]) {
                    out.push(FindMatch {
                        run: cursor.run as u32,
                        start: p as u32,
                        end: (p + m) as u32,
                    });
                    cursor.pos = p + m;
                } else {
                    cursor.pos += 1;
                }
                since_check += 1;
                if since_check >= 8192 {
                    since_check = 0;
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return false;
                    }
                }
            }
            cursor.run += 1;
            cursor.pos = 0;
        }
        true
    }

    /// The text nodes a match covers, with the DOM byte range of each, in order.
    pub fn ranges(&self, m: &FindMatch, mut f: impl FnMut(NodeId, u32, u32)) {
        let run = &self.runs[m.run as usize];
        let (start, end) = (m.start as usize, m.end as usize);
        let mut i = start;
        while i < end {
            if run.dom[i] == u32::MAX {
                i += 1;
                continue;
            }
            let Some(node) = run.node_of(i) else {
                i += 1;
                continue;
            };
            // The stretch of this node: to the next piece's first character or the match's end.
            let k = run
                .pieces
                .partition_point(|&(first, _)| first as usize <= i);
            let stop = run
                .pieces
                .get(k)
                .map_or(end, |&(first, _)| first as usize)
                .min(end);
            let mut last = i;
            for j in i..stop {
                if run.dom[j] != u32::MAX {
                    last = j;
                }
            }
            let hi = run.dom[last] + run.chars[last].len_utf8() as u32;
            f(node, run.dom[i], hi);
            i = stop;
        }
    }

    /// The match's text (as the page shows it).
    pub fn text(&self, m: &FindMatch) -> String {
        self.runs[m.run as usize].chars[m.start as usize..m.end as usize]
            .iter()
            .collect()
    }

    /// Where a match is in document order, comparable across searches of one index.
    pub fn order_key(m: &FindMatch) -> (u32, u32) {
        (m.run, m.start)
    }

    /// The first match at or after the position `key` ([`FindIndex::order_key`]) of `matches`
    /// (sorted), if any.
    pub fn first_from(matches: &[FindMatch], key: (u32, u32)) -> Option<usize> {
        let i = matches.partition_point(|m| Self::order_key(m) < key);
        (i < matches.len()).then_some(i)
    }
}

/// Per text node: (DOM byte start, end, match number), the numbers counting from 0.
pub type NodeRanges = HashMap<NodeId, Vec<(u32, u32, u32)>>;

/// The ranges of the text nodes to paint as find matches, with the active one.
#[derive(Clone, Debug, Default)]
pub struct FindHighlights {
    /// Per text node: (DOM byte start, end, match number), the match numbers counting from 0.
    pub ranges: Arc<NodeRanges>,
    /// The active match's number.
    pub current: Option<u32>,
}

impl PartialEq for FindHighlights {
    fn eq(&self, other: &Self) -> bool {
        self.current == other.current && Arc::ptr_eq(&self.ranges, &other.ranges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes<const K: usize>() -> [NodeId; K] {
        let mut dom = snowlark_dom::Dom::new();
        let doc = dom.create_html_document(None);
        std::array::from_fn(|_| dom.create_text_node(doc, ""))
    }

    fn text(node: NodeId, len: u32) -> FindItem {
        FindItem::Text {
            node,
            range: 0..len,
        }
    }

    #[test]
    fn matches_across_inline_elements_and_ignores_case() {
        let [a, b] = nodes();
        let items = vec![text(a, 6), text(b, 5)];
        let src = |id: NodeId| -> Option<&'static str> {
            if id == a {
                Some("Hello ")
            } else {
                Some("WORLD")
            }
        };
        let idx = FindIndex::build(&items, src);
        assert_eq!(idx.run_text(0), "Hello WORLD");
        let mut cur = idx.start("o w").unwrap();
        let mut out = Vec::new();
        assert!(idx.search(&mut cur, &mut out, None, 100));
        assert_eq!(out.len(), 1);
        let mut parts = Vec::new();
        idx.ranges(&out[0], |nd, lo, hi| parts.push((nd == a, lo, hi)));
        assert_eq!(parts, vec![(true, 4, 6), (false, 0, 1)]);
    }

    #[test]
    fn breaks_stop_matches_and_lines_wrap_with_a_space() {
        let [a, b] = nodes();
        let src =
            |id: NodeId| -> Option<&'static str> { Some(if id == a { "foo" } else { "bar" }) };
        let blocks = FindIndex::build(&[text(a, 3), FindItem::Break, text(b, 3)], src);
        assert_eq!(blocks.run_count(), 2);
        let mut out = Vec::new();
        let mut c = blocks.start("foobar").unwrap();
        blocks.search(&mut c, &mut out, None, 10);
        assert!(out.is_empty());
        let lines = FindIndex::build(&[text(a, 3), FindItem::Space, text(b, 3)], src);
        assert_eq!(lines.run_text(0), "foo bar");
        let mut c = lines.start("foo bar").unwrap();
        lines.search(&mut c, &mut out, None, 10);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn white_space_collapses_and_matches_do_not_overlap() {
        let [a] = nodes();
        let src = |_: NodeId| Some("a \n\t b   aaaa");
        let idx = FindIndex::build(&[text(a, 13)], src);
        assert_eq!(idx.run_text(0), "a b aaaa");
        let mut out = Vec::new();
        let mut c = idx.start("aa").unwrap();
        idx.search(&mut c, &mut out, None, 10);
        assert_eq!(out.len(), 2);
        let mut c = idx.start("A  B").unwrap();
        out.clear();
        idx.search(&mut c, &mut out, None, 10);
        assert_eq!(out.len(), 1);
        assert!(idx.start("   ").is_none());
    }

    #[test]
    fn search_resumes_after_a_deadline() {
        let [a] = nodes();
        let big = "ab ".repeat(30_000);
        let idx = FindIndex::build(&[text(a, big.len() as u32)], |_| Some(big.as_str()));
        let mut c = idx.start("ab").unwrap();
        let mut out = Vec::new();
        // (a deadline in the past: stops at the first check)
        let past = Instant::now() - std::time::Duration::from_secs(1);
        let mut slices = 0;
        while !idx.search(&mut c, &mut out, Some(past), usize::MAX) {
            slices += 1;
            assert!(slices < 100);
        }
        assert!(slices > 1);
        assert_eq!(out.len(), 30_000);
        // (in order, none twice)
        assert!(out.windows(2).all(|w| w[0].start < w[1].start));
    }

    #[test]
    fn first_from_picks_the_match_at_or_after_a_position() {
        let ms = [
            FindMatch {
                run: 0,
                start: 3,
                end: 5,
            },
            FindMatch {
                run: 0,
                start: 9,
                end: 11,
            },
            FindMatch {
                run: 2,
                start: 0,
                end: 2,
            },
        ];
        assert_eq!(FindIndex::first_from(&ms, (0, 4)), Some(1));
        assert_eq!(FindIndex::first_from(&ms, (0, 9)), Some(1));
        assert_eq!(FindIndex::first_from(&ms, (1, 0)), Some(2));
        assert_eq!(FindIndex::first_from(&ms, (3, 0)), None);
    }
}
