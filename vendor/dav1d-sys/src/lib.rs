// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// Changed for Snowlark (SNOWLARK-PATCHES.md): the bindings are checked in, and a build without the
// dav1d library defines the functions itself (`missing`).

#[allow(warnings)]
#[cfg(feature = "dav1d")]
pub mod bindings {
    include!("bindings.rs");
}

/// Whether the dav1d library is linked: `false` when the build found none, and every dav1d
/// function fails.
pub const LINKED: bool = cfg!(not(dav1d_missing));

/// The functions CrabbyAvif calls, for a build without the library: opening a decoder fails
/// (`DAV1D_ERR(ENOSYS)`), so no picture is ever asked for, and the others do nothing.
#[cfg(all(feature = "dav1d", dav1d_missing))]
mod missing {
    use super::bindings::*;
    use std::os::raw::{c_char, c_int};

    /// `ENOSYS` in the C runtime dav1d is built against.
    const ENOSYS: c_int = 40;

    #[no_mangle]
    pub extern "C" fn dav1d_version() -> *const c_char {
        c"none".as_ptr()
    }
    #[no_mangle]
    pub extern "C" fn dav1d_default_settings(_s: *mut Dav1dSettings) {}
    #[no_mangle]
    pub extern "C" fn dav1d_open(
        _c_out: *mut *mut Dav1dContext,
        _s: *const Dav1dSettings,
    ) -> c_int {
        -ENOSYS
    }
    #[no_mangle]
    pub extern "C" fn dav1d_send_data(_c: *mut Dav1dContext, _in: *mut Dav1dData) -> c_int {
        -ENOSYS
    }
    #[no_mangle]
    pub extern "C" fn dav1d_get_picture(_c: *mut Dav1dContext, _out: *mut Dav1dPicture) -> c_int {
        -ENOSYS
    }
    #[no_mangle]
    pub extern "C" fn dav1d_picture_unref(_p: *mut Dav1dPicture) {}
    #[no_mangle]
    pub extern "C" fn dav1d_data_create(_data: *mut Dav1dData, _sz: usize) -> *mut u8 {
        std::ptr::null_mut()
    }
    #[no_mangle]
    pub extern "C" fn dav1d_data_unref(_data: *mut Dav1dData) {}
    #[no_mangle]
    pub extern "C" fn dav1d_close(_c_out: *mut *mut Dav1dContext) {}
    #[no_mangle]
    pub extern "C" fn dav1d_flush(_c: *mut Dav1dContext) {}
}
