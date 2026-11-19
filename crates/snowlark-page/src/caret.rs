// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The mouse in text fields and textareas: a press puts the caret at the cluster under the pointer
//! (Shift extends the selection), a drag selects, a double-click selects a word and a triple-click the
//! line (the whole value of a one-line field), and dragging after them extends by words or lines. The
//! offsets come from layout's shaped text (`snowlark_layout::controls::control_text_offset_at`) and are
//! kept in UTF-16 code units of the value, as the selection APIs count. A one-line field's text scrolls
//! so that the caret stays in view (after each rendering update's layout).

use snowlark_dom::NodeId;
use snowlark_paint::Point;

use crate::engine::Engine;
use crate::input::Modifiers;

/// What a drag in a text control extends by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Granularity {
    Character,
    Word,
    Line,
}

/// A press-and-drag in a text control: the element, the anchor range (a word or a line for a
/// double or triple click; empty for a single one) and the granularity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextDrag {
    pub element: NodeId,
    pub anchor: (u32, u32),
    pub granularity: Granularity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Word characters, by script: a change of script is a word boundary (Hangul next to Latin).
    Word(Script),
    Space,
    Newline,
    Other,
}

/// The scripts whose letters do not join a word of another script's letters.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    Hangul,
    Han,
    Hiragana,
    Katakana,
    /// Every other script (Latin, Cyrillic, ...), digits and the joiners `_` and `'`.
    Alphabetic,
}

fn script(c: char) -> Script {
    match c as u32 {
        0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF | 0xFFA0..=0xFFDC => {
            Script::Hangul
        }
        0x3040..=0x309F => Script::Hiragana,
        0x30A0..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => Script::Katakana,
        0x2E80..=0x2FDF
        | 0x3005..=0x3007
        | 0x3021..=0x3029
        | 0x3038..=0x303B
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF
        | 0x20000..=0x3FFFF => Script::Han,
        _ => Script::Alphabetic,
    }
}

fn class(c: char) -> Class {
    if c == '\n' {
        Class::Newline
    } else if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' || c == '\'' {
        Class::Word(script(c))
    } else {
        Class::Other
    }
}

/// What a character is to word movement over rendered content (`Engine::step_word`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WordUnit {
    /// A line boundary or an atomic inline: a unit of its own.
    Boundary,
    Space,
    /// Letters of one script.
    Word(u8),
    /// Punctuation and symbols: one unit whatever the characters.
    Punctuation,
}

pub(crate) fn word_unit(c: char) -> WordUnit {
    match class(c) {
        Class::Newline => WordUnit::Boundary,
        Class::Space => WordUnit::Space,
        Class::Word(s) => WordUnit::Word(s as u8),
        Class::Other => WordUnit::Punctuation,
    }
}

/// Where moving by a word from `at` (UTF-16) goes in `value` (`Selection.modify`, Ctrl+arrows). A
/// unit is a word (letters of one script) or a run of punctuation, whatever its characters; forward
/// goes to the end of the next unit and over the spaces after it (as on Windows), backward to the
/// start of the unit before. A line break is a unit of its own.
pub fn word_move(value: &str, at: u32, forward: bool) -> u32 {
    let s: Vec<char> = value.chars().collect();
    let i = char_at(&s, at);
    // Punctuation characters of different kinds are one unit.
    let same = |a: char, b: char| class(a) == class(b) && class(a) != Class::Newline;
    let to = if forward {
        let mut j = i;
        if j < s.len() && s[j] == '\n' {
            j += 1;
        } else {
            while j < s.len() && class(s[j]) == Class::Space {
                j += 1;
            }
            if j < s.len() && s[j] != '\n' {
                let first = s[j];
                j += 1;
                while j < s.len() && same(first, s[j]) {
                    j += 1;
                }
                while j < s.len() && class(s[j]) == Class::Space {
                    j += 1;
                }
            }
        }
        j
    } else {
        let mut j = i;
        while j > 0 && class(s[j - 1]) == Class::Space {
            j -= 1;
        }
        if j > 0 {
            let last = s[j - 1];
            j -= 1;
            while j > 0 && same(last, s[j - 1]) {
                j -= 1;
            }
        }
        j
    };
    utf16_of(&s, to)
}

