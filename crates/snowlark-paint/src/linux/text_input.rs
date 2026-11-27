// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Input methods on Wayland: the text-input protocol (`zwp_text_input_v3`, text-input-unstable-v3).
//! The desktop's input method (IBus, Fcitx, the compositor's own) composes text for the surface
//! with the keyboard focus and sends it as a preedit string and a commit string; the window turns
//! them into the [`ImeEvent`]s a Windows input method's messages become. The text input is enabled
//! while a window of the loop has the keyboard focus, as the Windows window keeps its input context
//! (the page decides what a composition does: only a focused text control or editing host takes
//! it).
//!
//! The protocol's events come in batches ended by `done`; a batch is applied in the order the
//! protocol gives (the deletion of surrounding text, then the commit string, then the new preedit
//! string), and the preedit only when it changed. The window sends no surrounding text, so
//! deletions of it are not asked for and are ignored.

use sctk::reexports::client::globals::GlobalList;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Dispatch, QueueHandle};
use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    self, ContentHint, ContentPurpose, ZwpTextInputV3,
};
use smithay_client_toolkit as sctk;

use super::{Event, ImeEvent, State};

/// A preedit string and its cursor (byte offsets into it; `None` hides the cursor).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Preedit {
    pub text: String,
    pub cursor: Option<(usize, usize)>,
}

/// What a batch of the protocol's events asks for, until its `done`.
#[derive(Debug, Default)]
pub(super) struct Batch {
    pub preedit: Preedit,
    pub commit: Option<String>,
}

/// What the window keeps of the input method's composition.
#[derive(Debug, Default)]
pub(super) struct Composing {
    /// The preedit string shown now.
    pub preedit: Preedit,
    /// A composition is open (its [`ImeEvent::Start`] was sent).
    pub open: bool,
}

/// The [`ImeEvent`]s a batch makes, in order; `state` becomes what the batch leaves.
pub(super) fn apply(state: &mut Composing, batch: Batch) -> Vec<ImeEvent> {
    let mut out = Vec::new();
    if let Some(text) = batch.commit.filter(|t| !t.is_empty()) {
        if !state.open {
            out.push(ImeEvent::Start);
        }
        // (the commit replaces the preedit; the page ends the composition with it)
        out.push(ImeEvent::Commit(text));
        state.open = false;
        state.preedit = Preedit::default();
    }
    if batch.preedit != state.preedit {
        if batch.preedit.text.is_empty() {
            if state.open {
                out.push(ImeEvent::End);
                state.open = false;
            }
        } else {
            if !state.open {
                out.push(ImeEvent::Start);
                state.open = true;
            }
            let at = batch
                .preedit
                .cursor
                .map_or(batch.preedit.text.len(), |(begin, _)| begin);
            out.push(ImeEvent::Update {
                text: batch.preedit.text.clone(),
                cursor: utf16_len(&batch.preedit.text, at),
            });
        }
        state.preedit = batch.preedit;
    }
    out
}

/// The UTF-16 length of `text`'s first `byte` bytes (clamped to a character boundary at or
/// before it).
fn utf16_len(text: &str, byte: usize) -> u32 {
    let mut end = byte.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].encode_utf16().count() as u32
}

/// The loop's text input: the desktop's manager, the seat's text input and the surface it is on.
#[derive(Default)]
pub(super) struct TextInput {
    manager: Option<ZwpTextInputManagerV3>,
    input: Option<ZwpTextInputV3>,
    surface: Option<WlSurface>,
    batch: Batch,
    composing: Composing,
}

