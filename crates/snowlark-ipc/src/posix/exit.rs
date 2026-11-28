// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The end of a process that runs no other library's unload code (`crate::exit_now`).

/// Writes out what the C library holds in the buffers of its open streams (stdout when it is a pipe
/// or a file, written by C and C++ libraries).
pub(crate) fn flush_c_streams() {
    // SAFETY: a null stream asks the C library to flush every open output stream; it takes each
    // stream's own lock.
    unsafe {
        libc::fflush(std::ptr::null_mut());
    }
}

/// Ends this process with `code` at once: `_exit`, so no `atexit` handler, shared library
/// destructor or stream flush of the C library runs.
pub(crate) fn terminate(code: i32) -> ! {
    // SAFETY: `_exit` is async-signal-safe and takes only the status.
    unsafe { libc::_exit(code) }
}
