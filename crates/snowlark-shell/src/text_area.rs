// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A multi-line text editor for the window's panels (the element tree's "Edit as HTML", and
//! whatever else needs more than one line): lines split at `\n`, no wrapping (long lines scroll
//! sideways), the usual editing keys, Up and Down by line, Enter for a new line. With
//! `single_line` it is a one-line field whose Enter commits (the attribute editor).

use std::cell::Cell;

use snowlark_paint::window::{keys, Modifiers};
use snowlark_paint::{DisplayListBuilder, Point, Radii, Rect};

use crate::address::TextField;
use crate::chrome::{Face, Fonts, Theme};
use crate::driver::Clipboard;
use crate::widgets::{self, KeyEdit};

const PAD: f32 = 6.0;

/// What a key did to the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AreaKey {
    /// Not a key of the editor.
    Ignored,
    /// The caret, selection or text changed (redraw).
    Edited,
    /// Enter (single line) or Ctrl+Enter: the text is final.
    Commit,
    /// Escape.
    Cancel,
}

pub struct TextArea {
    field: TextField,
    single_line: bool,
    /// The scroll offsets as last drawn (x, y), kept so the caret stays in view.
    scroll: Cell<(f32, f32)>,
}

impl TextArea {
    pub fn new(text: &str, single_line: bool) -> TextArea {
        let mut field = TextField::default();
        if single_line {
            field.set_text(text);
        } else {
            field.set_multiline_text(text);
        }
        TextArea {
            field,
            single_line,
            scroll: Cell::new((0.0, 0.0)),
        }
    }

    pub fn text(&self) -> &str {
        self.field.text()
    }

    pub fn single_line(&self) -> bool {
        self.single_line
    }

    /// The line count.
    pub fn lines(&self) -> usize {
        self.text().split('\n').count()
    }

    /// The byte range of the line holding byte `at` (without its line break).
    fn line_at(&self, at: usize) -> (usize, usize) {
        line_at(self.text(), at)
    }

    /// Moves the caret to the same column of the line above or below.
    fn vertical(&mut self, down: bool, extend: bool) {
        vertical(&mut self.field, down, extend);
    }

    /// A key while the editor has the keyboard.
    pub fn key(&mut self, key: u16, mods: Modifiers, clip: &dyn Clipboard) -> AreaKey {
        let ctrl = mods.ctrl && !mods.alt;
        match key {
            keys::ESCAPE => return AreaKey::Cancel,
            keys::ENTER if self.single_line || ctrl => return AreaKey::Commit,
            keys::TAB if self.single_line => return AreaKey::Commit,
            keys::ENTER => self.field.insert_multiline("\n"),
            keys::TAB => self.field.insert_multiline("  "),
            keys::UP | keys::DOWN if !self.single_line => {
                self.vertical(key == keys::DOWN, mods.shift)
            }
            keys::HOME | keys::END if !self.single_line && !ctrl => {
                let (s, e) = self.line_at(self.field.caret());
                self.field
                    .set_caret(if key == keys::HOME { s } else { e }, mods.shift);
            }
            k if ctrl && k == b'V' as u16 && !self.single_line => match clip.text() {
                Some(t) => self.field.insert_multiline(&t),
                None => return AreaKey::Ignored,
            },
            k => {
                return match widgets::edit_key(&mut self.field, k, mods, clip) {
                    KeyEdit::Ignored => AreaKey::Ignored,
                    _ => AreaKey::Edited,
                }
            }
        }
        AreaKey::Edited
    }

    /// A typed character; whether it was text.
    pub fn typed(&mut self, c: char) -> bool {
        if c.is_control() {
            return false;
        }
        let mut buf = [0u8; 4];
        self.field.insert_multiline(c.encode_utf8(&mut buf));
        true
    }

    fn line_height(size: f32) -> f32 {
        (size * 1.5).round()
    }

    /// The height that shows `lines` lines (at most `max`) in a box.
    pub fn height_for(lines: usize, max: usize, size: f32) -> f32 {
        lines.clamp(1, max.max(1)) as f32 * Self::line_height(size) + 2.0 * PAD
    }

