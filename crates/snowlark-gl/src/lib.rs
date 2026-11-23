// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL's service side (`docs/webgl.md`): the [`protocol`] a page's contexts speak, and
//! [`Service`], which runs it on ANGLE. The page crate validates and encodes; nothing here knows
//! about pages, and nothing in the page crates calls OpenGL.

mod ffi;
pub mod protocol;
mod service;

pub use protocol::{
    Attributes, Backend, Command, ContextId, Created, Kind, Query, Readback, Reply,
};
pub use service::Service;

/// Whether ANGLE's libraries are where the service looks for them.
pub fn available() -> bool {
    ffi::load().is_ok()
}
