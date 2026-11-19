// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Find in page, the browser feature (Ctrl+F): a [`FindSession`] searches the rendered text of the
//! top-level document and of the frames it shows (documents rendered in this process; open and
//! closed shadow trees are part of the flat tree layout renders, so they are searched too), keeps
//! the matches and the active one, and shows them through the renderers' highlight paint
//! ([`snowlark_layout::find`]) without touching the DOM: no nodes, no globals, no events and no
//! selection change while searching. Only closing the search with a match active selects it, as
//! Chrome does.
//!
//! Nothing runs unless the embedder has a session: it is created when the find box opens. The text
//! is read once per layout (and again, at most twice a second, when the page laid out anew), and a
//! search is cut into slices of bounded time ([`FindSession::work`]) so that a long document does not
//! hold the page thread up while the user types.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use snowlark_dom::{Dom, NodeId};
use snowlark_layout::find::{FindCursor, FindHighlights, FindIndex, FindMatch, NodeRanges};

use crate::engine::Engine;

/// The most matches kept (the counter shows this many and the rest are not highlighted).
pub const MAX_MATCHES: usize = 100_000;
/// How soon a page that laid out anew is read again.
const REINDEX_EVERY: Duration = Duration::from_millis(500);
/// How often the highlights are repainted while a long search goes on.
const PUBLISH_EVERY: Duration = Duration::from_millis(120);

/// What the find box shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindStatus {
    pub query: String,
    /// How many matches there are (so far, while `searching`).
    pub total: usize,
    /// The active match, counted from 0.
    pub current: Option<usize>,
    /// The search has not finished.
    pub searching: bool,
    /// There are more matches or more text than were kept.
    pub truncated: bool,
}

/// What the session wants of its caller after [`FindSession::work`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FindWork {
    /// The session was changed and has more to do now: call again.
    pub busy: bool,
    /// When to call again without being asked to.
    pub next: Option<Instant>,
}

/// One document's text and matches.
struct DocIndex {
    doc: NodeId,
    /// Which layout the index is of ([`snowlark_render::Renderer::find_items`]).
    layout: usize,
    index: FindIndex,
    matches: Vec<FindMatch>,
    /// The number of the first of this document's matches among all.
    first: usize,
    ranges: Arc<NodeRanges>,
    ranges_stale: bool,
}

impl DocIndex {
    fn owns(&self, global: usize) -> Option<usize> {
        (global >= self.first && global < self.first + self.matches.len())
            .then(|| global - self.first)
    }
}

/// A find in page.
#[derive(Default)]
pub struct FindSession {
    query: String,
    docs: Vec<DocIndex>,
    /// The rendered documents when the text was read (some may have had no layout to read).
    seen: Vec<NodeId>,
    /// A search in progress: the document it is in, and where.
    cursor: Option<(usize, FindCursor)>,
    total: usize,
    current: Option<usize>,
    /// The top-level document the session searched.
    top: Option<NodeId>,
    indexed_at: Option<Instant>,
    /// The documents or their layouts changed, or the document is new: search anew.
    stale: bool,
    /// Where the active match was (its document and position in the index), to pick the match at
    /// or after it in a new search.
    anchor: Option<(NodeId, (u32, u32))>,
    /// The active match is to be scrolled into view.
    reveal: bool,
    truncated: bool,
    published_at: Option<Instant>,
    /// What changed since the highlights were published.
    unpublished: bool,
}

impl FindSession {
    pub fn new() -> FindSession {
        FindSession::default()
    }

    /// The text of the page's selection to start a search with (the top-level document's, first
    /// line, trimmed).
    pub fn selected_text(engine: &Engine) -> Option<String> {
        let top = *engine.shown_documents().first()?;
        let text = engine.selection_text(top)?;
        let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
        Some(line.chars().take(256).collect())
    }

    pub fn status(&self) -> FindStatus {
        FindStatus {
            query: self.query.clone(),
            total: self.total,
            current: self.current,
            searching: self.cursor.is_some() || self.stale,
            truncated: self.truncated,
        }
    }

    /// Searches for `query` (case-insensitively). The active match is the first at or after the
    /// one that was active, so typing on keeps the match in place. The caller calls
    /// [`FindSession::work`] until it is done.
    pub fn set_query(&mut self, engine: &Engine, query: &str) {
        if let Some(anchor) = self.active_anchor() {
            self.anchor = Some(anchor);
        }
        self.query = query.to_owned();
        self.restart(engine, true);
    }