    /// The caret index under `p` in a box `r` drawn at `size`.
    pub fn index_at(&self, fonts: &Fonts, face: Face, r: Rect, p: Point, size: f32) -> usize {
        let (sx, sy) = self.scroll.get();
        let lh = Self::line_height(size);
        let row = ((p.y - r.y - PAD + sy) / lh).floor().max(0.0) as usize;
        let mut start = 0;
        for (i, line) in self.text().split('\n').enumerate() {
            if i == row || start + line.len() >= self.text().len() {
                let x = p.x - r.x - PAD + sx;
                return start + fonts.index_at_in(face, line, x, size).min(line.len());
            }
            start += line.len() + 1;
        }
        self.text().len()
    }

    /// A press in the box (`r`): the caret goes there, a double click selects a word, a triple
    /// click all.
    #[allow(clippy::too_many_arguments)]
    pub fn press(
        &mut self,
        fonts: &Fonts,
        face: Face,
        r: Rect,
        p: Point,
        size: f32,
        clicks: u32,
        shift: bool,
    ) {
        let at = self.index_at(fonts, face, r, p, size);
        self.field.press(at, clicks, shift);
    }

    /// Draws the box `r`: lines of text in `face`, the selection and the caret.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        b: &mut DisplayListBuilder,
        fonts: &Fonts,
        t: &Theme,
        face: Face,
        r: Rect,
        size: f32,
        caret_on: bool,
    ) {
        b.fill_rounded_rect(r, Radii::uniform(4.0), t.field);
        b.stroke_rounded_rect(r.inflate(-0.5, -0.5), Radii::uniform(4.0), 2.0, t.accent);
        let lh = Self::line_height(size);
        let inner = Rect::new(
            r.x + PAD,
            r.y + PAD,
            (r.width - 2.0 * PAD).max(1.0),
            (r.height - 2.0 * PAD).max(lh),
        );
        let text = self.text();
        let caret = self.field.caret();
        let (cs, ce) = self.line_at(caret);
        let caret_row = text[..cs].matches('\n').count();
        let caret_x = fonts.caret_x_in(face, &text[cs..ce], caret - cs, size);
        // Scroll to keep the caret line and column in view.
        let (mut sx, mut sy) = self.scroll.get();
        let rows = (inner.height / lh).floor().max(1.0);
        let top = caret_row as f32 * lh;
        if top < sy {
            sy = top;
        } else if top + lh > sy + rows * lh {
            sy = top + lh - rows * lh;
        }
        sy = sy.clamp(0.0, ((self.lines() as f32) * lh - inner.height).max(0.0));
        if caret_x < sx {
            sx = caret_x;
        } else if caret_x > sx + inner.width - 4.0 {
            sx = caret_x - inner.width + 4.0;
        }
        sx = sx.max(0.0);
        self.scroll.set((sx, sy));
        b.push_clip_rect(inner);
        let sel = self.field.selection();
        let mut start = 0;
        for (i, line) in text.split('\n').enumerate() {
            let end = start + line.len();
            let y = inner.y + i as f32 * lh - sy;
            if y + lh >= inner.y && y <= inner.bottom() {
                if self.field.has_selection() && sel.start <= end && sel.end >= start {
                    let a = sel.start.max(start) - start;
                    let z = sel.end.min(end) - start;
                    let x0 = fonts.caret_x_in(face, line, a, size);
                    // (a selected line break shows as a little extra width)
                    let extra = if sel.end > end { 4.0 } else { 0.0 };
                    let x1 = fonts.caret_x_in(face, line, z, size) + extra;
                    b.fill_rect(
                        Rect::new(inner.x + x0 - sx, y, (x1 - x0).max(0.0), lh),
                        t.selection,
                    );
                }
                fonts.label_in(
                    b,
                    face,
                    line,
                    inner.x - sx,
                    y + lh / 2.0,
                    size,
                    t.text,
                    f32::INFINITY,
                );
            }
            start = end + 1;
        }
        if caret_on && !self.field.has_selection() {
            let x = (inner.x + caret_x - sx).round();
            let y = inner.y + caret_row as f32 * lh - sy;
            b.fill_rect(Rect::new(x, y + 1.0, 1.0, lh - 2.0), t.text);
        }
        b.pop_clip();
    }
}

