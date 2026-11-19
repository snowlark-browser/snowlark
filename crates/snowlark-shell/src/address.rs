// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The address bar's logic: what typed text means ([`interpret`]: an address or a web search), and
//! the single-line text field it edits ([`TextField`]: caret, selection, words, inline completion).

use std::borrow::Cow;
use std::ops::Range;

use snowlark_net::Url;

/// Where a search phrase goes.
pub const SEARCH_URL: &str = "https://duckduckgo.com/?q=";

/// The start page's address.
pub use snowlark_tab::renderer_driver::START_PAGE;

/// What the address bar's text means: an address, or else a web search (as every browser's address
/// bar does). `None` for blank text.
pub fn interpret(text: &str) -> Option<Url> {
    interpret_with(text, SEARCH_URL)
}

/// The start of an internal page's address before the browser was renamed, and today.
const OLD_INTERNAL: &str = "osmium://";
const INTERNAL: &str = "snowlark://";

/// `url` with an internal page's old scheme replaced by today's, anything else as it is: an address
/// typed, or read from a bookmark, a saved session or a setting written before the rename, opens the
/// page it always did.
pub fn renamed(url: &str) -> Cow<'_, str> {
    match url.get(..OLD_INTERNAL.len()) {
        Some(start) if start.eq_ignore_ascii_case(OLD_INTERNAL) => {
            Cow::Owned(format!("{INTERNAL}{}", &url[OLD_INTERNAL.len()..]))
        }
        _ => Cow::Borrowed(url),
    }
}

/// [`interpret`], searching with the engine whose search URL is `search` (the phrase follows it).
pub fn interpret_with(text: &str, search: &str) -> Option<Url> {
    let text = renamed(text.trim());
    let text = text.as_ref();
    if text.is_empty() {
        return None;
    }
    // A Windows path ("C:\dir\page.html", "\\server\share\page.html") opens the file.
    let b = text.as_bytes();
    if (b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/'))
        || text.starts_with("\\\\")
    {
        if let Ok(u) = Url::from_file_path(text) {
            return Some(u);
        }
    }
    let lower = text.to_ascii_lowercase();
    let has_scheme = text.contains("://")
        || ["about:", "data:", "file:", "view-source:", "blob:"]
            .iter()
            .any(|s| lower.starts_with(s));
    if has_scheme {
        if let Ok(u) = Url::parse(text) {
            return Some(u);
        }
    }
    let looks_like_host = !text.contains(char::is_whitespace)
        && text.parse::<f64>().is_err()
        && (text.contains('.') || lower.starts_with("localhost") || host_with_port(text))
        && !text.starts_with('.')
        && !text.ends_with('.');
    if looks_like_host {
        let local = lower.starts_with("localhost")
            || lower.starts_with("127.")
            || lower.starts_with("[::1]")
            || lower
                .split(['/', ':'])
                .next()
                .is_some_and(|h| h.ends_with(".localhost"));
        let scheme = if local { "http" } else { "https" };
        if let Ok(u) = Url::parse(&format!("{scheme}://{text}")) {
            if u.host_str().is_some_and(|h| !h.is_empty()) {
                return Some(u);
            }
        }
    }
    Some(search_url_with(search, text))
}

/// "name:1234/...": a host with a port.
fn host_with_port(text: &str) -> bool {
    let Some((host, rest)) = text.split_once(':') else {
        return false;
    };
    let port: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    !host.is_empty()
        && !port.is_empty()
        && rest[port.len()..].chars().next().is_none_or(|c| c == '/')
}

/// The search for `phrase` (application/x-www-form-urlencoded, as a search form submits it).
pub fn search_url(phrase: &str) -> Url {
    search_url_with(SEARCH_URL, phrase)
}

/// The search for `phrase` with the engine whose search URL is `search`.
pub fn search_url_with(search: &str, phrase: &str) -> Url {
    let mut q = String::with_capacity(phrase.len() * 3);
    for b in phrase.trim().bytes() {
        match b {
            b' ' => q.push('+'),
            b'*' | b'-' | b'.' | b'_' => q.push(b as char),
            b if b.is_ascii_alphanumeric() => q.push(b as char),
            b => q.push_str(&format!("%{b:02X}")),
        }
    }
    Url::parse(&format!("{search}{q}")).expect("search URL")
}

/// How the address bar shows a page's URL while it isn't being edited (the start page shows blank).
pub fn display_url(url: &str) -> String {
    if url == START_PAGE || url == "about:blank" {
        return String::new();
    }
    url.to_owned()
}

/// Pasted text as one line, as Chrome's address bar takes it: every run of whitespace that holds a
/// line break becomes one space, and the ends are trimmed.
pub fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.trim().chars() {
        if c.is_whitespace() {
            run.push(c);
            continue;
        }
        if !run.is_empty() {
            if run.contains(['\r', '\n']) {
                out.push(' ');
            } else {
                out.push_str(&run);
            }
            run.clear();
        }
        out.push(c);
    }
    out
}

