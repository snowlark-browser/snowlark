// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The GPU on Linux, before there is a GPU backend: the choice of device the canvas takes
//! ([`Driver`]), and a [`Gpu`] that cannot be made yet. Tiles and the compositor's device come with
//! stage 5 (docs/linux.md).

/// Which kind of device to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    /// The graphics card.
    Hardware,
    /// A software rasterizer (no GPU needed).
    Warp,
}

use std::sync::Arc;

use snowlark_graphics::{Error, Result};

/// A GPU device: none can be made on Linux yet, so every `Option<Arc<Gpu>>` is `None` there.
#[derive(Debug)]
pub enum Gpu {}

impl Gpu {
    /// The hardware device, or a software one: an error here.
    pub fn new() -> Result<Arc<Gpu>> {
        Gpu::with_driver(Driver::Hardware)
    }

    pub fn with_driver(_driver: Driver) -> Result<Arc<Gpu>> {
        Err(Error::Platform("no GPU device on this system yet".into()))
    }
}
