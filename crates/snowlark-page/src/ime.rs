// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text composition through an input method editor (UI Events §5.6 composition events, Input Events
//! Level 2): the embedder reports a composition starting, its string changing (with the IME's cursor)
//! and being committed or ended; a focused text control shows the composition in place of its
//! selection, underlined, while `compositionstart`, `compositionupdate`, `compositionend`, `beforeinput`
//! and `input` (`insertCompositionText`, `isComposing`) fire. Keys the IME processes arrive as
//! `keydown` with the key "Process" and do nothing else.

use snowlark_bindings::rt::Cx;
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;

use crate::engine::events::{EventExt, UiFields};
use crate::engine::{node_handle, Engine};
use crate::Page;

/// The composition in progress: the control or editing host, and where its text is (UTF-16): in the
/// control's value, or in `text`, the text node of the host that holds it (none until the
/// composition has text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Composition {
    pub element: NodeId,
    pub start: u32,
    pub len: u32,
    pub host: bool,
    pub text: Option<NodeId>,
}

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

/// `value` with the UTF-16 range `start..start + len` replaced by `text`.
pub fn splice_utf16(value: &str, start: u32, len: u32, text: &str) -> String {
    let a = byte_at(value, start);
    let b = byte_at(value, start + len);
    format!("{}{}{}", &value[..a], text, &value[b..])
}

impl Engine {
    /// The composition in progress, if any.
    pub fn composition(&self) -> Option<Composition> {
        *self.composition.borrow()
    }

    fn composition_event(&self, cx: &mut Cx, target: NodeId, type_: &str, data: &str) {
        let u = UiFields {
            view: self.window_of_document(self.dom().node_document(target)),
            data: Some(data.to_owned()),
            ..Default::default()
        };
        let e = self.new_event(
            InterfaceId::CompositionEvent,
            type_,
            true,
            type_ == "compositionstart",
            true,
            EventExt::Ui(Box::new(u)),
        );
        self.ev_mut(e, |ev| ev.is_trusted = true);
        self.dispatch(cx, e, node_handle(target), false);
    }

    fn composition_input(&self, cx: &mut Cx, target: NodeId, type_: &str, data: &str) {
        let u = UiFields {
            view: self.window_of_document(self.dom().node_document(target)),
            data: Some(data.to_owned()),
            input_type: "insertCompositionText".into(),
            is_composing: true,
            ..Default::default()
        };
        let e = self.new_event(
            InterfaceId::InputEvent,
            type_,
            true,
            false,
            true,
            EventExt::Ui(Box::new(u)),
        );
        self.ev_mut(e, |ev| ev.is_trusted = true);
        self.dispatch(cx, e, node_handle(target), false);
    }

    /// A composition starts at the focused element: `compositionstart` (with the selected text),
    /// and, in an editable text control, the composition replaces the selection from now on.
    pub fn composition_start(&self, cx: &mut Cx) {
        if self.composition.borrow().is_some() {
            return;
        }
        let Some(target) = self.editing_target() else {
            if let Some(host) = self.active_editing_host() {
                // An editing host: the composition replaces the selection once it has text.
                let doc = self.dom().node_document(host);
                let selected = self.selection_text(doc).unwrap_or_default();
                *self.composition.borrow_mut() = Some(Composition {
                    element: host,
                    start: 0,
                    len: 0,
                    host: true,
                    text: None,
                });
                self.composition_event(cx, host, "compositionstart", &selected);
                self.microtask_checkpoint(cx);
            }
            return;
        };
        let (s, e, _) = self.ctl(target, |c| c.selection);
        let value = self.editor_value(target);
        let selected = value[byte_at(&value, s)..byte_at(&value, e)].to_owned();
        *self.composition.borrow_mut() = Some(Composition {
            element: target,
            start: s,
            len: e - s,
            host: false,
            text: None,
        });
        self.composition_event(cx, target, "compositionstart", &selected);
        self.microtask_checkpoint(cx);
    }

    /// The focused editable text control, where composition goes.
    fn editing_target(&self) -> Option<NodeId> {
        let doc = self.focus_document()?;
        let el = self.focused_element(doc)?;
        let ok = {
            let dom = self.dom();
            dom.is_alive(el)
                && dom.get_attribute(el, "readonly").is_none()
                && !crate::engine::html::is_disabled(&dom, el)
        };
        (ok && self.is_text_control(el)).then_some(el)
    }

    /// A composition in an editing host shows `text` in place of its previous text (the selection,
    /// deleted, the first time), the caret `cursor` code units into it.
    fn host_composition_update(&self, cx: &mut Cx, c: Composition, text: &str, cursor: u32) {
        let doc = self.dom().node_document(c.element);
        let len = text.encode_utf16().count() as u32;
        let live = c.text.filter(|&t| {
            let dom = self.dom();
            dom.is_alive(t) && dom.is_connected(t) && dom.length(t) as u32 >= c.start + c.len
        });
        let (node, start) = match live {
            Some(t) => {
                let _ = self
                    .dom_mut()
                    .replace_data(t, c.start as usize, c.len as usize, text);
                self.after_mutation(cx);
                (Some(t), c.start)
            }
            None => {
                // The selection goes; the text goes in where the caret is, as typing puts it.
                self.ec_insert_text(cx, doc, "");
                if len > 0 {
                    self.ec_insert_text(cx, doc, text);
                }
                self.after_mutation(cx);
                match self.doc_anchor_focus(doc) {
                    Some((_, (n, o))) if len > 0 && self.dom().is_text(n) && o >= len => {
                        (Some(n), o - len)
                    }
                    _ => (None, 0),
                }
            }
        };
        if let Some(t) = node {
            let caret = (t, start + cursor.min(len));
            self.set_doc_selection(doc, Some((caret, caret)));
        }
        *self.composition.borrow_mut() = Some(Composition {
            start,
            len,
            text: node,
            ..c
        });
    }