/// UTF-16 offset → char index in `s`.
fn char_at(s: &[char], utf16: u32) -> usize {
    let mut n = 0u32;
    for (i, c) in s.iter().enumerate() {
        if n >= utf16 {
            return i;
        }
        n += c.len_utf16() as u32;
    }
    s.len()
}

fn utf16_of(s: &[char], i: usize) -> u32 {
    s[..i].iter().map(|c| c.len_utf16() as u32).sum()
}

/// The word around `at` (UTF-16): a run of word characters (with the spaces after it, as Windows
/// selects), a run of spaces, or one other character. At the end of the text, the word before.
pub fn word_at(value: &str, at: u32) -> (u32, u32) {
    let s: Vec<char> = value.chars().collect();
    if s.is_empty() {
        return (0, 0);
    }
    let mut i = char_at(&s, at).min(s.len() - 1);
    // At a line end (or the text's), the word is the one before.
    if (char_at(&s, at) == s.len() || s[i] == '\n') && i > 0 && s[i - 1] != '\n' {
        i = if char_at(&s, at) == s.len() {
            s.len() - 1
        } else {
            i - 1
        };
    }
    let k = class(s[i]);
    if k == Class::Newline {
        return (utf16_of(&s, i), utf16_of(&s, i));
    }
    let mut start = i;
    let mut end = i + 1;
    if k != Class::Other {
        while start > 0 && class(s[start - 1]) == k {
            start -= 1;
        }
        while end < s.len() && class(s[end]) == k {
            end += 1;
        }
    }
    if matches!(k, Class::Word(_)) {
        while end < s.len() && class(s[end]) == Class::Space {
            end += 1;
        }
    }
    (utf16_of(&s, start), utf16_of(&s, end))
}

/// The line (paragraph) around `at`, without its newline.
pub fn line_at(value: &str, at: u32) -> (u32, u32) {
    let s: Vec<char> = value.chars().collect();
    let i = char_at(&s, at);
    let mut start = i;
    while start > 0 && s[start - 1] != '\n' {
        start -= 1;
    }
    let mut end = i;
    while end < s.len() && s[end] != '\n' {
        end += 1;
    }
    (utf16_of(&s, start), utf16_of(&s, end))
}

/// The selection a drag makes: from the anchor range to the unit at `focus`, with its direction.
fn drag_selection(value: &str, drag: &TextDrag, focus: u32) -> (u32, u32, &'static str) {
    let (a0, a1) = drag.anchor;
    let (f0, f1) = match drag.granularity {
        Granularity::Character => (focus, focus),
        Granularity::Word => word_at(value, focus),
        Granularity::Line => line_at(value, focus),
    };
    if f0 < a0 {
        (f0, a1, "backward")
    } else {
        (a0, f1.max(a1), "forward")
    }
}

impl Engine {
    /// The caret offset (UTF-16 in the value) in text control `el` nearest to the top-level viewport
    /// point `p`.
    pub(crate) fn text_offset_at(&self, el: NodeId, p: Point) -> Option<u32> {
        let content = self.top_viewport_box(el, true)?;
        let local = Point::new(p.x - content.x, p.y - content.y);
        let byte = self
            .with_layout(el, |r, _| {
                r.with_box_fragment(el, |f| {
                    snowlark_layout::controls::control_text_offset_at(f, local)
                })
            })
            .flatten()
            .flatten()?;
        let masked = self.input_type(el).as_deref() == Some("password");
        let value = self.editor_value(el);
        Some(snowlark_render::controls::utf16_of_display(
            &value, byte, masked,
        ))
    }

