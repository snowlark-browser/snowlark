// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drags from other programs on Linux: the types the window's drag events carry (the window's
//! `drag` module takes them from the desktop's data device).

pub use crate::drag_data::{
    shortcut_file_contents, shortcut_file_name, DragData, DragImage, DragLink, DragOut, DropEffect,
};