    /// The composition string changed to `text` with the IME's cursor at `cursor` (UTF-16 in it):
    /// `beforeinput`, the value with the text in place of the composition, `compositionupdate`,
    /// `input`.
    pub fn composition_update(&self, cx: &mut Cx, text: &str, cursor: u32) {
        if self.composition.borrow().is_none() {
            self.composition_start(cx);
        }
        let Some(c) = self.composition() else {
            return;
        };
        if !self.dom().is_alive(c.element) {
            *self.composition.borrow_mut() = None;
            return;
        }
        self.composition_input(cx, c.element, "beforeinput", text);
        if c.host {
            if !self.dom().is_alive(c.element) {
                *self.composition.borrow_mut() = None;
                return;
            }
            self.host_composition_update(cx, c, text, cursor);
            self.composition_event(cx, c.element, "compositionupdate", text);
            self.composition_input(cx, c.element, "input", text);
            self.microtask_checkpoint(cx);
            return;
        }
        let value = self.editor_value(c.element);
        let new = splice_utf16(&value, c.start, c.len, text);
        let len = text.encode_utf16().count() as u32;
        self.set_edited_text(c.element, new);
        self.html.borrow_mut().edited_since_focus.insert(c.element);
        let caret = c.start + cursor.min(len);
        self.set_selection(c.element, caret, caret, None);
        *self.composition.borrow_mut() = Some(Composition { len, ..c });
        self.control_edited(c.element);
        self.composition_event(cx, c.element, "compositionupdate", text);
        self.composition_input(cx, c.element, "input", text);
        self.microtask_checkpoint(cx);
    }

    /// The composition was committed with `text`: the value keeps it (the caret after it), then
    /// `compositionend`.
    pub fn composition_commit(&self, cx: &mut Cx, text: &str) {
        let len = text.encode_utf16().count() as u32;
        self.composition_update(cx, text, len);
        self.composition_finish(cx, text);
    }

    /// The composition ended without a commit (canceled): its text goes, then `compositionend`.
    pub fn composition_end(&self, cx: &mut Cx) {
        let Some(c) = self.composition() else {
            return;
        };
        if c.len > 0 {
            self.composition_update(cx, "", 0);
        }
        self.composition_finish(cx, "");
    }

    fn composition_finish(&self, cx: &mut Cx, data: &str) {
        let Some(c) = self.composition.borrow_mut().take() else {
            return;
        };
        if !c.host {
            self.control_edited(c.element);
        }
        if self.dom().is_alive(c.element) {
            self.composition_event(cx, c.element, "compositionend", data);
        }
        self.microtask_checkpoint(cx);
    }

    /// The focus moves while composing: the composition is kept as it is (committed).
    pub(crate) fn composition_interrupted(&self, cx: &mut Cx) {
        let Some(c) = self.composition() else {
            return;
        };
        let text = if c.host {
            let dom = self.dom();
            match c.text.filter(|&t| dom.is_alive(t)) {
                Some(t) => {
                    let s = dom.character_data(t).unwrap_or("");
                    let units: Vec<u16> = s.encode_utf16().collect();
                    let a = (c.start as usize).min(units.len());
                    let b = ((c.start + c.len) as usize).min(units.len());
                    String::from_utf16_lossy(&units[a..b])
                }
                None => String::new(),
            }
        } else if self.dom().is_alive(c.element) {
            let value = self.editor_value(c.element);
            value[byte_at(&value, c.start)..byte_at(&value, c.start + c.len)].to_owned()
        } else {
            String::new()
        };
        self.composition_finish(cx, &text);
    }

    /// The composition's range in a control (for its rendering).
    pub(crate) fn composition_range(&self, el: NodeId) -> Option<(u32, u32)> {
        self.composition()
            .filter(|c| c.element == el && !c.host && c.len > 0)
            .map(|c| (c.start, c.start + c.len))
    }
}

impl Page {
    pub fn composition_start(&mut self) {
        self.with(|e, cx| {
            e.dispatch_held_move(cx);
            e.composition_start(cx)
        })
    }
    pub fn composition_update(&mut self, text: &str, cursor: u32) {
        self.with(|e, cx| {
            e.dispatch_held_move(cx);
            e.composition_update(cx, text, cursor)
        })
    }
    pub fn composition_commit(&mut self, text: &str) {
        self.with(|e, cx| {
            e.dispatch_held_move(cx);
            e.composition_commit(cx, text)
        })
    }
    pub fn composition_end(&mut self) {
        self.with(|e, cx| {
            e.dispatch_held_move(cx);
            e.composition_end(cx)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositions_replace_their_range() {
        assert_eq!(splice_utf16("abcd", 1, 2, "XY"), "aXYd");
        assert_eq!(splice_utf16("ab", 2, 0, "\u{65e5}"), "ab\u{65e5}");
        assert_eq!(splice_utf16("a\u{1F600}b", 1, 2, ""), "ab");
    }
}