    /// A primary press at `p` on a focused text control: the caret (or the word, or the line) and the
    /// drag that follows.
    pub(crate) fn text_control_pressed(&self, el: NodeId, p: Point, clicks: u32, mods: Modifiers) {
        let Some(at) = self.text_offset_at(el, p) else {
            return;
        };
        let value = self.editor_value(el);
        let (granularity, anchor) = match clicks {
            0 | 1 => (Granularity::Character, (at, at)),
            2 => (Granularity::Word, word_at(&value, at)),
            _ if self.is_textarea_el(el) => (Granularity::Line, line_at(&value, at)),
            _ => {
                let len = value.encode_utf16().count() as u32;
                (Granularity::Line, (0, len))
            }
        };
        let mut drag = TextDrag {
            element: el,
            anchor,
            granularity,
        };
        if granularity == Granularity::Character && mods.shift {
            // Shift: extend from the existing selection's anchor.
            let (s, e, d) = self.ctl(el, |c| c.selection);
            let a = if d == "backward" { e } else { s };
            drag.anchor = (a, a);
            let (s, e, d) = drag_selection(&value, &drag, at);
            self.set_selection(el, s, e, Some(d));
        } else {
            let (s, e) = anchor;
            self.set_selection(el, s, e, None);
        }
        self.control_edited(el);
        self.pointer.borrow_mut().text_drag = Some(drag);
    }

    /// The pointer moved to `p` with the primary button held after a press in a text control: the
    /// selection follows.
    pub(crate) fn text_control_dragged(&self, p: Point) {
        let Some(drag) = self.pointer.borrow().text_drag else {
            return;
        };
        if !self.dom().is_alive(drag.element) {
            self.pointer.borrow_mut().text_drag = None;
            return;
        }
        let Some(at) = self.text_offset_at(drag.element, p) else {
            return;
        };
        let value = self.editor_value(drag.element);
        let (s, e, d) = drag_selection(&value, &drag, at);
        if self.ctl(drag.element, |c| (c.selection.0, c.selection.1)) != (s, e) {
            self.set_selection(drag.element, s, e, Some(d));
            self.control_edited(drag.element);
        }
    }

    fn is_textarea_el(&self, el: NodeId) -> bool {
        self.dom()
            .is_html_element_named(el, &snowlark_dom::local_name!("textarea"))
    }

    /// After a layout: the focused one-line field of `doc` scrolls its text so that the caret is in
    /// view (moving as little as possible). Returns whether it scrolled (lay out again).
    pub(crate) fn keep_caret_in_view(&self, doc: NodeId) -> bool {
        let Some(el) = self.focused_element(doc) else {
            return false;
        };
        if !self.is_text_control(el) || self.is_textarea_el(el) {
            return false;
        }
        let (_, _, d) = self.ctl(el, |c| c.selection);
        let (s, e, _) = self.ctl(el, |c| c.selection);
        let focus = if d == "backward" { s } else { e };
        let masked = self.input_type(el).as_deref() == Some("password");
        let value = self.editor_value(el);
        let byte = snowlark_render::controls::display_byte(&value, focus, masked);
        let metrics = self
            .with_doc_renderer(doc, |r, _| {
                r.with_box_fragment(el, |f| {
                    snowlark_layout::controls::control_caret_metrics(f, byte)
                })
            })
            .flatten()
            .flatten();
        let Some((caret, text_width, width)) = metrics else {
            return false;
        };
        let old = self.ctl(el, |c| c.scroll_x);
        let new = scroll_for_caret(old, caret, text_width, width);
        if (new - old).abs() < 0.01 {
            return false;
        }
        self.ctl(el, |c| c.scroll_x = new);
        true
    }
}

/// The scroll that keeps `caret` (from the text's start) in a view `width` wide, moving `old` as
/// little as possible, within the text (and a pixel for the caret at its end).
pub fn scroll_for_caret(old: f32, caret: f32, text_width: f32, width: f32) -> f32 {
    let mut s = old;
    if caret < s {
        s = caret;
    }
    if caret > s + width - 1.0 {
        s = caret - width + 1.0;
    }
    let max = (text_width + 1.0 - width).max(0.0);
    s.clamp(0.0, max)
}

