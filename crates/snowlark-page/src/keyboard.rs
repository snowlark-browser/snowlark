// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Keyboard input (UI Events §3.7): `keydown`, `keypress` and `keyup` with the `key` and `code`
//! values of Windows virtual keys and scan codes (UI Events KeyboardEvent key Values and code
//! Values), at the focused element; and the default actions a canceled `keydown` suppresses:
//! sequential focus navigation with Tab (HTML §6.6.5), activation with Enter and Space, implicit form
//! submission (HTML §4.10.21.2), `select` option changes with the arrows, and the editing of text
//! fields and textareas (HTML §4.10.5 and §4.10.11: the dirty value, the caret and the selection in
//! UTF-16 code units, `maxlength`, `readonly` and `disabled`, `beforeinput`/`input`/`change`, the
//! clipboard).

use snowlark_bindings::generated::HTMLElementImpl;
use snowlark_bindings::rt::Cx;
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, NodeId};

use crate::engine::events::{EventExt, UiFields};
use crate::engine::{node_handle, Engine};
use crate::input::Modifiers;
use crate::Page;

/// A key event from the embedder.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyInput {
    /// Windows virtual-key code.
    pub vk: u16,
    /// Scan code with `0x100` for extended keys.
    pub scan: u16,
    /// The character the key types (the `key` of a printable key).
    pub text: Option<char>,
    pub mods: Modifiers,
    pub repeat: bool,
    /// The clipboard's text, read by the embedder for Ctrl+V (and Shift+Insert).
    pub paste: Option<String>,
    /// The clipboard's markup (`text/html`), with `paste`.
    pub paste_html: Option<String>,
}

/// What a key event did, for the embedder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyOutcome {
    /// Neither the page nor an element's default action took the key: the embedder may scroll.
    pub unhandled: bool,
    /// Text to put on the clipboard (copy, cut).
    pub copy: Option<String>,
    /// Markup to put on the clipboard with `copy` (`text/html`).
    pub copy_html: Option<String>,
}

/// UI Events `code` of a scan code (Windows set 1 with `0x100` for the extended keys).
pub fn code_for_scan(scan: u16) -> &'static str {
    const LETTERS: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
        "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
        "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    let letter = |c: u8| LETTERS[(c - b'A') as usize];
    match scan {
        0x01 => "Escape",
        0x02..=0x0A => [
            "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
            "Digit9",
        ][(scan - 0x02) as usize],
        0x0B => "Digit0",
        0x0C => "Minus",
        0x0D => "Equal",
        0x0E => "Backspace",
        0x0F => "Tab",
        0x10 => letter(b'Q'),
        0x11 => letter(b'W'),
        0x12 => letter(b'E'),
        0x13 => letter(b'R'),
        0x14 => letter(b'T'),
        0x15 => letter(b'Y'),
        0x16 => letter(b'U'),
        0x17 => letter(b'I'),
        0x18 => letter(b'O'),
        0x19 => letter(b'P'),
        0x1A => "BracketLeft",
        0x1B => "BracketRight",
        0x1C => "Enter",
        0x11C => "NumpadEnter",
        0x1D => "ControlLeft",
        0x11D => "ControlRight",
        0x1E => letter(b'A'),
        0x1F => letter(b'S'),
        0x20 => letter(b'D'),
        0x21 => letter(b'F'),
        0x22 => letter(b'G'),
        0x23 => letter(b'H'),
        0x24 => letter(b'J'),
        0x25 => letter(b'K'),
        0x26 => letter(b'L'),
        0x27 => "Semicolon",
        0x28 => "Quote",
        0x29 => "Backquote",
        0x2A => "ShiftLeft",
        0x2B => "Backslash",
        0x2C => letter(b'Z'),
        0x2D => letter(b'X'),
        0x2E => letter(b'C'),
        0x2F => letter(b'V'),
        0x30 => letter(b'B'),
        0x31 => letter(b'N'),
        0x32 => letter(b'M'),
        0x33 => "Comma",
        0x34 => "Period",
        0x35 => "Slash",
        0x135 => "NumpadDivide",
        0x36 => "ShiftRight",
        0x37 => "NumpadMultiply",
        0x38 => "AltLeft",
        0x138 => "AltRight",
        0x39 => "Space",
        0x3A => "CapsLock",
        0x3B..=0x44 => {
            ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10"][(scan - 0x3B) as usize]
        }
        0x45 => "Pause",
        0x145 => "NumLock",
        0x46 => "ScrollLock",
        0x47 => "Numpad7",
        0x147 => "Home",
        0x48 => "Numpad8",
        0x148 => "ArrowUp",
        0x49 => "Numpad9",
        0x149 => "PageUp",
        0x4A => "NumpadSubtract",
        0x4B => "Numpad4",
        0x14B => "ArrowLeft",
        0x4C => "Numpad5",
        0x4D => "Numpad6",
        0x14D => "ArrowRight",
        0x4E => "NumpadAdd",
        0x4F => "Numpad1",
        0x14F => "End",
        0x50 => "Numpad2",
        0x150 => "ArrowDown",
        0x51 => "Numpad3",
        0x151 => "PageDown",
        0x52 => "Numpad0",
        0x152 => "Insert",
        0x53 => "NumpadDecimal",
        0x153 => "Delete",
        0x56 => "IntlBackslash",
        0x57 => "F11",
        0x58 => "F12",
        0x15B => "MetaLeft",
        0x15C => "MetaRight",
        0x15D => "ContextMenu",
        0x137 => "PrintScreen",
        _ => "Unidentified",
    }
}

