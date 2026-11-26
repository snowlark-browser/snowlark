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
//! Elsewhere than Windows only [`script_check`] and [`webdriver_actions`] are built so far: the
//! headless modes use them; the tab's threads and processes wait on docs/linux.md stages 4 and 5.

#![forbid(unsafe_code)]

#[cfg(windows)]
pub mod comp;
#[cfg(windows)]
pub mod devtools;
#[cfg(windows)]
pub mod driver;
#[cfg(windows)]
pub mod evil;
#[cfg(windows)]
pub mod page_driver;
#[cfg(windows)]
mod raster_link;
#[cfg(all(test, windows))]
mod raster_pixels;
#[cfg(windows)]
pub mod raster_thread;
#[cfg(windows)]
pub mod renderer_driver;
#[cfg(windows)]
pub mod renderer_main;
#[cfg(windows)]
pub mod renderer_process;
pub mod script_check;
#[cfg(windows)]
pub mod services;
#[cfg(windows)]
pub mod spare;
#[cfg(windows)]
pub mod tab;
#[cfg(windows)]
pub mod video;
#[cfg(windows)]
pub mod watchdog;
pub mod webdriver_actions;
