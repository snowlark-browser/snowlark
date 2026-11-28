// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The end of a process that runs no other library's unload code (`crate::exit_now`).

use std::ffi::c_void;

use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

extern "C" {
    // (the C runtime's, which every Rust program on this target links)
    fn fflush(stream: *mut c_void) -> i32;
}

/// Writes out what the C runtime holds in the buffers of its open streams (stdout when it is a pipe
/// or a file, written by C and C++ libraries).
pub(crate) fn flush_c_streams() {
    // SAFETY: a null stream asks the C runtime to flush every open output stream; it takes each
    // stream's own lock.
    unsafe {
        fflush(std::ptr::null_mut());
    }
}

/// Ends this process with `code` at once: every thread stops where it is, and no library's
/// detach code, C runtime exit handler or destructor runs.
pub(crate) fn terminate(code: i32) -> ! {
    // SAFETY: the pseudo handle of this process, always valid. The call does not return when it
    // succeeds: the process is ending.
    unsafe {
        let _ = TerminateProcess(GetCurrentProcess(), code as u32);
    }
    // (it cannot fail on the process's own pseudo handle; the ordinary end if it ever did)
    std::process::exit(code)
}
