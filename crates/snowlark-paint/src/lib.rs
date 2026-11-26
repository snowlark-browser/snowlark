// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snowlark painting: the platform's half of drawing. The display list layout produces, the
//! geometry, filters, layer animations, the image decoders written in Rust and the text types are
//! `snowlark-graphics`, re-exported here under the same paths ([`display_list`], [`geom`],
//! [`filter`], [`layer_animation`], [`color_profile`], [`animation`], ...); this crate has what
//! sits on the platform's APIs:
//!
//! - [`text`]: [`TextEngine`] on DirectWrite: font lists from CSS family lists, web fonts from memory,
//!   shaping to glyphs with clusters and break opportunities, fallback, metrics.
//! - [`text_cache`]: [`TextCache`]: shaped runs, and shaping per word, for text shaped again.
//! - [`image`]: the formats not decoded in Rust (BMP, ICO, TIFF and others) through WIC, and PNG
//!   encoding of rendered [`Pixmap`]s.
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

// The platform-free modules live in `snowlark-graphics`; they are re-exported under the same paths.
// (`image`, `text` and `text_cache` are modules of this crate that re-export theirs.)
pub use snowlark_graphics::{
    animation, color_profile, display_list, filter, geom, layer_animation, scroll_curve,
    svg_filter, ticks,
};

// Modules with a half per platform: Windows' in `<name>.rs`, Linux's in `linux/<name>.rs`, with
// the same items. The other platform modules are Windows-only for now (docs/linux.md, "Stages").
#[cfg_attr(not(windows), path = "linux/canvas.rs")]
pub mod canvas;
#[cfg_attr(not(windows), path = "linux/canvas_thread.rs")]
pub mod canvas_thread;
#[cfg_attr(not(windows), path = "linux/gpu.rs")]
pub mod gpu;
#[cfg_attr(not(windows), path = "linux/image.rs")]
pub mod image;
#[cfg_attr(not(windows), path = "linux/raster.rs")]
pub mod raster;
#[cfg_attr(not(windows), path = "linux/text.rs")]
pub mod text;
#[cfg_attr(not(windows), path = "linux/text_cache.rs")]
pub mod text_cache;

pub mod autoscroll_cursor;
#[cfg(windows)]
mod com;
#[cfg(windows)]
pub mod compositor;
#[cfg(windows)]
pub mod cursors;
#[cfg(windows)]
pub mod demo;
#[cfg(windows)]
pub mod drop_target;
#[cfg(windows)]
mod effects;
pub mod font_files;
#[cfg(windows)]
mod ink;
pub mod input;
#[cfg(windows)]
pub mod os_crypt;
#[cfg_attr(not(windows), path = "linux/pdf.rs")]
pub mod pdf;
pub mod placement;
#[cfg(windows)]
pub mod raster_tables;
#[cfg(windows)]
mod soft_blur;
#[cfg(windows)]
mod svg_effects;
mod tiling;
#[cfg(windows)]
pub mod timer_resolution;
#[cfg_attr(not(windows), path = "linux/video.rs")]
pub mod video;
#[cfg(windows)]
pub mod vsync;
pub mod web_font;
#[cfg_attr(not(windows), path = "linux/window.rs")]
pub mod window;

#[cfg(windows)]
pub use compositor::{Compositor, ContentPass};
pub use display_list::*;
pub use filter::{ColorFilter, ColorFilters, FilterOp, FilterRange};
pub use geom::*;
pub use gpu::{Driver, Gpu};
pub use gpu::{RasterStats, RasterWork, TileRasterizer, TiledFrame, TILE_SIZE};
pub use image::{DecodedImage, Frame, ImageStore, Pixmap, ShownImage};
pub use raster::{FontResolver, HeadlessRenderer, ImageResolver, Sources, Viewport};
#[cfg(windows)]
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
#[cfg(windows)]
pub use windows::core::{Error as WinError, Result as WinResult};