/// A state the field can go back to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    text: String,
    caret: usize,
    anchor: usize,
}

/// What the last edit was, so a run of typing (or of Backspace, or of Delete) is one undo step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EditKind {
    #[default]
    Other,
    Typing,
    Backspace,
    Delete,
}

/// A single-line editable text: caret and selection anchor as byte offsets on char boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextField {
    text: String,
    caret: usize,
    anchor: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last: EditKind,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

impl TextField {
    pub fn new(text: &str) -> TextField {
        let mut f = TextField::default();
        f.set_text(text);
        f
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Replaces the text; the caret goes to the end. What was typed before can't be undone.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.replace(['\r', '\n'], "");
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.undo.clear();
        self.redo.clear();
        self.last = EditKind::Other;
    }

    /// [`TextField::set_text`] for a multi-line text (line breaks kept, as LF).
    pub fn set_multiline_text(&mut self, text: &str) {
        self.set_text("");
        self.text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.caret = self.text.len();
        self.anchor = self.caret;
    }

    /// [`TextField::insert`] for a multi-line text (line breaks kept, as LF).
    pub fn insert_multiline(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let one_char = s.chars().count() == 1 && !self.has_selection();
        self.replace_selection(
            &s,
            if one_char {
                EditKind::Typing
            } else {
                EditKind::Other
            },
        );
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            caret: self.caret,
            anchor: self.anchor,
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.text = s.text;
        self.caret = s.caret;
        self.anchor = s.anchor;
        self.last = EditKind::Other;
    }

    /// Notes that an edit of `kind` is about to change the text: the state before it is kept for
    /// Undo, except inside a run of the same kind.
    fn record(&mut self, kind: EditKind) {
        if kind == EditKind::Other || kind != self.last {
            let s = self.snapshot();
            self.undo.push(s);
        }
        self.redo.clear();
        self.last = kind;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Goes back one edit (Ctrl+Z); whether there was one.
    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else {
            return false;
        };
        let now = self.snapshot();
        self.redo.push(now);
        self.restore(s);
        true
    }