/// UI Events `key` of a virtual key that types no character (named key values).
pub fn named_key(vk: u16) -> Option<&'static str> {
    Some(match vk {
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0C => "Clear",
        0x0D => "Enter",
        0x10 | 0xA0 | 0xA1 => "Shift",
        0x11 | 0xA2 | 0xA3 => "Control",
        0x12 | 0xA4 | 0xA5 => "Alt",
        0x13 => "Pause",
        0x14 => "CapsLock",
        0x1B => "Escape",
        0x20 => " ",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "ArrowLeft",
        0x26 => "ArrowUp",
        0x27 => "ArrowRight",
        0x28 => "ArrowDown",
        0x2C => "PrintScreen",
        0x2D => "Insert",
        0x2E => "Delete",
        0x5B | 0x5C => "Meta",
        0x5D => "ContextMenu",
        0x70..=0x87 => {
            const F: [&str; 24] = [
                "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13",
                "F14", "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
            ];
            F[(vk - 0x70) as usize]
        }
        0x90 => "NumLock",
        0x91 => "ScrollLock",
        0xA6 => "BrowserBack",
        0xA7 => "BrowserForward",
        0xA8 => "BrowserRefresh",
        0xAC => "BrowserHome",
        0xAD => "AudioVolumeMute",
        0xAE => "AudioVolumeDown",
        0xAF => "AudioVolumeUp",
        0xB0 => "MediaTrackNext",
        0xB1 => "MediaTrackPrevious",
        0xB2 => "MediaStop",
        0xB3 => "MediaPlayPause",
        0xE5 => "Process",
        _ => return None,
    })
}

/// The `key` value: the named key, else the character typed, else "Unidentified".
pub fn key_value(input: &KeyInput) -> String {
    if let Some(k) = named_key(input.vk) {
        return k.to_owned();
    }
    match input.text {
        Some(c) => c.to_string(),
        None => "Unidentified".into(),
    }
}

/// `KeyboardEvent.location` from the code.
/// HTML §6.6.5 a focus navigation scope owner other than a document: a shadow host, or a slot in a
/// shadow tree.
fn focus_scope_owner(dom: &snowlark_dom::Dom, el: NodeId) -> bool {
    dom.shadow_root(el).is_some()
        || (dom.is_html_element_named(el, &local_name!("slot")) && dom.is_shadow_root(dom.root(el)))
}

fn location(code: &str) -> u32 {
    if code.ends_with("Left") && !code.starts_with("Arrow") && code != "BracketLeft" {
        1
    } else if code.ends_with("Right") && !code.starts_with("Arrow") && code != "BracketRight" {
        2
    } else if code.starts_with("Numpad") || code == "NumLock" {
        3
    } else {
        0
    }
}