    /// The active match, as a position that survives a search anew.
    fn active_anchor(&self) -> Option<(NodeId, (u32, u32))> {
        let g = self.current?;
        let d = self.docs.iter().find(|d| d.owns(g).is_some())?;
        let m = d.matches.get(g - d.first)?;
        Some((d.doc, FindIndex::order_key(m)))
    }

    /// Goes to the next (or previous) match, wrapping round.
    pub fn step(&mut self, engine: &Engine, forward: bool) {
        if self.total == 0 {
            return;
        }
        let n = self.total;
        self.current = Some(match (self.current, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(c), true) => (c + 1) % n,
            (Some(c), false) => (c + n - 1) % n,
        });
        self.reveal = true;
        self.publish(engine, true);
        self.reveal_current(engine);
    }

    /// Ends the search: the highlights go, and with `keep_selection` the active match is left
    /// selected in its document.
    pub fn close(&mut self, engine: &Engine, keep_selection: bool) {
        if keep_selection {
            self.select_current(engine);
        }
        for d in &self.docs {
            engine.set_find_highlights(d.doc, None);
        }
        // (a frame that left the session's list still has none to clear)
        *self = FindSession::default();
    }

    /// Searches anew: reads the text of every rendered document, and starts a search for the query.
    fn restart(&mut self, engine: &Engine, user: bool) {
        self.stale = false;
        self.cursor = None;
        self.reveal = user || self.reveal;
        self.total = 0;
        self.current = None;
        self.truncated = false;
        let docs = engine.shown_documents();
        self.top = docs.first().copied();
        // The text of the layouts already read is searched again, not read again.
        let same = docs == self.seen
            && self.docs.len() == docs.len()
            && self
                .docs
                .iter()
                .all(|d| engine.find_layout(d.doc) == Some(d.layout));
        if same {
            for d in &mut self.docs {
                d.matches.clear();
                d.first = 0;
                d.ranges = Arc::default();
                d.ranges_stale = true;
            }
            self.truncated = self.docs.iter().any(|d| d.index.truncated());
        } else {
            self.seen = docs.clone();
            let old_docs: Vec<NodeId> = self.docs.drain(..).map(|d| d.doc).collect();
            self.indexed_at = Some(Instant::now());
            for doc in docs.iter().copied() {
                let Some(index) = engine.find_index(doc) else {
                    continue;
                };
                self.truncated |= index.1.truncated();
                self.docs.push(DocIndex {
                    doc,
                    layout: index.0,
                    index: index.1,
                    matches: Vec::new(),
                    first: 0,
                    ranges: Arc::default(),
                    ranges_stale: true,
                });
            }
            // A document that is no longer shown keeps no highlights.
            for gone in old_docs {
                if !self.docs.iter().any(|d| d.doc == gone) {
                    engine.set_find_highlights(gone, None);
                }
            }
        }
        let query = self.query.clone();
        if !query.is_empty() {
            if let Some(first) = self.docs.first() {
                if let Some(c) = first.index.start(&query) {
                    self.cursor = Some((0, c));
                }
            }
        }
        if self.cursor.is_none() {
            self.anchor = None;
        }
        self.unpublished = true;
        self.publish(engine, true);
    }

    /// Runs the search for up to `budget`, notices that the page changed, and updates the
    /// highlights; `ready` is whether the document has finished loading (a document still loading
    /// is searched when it is done, or when asked to).
    pub fn work(&mut self, engine: &Engine, ready: bool, budget: Duration) -> FindWork {
        let start = Instant::now();
        let mut work = FindWork::default();
        let docs = engine.shown_documents();
        // A new document: what was found is of the old one.
        if docs.first().copied() != self.top {
            self.top = docs.first().copied();
            self.docs.clear();
            self.seen.clear();
            self.cursor = None;
            self.total = 0;
            self.current = None;
            self.anchor = None;
            self.stale = !self.query.is_empty();
            self.unpublished = true;
        }
        if !self.query.is_empty() && self.cursor.is_none() && !self.stale {
            // The page laid out anew, or a frame came or went.
            let changed = docs != self.seen
                || self.docs.len() != docs.len()
                || self
                    .docs
                    .iter()
                    .any(|d| engine.find_layout(d.doc) != Some(d.layout));
            if changed {
                self.stale = true;
            }
        }
        if self.stale && ready {
            let due = self
                .indexed_at
                .map_or(start, |t| t + REINDEX_EVERY)
                .max(start);
            if due > start && !self.docs.is_empty() {
                work.next = Some(due);
            } else {
                // (what was active stays: the match at or after it)
                if let Some(a) = self.active_anchor() {
                    self.anchor = Some(a);
                }
                self.restart(engine, false);
            }
        }
        if let Some((mut di, mut cursor)) = self.cursor.take() {
            let deadline = start + budget;
            loop {
                let remaining = MAX_MATCHES.saturating_sub(self.total);
                let d = &mut self.docs[di];
                let before = d.matches.len();
                let cap = before + remaining;
                let done = d
                    .index
                    .search(&mut cursor, &mut d.matches, Some(deadline), cap);
                let added = d.matches.len() - before;
                if added > 0 {
                    d.ranges_stale = true;
                    self.total += added;
                    self.unpublished = true;
                }
                if self.total >= MAX_MATCHES {
                    self.truncated = true;
                }
                if !done && self.total < MAX_MATCHES {
                    self.cursor = Some((di, cursor));
                    work.busy = true;
                    break;
                }
                // The next document.
                di += 1;
                if di >= self.docs.len() || self.total >= MAX_MATCHES {
                    break;
                }
                self.docs[di].first = self.total;
                match self.docs[di].index.start(&self.query) {
                    Some(c) => cursor = c,
                    None => break,
                }
                if Instant::now() >= deadline {
                    self.cursor = Some((di, cursor));
                    work.busy = true;
                    break;
                }
            }
            self.pick_current(engine);
        }
        if self.unpublished {
            let finished = self.cursor.is_none();
            let due = self
                .published_at
                .is_none_or(|t| start.duration_since(t) >= PUBLISH_EVERY);
            if finished || due {
                self.publish(engine, false);
            } else {
                work.busy = true;
            }
        }
        if self.stale && !ready {
            // (waits for the load to finish)
        }
        work
    }

    /// Chooses the active match once there is one to choose: the first at or after where the last
    /// one was (else the first of all), and scrolls to it.
    fn pick_current(&mut self, engine: &Engine) {
        if self.current.is_some() || self.total == 0 {
            return;
        }
        let finished = self.cursor.is_none();
        let chosen = match self.anchor {
            Some((doc, key)) => {
                let mut found = None;
                for d in &self.docs {
                    if d.doc == doc {
                        match FindIndex::first_from(&d.matches, key) {
                            Some(j) => {
                                found = Some(d.first + j);
                            }
                            None => {
                                // (the first of what follows, if it is found yet)
                                let next = d.first + d.matches.len();
                                if next < self.total {
                                    found = Some(next);
                                }
                            }
                        }
                        break;
                    }
                }
                match found {
                    Some(g) => Some(g),
                    // (nothing at or after it, and no more to find: wrap round)
                    None if finished => Some(0),
                    None => None,
                }
            }
            None => Some(0),
        };
        if let Some(g) = chosen {
            self.current = Some(g);
            self.anchor = None;
            self.unpublished = true;
            self.publish(engine, true);
            if self.reveal {
                self.reveal_current(engine);
            }
        }
    }

    /// Hands the matches and the active one to the renderers.
    fn publish(&mut self, engine: &Engine, force: bool) {
        if !self.unpublished && !force {
            return;
        }
        self.unpublished = false;
        self.published_at = Some(Instant::now());
        let current = self.current;
        for d in &mut self.docs {
            if d.ranges_stale {
                let mut map: NodeRanges = HashMap::new();
                for (j, m) in d.matches.iter().enumerate() {
                    let id = (d.first + j) as u32;
                    d.index.ranges(m, |node, lo, hi| {
                        map.entry(node).or_default().push((lo, hi, id));
                    });
                }
                d.ranges = Arc::new(map);
                d.ranges_stale = false;
            }
            let active = current
                .and_then(|c| d.owns(c))
                .map(|j| (d.first + j) as u32);
            let h = if d.matches.is_empty() {
                None
            } else {
                Some(FindHighlights {
                    ranges: d.ranges.clone(),
                    current: active,
                })
            };
            engine.set_find_highlights(d.doc, h);
        }
    }

    /// The first text range of the active match, with its document.
    fn current_range(&self) -> Option<(NodeId, NodeId, u32, u32)> {
        let g = self.current?;
        let d = self.docs.iter().find(|d| d.owns(g).is_some())?;
        let m = d.matches.get(g - d.first)?;
        let mut first = None;
        d.index.ranges(m, |n, lo, hi| {
            first.get_or_insert((d.doc, n, lo, hi));
        });
        first
    }

    fn reveal_current(&mut self, engine: &Engine) {
        self.reveal = false;
        if let Some((doc, node, lo, hi)) = self.current_range() {
            engine.find_reveal(doc, node, lo, hi);
        }
    }

    /// Selects the active match in its document (what closing the box leaves).
    fn select_current(&self, engine: &Engine) {
        let Some(g) = self.current else {
            return;
        };
        let Some(d) = self.docs.iter().find(|d| d.owns(g).is_some()) else {
            return;
        };
        let Some(m) = d.matches.get(g - d.first) else {
            return;
        };
        let mut ends: Option<((NodeId, u32), (NodeId, u32))> = None;
        d.index.ranges(m, |n, lo, hi| match &mut ends {
            None => ends = Some(((n, lo), (n, hi))),
            Some((_, end)) => *end = (n, hi),
        });
        if let Some((a, b)) = ends {
            engine.find_select(d.doc, a, b);
        }
    }
}

