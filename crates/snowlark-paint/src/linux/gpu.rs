// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The GPU on Linux, before there is a GPU backend: only the choice of device the canvas takes
//! ([`Driver`]). Tiles and the compositor's device come with stage 5 (docs/linux.md).

/// Which kind of device to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    /// The graphics card.
    Hardware,
    /// A software rasterizer (no GPU needed).
    Warp,
}
