// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A `<canvas>` 2D context's bitmap on Linux: the operations' data types (`snowlark-graphics`) and no
//! GPU bitmap yet, so `snowlark-render` draws its canvases on the CPU (`crate::canvas_thread`).

pub use snowlark_graphics::canvas::*;
