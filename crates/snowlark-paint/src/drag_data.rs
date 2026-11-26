// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a drag from another program carries and how the window answers it, on every platform
//! (re-exported by each platform's `drop_target`).

use std::path::PathBuf;

/// A drag operation, as the drop target answers it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DropEffect {
    #[default]
    None,
    Copy,
    Move,
    Link,
}

/// What a drag from another program carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DragData {
    /// The files (and folders) dragged.
    pub files: Vec<PathBuf>,
    /// The text.
    pub text: Option<String>,
    /// The operations the source allows.
    pub copy: bool,
    pub moves: bool,
    pub link: bool,
}
