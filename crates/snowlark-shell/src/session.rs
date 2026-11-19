// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tab's session history (HTML §7.4's joint session history, for one top-level traversable with
//! no frames): the entries back and forward, each with its title and the scroll position to
//! restore when it is traversed to. A document's own entries (`pushState`, fragment navigations)
//! are entries here too, as in Chromium's `NavigationController` (one entry per same-document
//! commit, `RendererDidNavigateToNewEntry`): each knows its document and the document's own
//! index for it, so a traversal between two entries of one document stays in the document.

use snowlark_net::Url;
use snowlark_paint::Point;

/// How many entries a tab keeps.
const MAX_ENTRIES: usize = 50;

#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub url: Url,
    pub title: String,
    /// The viewport's scroll position when the entry was left.
    pub scroll: Point,
    /// The document the entry is of: the number of the navigation that committed it (0: none yet,
    /// as in a restored history, whose entries each load afresh).
    pub document: u64,
    /// The document's own index for the entry (its `pushState` and fragment entries).
    pub item: usize,
}

impl SessionEntry {
    pub fn new(url: Url) -> SessionEntry {
        SessionEntry {
            url,
            title: String::new(),
            scroll: Point::ZERO,
            document: 0,
            item: 0,
        }
    }
}

/// What the entry shows and restores (the document and its index are the live state of a
/// session, not what a trail compares or keeps).
impl PartialEq for SessionEntry {
    fn eq(&self, other: &SessionEntry) -> bool {
        self.url == other.url && self.title == other.title && self.scroll == other.scroll
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionHistory {
    entries: Vec<SessionEntry>,
    index: usize,
}

/// A copy of a tab's session history (the session file's source) and the history a restored tab
/// starts with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Trail {
    pub entries: Vec<SessionEntry>,
    pub index: usize,
}

impl SessionHistory {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn trail(&self) -> Trail {
        Trail {
            entries: self.entries.clone(),
            index: self.index,
        }
    }

