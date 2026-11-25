// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! snowlark-render: one document from bytes to a display list. The page crate drives it; `--render` uses
//! it headless.
//!
//! - [`RenderStyle`] ([`adapter`]): stylo's `ComputedValues` as layout's [`snowlark_layout::Style`].
//! - [`DocumentStyles`] ([`source`]): layout's [`snowlark_layout::StyleSource`] over the style engine:
//!   element and pseudo-element styles, anonymous box styles, generated content with counters
//!   ([`content`]), the replaced content of images.
//! - [`Renderer`] ([`renderer`]): parsing, stylesheets (`<link>`, `@import`, charset), images (`<img>`
//!   with `srcset`/`sizes`/`<picture>`, CSS `url()`s), `@font-face` (checked by OTS and
//!   registered with the text engine: `snowlark_paint::web_font`), all fetched asynchronously through one `NetClient` and applied
//!   by [`Renderer::pump`]; then restyle, box tree, layout, paint, PNG.
//!
//! Driving it from an event loop:
//!
//! ```text
//! let mut r = Renderer::new(net, text, DeviceConfig { viewport: (w, h), ..Default::default() });
//! r.set_notify(Some(wake_event_loop));          // network progress wakes the loop
//! r.load_html(&bytes, &url, charset);
//! loop {                                         // each turn of the event loop
//!     r.pump();                                  // apply arrived sheets, images, fonts
//!     // a rendering opportunity (HTML "update the rendering"); None while render-blocked:
//!     if r.update_rendering(viewport).is_some() {
//!         let list = r.paint(viewport, scroll);  // hand to the compositor
//!     }
//!     if r.animating() { /* schedule the next frame */ }
//! }
//! ```
//!
//! `update_rendering` moves the document timeline to the clock's time ([`TimelineClock`]), so CSS
//! transitions and animations advance, then restyles and lays out. Script-driven reads use
//! [`Renderer::restyle`] (`getComputedStyle`) and [`Renderer::layout`] (geometry): they ignore
//! render-blocking and do not move the timeline.
//!
//! Headless: `load_html`, [`Renderer::render_until_settled`], [`Renderer::render_png`].

pub mod adapter;
mod anchor;
pub mod canvas;
mod clip_ref;
mod compositor_animations;
pub mod content;
pub mod controls;
mod encoding_url;
pub mod env;
mod fetch;
mod filter_ref;
mod inline_svg;
pub mod metrics;
mod motion;
pub mod renderer;
mod scroll_anchor;
pub mod source;
pub mod srcset;
pub mod svg;
pub mod svg_geometry;
pub mod svg_paint;
pub mod svg_text;
pub mod used;

pub use adapter::RenderStyle;
pub use encoding_url::encoding_parse_url;
pub use fetch::decode_stylesheet;
pub use renderer::{
    FaceInfo, FaceState, FetchPolicy, ImageStatus, PumpResult, RenderStats, Renderer,
    ResourceCounts, Settled, StyleBlockCheck, TimelineClock, Timings, ViewportScrollbar,
    DEFAULT_RENDER_BLOCKING_TIMEOUT,
};
pub use source::{DocumentStyles, ImageState};
mod svg_resource;
pub use used::NodeUsedValues;

#[cfg(test)]
mod tests;
