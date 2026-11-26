// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snowlark's graphics types that no platform API is behind: what layout produces and the painting
//! and compositing halves (`snowlark-paint`) consume, as plain data.
//!
//! - [`geom`]: points, sizes, rectangles, matrices, colours, sides and corner radii.
//! - [`display_list`]: [`DisplayListBuilder`] builds a [`DisplayList`] (plain vectors, items in paint
//!   order, balanced clip/transform/opacity groups); it can be encoded and validated-decoded.
//! - [`canvas`]: a `<canvas>` 2D context's drawing operations as data.
//! - [`filter`] and [`svg_filter`]: CSS and SVG filter operations and the area they reach.
//! - [`layer_animation`]: transform, opacity and filter animations a compositor runs without the page.
//! - [`raster`]: what rasterizing a list takes that no platform is behind (image sources, the
//!   viewport).
//! - [`scroll_curve`]: the curves smooth scrolls follow.
//! - [`color_profile`]: images' colour spaces (ICC profiles, PNG `cICP`, `gAMA` and `cHRM`)
//!   converted to sRGB.
//! - [`animation`]: animated images' frame timeline, container parsing and frame composition.
//! - [`image`]: decoding (PNG, JPEG, GIF, WebP and AVIF in Rust, `decoders`) into an [`ImageStore`]
//!   that display lists refer to by [`ImageId`].
//! - [`text`]: the text types every text engine fills in (font queries, shaping options, shaped
//!   runs and clusters), and [`text_cache`]'s cache of shaped runs.
//! - [`ticks`]: a moment in time that can cross a process boundary.

pub mod animation;
pub mod canvas;
pub mod color_profile;
mod decoders;
pub mod display_list;
mod error;
pub mod filter;
pub mod geom;
pub mod image;
pub mod layer_animation;
pub mod raster;
pub mod scroll_curve;
pub mod svg_filter;
pub mod text;
pub mod text_cache;
pub mod ticks;

pub use display_list::*;
pub use error::{Error, Result};
pub use filter::{ColorFilter, ColorFilters, FilterOp, FilterRange};
pub use geom::*;
pub use image::{DecodedImage, Frame, ImageStore, ShownImage};
pub use text::*;
pub use text_cache::{ShapeCache, ShapeKey, WordCacheStats};
pub use ticks::Ticks;
