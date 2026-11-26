// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drags from other programs on Linux: the types the window's drag events carry. The window does
//! not take drops yet (`wl_data_device`'s drag and drop), so no drag event is sent.

pub use crate::drag_data::{DragData, DropEffect};
