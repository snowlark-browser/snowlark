// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! snowlark-page: a document's lifetime.
//!
//! - [`engine::Engine`] is the type behind the generated bindings: it implements every `*Impl` trait of
//!   `snowlark-bindings` over one [`snowlark_dom::Dom`], plus events and their dispatch (DOM §2), custom
//!   elements and their reactions (HTML §4.13), mutation observers (DOM §4.3), ranges and traversal,
//!   the window's members and the HTML element members the WPT areas of milestone M1 need.
//! - [`Page`] owns the V8 isolate and the window realm (a context whose global object is the `Window`,
//!   made from the generated `[Global]` template), and drives the event loop (HTML §8.1.7): task queues,
//!   the microtask checkpoint, timers, animation frames at 60 Hz, network events.
//! - Loading: navigate, fetch through `snowlark-net`, sniff the encoding, parse incrementally with
//!   `snowlark-dom`'s [`snowlark_dom::HtmlParser`], running classic and module scripts at the points HTML
//!   gives, `document.write`, `DOMContentLoaded` and `load`.
//!
//! - Rendering ([`render`]): the top-level document's `snowlark_render::Renderer` over the shared DOM,
//!   fed with the DOM's reactions and the loader's stylesheets; the event loop's "update the
//!   rendering" step; the layout-dependent APIs (CSSOM View, `innerText`, `getComputedStyle`) read it.
//! - Content blocking's document hooks ([`content_filter`]): cosmetic filters as a user sheet,
//!   scriptlets at document start, procedural filters after DOM changes.

/// The stack size of a thread that runs a page: style, layout and painting recurse over the DOM and
/// box trees, so deeply nested documents need more than a default thread's 1 or 2 MiB (Windows
/// commits the reserve as it is used).
pub const PAGE_THREAD_STACK: usize = 64 << 20;

pub mod caret;
mod caret_walk;
mod clipboard_events;
pub mod content_filter;
mod csp;
pub mod devtools;
pub mod devtools_node;
pub mod dnd;
pub mod editing;
pub mod embedder;
pub mod engine;
pub mod error_page;
mod event_loop;
mod exec_command;
pub mod find;
pub mod ime;
pub mod import_map;
pub mod input;
pub mod keyboard;
mod loader;
pub mod media;
mod page;
pub mod popup;
pub mod profile;
mod refresh;
pub mod render;
mod resizer;
mod script;
mod speculative;
mod undo;
pub mod web_storage;
mod worker;

pub use content_filter::{ContentFilter, DocumentFilters};
pub use engine::broadcast::{BroadcastBackend, BroadcastHub, Inbox, LocalBroadcast};
pub use engine::platform::ClipboardAccess;
pub use engine::window::ConsoleOutput;
pub use engine::window::ScreenPlacement;
pub use engine::Engine;
pub use engine::{HostedWindow, WindowChange};
pub use event_loop::BeginFrame;
pub use loader::{CommitParams, CommitResponse, LoadStats};
pub use page::{Page, PageMemory, PageOptions, Terminator};
pub use render::{RenderSetup, RenderState};
/// The IndexedDB backend a browser's pages share (`snowlark_idb::Backend`).
pub use snowlark_idb::Backend as IdbBackend;
pub use speculative::EarlyStats;
