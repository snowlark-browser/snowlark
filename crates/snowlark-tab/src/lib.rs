// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! snowlark-tab: the renderer half of a tab.
//!
//! One thread per tab ([`tab`]) owns the tab's [`driver::TabDriver`] (the document: by default
//! [`page_driver::PageDriver`] over the page crate's `Page`, with scripts;
//! [`renderer_driver::RendererDriver`] renders without them) and a tile rasterizer
//! ([`raster_thread`]). It takes [`tab::TabCommand`]s and answers with [`tab::TabEvent`]s through an
//! [`tab::EventSink`]; it hands frames to the compositor as [`comp::FrameData`]. Nothing here knows the
//! browser's window or its pages: the browser half implements [`tab::EventSink`] and
//! [`driver::InternalPages`], and the messages are plain data, so the two halves can be separate
//! processes: [`renderer_process`] is the browser's end of a tab in a process of its own, and
//! [`renderer_main`] that process's.
//!
//! On Linux all of it builds: the tiles are painted on the CPU and videos show no frames yet
//! (docs/linux.md, stages 5 and 7).

#![forbid(unsafe_code)]

pub mod comp;
pub mod devtools;
pub mod driver;
pub mod evil;
pub mod page_driver;
mod raster_link;
#[cfg(all(test, windows))]
mod raster_pixels;
pub mod raster_thread;
pub mod renderer_driver;
pub mod renderer_main;
pub mod renderer_process;
pub mod script_check;
pub mod services;
pub mod spare;
pub mod tab;
pub mod video;
pub mod watchdog;
pub mod webdriver_actions;