impl Engine {
    /// The primary button went up: a drag in a text control ends.
    pub(crate) fn text_drag_ended(&self) {
        self.pointer.borrow_mut().text_drag = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_lines() {
        let s = "hello  world, foo\nbar";
        assert_eq!(word_at(s, 1), (0, 7));
        assert_eq!(word_at(s, 6), (5, 7));
        assert_eq!(word_at(s, 9), (7, 12));
        assert_eq!(word_at(s, 12), (12, 13));
        // At the end of a line: the word before it.
        assert_eq!(word_at(s, 17), (14, 17));
        assert_eq!(word_at(s, 21), (18, 21));
        assert_eq!(line_at(s, 3), (0, 17));
        assert_eq!(line_at(s, 19), (18, 21));
        assert_eq!(word_at("", 0), (0, 0));
        // UTF-16 offsets past astral characters.
        assert_eq!(word_at("\u{1F600} ab", 3), (3, 5));
        // A change of script ends a word.
        assert_eq!(word_at("\u{D76C}\u{C9C4}DJ", 1), (0, 2));
    }

    #[test]
    fn moving_by_word() {
        let s = "one two, three";
        assert_eq!(word_move(s, 0, true), 4);
        assert_eq!(word_move(s, 4, true), 7);
        assert_eq!(word_move(s, 7, true), 9);
        assert_eq!(word_move(s, 14, false), 9);
        assert_eq!(word_move(s, 9, false), 7);
        assert_eq!(word_move(s, 7, false), 4);
        // Hangul next to Latin: two words.
        let k = "\u{D76C}\u{C9C4}DJ";
        assert_eq!(word_move(k, 0, true), 2);
        assert_eq!(word_move(k, 4, false), 2);
        // A run of punctuation is one unit, whatever its characters.
        let p = "~>>>>>p+++";
        assert_eq!(word_move(p, 6, false), 0);
        assert_eq!(word_move(p, 7, true), 10);
        // A line break is a stop.
        assert_eq!(word_move("ab\ncd", 2, true), 3);
        assert_eq!(word_move("ab\ncd", 3, false), 2);
        assert_eq!(word_move("ab \ncd", 0, true), 3);
    }

    #[test]
    fn drags_extend_by_their_unit() {
        let s = "one two three";
        let drag = TextDrag {
            element: NodeId::from_bits(1 << 32).unwrap(),
            anchor: word_at(s, 5),
            granularity: Granularity::Word,
        };
        assert_eq!(drag_selection(s, &drag, 10), (4, 13, "forward"));
        assert_eq!(drag_selection(s, &drag, 1), (0, 8, "backward"));
        let chars = TextDrag {
            anchor: (5, 5),
            granularity: Granularity::Character,
            ..drag
        };
        assert_eq!(drag_selection(s, &chars, 2), (2, 5, "backward"));
        assert_eq!(drag_selection(s, &chars, 9), (5, 9, "forward"));
    }

    #[test]
    fn the_caret_stays_in_view() {
        // Text 300 wide in a 100 wide field.
        assert_eq!(scroll_for_caret(0.0, 50.0, 300.0, 100.0), 0.0);
        assert_eq!(scroll_for_caret(0.0, 150.0, 300.0, 100.0), 51.0);
        // Moving left within the view keeps the scroll; past its left edge follows the caret.
        assert_eq!(scroll_for_caret(51.0, 80.0, 300.0, 100.0), 51.0);
        assert_eq!(scroll_for_caret(51.0, 20.0, 300.0, 100.0), 20.0);
        // Short text never scrolls.
        assert_eq!(scroll_for_caret(30.0, 10.0, 50.0, 100.0), 0.0);
    }
}