impl TextInput {
    /// Binds the desktop's text-input manager, if it has one.
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<State>) -> TextInput {
        TextInput {
            manager: globals.bind(qh, 1..=1, ()).ok(),
            ..TextInput::default()
        }
    }

    /// The keyboard's seat: its text input.
    pub fn seat(&mut self, qh: &QueueHandle<State>, seat: &WlSeat) {
        if let (Some(manager), None) = (&self.manager, &self.input) {
            self.input = Some(manager.get_text_input(seat, qh, ()));
        }
    }

    /// The keyboard's seat lost its keyboard.
    pub fn release(&mut self) {
        if let Some(input) = self.input.take() {
            input.destroy();
        }
        self.surface = None;
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for State {
    fn event(
        _: &mut State,
        _: &ZwpTextInputManagerV3,
        _: <ZwpTextInputManagerV3 as sctk::reexports::client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for State {
    fn event(
        state: &mut State,
        input: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use zwp_text_input_v3::Event as E;
        let ti = &mut state.text_input;
        match event {
            E::Enter { surface } => {
                ti.surface = Some(surface);
                ti.batch = Batch::default();
                ti.composing = Composing::default();
                input.enable();
                input.set_content_type(ContentHint::None, ContentPurpose::Normal);
                input.commit();
            }
            E::Leave { surface } => {
                input.disable();
                input.commit();
                let open = std::mem::take(&mut ti.composing).open;
                ti.surface = None;
                if open {
                    if let Some(id) = state.win_of(&surface) {
                        state.send(id, Event::Ime(ImeEvent::End));
                    }
                }
            }
            E::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                let cursor = (cursor_begin >= 0 && cursor_end >= 0)
                    .then_some((cursor_begin as usize, cursor_end as usize));
                ti.batch.preedit = Preedit {
                    text: text.unwrap_or_default(),
                    cursor,
                };
            }
            E::CommitString { text } => ti.batch.commit = text,
            // (no surrounding text is sent, so none is deleted)
            E::DeleteSurroundingText { .. } => {}
            E::Done { .. } => {
                let batch = std::mem::take(&mut ti.batch);
                let events = apply(&mut ti.composing, batch);
                let Some(id) = ti.surface.clone().and_then(|s| state.win_of(&s)) else {
                    return;
                };
                for e in events {
                    state.send(id, Event::Ime(e));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preedit(text: &str, cursor: Option<(usize, usize)>) -> Batch {
        Batch {
            preedit: Preedit {
                text: text.into(),
                cursor,
            },
            commit: None,
        }
    }

    #[test]
    fn a_composition_opens_updates_and_commits() {
        let mut s = Composing::default();
        assert_eq!(
            apply(&mut s, preedit("に", Some((3, 3)))),
            vec![
                ImeEvent::Start,
                ImeEvent::Update {
                    text: "に".into(),
                    cursor: 1
                }
            ]
        );
        // (the cursor is a byte offset in the protocol, UTF-16 in the event)
        assert_eq!(
            apply(&mut s, preedit("にほ", Some((3, 3)))),
            vec![ImeEvent::Update {
                text: "にほ".into(),
                cursor: 1
            }]
        );
        // (an unchanged preedit sends nothing)
        assert_eq!(apply(&mut s, preedit("にほ", Some((3, 3)))), vec![]);
        let commit = Batch {
            preedit: Preedit::default(),
            commit: Some("日本".into()),
        };
        assert_eq!(apply(&mut s, commit), vec![ImeEvent::Commit("日本".into())]);
        assert!(!s.open);
    }

    #[test]
    fn a_commit_without_a_preedit_is_a_composition_of_its_own() {
        let mut s = Composing::default();
        let commit = Batch {
            preedit: Preedit::default(),
            commit: Some("ü".into()),
        };
        assert_eq!(
            apply(&mut s, commit),
            vec![ImeEvent::Start, ImeEvent::Commit("ü".into())]
        );
    }

    #[test]
    fn a_commit_and_a_new_preedit_in_one_batch_end_one_and_open_the_next() {
        let mut s = Composing::default();
        apply(&mut s, preedit("にほん", None));
        let batch = Batch {
            preedit: Preedit {
                text: "ご".into(),
                cursor: None,
            },
            commit: Some("日本".into()),
        };
        assert_eq!(
            apply(&mut s, batch),
            vec![
                ImeEvent::Commit("日本".into()),
                ImeEvent::Start,
                ImeEvent::Update {
                    text: "ご".into(),
                    cursor: 1
                }
            ]
        );
    }

    #[test]
    fn an_emptied_preedit_cancels_the_composition() {
        let mut s = Composing::default();
        apply(&mut s, preedit("a\u{1F600}", Some((5, 5))));
        assert_eq!(apply(&mut s, preedit("", None)), vec![ImeEvent::End]);
        assert!(!s.open);
        assert_eq!(utf16_len("a\u{1F600}", 5), 3);
        assert_eq!(utf16_len("a\u{1F600}", 3), 1);
    }
}