    /// Goes forward again after an undo (Ctrl+Y, Ctrl+Shift+Z); whether there was one.
    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else {
            return false;
        };
        let now = self.snapshot();
        self.undo.push(now);
        self.restore(s);
        true
    }

    pub fn selection(&self) -> Range<usize> {
        self.caret.min(self.anchor)..self.caret.max(self.anchor)
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
        self.last = EditKind::Other;
    }

    /// Puts the caret at `index` (snapped to a char boundary), extending the selection or not.
    pub fn set_caret(&mut self, index: usize, extend: bool) {
        let mut i = index.min(self.text.len());
        while !self.text.is_char_boundary(i) {
            i -= 1;
        }
        self.caret = i;
        if !extend {
            self.anchor = i;
        }
        self.last = EditKind::Other;
    }

    /// Selects the word around `index` (a double-click).
    pub fn select_word_at(&mut self, index: usize) {
        self.set_caret(index, false);
        let start = self.word_start(self.caret);
        let end = self.word_end(self.caret);
        self.anchor = start;
        self.caret = end;
    }

    /// A left press at `index` that was the `clicks`th in a row (`shift` extends the selection): a
    /// triple click selects everything, a double click the word, one click puts the caret there.
    /// Whether a drag from here selects.
    pub fn press(&mut self, index: usize, clicks: u32, shift: bool) -> bool {
        match clicks {
            0 | 1 => {
                self.set_caret(index, shift);
                true
            }
            2 => {
                self.select_word_at(index);
                false
            }
            _ => {
                self.select_all();
                false
            }
        }
    }

    /// A right press at `index`: inside the selection nothing changes, anywhere else the caret goes
    /// there and the selection is gone (as in Chrome's address bar).
    pub fn context_press(&mut self, index: usize) {
        let sel = self.selection();
        if !self.has_selection() || index < sel.start || index > sel.end {
            self.set_caret(index, false);
        }
    }

    /// What the user typed: the text before an inline completion (which is selected to the end).
    pub fn typed_text(&self) -> &str {
        let sel = self.selection();
        if self.has_selection() && sel.end == self.text.len() {
            &self.text[..sel.start]
        } else {
            &self.text
        }
    }

    /// Shows the rest of `full` after what is typed, selected, so more typing replaces it. Only
    /// when the caret is at the end with nothing selected and `full` starts with the text.
    pub fn inline_complete(&mut self, full: &str) {
        if self.has_selection()
            || self.caret != self.text.len()
            || full.len() <= self.text.len()
            || !full.is_char_boundary(self.text.len())
            || !full[..self.text.len()].eq_ignore_ascii_case(&self.text)
        {
            return;
        }
        let typed = self.text.len();
        self.text.push_str(&full[typed..]);
        self.anchor = typed;
        self.caret = self.text.len();
    }

    /// Types `s` over the selection (line breaks dropped).
    pub fn insert(&mut self, s: &str) {
        let s = s.replace(['\r', '\n'], "");
        let one_char = s.chars().count() == 1 && !self.has_selection();
        self.replace_selection(
            &s,
            if one_char {
                EditKind::Typing
            } else {
                EditKind::Other
            },
        );
    }

    /// Pastes `s` over the selection, collapsed to one line ([`one_line`]); nothing for blank text.
    pub fn paste(&mut self, s: &str) {
        let s = one_line(s);
        if !s.is_empty() {
            self.replace_selection(&s, EditKind::Other);
        }
    }

    fn replace_selection(&mut self, s: &str, kind: EditKind) {
        if s.is_empty() && !self.has_selection() {
            return;
        }
        self.record(kind);
        self.delete_selection();
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
        self.anchor = self.caret;
    }

    fn delete_selection(&mut self) -> bool {
        if !self.has_selection() {
            return false;
        }
        let r = self.selection();
        self.text.replace_range(r.clone(), "");
        self.caret = r.start;
        self.anchor = r.start;
        true
    }

    fn prev_char(&self, i: usize) -> usize {
        self.text[..i]
            .char_indices()
            .next_back()
            .map_or(0, |(j, _)| j)
    }

    fn next_char(&self, i: usize) -> usize {
        self.text[i..]
            .chars()
            .next()
            .map_or(self.text.len(), |c| i + c.len_utf8())
    }

    /// The start of the word before `i` (skipping separators first), as Ctrl+Left moves.
    fn prev_word(&self, i: usize) -> usize {
        let mut i = i;
        while i > 0 && !self.text[..i].chars().next_back().is_some_and(is_word) {
            i = self.prev_char(i);
        }
        while i > 0 && self.text[..i].chars().next_back().is_some_and(is_word) {
            i = self.prev_char(i);
        }
        i
    }

    /// The end of the word after `i` (skipping separators first), as Ctrl+Right moves.
    fn next_word(&self, i: usize) -> usize {
        let mut i = i;
        while i < self.text.len() && !self.text[i..].chars().next().is_some_and(is_word) {
            i = self.next_char(i);
        }
        while i < self.text.len() && self.text[i..].chars().next().is_some_and(is_word) {
            i = self.next_char(i);
        }
        i
    }

    fn word_start(&self, i: usize) -> usize {
        let mut i = i;
        while i > 0 && self.text[..i].chars().next_back().is_some_and(is_word) {
            i = self.prev_char(i);
        }
        i
    }

    fn word_end(&self, i: usize) -> usize {
        let mut i = i;
        while i < self.text.len() && self.text[i..].chars().next().is_some_and(is_word) {
            i = self.next_char(i);
        }
        i
    }

    pub fn left(&mut self, extend: bool, word: bool) {
        let to = if word {
            self.prev_word(self.caret)
        } else if self.has_selection() && !extend {
            self.selection().start
        } else {
            self.prev_char(self.caret)
        };
        self.set_caret(to, extend);
    }

    pub fn right(&mut self, extend: bool, word: bool) {
        let to = if word {
            self.next_word(self.caret)
        } else if self.has_selection() && !extend {
            self.selection().end
        } else {
            self.next_char(self.caret)
        };
        self.set_caret(to, extend);
    }

    pub fn home(&mut self, extend: bool) {
        self.set_caret(0, extend);
    }

    pub fn end(&mut self, extend: bool) {
        self.set_caret(self.text.len(), extend);
    }

    pub fn backspace(&mut self, word: bool) {
        if !self.has_selection() {
            if self.caret == 0 {
                return;
            }
            self.record(if word {
                EditKind::Other
            } else {
                EditKind::Backspace
            });
            self.anchor = if word {
                self.prev_word(self.caret)
            } else {
                self.prev_char(self.caret)
            };
        } else {
            self.record(EditKind::Other);
        }
        self.delete_selection();
    }

    pub fn delete(&mut self, word: bool) {
        if !self.has_selection() {
            if self.caret >= self.text.len() {
                return;
            }
            self.record(if word {
                EditKind::Other
            } else {
                EditKind::Delete
            });
            self.anchor = if word {
                self.next_word(self.caret)
            } else {
                self.next_char(self.caret)
            };
        } else {
            self.record(EditKind::Other);
        }
        self.delete_selection();
    }

    /// Removes and returns the selection (Ctrl+X, the menu's Cut).
    pub fn cut(&mut self) -> String {
        let s = self.selected_text().to_owned();
        if self.has_selection() {
            self.record(EditKind::Other);
            self.delete_selection();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> String {
        interpret(text).map(|u| u.to_string()).unwrap_or_default()
    }

    #[test]
    fn addresses() {
        assert_eq!(s("news.ycombinator.com"), "https://news.ycombinator.com/");
        assert_eq!(
            s("en.wikipedia.org/wiki/Snowlark"),
            "https://en.wikipedia.org/wiki/Snowlark"
        );
        assert_eq!(
            s("  https://example.com/a?b=c  "),
            "https://example.com/a?b=c"
        );
        assert_eq!(s("http://example.com"), "http://example.com/");
        assert_eq!(s("localhost:8000/x"), "http://localhost:8000/x");
        assert_eq!(s("localhost"), "http://localhost/");
        assert_eq!(s("127.0.0.1:8080"), "http://127.0.0.1:8080/");
        assert_eq!(s("intranet:3000"), "https://intranet:3000/");
        assert_eq!(s("about:blank"), "about:blank");
        assert_eq!(s("data:text/html,hi"), "data:text/html,hi");
        assert_eq!(s("C:\\pages\\a.html"), "file:///C:/pages/a.html");
        assert!(interpret("   ").is_none());
    }

    #[test]
    fn searches() {
        assert_eq!(
            s("rust borrow checker"),
            format!("{SEARCH_URL}rust+borrow+checker")
        );
        assert_eq!(s("snowlark"), format!("{SEARCH_URL}snowlark"));
        assert_eq!(s("1.5"), format!("{SEARCH_URL}1.5"));
        assert_eq!(s("what is 2+2?"), format!("{SEARCH_URL}what+is+2%2B2%3F"));
        assert_eq!(s("ünïcode"), format!("{SEARCH_URL}%C3%BCn%C3%AFcode"));
        assert_eq!(s("end."), format!("{SEARCH_URL}end."));
        assert_eq!(s("a.b c"), format!("{SEARCH_URL}a.b+c"));
    }

    #[test]
    fn start_page_shows_blank() {
        assert_eq!(display_url(START_PAGE), "");
        assert_eq!(display_url("https://a.b/"), "https://a.b/");
    }

    #[test]
    fn an_internal_page_under_its_old_scheme_is_the_same_page() {
        assert_eq!(s("osmium://settings/help"), "snowlark://settings/help");
        assert_eq!(s("Osmium://history"), "snowlark://history");
        assert_eq!(
            renamed("osmium://bookmarks/?q=a"),
            "snowlark://bookmarks/?q=a"
        );
        // Only an address: the word is searched for, and other addresses are untouched.
        assert_eq!(s("osmium"), format!("{SEARCH_URL}osmium"));
        assert_eq!(
            s("osmium: the metal"),
            format!("{SEARCH_URL}osmium%3A+the+metal")
        );
        assert_eq!(renamed("https://a.b/osmium://x"), "https://a.b/osmium://x");
        assert_eq!(renamed("snowlark://history"), "snowlark://history");
        assert_eq!(renamed("osm"), "osm");
    }

    #[test]
    fn internal_pages_are_shown_and_typed_as_snowlark() {
        assert_eq!(
            display_url("snowlark://settings/help"),
            "snowlark://settings/help"
        );
        assert_eq!(s("Snowlark://history"), "snowlark://history");
        assert_eq!(s("snowlark://credits"), "snowlark://credits");
        // What the address bar shows opens the page it shows.
        let shown = display_url("snowlark://bookmarks/?q=a");
        assert_eq!(s(&shown), "snowlark://bookmarks/?q=a");
    }

    #[test]
    fn typing_and_selection() {
        let mut f = TextField::new("hello world");
        f.select_all();
        assert_eq!(f.selected_text(), "hello world");
        f.insert("abc");
        assert_eq!(f.text(), "abc");
        f.left(false, false);
        f.insert("X");
        assert_eq!(f.text(), "abXc");
        f.home(false);
        f.right(true, false);
        f.right(true, false);
        assert_eq!(f.selected_text(), "ab");
        f.backspace(false);
        assert_eq!(f.text(), "Xc");
        f.end(false);
        f.backspace(false);
        assert_eq!(f.text(), "X");
        f.delete(false);
        assert_eq!(f.text(), "X");
        f.home(false);
        f.delete(false);
        assert_eq!(f.text(), "");
    }

    #[test]
    fn words() {
        let mut f = TextField::new("https://news.ycombinator.com/item");
        f.backspace(true);
        assert_eq!(f.text(), "https://news.ycombinator.com/");
        f.left(false, true);
        assert_eq!(&f.text()[f.caret()..], "com/");
        f.left(false, true);
        assert_eq!(&f.text()[f.caret()..], "ycombinator.com/");
        f.right(false, true);
        assert_eq!(&f.text()[f.caret()..], ".com/");
        f.delete(true);
        assert_eq!(f.text(), "https://news.ycombinator/");
        f.select_word_at(10);
        assert_eq!(f.selected_text(), "news");
    }

    #[test]
    fn multibyte_text_keeps_char_boundaries() {
        let mut f = TextField::new("žluťoučký kůň");
        f.left(false, false);
        f.backspace(false);
        assert_eq!(f.text(), "žluťoučký kň");
        f.set_caret(1, false);
        assert_eq!(f.caret(), 0);
        f.right(true, false);
        assert_eq!(f.selected_text(), "ž");
    }

    #[test]
    fn inline_completion() {
        let mut f = TextField::new("news");
        f.inline_complete("news.ycombinator.com");
        assert_eq!(f.text(), "news.ycombinator.com");
        assert_eq!(f.typed_text(), "news");
        assert_eq!(f.selected_text(), ".ycombinator.com");
        // Typing replaces the completion.
        f.insert(".");
        assert_eq!(f.text(), "news.");
        // No completion when the caret isn't at the end.
        f.left(false, false);
        f.inline_complete("news.ycombinator.com");
        assert_eq!(f.text(), "news.");
        // Case-insensitive prefix.
        let mut f = TextField::new("NEWS");
        f.inline_complete("news.ycombinator.com");
        assert_eq!(f.text(), "NEWS.ycombinator.com");
    }

    #[test]
    fn clicks_select_the_caret_the_word_and_everything() {
        let mut f = TextField::new("https://news.ycombinator.com/item");
        assert!(f.press(8, 1, false), "a single click starts a drag");
        assert!(!f.has_selection());
        assert_eq!(f.caret(), 8);
        // Shift+click extends from the caret.
        f.press(12, 1, true);
        assert_eq!(f.selected_text(), "news");
        assert!(!f.press(10, 2, false));
        assert_eq!(f.selected_text(), "news");
        assert!(!f.press(10, 3, false));
        assert_eq!(f.selected_text(), "https://news.ycombinator.com/item");
        // Further clicks of the run keep everything selected.
        f.press(3, 4, false);
        assert_eq!(f.selection(), 0..f.text().len());
    }

    #[test]
    fn right_press_keeps_a_selection_it_lands_in() {
        let mut f = TextField::new("hello brave world");
        f.select_word_at(8);
        assert_eq!(f.selected_text(), "brave");
        f.context_press(8);
        assert_eq!(f.selected_text(), "brave");
        f.context_press(6);
        f.context_press(11);
        assert_eq!(f.selected_text(), "brave", "the edges are inside");
        f.context_press(2);
        assert!(!f.has_selection());
        assert_eq!(f.caret(), 2);
        // With no selection the caret goes there.
        f.context_press(15);
        assert_eq!(f.caret(), 15);
    }

    #[test]
    fn undo_and_redo_by_runs() {
        let mut f = TextField::new("");
        assert!(!f.can_undo());
        for c in "abc".chars() {
            f.insert(&c.to_string());
        }
        f.insert(" ");
        assert_eq!(f.text(), "abc ");
        // A run of typing is one step.
        assert!(f.undo());
        assert_eq!(f.text(), "");
        assert!(!f.can_undo());
        assert!(f.can_redo());
        assert!(f.redo());
        assert_eq!(f.text(), "abc ");
        // Moving the caret ends the run.
        f.left(false, false);
        f.insert("X");
        f.insert("Y");
        assert_eq!(f.text(), "abcXY ");
        f.undo();
        assert_eq!(f.text(), "abc ");
        // A new edit drops the redo history.
        f.backspace(false);
        f.backspace(false);
        assert_eq!(f.text(), "a ");
        assert!(!f.can_redo());
        f.undo();
        assert_eq!(f.text(), "abc ");
        // Cut and paste are steps of their own and restore the selection.
        f.select_all();
        assert_eq!(f.cut(), "abc ");
        assert_eq!(f.text(), "");
        f.paste("zzz");
        f.undo();
        assert_eq!(f.text(), "");
        f.undo();
        assert_eq!(f.text(), "abc ");
        assert_eq!(f.selected_text(), "abc ");
        // Nothing to delete is nothing to undo; set_text starts over.
        let mut g = TextField::new("x");
        g.delete(false);
        assert!(!g.can_undo());
        g.insert("y");
        g.set_text("fresh");
        assert!(!g.can_undo() && !g.can_redo());
    }

    #[test]
    fn paste_is_one_line() {
        assert_eq!(one_line("  a\r\n  b\n\n c  "), "a b c");
        assert_eq!(one_line("a  b\tc"), "a  b\tc");
        assert_eq!(one_line("\n\n"), "");
        assert_eq!(one_line("https://a.example/x\r\n"), "https://a.example/x");
        let mut f = TextField::new("old");
        f.select_all();
        f.paste("first line\nsecond");
        assert_eq!(f.text(), "first line second");
        f.select_all();
        f.paste("  \n ");
        assert_eq!(f.text(), "first line second", "blank pastes nothing");
        // What the bar then makes of it: the same as typing it.
        assert_eq!(s(f.text()), format!("{SEARCH_URL}first+line+second"));
    }

    #[test]
    fn pasted_newlines_are_dropped() {
        let mut f = TextField::new("");
        f.insert("a\r\nb");
        assert_eq!(f.text(), "ab");
        assert_eq!(f.cut(), "");
        f.select_all();
        assert_eq!(f.cut(), "ab");
        assert_eq!(f.text(), "");
    }
}
