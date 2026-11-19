// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Input types with no Win32 in them: the window produces them and the tab (a renderer, possibly in
//! another process) consumes them. [`crate::window`] re-exports them.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// The "back" side button (XBUTTON1).
    Back,
    /// The "forward" side button (XBUTTON2).
    Forward,
}

/// Modifier keys held during an event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Mouse cursors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Cursor {
    #[default]
    Arrow,
    Hand,
    IBeam,
    Wait,
    /// The arrow with a busy indicator.
    Progress,
    Crosshair,
    Move,
    NotAllowed,
    ResizeEW,
    ResizeNS,
    ResizeNWSE,
    ResizeNESW,
    Help,
    /// Middle-button autoscroll: the page scrolls toward `(dx, dy)` (each -1, 0 or 1); `(0, 0)` at
    /// the origin ([`crate::autoscroll_cursor`]).
    AutoScroll(i8, i8),
    /// No cursor at all (`cursor: none`).
    Hidden,
    /// The ones below have no system cursor: [`crate::cursors`] draws them.
    VerticalText,
    Alias,
    Copy,
    ContextMenu,
    ZoomIn,
    ZoomOut,
    Grab,
    Grabbing,
}

/// Virtual-key codes the shell refers to (Win32 `VK_*` values; letters and digits are their ASCII
/// upper-case codes).
pub mod keys {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const ENTER: u16 = 0x0D;
    pub const ESCAPE: u16 = 0x1B;
    pub const SPACE: u16 = 0x20;
    pub const PAGE_UP: u16 = 0x21;
    pub const PAGE_DOWN: u16 = 0x22;
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const DELETE: u16 = 0x2E;
    pub const NUMPAD0: u16 = 0x60;
    pub const ADD: u16 = 0x6B;
    pub const SUBTRACT: u16 = 0x6D;
    pub const F3: u16 = 0x72;
    pub const F4: u16 = 0x73;
    pub const F5: u16 = 0x74;
    pub const F6: u16 = 0x75;
    pub const F10: u16 = 0x79;
    pub const F11: u16 = 0x7A;
    pub const F12: u16 = 0x7B;
    /// The context menu key.
    pub const APPS: u16 = 0x5D;
    pub const BROWSER_BACK: u16 = 0xA6;
    pub const BROWSER_FORWARD: u16 = 0xA7;
    pub const BROWSER_REFRESH: u16 = 0xA8;
    pub const BROWSER_HOME: u16 = 0xAC;
    /// `=`/`+` on the main keyboard.
    pub const OEM_PLUS: u16 = 0xBB;
    pub const OEM_MINUS: u16 = 0xBD;
}

/// What an input method editor did (WM_IME_STARTCOMPOSITION, WM_IME_COMPOSITION,
/// WM_IME_ENDCOMPOSITION). The window shows the composition itself (inline, in the focused field), so
/// the IME's own composition window is hidden; committed text arrives here, not as characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImeEvent {
    Start,
    /// The composition string so far, with the IME's cursor in it (UTF-16 code units).
    Update {
        text: String,
        cursor: u32,
    },
    /// The composition was committed with this text.
    Commit(String),
    End,
}