/// The byte range of the line of `text` holding byte `at` (without its line break).
pub fn line_at(text: &str, at: usize) -> (usize, usize) {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    (start, end)
}

/// Moves a multi-line field's caret to the same column of the line above or below (from the
/// first line Up goes to the start, from the last Down to the end); `extend` keeps the anchor.
pub fn vertical(field: &mut TextField, down: bool, extend: bool) {
    let caret = field.caret();
    let (start, end) = line_at(field.text(), caret);
    let column = field.text()[start..caret].chars().count();
    let target = if down {
        if end >= field.text().len() {
            field.set_caret(field.text().len(), extend);
            return;
        }
        end + 1
    } else {
        if start == 0 {
            field.set_caret(0, extend);
            return;
        }
        line_at(field.text(), start - 1).0
    };
    let (ls, le) = line_at(field.text(), target);
    let line = &field.text()[ls..le];
    let at = line
        .char_indices()
        .nth(column)
        .map_or(line.len(), |(i, _)| i);
    field.set_caret(ls + at, extend);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::MemoryClipboard;

    fn key(a: &mut TextArea, k: u16, ctrl: bool, clip: &MemoryClipboard) -> AreaKey {
        a.key(
            k,
            Modifiers {
                ctrl,
                ..Default::default()
            },
            clip,
        )
    }

    fn type_str(a: &mut TextArea, s: &str) {
        for c in s.chars() {
            assert!(a.typed(c));
        }
    }

    #[test]
    fn enter_adds_a_line_and_ctrl_enter_commits() {
        let clip = MemoryClipboard::default();
        let mut a = TextArea::new("<p>a</p>", false);
        assert_eq!(key(&mut a, keys::ENTER, false, &clip), AreaKey::Edited);
        type_str(&mut a, "<p>b</p>");
        assert_eq!(a.text(), "<p>a</p>\n<p>b</p>");
        assert_eq!(a.lines(), 2);
        assert_eq!(key(&mut a, keys::ENTER, true, &clip), AreaKey::Commit);
        assert_eq!(key(&mut a, keys::ESCAPE, false, &clip), AreaKey::Cancel);
        // A single line commits on Enter and keeps no line break.
        let mut f = TextArea::new("a=\"1\"", true);
        assert_eq!(key(&mut f, keys::ENTER, false, &clip), AreaKey::Commit);
        assert_eq!(TextArea::new("x\ny", true).text(), "xy");
    }

    #[test]
    fn up_and_down_keep_the_column_and_home_end_are_per_line() {
        let clip = MemoryClipboard::default();
        let mut a = TextArea::new("abcdef\nxy\nlonger line", false);
        // The caret starts at the end (column 11 of the last line); Up lands at the end of "xy".
        key(&mut a, keys::UP, false, &clip);
        assert_eq!(a.field.caret(), "abcdef\nxy".len());
        key(&mut a, keys::HOME, false, &clip);
        assert_eq!(a.field.caret(), "abcdef\n".len());
        key(&mut a, keys::UP, false, &clip);
        assert_eq!(a.field.caret(), 0);
        key(&mut a, keys::DOWN, false, &clip);
        assert_eq!(a.field.caret(), "abcdef\n".len());
        key(&mut a, keys::END, false, &clip);
        assert_eq!(a.field.caret(), "abcdef\nxy".len());
        // Up on the first line goes to the start, Down on the last to the end.
        key(&mut a, keys::UP, false, &clip);
        key(&mut a, keys::UP, false, &clip);
        assert_eq!(a.field.caret(), 0);
        for _ in 0..4 {
            key(&mut a, keys::DOWN, false, &clip);
        }
        assert_eq!(a.field.caret(), a.text().len());
    }

    #[test]
    fn paste_keeps_line_breaks_in_an_area_and_not_in_a_field() {
        let clip = MemoryClipboard::default();
        clip.set_text("<b>\r\n  x\r\n</b>");
        let mut a = TextArea::new("", false);
        key(&mut a, b'V' as u16, true, &clip);
        assert_eq!(a.text(), "<b>\n  x\n</b>");
        let mut f = TextArea::new("", true);
        key(&mut f, b'V' as u16, true, &clip);
        assert!(!f.text().contains('\n'));
    }
}