    /// A history that is `trail` (the current entry clamped into it).
    pub fn from_trail(trail: Trail) -> SessionHistory {
        let mut entries = trail.entries;
        entries.truncate(MAX_ENTRIES);
        let index = trail.index.min(entries.len().saturating_sub(1));
        SessionHistory { entries, index }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn current(&self) -> Option<&SessionEntry> {
        self.entries.get(self.index)
    }

    pub fn current_mut(&mut self) -> Option<&mut SessionEntry> {
        self.entries.get_mut(self.index)
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Makes entry `index` current again (a traversal whose navigation committed nothing: a
    /// 204, a download).
    pub fn set_index(&mut self, index: usize) {
        if index < self.entries.len() {
            self.index = index;
        }
    }

    /// Removes the current entry (a navigation pushed and then committed nothing); the one before
    /// it is current again.
    pub fn remove_current(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.entries.remove(self.index);
        self.index = self.index.min(self.entries.len().saturating_sub(1));
    }

    /// A same-document entry of the current document (`pushState`, a fragment navigation): the
    /// forward entries are dropped, as for any push.
    pub fn push_same_document(&mut self, url: Url, document: u64, item: usize) {
        self.push(url);
        self.set_document(document, item);
    }

    /// The documents of the entries are gone (the renderer that had them crashed): each entry loads
    /// afresh, as in a restored history.
    pub fn forget_documents(&mut self) {
        for e in &mut self.entries {
            e.document = 0;
            e.item = 0;
        }
    }

    /// The current entry is of `document`, its own entry `item`.
    pub fn set_document(&mut self, document: u64, item: usize) {
        if let Some(e) = self.current_mut() {
            e.document = document;
            e.item = item;
        }
    }

    /// A navigation to a new document: the forward entries are dropped (HTML's "navigate" with
    /// history handling "push").
    pub fn push(&mut self, url: Url) {
        if !self.entries.is_empty() {
            self.entries.truncate(self.index + 1);
        }
        self.entries.push(SessionEntry::new(url));
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.index = self.entries.len() - 1;
    }

    /// The current entry's document ended up at another URL (redirects).
    pub fn replace_url(&mut self, url: Url) {
        if let Some(e) = self.current_mut() {
            e.url = url;
        }
    }

    pub fn set_title(&mut self, title: &str) {
        if let Some(e) = self.current_mut() {
            e.title = title.to_owned();
        }
    }

    /// Remembers where the current entry was scrolled to before leaving it.
    pub fn save_scroll(&mut self, scroll: Point) {
        if let Some(e) = self.current_mut() {
            e.scroll = scroll;
        }
    }

    pub fn can_go(&self, delta: isize) -> bool {
        let to = self.index as isize + delta;
        delta != 0 && to >= 0 && (to as usize) < self.entries.len()
    }

    /// Traverses by `delta` (−1 back, +1 forward); the new current entry.
    pub fn go(&mut self, delta: isize) -> Option<&SessionEntry> {
        if !self.can_go(delta) {
            return None;
        }
        self.index = (self.index as isize + delta) as usize;
        self.current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn back_forward_with_scroll_restoration() {
        let mut h = SessionHistory::default();
        assert!(!h.can_go(-1));
        h.push(u("https://a.example/"));
        h.save_scroll(Point::new(0.0, 500.0));
        h.push(u("https://b.example/"));
        assert!(h.can_go(-1));
        assert!(!h.can_go(1));
        h.save_scroll(Point::new(0.0, 70.0));
        let back = h.go(-1).unwrap();
        assert_eq!(back.url.as_str(), "https://a.example/");
        assert_eq!(back.scroll.y, 500.0);
        assert!(h.can_go(1));
        let fwd = h.go(1).unwrap();
        assert_eq!(fwd.scroll.y, 70.0);
        // Navigating from the middle drops the forward entries.
        h.go(-1);
        h.push(u("https://c.example/"));
        assert_eq!(h.len(), 2);
        assert!(!h.can_go(1));
        assert_eq!(h.current().unwrap().scroll, Point::ZERO);
        h.replace_url(u("https://c.example/after-redirect"));
        h.set_title("C");
        assert_eq!(h.current().unwrap().url.path(), "/after-redirect");
        assert_eq!(h.current().unwrap().title, "C");
        assert!(h.go(5).is_none());
    }

    #[test]
    fn a_trail_round_trips() {
        let mut h = SessionHistory::default();
        h.push(u("https://a.example/"));
        h.set_title("A");
        h.push(u("https://b.example/"));
        h.save_scroll(Point::new(0.0, 9.0));
        let trail = h.trail();
        let mut r = SessionHistory::from_trail(trail.clone());
        assert_eq!(r.trail(), trail);
        assert_eq!(r.current().unwrap().url.as_str(), "https://b.example/");
        assert!(r.can_go(-1) && !r.can_go(1));
        assert_eq!(r.go(-1).unwrap().title, "A");
        // An index past the end is clamped.
        let r = SessionHistory::from_trail(Trail {
            entries: trail.entries,
            index: 99,
        });
        assert_eq!(r.current().unwrap().url.as_str(), "https://b.example/");
    }

    #[test]
    fn keeps_a_bounded_number_of_entries() {
        let mut h = SessionHistory::default();
        for i in 0..(MAX_ENTRIES + 10) {
            h.push(u(&format!("https://a.example/{i}")));
        }
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(
            h.current().unwrap().url.path(),
            format!("/{}", MAX_ENTRIES + 9)
        );
        assert!(h.can_go(-(MAX_ENTRIES as isize - 1)));
        assert!(!h.can_go(-(MAX_ENTRIES as isize)));
    }

    #[test]
    fn same_document_entries_know_their_document() {
        let mut h = SessionHistory::default();
        h.push(u("https://a.example/"));
        h.set_document(1, 0);
        h.push_same_document(u("https://a.example/#x"), 1, 1);
        h.push_same_document(u("https://a.example/two"), 1, 2);
        h.push(u("https://b.example/"));
        h.set_document(2, 0);
        assert_eq!(h.len(), 4);
        let docs: Vec<(u64, usize)> = h.entries.iter().map(|e| (e.document, e.item)).collect();
        assert_eq!(docs, [(1, 0), (1, 1), (1, 2), (2, 0)]);
        // A traversal that committed nothing: the index goes back, or the pushed entry goes.
        h.go(-2);
        assert_eq!(h.index(), 1);
        h.set_index(3);
        assert_eq!(h.current().unwrap().url.as_str(), "https://b.example/");
        h.push(u("https://c.example/"));
        h.remove_current();
        assert_eq!(h.len(), 4);
        assert_eq!(h.current().unwrap().url.as_str(), "https://b.example/");
        // (a trail compares what it keeps: the document is not in it)
        let mut other = SessionEntry::new(u("https://b.example/"));
        other.document = 9;
        assert_eq!(*h.current().unwrap(), other);
    }
}
