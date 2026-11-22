// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snowlark painting: the display list layout produces, and its rasterization with Direct2D.
//!
//! - [`display_list`]: [`DisplayListBuilder`] builds a [`DisplayList`] (plain vectors, items in paint
//!   order, balanced clip/transform/opacity groups); it can be encoded and validated-decoded.
//! - [`text`]: [`TextEngine`] on DirectWrite: font lists from CSS family lists, web fonts from memory,
//!   shaping to glyphs with clusters and break opportunities, fallback, metrics.
//! - [`text_cache`]: [`TextCache`]: shaped runs, and shaping per word, for text shaped again.
//! - [`image`]: WIC decoding into an [`ImageStore`] that display lists refer to by [`ImageId`].
//! - [`animation`]: animated images' frame timeline, container parsing and frame composition.
//! - [`raster`]: [`HeadlessRenderer`] (software, WIC bitmap) and the painter both paths share.
//! - [`gpu`]: the shared [`Gpu`] device and per-thread [`TileRasterizer`]s painting 512 px tiles.
//! - [`canvas`]: [`GpuCanvas`](canvas::GpuCanvas), a `<canvas>` 2D context's bitmap drawn by Direct2D.
//! - [`video`]: [`VideoStore`]: video frames on the GPU that the compositor draws.
//! - [`window`]: a Win32 window with a frame its owner draws, behind a safe event interface.
//! - [`compositor`]: the window's DirectComposition layers (a waitable flip-model swap chain for
//!   page content, a surface for the chrome) composing [`TiledFrame`]s and display lists.
//!
//! Threads: `TextEngine`, `ImageStore`, `Gpu` and `DisplayList` are `Send + Sync`; `TiledFrame` is
//! `Send`; `HeadlessRenderer` and `TileRasterizer` own a Direct2D context and stay on the thread that
//! made them. COM objects returned by getters are released by their Rust owners on drop, so nothing
//! here leaks the references that cost the C# engine 70 MB/s.

pub mod animation;
pub mod autoscroll_cursor;
pub mod canvas;
pub mod canvas_thread;
mod com;
pub mod compositor;
pub mod cursors;
pub mod demo;
pub mod display_list;
pub mod drop_target;
mod effects;
pub mod filter;
pub mod font_files;
pub mod geom;
pub mod gpu;
pub mod image;
mod ink;
pub mod input;
pub mod layer_animation;
pub mod pdf;
pub mod placement;
pub mod raster;
pub mod raster_tables;
pub mod scroll_curve;
mod soft_blur;
mod svg_effects;
pub mod svg_filter;
pub mod text;
pub mod text_cache;
pub mod ticks;
pub mod timer_resolution;
pub mod video;
pub mod vsync;
pub mod window;

pub use compositor::{Compositor, ContentPass};
pub use display_list::*;
pub use filter::{ColorFilter, ColorFilters, FilterOp, FilterRange};
pub use geom::*;
pub use gpu::{Driver, Gpu, RasterStats, RasterWork, TileRasterizer, TiledFrame, TILE_SIZE};
pub use image::{DecodedImage, Frame, ImageStore, Pixmap, ShownImage};
pub use raster::{FontResolver, HeadlessRenderer, ImageResolver, Sources, Viewport};
pub use raster_tables::{
    FileHead, FontFile, FontSource, FontTable, ImageTable, Refused, Resource, ResourceHead,
    SystemFontFiles, MAX_FONT_AXES, MAX_FONT_PATH, MAX_LIST_BYTES, MAX_TILES,
};
pub use text::*;
pub use text_cache::{ShapeCache, ShapeKey, TextCache, WordCache, WordCacheStats};
pub use ticks::Ticks;
pub use video::{VideoSlot, VideoStore};
pub use window::{WindowHandle, WindowProxy};
/// The error type of the Windows graphics calls (an HRESULT with its message).
pub use windows::core::{Error as WinError, Result as WinResult};