/// What of `text` the user can insert into a number field holding `value` with the selection
/// `s..e` (UTF-16): only characters a number has (digits, `.`, `e`, signs), and each only where it
/// can still make a number: one decimal point, before any exponent; one exponent, after any decimal
/// point; signs at the start or right after the exponent, at most two, never before another sign,
/// and no leading `+` before an exponent; no digit before a leading sign or between the exponent
/// and its sign.
fn filter_number_text(value: &str, s: u32, e: u32, text: &str) -> String {
    let is_e = |c: char| matches!(c, 'e' | 'E');
    let is_sign = |c: char| matches!(c, '+' | '-');
    let mut left = value[..byte_at(value, s)].to_owned();
    let right = &value[byte_at(value, e)..];
    let sign_not_after_e = |t: &str| {
        t.char_indices()
            .any(|(i, c)| is_sign(c) && !t[..i].ends_with(is_e))
    };
    let mut out = String::new();
    for c in text
        .chars()
        .filter(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+'))
    {
        let reject = if c == '.' {
            left.contains('.')
                || right.contains('.')
                || left.contains(is_e)
                || sign_not_after_e(right)
        } else if is_e(c) {
            left.contains(is_e) || right.contains(is_e) || right.contains('.')
        } else if is_sign(c) {
            let both = format!("{left}{right}");
            both.chars().filter(|&c| is_sign(c)).count() >= 2
                || (both.contains(is_e) && !(left.is_empty() || left.ends_with(is_e)))
                || right.starts_with(is_sign)
                || (c == '+' && both.contains(is_e) && left.is_empty())
        } else {
            (left.is_empty() && right.starts_with(is_sign))
                || (left.ends_with(is_e) && right.starts_with(is_sign))
        };
        if !reject {
            left.push(c);
            out.push(c);
        }
    }
    out
}

/// UTF-16 offset → byte offset in `s` (clamped to the end).
fn byte_at(s: &str, utf16: u32) -> usize {
    let mut n = 0u32;
    for (i, c) in s.char_indices() {
        if n >= utf16 {
            return i;
        }
        n += c.len_utf16() as u32;
    }
    s.len()
}

fn utf16_len(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

/// The UTF-16 offset of the character boundary before / after `at` (a grapheme approximated by a
/// scalar value).
fn prev_boundary(s: &str, at: u32) -> u32 {
    let b = byte_at(s, at);
    s[..b]
        .chars()
        .next_back()
        .map_or(0, |c| at - c.len_utf16() as u32)
}

fn next_boundary(s: &str, at: u32) -> u32 {
    let b = byte_at(s, at);
    s[b..]
        .chars()
        .next()
        .map_or(at, |c| at + c.len_utf16() as u32)
}

/// The start of the word before `at` / the end of the word after it (Ctrl+arrows, Ctrl+Backspace).
fn word_before(s: &str, at: u32) -> u32 {
    crate::caret::word_move(s, at, false)
}

fn word_after(s: &str, at: u32) -> u32 {
    crate::caret::word_move(s, at, true)
}

/// The start / end of the line `at` is on (textarea Home/End).
fn line_start(s: &str, at: u32) -> u32 {
    let b = byte_at(s, at);
    utf16_len(&s[..s[..b].rfind('\n').map_or(0, |i| i + 1)])
}

fn line_end(s: &str, at: u32) -> u32 {
    let b = byte_at(s, at);
    utf16_len(&s[..s[b..].find('\n').map_or(s.len(), |i| b + i)])
}

/// The same column on the previous / next line (textarea Up/Down).
fn vertical(s: &str, at: u32, down: bool) -> u32 {
    let start = line_start(s, at);
    let col = at - start;
    if down {
        let end = line_end(s, at);
        if end >= utf16_len(s) {
            return utf16_len(s);
        }
        let next = end + 1;
        (next + col).min(line_end(s, next))
    } else {
        if start == 0 {
            return 0;
        }
        let prev = line_start(s, start - 1);
        (prev + col).min(start - 1)
    }
}

impl Engine {
    /// The document the focus is in: the top-level one, or the frame whose container is its
    /// parent's focused element, recursively.
    pub(crate) fn focus_document(&self) -> Option<NodeId> {
        let mut doc = self.rendered_top().or_else(|| self.document())?;
        for _ in 0..32 {
            let Some(f) = self.focused_element(doc) else {
                break;
            };
            match self
                .rendered_frames_of(doc)
                .into_iter()
                .find(|(c, _)| *c == f)
            {
                Some((_, d)) => doc = d,
                None => break,
            }
        }
        Some(doc)
    }

    /// The element key events go to: the focused element of the focused document, else its body.
    fn key_target(&self) -> Option<NodeId> {
        let doc = self.focus_document()?;
        self.focused_element(doc)
            .filter(|&f| self.dom().is_alive(f))
            .or_else(|| self.dom().body(doc))
            .or_else(|| self.dom().document_element(doc))
    }

    /// Fires a KeyboardEvent; returns whether it was not canceled.
    fn fire_key(
        &self,
        cx: &mut Cx,
        type_: &str,
        target: NodeId,
        input: &KeyInput,
        key: &str,
        char_code: u32,
    ) -> bool {
        let code = code_for_scan(input.scan);
        let doc = self.dom().node_document(target);
        // The legacy `keyCode`: the character code for keypress, else the virtual key code (which
        // is the upper-case ASCII of letters and digits).
        let key_code = if type_ == "keypress" {
            char_code
        } else {
            input.vk as u32
        };
        let u = UiFields {
            view: self.window_of_document(doc),
            ctrl: input.mods.ctrl,
            shift: input.mods.shift,
            alt: input.mods.alt,
            meta: input.mods.meta,
            key: key.to_owned(),
            code: code.to_owned(),
            location: location(code),
            repeat: input.repeat,
            is_composing: self.composition.borrow().is_some(),
            char_code: if type_ == "keypress" { char_code } else { 0 },
            key_code,
            which: key_code,
            ..Default::default()
        };
        let e = self.new_event(
            InterfaceId::KeyboardEvent,
            type_,
            true,
            true,
            true,
            EventExt::Ui(Box::new(u)),
        );
        self.ev_mut(e, |ev| ev.is_trusted = true);
        self.dispatch(cx, e, node_handle(target), false)
    }

    /// An InputEvent (`beforeinput` cancelable, `input` not) at a text control.
    fn fire_input(
        &self,
        cx: &mut Cx,
        type_: &str,
        target: NodeId,
        input_type: &str,
        data: Option<&str>,
    ) -> bool {
        let u = UiFields {
            view: self.window_of_document(self.dom().node_document(target)),
            data: data.map(str::to_owned),
            input_type: input_type.to_owned(),
            ..Default::default()
        };
        let e = self.new_event(
            InterfaceId::InputEvent,
            type_,
            true,
            type_ == "beforeinput",
            true,
            EventExt::Ui(Box::new(u)),
        );
        self.ev_mut(e, |ev| ev.is_trusted = true);
        self.dispatch(cx, e, node_handle(target), false)
    }

    /// Whether `el` is a text control the user can edit (not readonly or disabled).
    fn editable(&self, el: NodeId) -> bool {
        if !self.is_text_control(el) {
            return false;
        }
        let dom = self.dom();
        dom.get_attribute(el, "readonly").is_none() && !crate::engine::html::is_disabled(&dom, el)
    }

    /// Whether `maxlength` applies to `el` (HTML §4.10.5.3: the input types with the attribute).
    fn max_length_applies(&self, el: NodeId) -> bool {
        self.is_textarea(el)
            || matches!(
                self.input_type(el).as_deref(),
                Some("text" | "search" | "url" | "tel" | "email" | "password")
            )
    }

    fn is_textarea(&self, el: NodeId) -> bool {
        self.dom()
            .is_html_element_named(el, &local_name!("textarea"))
    }

    /// The selection (start, end) and the focus end of a text control.
    fn sel(&self, el: NodeId) -> (u32, u32, u32) {
        let (s, e, d) = self.ctl(el, |c| c.selection);
        (s, e, if d == "backward" { s } else { e })
    }

    /// Replaces the selection of a text control with `text` as the user does (HTML's user edits):
    /// `beforeinput`, `maxlength`, the dirty value, the caret after the insertion, `input`.
    fn edit(
        &self,
        cx: &mut Cx,
        el: NodeId,
        text: &str,
        input_type: &str,
        range: Option<(u32, u32)>,
    ) {
        if !self.editable(el) {
            return;
        }
        let data = (!text.is_empty()).then_some(text);
        if !self.fire_input(cx, "beforeinput", el, input_type, data) {
            return;
        }
        let value = self.editor_value(el);
        let (s, e) = range.unwrap_or_else(|| {
            let (s, e, _) = self.sel(el);
            (s, e)
        });
        let mut text = if self.is_textarea(el) {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.replace(['\r', '\n'], "")
        };
        if self.input_type(el).as_deref() == Some("number") {
            text = filter_number_text(&value, s, e, &text);
        }
        // `maxlength` limits what the user can type, in the controls it applies to (textarea; text,
        // search, url, tel, email and password inputs).
        let max = if self.max_length_applies(el) {
            self.dom()
                .get_attribute(el, "maxlength")
                .and_then(snowlark_bindings::rt::parse_non_negative_integer)
        } else {
            None
        };
        if let Some(max) = max {
            let room = (max as u32).saturating_sub(utf16_len(&value) - (e - s));
            if utf16_len(&text) > room {
                let cut = byte_at(&text, room);
                text.truncate(cut);
            }
        }
        let (bs, be) = (byte_at(&value, s), byte_at(&value, e));
        let new = format!("{}{}{}", &value[..bs], text, &value[be..]);
        if new == value {
            return;
        }
        let (s0, e0, _) = self.sel(el);
        self.set_edited_text(el, new.clone());
        self.html.borrow_mut().edited_since_focus.insert(el);
        let caret = s + utf16_len(&text);
        self.set_selection(el, caret, caret, None);
        self.control_edited(el);
        self.undo_push(crate::undo::Step {
            target: el,
            merge: crate::undo::merge_kind(input_type),
            content: crate::undo::Content::Value {
                before: value,
                after: new,
            },
            before: crate::undo::Sel::Control(s0, e0),
            after: crate::undo::Sel::Control(caret, caret),
        });
        self.fire_input(cx, "input", el, input_type, data);
    }

    /// Ctrl+X or Shift+Delete in a text control: `cut` at it, then, unless canceled, the selection
    /// (not a password's) to the clipboard and deleted (`deleteByCut`).
    fn cut_in_control(
        &self,
        cx: &mut Cx,
        el: NodeId,
        selected: &str,
        password: bool,
        out: &mut KeyOutcome,
    ) {
        let doc = self.dom().node_document(el);
        if let crate::clipboard_events::ClipboardOutcome::Canceled =
            self.clipboard_copy_event(cx, doc, true, out)
        {
            return;
        }
        if selected.is_empty() || password {
            return;
        }
        out.copy = Some(selected.to_owned());
        self.edit(cx, el, "", "deleteByCut", None);
    }

    /// Ctrl+Z (`redo` false), Ctrl+Y or Ctrl+Shift+Z in a text control: `beforeinput`
    /// (`historyUndo`/`historyRedo`, cancelable), the control's newest edit undone or redone,
    /// `input`.
    fn control_history(&self, cx: &mut Cx, el: NodeId, redo: bool) {
        if !self.can_undo(el, redo) {
            return;
        }
        let input_type = if redo { "historyRedo" } else { "historyUndo" };
        if !self.fire_input(cx, "beforeinput", el, input_type, None) {
            return;
        }
        if self.undo_apply(cx, el, redo) {
            self.fire_input(cx, "input", el, input_type, None);
        }
    }

    /// A text control's value or selection changed through the user: its rendering follows.
    pub(crate) fn control_edited(&self, el: NodeId) {
        self.controls_dirty.borrow_mut().insert(el);
        self.render.borrow_mut().dirty = true;
    }

    /// Moves the caret of a text control (extending the selection with Shift).
    fn move_caret(&self, el: NodeId, to: u32, extend: bool) {
        let (s, e, focus) = self.sel(el);
        if extend {
            let anchor = if focus == e { s } else { e };
            let (a, b) = if to < anchor {
                (to, anchor)
            } else {
                (anchor, to)
            };
            let dir = if to < anchor { "backward" } else { "forward" };
            self.set_selection(el, a, b, Some(dir));
        } else {
            self.set_selection(el, to, to, None);
        }
        self.control_edited(el);
    }

    /// The editing keys of a focused text control. Returns whether the key was one.
    fn edit_key(&self, cx: &mut Cx, el: NodeId, input: &KeyInput, out: &mut KeyOutcome) -> bool {
        let value = self.editor_value(el);
        let (s, e, focus) = self.sel(el);
        let len = utf16_len(&value);
        let ctrl = input.mods.ctrl;
        let shift = input.mods.shift;
        let textarea = self.is_textarea(el);
        let password = self.input_type(el).as_deref() == Some("password");
        let selected = || value[byte_at(&value, s)..byte_at(&value, e)].to_owned();
        match input.vk {
            0x08 => {
                // Backspace: the selection, else the character (word with Ctrl) before the caret.
                let range = if s != e {
                    (s, e)
                } else if ctrl {
                    (word_before(&value, s), s)
                } else {
                    (prev_boundary(&value, s), s)
                };
                if range.0 != range.1 {
                    self.edit(cx, el, "", "deleteContentBackward", Some(range));
                }
            }
            0x2E if shift && s != e => self.cut_in_control(cx, el, &selected(), password, out),
            0x2E => {
                let range = if s != e {
                    (s, e)
                } else if ctrl {
                    (s, word_after(&value, s))
                } else {
                    (s, next_boundary(&value, s))
                };
                if range.0 != range.1 {
                    self.edit(cx, el, "", "deleteContentForward", Some(range));
                }
            }
            0x25 => {
                let to = if s != e && !shift {
                    s
                } else if ctrl {
                    word_before(&value, focus)
                } else {
                    prev_boundary(&value, focus)
                };
                self.move_caret(el, to, shift);
            }
            0x27 => {
                let to = if s != e && !shift {
                    e
                } else if ctrl {
                    word_after(&value, focus)
                } else {
                    next_boundary(&value, focus)
                };
                self.move_caret(el, to, shift);
            }
            0x26 if textarea => self.move_caret(el, vertical(&value, focus, false), shift),
            0x28 if textarea => self.move_caret(el, vertical(&value, focus, true), shift),
            0x24 => {
                let to = if textarea && !ctrl {
                    line_start(&value, focus)
                } else {
                    0
                };
                self.move_caret(el, to, shift);
            }
            0x23 => {
                let to = if textarea && !ctrl {
                    line_end(&value, focus)
                } else {
                    len
                };
                self.move_caret(el, to, shift);
            }
            k if ctrl && k == b'A' as u16 => {
                self.set_selection(el, 0, len, None);
                self.control_edited(el);
            }
            k if ctrl && (k == b'C' as u16 || k == 0x2D) => {
                let doc = self.dom().node_document(el);
                let go = matches!(
                    self.clipboard_copy_event(cx, doc, false, out),
                    crate::clipboard_events::ClipboardOutcome::Default
                );
                if go && s != e && !password {
                    out.copy = Some(selected());
                }
            }
            k if ctrl && k == b'X' as u16 => {
                self.cut_in_control(cx, el, &selected(), password, out);
            }
            k if (ctrl && k == b'V' as u16) || (shift && k == 0x2D) => {
                if let Some(text) = input.paste.clone() {
                    if self.paste_event_for_control(cx, el, &text) {
                        self.edit(cx, el, &text, "insertFromPaste", None);
                    }
                }
            }
            0x0D if textarea => self.edit(cx, el, "\n", "insertLineBreak", None),
            k if ctrl && k == b'Z' as u16 => self.control_history(cx, el, shift),
            k if ctrl && k == b'Y' as u16 => self.control_history(cx, el, true),
            _ => return false,
        }
        true
    }

    /// HTML §4.10.21.2 implicit submission: Enter in a field submits its form through its default
    /// button, or directly when the form has no submit button and at most one field that blocks
    /// implicit submission.
    fn implicit_submission(&self, cx: &mut Cx, field: NodeId) {
        let Some(form) = self.form_owner_of(field) else {
            return;
        };
        let (default_button, blocking) = {
            let dom = self.dom();
            let mut default_button = None;
            let mut blocking = 0;
            for n in dom.descendants(dom.root(form)) {
                if !dom.is_element(n) || self.form_owner_of_dom(&dom, n) != Some(form) {
                    continue;
                }
                if default_button.is_none() && crate::engine::forms::is_submit_button(&dom, n) {
                    default_button = Some(n);
                }
                if dom.is_html_element_named(n, &local_name!("input")) {
                    let t = dom
                        .get_attribute(n, "type")
                        .unwrap_or("text")
                        .to_ascii_lowercase();
                    if matches!(
                        t.as_str(),
                        "text"
                            | "search"
                            | "url"
                            | "tel"
                            | "email"
                            | "password"
                            | "date"
                            | "month"
                            | "week"
                            | "time"
                            | "datetime-local"
                            | "number"
                    ) {
                        blocking += 1;
                    }
                }
            }
            (default_button, blocking)
        };
        match default_button {
            Some(b) => {
                if !crate::engine::html::is_disabled(&self.dom(), b) {
                    let _ = HTMLElementImpl::click(self, cx, node_handle(b));
                }
            }
            None if blocking <= 1 => self.submit_form(cx, form, None, true),
            None => {}
        }
    }

    /// HTML §6.6.5 the members of a focus navigation scope owner's scope, in tree order: the
    /// elements whose associated focus navigation owner is `owner`. A document's scope holds its
    /// tree, a shadow host's its shadow tree, a slot's its assigned elements (or, with none, its
    /// fallback content) and what is under them; each owner in it is a member, and what it owns
    /// is not.
    fn focus_scope_members(&self, owner: NodeId) -> Vec<NodeId> {
        let starts: Vec<NodeId> = {
            let dom = self.dom();
            if dom.is_document(owner) {
                dom.children(owner).collect()
            } else if let Some(s) = dom.shadow_root(owner) {
                dom.children(s).collect()
            } else {
                drop(dom);
                let assigned = self.find_slottables(owner);
                if assigned.is_empty() {
                    self.dom().children(owner).collect()
                } else {
                    assigned
                }
            }
        };
        let dom = self.dom();
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = starts.into_iter().rev().collect();
        while let Some(n) = stack.pop() {
            if !dom.is_element(n) {
                continue;
            }
            out.push(n);
            if focus_scope_owner(&dom, n) {
                continue;
            }
            let len = stack.len();
            stack.extend(dom.children(n));
            stack[len..].reverse();
        }
        out
    }

    /// HTML §6.6.5 the "flattened tabindex-ordered focus navigation scope" of `owner`, appended to
    /// `out`: its scope's sequentially focusable elements and owners, positive `tabindex` values
    /// first in order, then 0 or none, each in tree order, with every owner followed by (or, when it
    /// is not focusable itself, replaced by) its own flattened scope. An owner with a negative
    /// `tabindex` leaves its scope out. `start`, the navigation's starting point, is placed where it
    /// would be with `tabindex` 0 when it is not in the order itself (a `tabindex` of -1), so that
    /// navigation moves on from where it is.
    fn flattened_focus_scope(
        &self,
        owner: NodeId,
        start: Option<NodeId>,
        out: &mut Vec<(i64, NodeId)>,
    ) {
        let mut items: Vec<(i64, NodeId, bool, bool)> = Vec::new();
        for m in self.focus_scope_members(owner) {
            let (ti, is_owner) = {
                let dom = self.dom();
                (
                    dom.get_attribute(m, "tabindex")
                        .and_then(snowlark_bindings::rt::parse_integer),
                    focus_scope_owner(&dom, m),
                )
            };
            let ti = ti.unwrap_or(0);
            if Some(m) == start && ti < 0 {
                items.push((0, m, true, false));
                continue;
            }
            if ti < 0 {
                continue;
            }
            let focusable =
                (self.is_focusable(m) && !self.view_client_rects(m).is_empty()) || Some(m) == start;
            if focusable || is_owner {
                items.push((ti, m, focusable, is_owner));
            }
        }
        // Positive tabindex first by value (stable: tree order among equals), then 0.
        items.sort_by_key(|&(ti, ..)| if ti > 0 { ti } else { i64::MAX });
        for (ti, m, focusable, is_owner) in items {
            if focusable {
                out.push((ti, m));
            }
            if is_owner {
                self.flattened_focus_scope(m, start, out);
            }
        }
    }

    /// HTML §6.6.5 sequential focus navigation: the next (or previous) element in the document's
    /// flattened tabindex-ordered focus navigation scope.
    fn sequential_focus(&self, cx: &mut Cx, backward: bool) {
        let Some(doc) = self.focus_document() else {
            return;
        };
        // (from the focused element, else the starting point while it is still in the document:
        // the focused element that was disabled or hidden, or what the user clicked)
        let current = self.focused_element(doc).or_else(|| {
            let start = self.doc_state(doc, |s| s.nav_start)?;
            let dom = self.dom();
            (dom.is_alive(start) && dom.is_connected(start) && dom.node_document(start) == doc)
                .then_some(start)
        });
        let mut order: Vec<(i64, NodeId)> = Vec::new();
        self.flattened_focus_scope(doc, current, &mut order);
        self.popover_focus_order(&mut order);
        let pos = current.and_then(|c| order.iter().position(|&(_, n)| n == c));
        if order.is_empty() || (pos.is_some() && order.len() == 1) {
            return;
        }
        let next = match (pos, backward) {
            (None, false) => 0,
            (None, true) => order.len() - 1,
            (Some(p), false) => (p + 1) % order.len(),
            (Some(p), true) => (p + order.len() - 1) % order.len(),
        };
        self.focus_from_keyboard.set(true);
        let target = order[next].1;
        self.focus_element(cx, target);
        if self.is_text_control(target) {
            let len = utf16_len(&self.editor_value(target));
            self.set_selection(target, 0, len, None);
            self.control_edited(target);
        }
        self.view_scroll_into_view(target, "nearest", "nearest");
    }

    /// `keydown` at the focused element, then the default action of the key.
    pub fn key_down(&self, cx: &mut Cx, input: &KeyInput) -> KeyOutcome {
        let mut out = KeyOutcome::default();
        // HTML §6.11.5: device input is suppressed during a drag; Escape cancels it.
        if self.dragging() {
            if key_value(input) == "Escape" {
                self.drag_canceled(cx, input.mods);
            }
            return out;
        }
        let Some(target) = self.key_target() else {
            out.unhandled = true;
            return out;
        };
        let key = key_value(input);
        // (Selectors 4 §9.4 `:focus-visible`: keyboard input since a pointer focused something makes
        // the next script focus show a ring again; shortcuts with Ctrl, Alt or Meta do not count)
        if !input.mods.ctrl && !input.mods.alt && !input.mods.meta {
            self.focus_from_keyboard.set(true);
            self.show_focus_ring_for_keyboard(target);
        }
        // HTML §6.4.3: a keydown (other than Escape) is an activation-triggering input event.
        if key != "Escape" {
            self.note_user_activation(target);
        }
        let go = self.fire_key(cx, "keydown", target, input, &key, 0);
        self.pointer.borrow_mut().suppress_char = !go;
        // A key the IME processes (VK_PROCESSKEY) or one pressed while composing does nothing more.
        if input.vk == 0xE5 || self.composition.borrow().is_some() {
            self.microtask_checkpoint(cx);
            return out;
        }
        if go {
            self.key_default(cx, target, input, &mut out);
        }
        self.microtask_checkpoint(cx);
        out
    }

    fn key_default(&self, cx: &mut Cx, target: NodeId, input: &KeyInput, out: &mut KeyOutcome) {
        let mods = input.mods;
        // Esc: a close request (HTML §6.10 close requests): the window's close watchers.
        if input.vk == 0x1B && !mods.ctrl && !mods.alt && !mods.shift {
            let doc = self.dom().node_document(target);
            if self.process_close_watchers(cx, doc) {
                return;
            }
        }
        // Tab: sequential focus navigation.
        if input.vk == 0x09 && !mods.ctrl && !mods.alt {
            self.sequential_focus(cx, mods.shift);
            return;
        }
        // An editing host: its editing keys.
        if let Some(host) = self.active_editing_host() {
            if self.editing_key(cx, host, input, out) {
                return;
            }
        } else if !self.is_text_control(target) {
            let doc = self.dom().node_document(target);
            if self.document_selection_key(cx, doc, input, out) {
                return;
            }
        }
        if self.editable(target) {
            if self.edit_key(cx, target, input, out) {
                return;
            }
            if input.vk == 0x0D {
                self.implicit_submission(cx, target);
                return;
            }
            // Other keys in a text field don't scroll the page (arrows, space type or move).
            if matches!(input.vk, 0x20 | 0x21 | 0x22 | 0x26 | 0x28) {
                return;
            }
        }
        let (is_link, is_button, is_select) = {
            let dom = self.dom();
            (
                (dom.is_html_element_named(target, &local_name!("a"))
                    || dom.is_html_element_named(target, &local_name!("area")))
                    && dom.get_attribute(target, "href").is_some(),
                dom.is_html_element_named(target, &local_name!("button")),
                dom.is_html_element_named(target, &local_name!("select")),
            )
        };
        // Enter activates links and buttons.
        if input.vk == 0x0D && (is_link || is_button) {
            let _ = HTMLElementImpl::click(self, cx, node_handle(target));
            return;
        }
        // Space on buttons, checkboxes and radios activates them (on key up in browsers; here on
        // the key going down).
        let toggles = matches!(
            self.input_type(target).as_deref(),
            Some("checkbox" | "radio" | "submit" | "reset" | "button" | "image")
        ) && self
            .dom()
            .is_html_element_named(target, &local_name!("input"));
        if input.vk == 0x20 && (is_button || toggles) {
            let _ = HTMLElementImpl::click(self, cx, node_handle(target));
            return;
        }
        // Alt+Up/Down, F4 and Space show a drop-down select's list.
        let opens = (mods.alt && matches!(input.vk, 0x26 | 0x28))
            || (!mods.alt && !mods.ctrl && matches!(input.vk, 0x73 | 0x20));
        if is_select && opens && crate::popup::is_drop_down(&self.dom(), target) {
            self.open_select_popup(target);
            return;
        }
        // Arrow keys change a drop-down select's option.
        if is_select && matches!(input.vk, 0x26 | 0x28 | 0x24 | 0x23) {
            let options = self.options_of(target);
            let current = self.select_selected_index(target);
            let enabled: Vec<usize> = {
                let dom = self.dom();
                options
                    .iter()
                    .enumerate()
                    .filter(|(_, &o)| !crate::engine::html::is_disabled(&dom, o))
                    .map(|(i, _)| i)
                    .collect()
            };
            let next = match input.vk {
                0x28 => enabled.iter().copied().find(|&i| i as i32 > current),
                0x26 => enabled
                    .iter()
                    .rev()
                    .copied()
                    .find(|&i| (i as i32) < current),
                0x24 => enabled.first().copied(),
                _ => enabled.last().copied(),
            };
            if let Some(n) = next {
                if n as i32 != current {
                    self.select_set_selected_index(cx, target, n as i32);
                    self.control_edited(target);
                    self.fire_event(cx, node_handle(target), "input", true, false);
                    self.set_user_validity(target, true);
                    self.fire_event(cx, node_handle(target), "change", true, false);
                }
            }
            return;
        }
        out.unhandled = true;
    }

    /// A character typed: `keypress`, then its insertion into a focused text control.
    pub fn key_char(&self, cx: &mut Cx, c: char, input: &KeyInput) -> KeyOutcome {
        let out = KeyOutcome::default();
        if std::mem::take(&mut self.pointer.borrow_mut().suppress_char) || self.dragging() {
            return out;
        }
        if c.is_control() && c != '\r' {
            return out;
        }
        let Some(target) = self.key_target() else {
            return out;
        };
        let text = c.to_string();
        let key = if c == '\r' {
            "Enter".to_owned()
        } else {
            text.clone()
        };
        let go = self.fire_key(cx, "keypress", target, input, &key, c as u32);
        if go && c != '\r' && self.editable(target) {
            self.edit(cx, target, &text, "insertText", None);
        } else if go && c != '\r' && !input.mods.ctrl {
            if let Some(host) = self.active_editing_host() {
                self.editing_char(cx, host, c);
            }
        }
        self.microtask_checkpoint(cx);
        out
    }

    /// `keyup` at the focused element.
    pub fn key_up(&self, cx: &mut Cx, input: &KeyInput) {
        if self.dragging() {
            return;
        }
        if let Some(target) = self.key_target() {
            let key = key_value(input);
            self.fire_key(cx, "keyup", target, input, &key, 0);
            self.microtask_checkpoint(cx);
        }
    }
}

impl Page {
    pub fn key_down(&mut self, input: &KeyInput) -> KeyOutcome {
        self.with(|e, cx| {
            e.dispatch_held_move(cx);
            e.key_down(cx, input)
        })
    }

    pub fn key_char(&mut self, c: char, input: &KeyInput) -> KeyOutcome {
        self.with(|e, cx| {
            e.dispatch_held_move(cx);
            e.key_char(cx, c, input)
        })
    }

    pub fn key_up(&mut self, input: &KeyInput) {
        self.with(|e, cx| {
            e.dispatch_held_move(cx);
            e.key_up(cx, input)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_codes() {
        assert_eq!(code_for_scan(0x1E), "KeyA");
        assert_eq!(code_for_scan(0x14B), "ArrowLeft");
        assert_eq!(code_for_scan(0x4B), "Numpad4");
        assert_eq!(code_for_scan(0x11D), "ControlRight");
        assert_eq!(location("ControlRight"), 2);
        assert_eq!(location("ArrowLeft"), 0);
        assert_eq!(location("Numpad4"), 3);
        let k = |vk, text| KeyInput {
            vk,
            scan: 0,
            text,
            mods: Modifiers::default(),
            repeat: false,
            paste: None,
            paste_html: None,
        };
        assert_eq!(key_value(&k(0x41, Some('a'))), "a");
        assert_eq!(key_value(&k(0x0D, None)), "Enter");
        assert_eq!(key_value(&k(0x20, Some(' '))), " ");
    }

    #[test]
    fn caret_movements() {
        let s = "ab cd\nefg";
        assert_eq!(word_before(s, 5), 3);
        // (over the space after the word, as on Windows)
        assert_eq!(word_after(s, 0), 3);
        assert_eq!(line_start(s, 8), 6);
        assert_eq!(line_end(s, 1), 5);
        assert_eq!(vertical(s, 7, false), 1);
        assert_eq!(vertical(s, 1, true), 7);
        assert_eq!(prev_boundary("a😀", 3), 1);
        assert_eq!(next_boundary("a😀", 1), 3);
        assert_eq!(byte_at("a😀b", 3), 5);
    }
}
