// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What rasterizing a display list takes that no platform is behind: where its images come from
//! ([`ImageResolver`]) and the pixels it is drawn into ([`Viewport`]). The rasterizers are
//! `snowlark-paint`'s.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::display_list::ImageId;
use crate::geom::Color;
use crate::image::{ImageStore, ShownImage};

/// Where a list's [`ImageId`]s resolve: the page's [`ImageStore`], or, on the far side of the raster
/// seam, `snowlark-paint`'s `ImageTable`.
pub trait ImageResolver {
    /// What `id` shows now.
    fn shown(&self, id: ImageId) -> Option<ShownImage>;
    /// [`ImageStore::version`].
    fn version(&self, id: ImageId) -> u64;
}

impl ImageResolver for ImageStore {
    fn shown(&self, id: ImageId) -> Option<ShownImage> {
        self.shown(id)
    }

    fn version(&self, id: ImageId) -> u64 {
        self.version(id)
    }
}

impl<T: ImageResolver + ?Sized> ImageResolver for Arc<T> {
    fn shown(&self, id: ImageId) -> Option<ShownImage> {
        (**self).shown(id)
    }

    fn version(&self, id: ImageId) -> u64 {
        (**self).version(id)
    }
}

/// The output size and how CSS pixels map onto it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// Device pixels.
    pub width: u32,
    pub height: u32,
    /// Device pixels per CSS pixel.
    pub scale: f32,
    /// Filled first. An opaque background makes the target opaque (24-bit, subpixel text).
    pub background: Color,
}
