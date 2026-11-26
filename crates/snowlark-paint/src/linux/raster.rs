// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rasterizing display lists on Linux, before there is a raster backend: [`HeadlessRenderer`] with
//! the Windows renderer's methods, which cannot be made ([`HeadlessRenderer::new`] fails), so the
//! crates above build. The backend is a decision of stage 3 (docs/linux.md, "Decisions still open").

use std::sync::Arc;

use crate::display_list::DisplayList;
use crate::image::Pixmap;
use crate::text::TextEngine;
pub use snowlark_graphics::raster::{ImageResolver, Viewport};
use snowlark_graphics::{Error, Result};

/// Where a list's `FontId`s resolve: the text engine that shaped its glyphs.
pub trait FontResolver {}

impl FontResolver for TextEngine {}

impl<T: FontResolver + ?Sized> FontResolver for Arc<T> {}

/// What a display list refers to: the fonts that shaped its glyphs and the images it shows.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub text: &'a dyn FontResolver,
    pub images: &'a dyn ImageResolver,
}

/// Renders display lists into pixels: none here yet (see the module documentation).
pub struct HeadlessRenderer {
    never: std::convert::Infallible,
}

impl HeadlessRenderer {
    /// Fails: there is no raster backend on this platform yet.
    pub fn new() -> Result<HeadlessRenderer> {
        Err(Error::Unsupported)
    }

    /// Grayscale text antialiasing on opaque targets too.
    pub fn set_grayscale_text(&mut self, _on: bool) {
        match self.never {}
    }

    /// Renders `list` at `view`'s size and scale.
    pub fn render(
        &mut self,
        _list: &DisplayList,
        _sources: Sources,
        _view: &Viewport,
    ) -> Result<Pixmap> {
        match self.never {}
    }
}