impl Engine {
    /// The address of the last layout of a rendered document (it differs for a new layout).
    pub(crate) fn find_layout(&self, doc: NodeId) -> Option<usize> {
        self.with_doc_renderer(doc, |r, _| r.layout_id()).flatten()
    }

    /// The searchable text of a rendered document, with the layout it is of.
    pub(crate) fn find_index(&self, doc: NodeId) -> Option<(usize, FindIndex)> {
        let (layout, items) = self
            .with_doc_renderer(doc, |r, _| r.find_items())
            .flatten()?;
        let dom = self.dom();
        // (the layout is the last one: a text node in it may be gone by now)
        let text = |n| dom.is_alive(n).then(|| dom.character_data(n)).flatten();
        Some((layout, FindIndex::build(&items, text)))
    }

    /// Paints (or stops painting) find matches in a rendered document.
    pub(crate) fn set_find_highlights(&self, doc: NodeId, h: Option<FindHighlights>) {
        self.with_doc_renderer(doc, |r, st| {
            if r.set_find(h) {
                st.needs_paint = true;
            }
        });
    }

    /// Scrolls the first line of a match (bytes `lo..hi` of text node `node` of `doc`) into view,
    /// through the scroll containers around it and the frames around its document; a match in view
    /// stays where it is, any other is centered.
    pub(crate) fn find_reveal(&self, doc: NodeId, node: NodeId, lo: u32, hi: u32) {
        if !self.dom().is_alive(node) {
            return;
        }
        let Some(area) = self
            .with_doc_renderer(doc, |r, _| {
                r.text_range_rects(node, lo, hi).first().copied()
            })
            .flatten()
        else {
            return;
        };
        let mut el = {
            let dom = self.dom();
            find_layout_element(&dom, node)
        };
        // (an element without a box of its own, `display: contents`: the nearest that has one)
        while let Some(e) = el {
            let boxed = self
                .with_node_renderer(e, |r, _| r.geometry(e).is_some())
                .unwrap_or(false);
            if boxed {
                self.view_scroll_rect_into_view(e, Some(area), "find", "nearest");
                break;
            }
            el = {
                let dom = self.dom();
                find_layout_element(&dom, e)
            };
        }
        // The frames around the document, innermost first.
        let mut d = doc;
        for _ in 0..32 {
            let Some(container) = self.rendered_container(d) else {
                break;
            };
            self.view_scroll_rect_into_view(container, None, "find", "nearest");
            d = self.dom().node_document(container);
        }
    }

    /// Selects from byte `a.1` of text node `a.0` to byte `b.1` of `b.0` in `doc`.
    pub(crate) fn find_select(&self, doc: NodeId, a: (NodeId, u32), b: (NodeId, u32)) {
        let point = |(n, byte): (NodeId, u32)| {
            let dom = self.dom();
            let s = dom.character_data(n).unwrap_or("");
            let end = (byte as usize).min(s.len());
            let end = (0..=end)
                .rev()
                .find(|&i| s.is_char_boundary(i))
                .unwrap_or(0);
            (n, s[..end].encode_utf16().count() as u32)
        };
        if self.dom().is_alive(a.0) && self.dom().is_alive(b.0) {
            self.set_doc_selection(doc, Some((point(a), point(b))));
        }
    }

    /// The text of the top-level document's selection, for the find box to start with.
    pub fn find_selected_text(&self) -> Option<String> {
        FindSession::selected_text(self)
    }
}

/// The element whose box holds a node: its parent, or the host of the shadow root it is in.
fn find_layout_element(dom: &Dom, n: NodeId) -> Option<NodeId> {
    let p = dom.parent(n)?;
    if dom.is_shadow_root(p) {
        return dom.host(p);
    }
    dom.is_element(p).then_some(p)
}
