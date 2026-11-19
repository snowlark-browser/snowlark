// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small pieces the window's panels are made of (the bookmark editor, confirmations, the
//! developer tools): single-line text fields with the usual editing keys, buttons, tab strips.
//! Geometry is in DIPs of whatever list the caller builds.

use snowlark_paint::window::{keys, Modifiers};
use snowlark_paint::{Color, DisplayListBuilder, Radii, Rect};

use crate::address::TextField;
use crate::chrome::{Face, Fonts, Theme};
use crate::driver::Clipboard;

pub const FIELD_SIZE: f32 = 13.0;

/// What a key did to a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEdit {
    /// Not an editing key.
    Ignored,
    /// The caret or selection moved.
    Moved,
    /// The text changed.
    Changed,
}

/// The editing keys of a single-line field (arrows, Home/End with Shift and Ctrl, Backspace,
/// Delete, Ctrl+A/C/X/V/Z/Y).
pub fn edit_key(f: &mut TextField, key: u16, mods: Modifiers, clip: &dyn Clipboard) -> KeyEdit {
    edit(f, key, mods, clip, false)
}

/// [`edit_key`] for the address bar, which takes a paste as one trimmed line.
pub fn edit_address_key(
    f: &mut TextField,
    key: u16,
    mods: Modifiers,
    clip: &dyn Clipboard,
) -> KeyEdit {
    edit(f, key, mods, clip, true)
}

fn edit(
    f: &mut TextField,
    key: u16,
    mods: Modifiers,
    clip: &dyn Clipboard,
    one_line_paste: bool,
) -> KeyEdit {
    let word = mods.ctrl;
    // (Ctrl+Alt is AltGr on many layouts: it types characters)
    let ctrl = mods.ctrl && !mods.alt;
    match key {
        keys::LEFT => f.left(mods.shift, word),
        keys::RIGHT => f.right(mods.shift, word),
        keys::HOME => f.home(mods.shift),
        keys::END => f.end(mods.shift),
        keys::BACK => {
            f.backspace(word);
            return KeyEdit::Changed;
        }
        keys::DELETE => {
            f.delete(word);
            return KeyEdit::Changed;
        }
        k if ctrl && k == b'A' as u16 => f.select_all(),
        k if ctrl && k == b'C' as u16 => {
            if f.has_selection() {
                clip.set_text(f.selected_text());
            }
        }
        k if ctrl && k == b'X' as u16 => {
            if f.has_selection() {
                clip.set_text(&f.cut());
                return KeyEdit::Changed;
            }
        }
        k if ctrl && k == b'V' as u16 => {
            if let Some(s) = clip.text() {
                if one_line_paste {
                    f.paste(&s);
                } else {
                    f.insert(&s);
                }
                return KeyEdit::Changed;
            }
        }
        k if ctrl && k == b'Z' as u16 && !mods.shift => {
            if f.undo() {
                return KeyEdit::Changed;
            }
        }
        k if ctrl && (k == b'Y' as u16 || (k == b'Z' as u16 && mods.shift)) => {
            if f.redo() {
                return KeyEdit::Changed;
            }
        }
        _ => return KeyEdit::Ignored,
    }
    KeyEdit::Moved
}

/// Types a character into a field; whether it was text.
pub fn type_char(f: &mut TextField, c: char) -> bool {
    if c.is_control() {
        return false;
    }
    let mut buf = [0u8; 4];
    f.insert(c.encode_utf8(&mut buf));
    true
}

/// A text field's look.
pub struct FieldStyle<'a> {
    pub focused: bool,
    pub caret_on: bool,
    pub placeholder: &'a str,
    pub size: f32,
    /// A boxed field (else the text alone, as in a console prompt).
    pub boxed: bool,
    pub face: Face,
}

/// How far a field's text scrolls left to keep its caret in `room`.
pub fn field_scroll(fonts: &Fonts, face: Face, f: &TextField, room: f32, size: f32) -> f32 {
    let cx = fonts.caret_x_in(face, f.text(), f.caret(), size);
    (cx - room + 2.0).max(0.0)
}

/// Draws a field in `r`.
pub fn draw_field(
    b: &mut DisplayListBuilder,
    fonts: &Fonts,
    t: &Theme,
    r: Rect,
    f: &TextField,
    style: &FieldStyle,
) {
    let pad = if style.boxed { 8.0 } else { 0.0 };
    if style.boxed {
        b.fill_rounded_rect(r, Radii::uniform(4.0), t.field);
        let border = if style.focused { t.accent } else { t.separator };
        b.stroke_rounded_rect(
            r.inflate(-0.5, -0.5),
            Radii::uniform(4.0),
            if style.focused { 2.0 } else { 1.0 },
            border,
        );
    }
    let room = (r.width - 2.0 * pad).max(1.0);
    let cy = r.y + r.height / 2.0;
    let face = style.face;
    b.push_clip_rect(Rect::new(r.x + pad - 1.0, r.y, room + 2.0, r.height));
    let scroll = if style.focused {
        field_scroll(fonts, face, f, room, style.size)
    } else {
        0.0
    };
    let x0 = r.x + pad - scroll;
    let line = style.size * 1.35;
    if f.text().is_empty() {
        if !style.placeholder.is_empty() {
            let (x, size) = (r.x + pad, style.size);
            fonts.label_in(b, face, style.placeholder, x, cy, size, t.text_dim, room);
        }
    } else {
        if style.focused && f.has_selection() {
            let sel = f.selection();
            let s0 = fonts.caret_x_in(face, f.text(), sel.start, style.size);
            let s1 = fonts.caret_x_in(face, f.text(), sel.end, style.size);
            b.fill_rect(
                Rect::new(x0 + s0, cy - line / 2.0, s1 - s0, line),
                t.selection,
            );
        }
        let size = style.size;
        fonts.label_in(b, face, f.text(), x0, cy, size, t.text, f32::INFINITY);
    }
    if style.focused && style.caret_on && !f.has_selection() {
        let cx = (x0 + fonts.caret_x_in(face, f.text(), f.caret(), style.size)).round();
        b.fill_rect(Rect::new(cx, cy - line / 2.0, 1.0, line), t.text);
    }
    b.pop_clip();
}

