// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL's service side (`docs/webgl.md`): the [`protocol`] a page's contexts speak, and
//! [`Service`], which runs it on ANGLE. The page crate validates and encodes; nothing here knows
//! about pages, and nothing in the page crates calls OpenGL.

mod ffi;
pub mod pixels;
pub mod protocol;
mod service;
mod thread;

#[cfg(windows)]
pub use protocol::SharedTargets;
pub use protocol::{
    Attributes, Backend, Command, ContextId, Created, Kind, Pixels, Query, Readback, Reply,
    SharedPresent,
};
pub use thread::Service;

/// Whether this process's WebGL contexts run in the GPU process (`SNOWLARK_WEBGL=gpu-process`, which
/// the browser sets for its renderers when its GPU process runs their WebGL): it then opens no
/// device of its own.
pub fn in_gpu_process() -> bool {
    std::env::var_os("SNOWLARK_WEBGL").is_some_and(|v| v == "gpu-process")
}

/// Whether ANGLE's libraries are where the service looks for them.
pub fn available() -> bool {
    ffi::load().is_ok()
}