/// The caret index at window x `x` in a field drawn in `r` (unscrolled).
pub fn field_index_at(fonts: &Fonts, f: &TextField, r: Rect, x: f32, style: &FieldStyle) -> usize {
    let pad = if style.boxed { 8.0 } else { 0.0 };
    let scroll = if style.focused {
        field_scroll(
            fonts,
            style.face,
            f,
            (r.width - 2.0 * pad).max(1.0),
            style.size,
        )
    } else {
        0.0
    };
    fonts.index_at_in(style.face, f.text(), x - r.x - pad + scroll, style.size)
}

/// A push button.
pub fn draw_button(
    b: &mut DisplayListBuilder,
    fonts: &Fonts,
    t: &Theme,
    r: Rect,
    label: &str,
    primary: bool,
    hover: bool,
) {
    let (bg, fg) = if primary {
        (
            t.accent,
            if t.dark {
                Color::hex(0x202124)
            } else {
                Color::WHITE
            },
        )
    } else {
        (if hover { t.dropdown_hover } else { t.dropdown }, t.accent)
    };
    b.fill_rounded_rect(r, Radii::uniform(r.height / 2.0), bg);
    if !primary {
        b.stroke_rounded_rect(
            r.inflate(-0.5, -0.5),
            Radii::uniform(r.height / 2.0 - 0.5),
            1.0,
            t.separator,
        );
    } else if hover {
        b.fill_rounded_rect(r, Radii::uniform(r.height / 2.0), t.button_hover);
    }
    let w = fonts.width(label, FIELD_SIZE);
    fonts.label(
        b,
        label,
        r.x + (r.width - w) / 2.0,
        r.y + r.height / 2.0,
        FIELD_SIZE,
        fg,
        f32::INFINITY,
    );
}

/// The width a button needs for `label`.
pub fn button_width(fonts: &Fonts, label: &str) -> f32 {
    (fonts.width(label, FIELD_SIZE) + 32.0).max(72.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::MemoryClipboard;

    #[test]
    fn editing_keys() {
        let clip = MemoryClipboard::default();
        let mut f = TextField::new("hello world");
        let none = Modifiers::default();
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(edit_key(&mut f, keys::BACK, ctrl, &clip), KeyEdit::Changed);
        assert_eq!(f.text(), "hello ");
        assert_eq!(edit_key(&mut f, keys::HOME, none, &clip), KeyEdit::Moved);
        assert!(type_char(&mut f, 'X'));
        assert!(!type_char(&mut f, '\u{8}'));
        assert_eq!(f.text(), "Xhello ");
        edit_key(&mut f, b'A' as u16, ctrl, &clip);
        edit_key(&mut f, b'X' as u16, ctrl, &clip);
        assert_eq!(f.text(), "");
        assert_eq!(clip.text().as_deref(), Some("Xhello "));
        edit_key(&mut f, b'V' as u16, ctrl, &clip);
        assert_eq!(f.text(), "Xhello ");
        assert_eq!(edit_key(&mut f, b'Q' as u16, none, &clip), KeyEdit::Ignored);
    }

    /// Every shortcut the address bar's menu shows, through the keys the bar gets.
    #[test]
    fn menu_shortcuts_work_as_keys() {
        let clip = MemoryClipboard::default();
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        let altgr = Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let key = |c: char| c as u16;
        let mut f = TextField::new("one two");
        let press = |f: &mut TextField, k: u16, m: Modifiers| edit_address_key(f, k, m, &clip);
        // Ctrl+A, Ctrl+C
        press(&mut f, key('A'), ctrl);
        assert_eq!(f.selected_text(), "one two");
        press(&mut f, key('C'), ctrl);
        assert_eq!(f.text(), "one two");
        // Ctrl+X
        assert_eq!(press(&mut f, key('X'), ctrl), KeyEdit::Changed);
        assert_eq!(f.text(), "");
        // Ctrl+Z brings it back, selected; Ctrl+Y and Ctrl+Shift+Z redo.
        assert_eq!(press(&mut f, key('Z'), ctrl), KeyEdit::Changed);
        assert_eq!(f.text(), "one two");
        assert_eq!(f.selected_text(), "one two");
        assert_eq!(press(&mut f, key('Y'), ctrl), KeyEdit::Changed);
        assert_eq!(f.text(), "");
        press(&mut f, key('Z'), ctrl);
        press(&mut f, key('Z'), ctrl_shift);
        assert_eq!(f.text(), "");
        assert_eq!(clip.text().as_deref(), Some("one two"));
        // Ctrl+V pastes one trimmed line.
        clip.set_text(
            "a
b ",
        );
        let r = edit_address_key(&mut f, key('V'), ctrl, &clip);
        assert_eq!((r, f.text()), (KeyEdit::Changed, "a b"));
        // Delete
        f.select_all();
        edit_address_key(&mut f, keys::DELETE, Modifiers::default(), &clip);
        assert_eq!(f.text(), "");
        // Ctrl+Alt (AltGr) is not a shortcut.
        let r = edit_address_key(&mut f, key('V'), altgr, &clip);
        assert_eq!((r, f.text()), (KeyEdit::Ignored, ""));
    }
}
