// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `exit_now`: a process that ends without running any library's unload code, its output written.
//! The child is this test binary again (`exit::exit_child`), with a hook that stands in for a
//! library's unload code: a TLS callback called at the process detach on Windows, an `atexit`
//! handler on Linux. When it runs, it leaves a marker file.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const ROLE: &str = "SNOWLARK_IPC_EXIT_ROLE";
const MARKER: &str = "SNOWLARK_IPC_EXIT_MARKER";

/// Where the unload hook leaves its marker; unset, the hook does nothing.
static MARKER_PATH: OnceLock<PathBuf> = OnceLock::new();

fn unload_hook_ran() {
    if let Some(path) = MARKER_PATH.get() {
        let _ = std::fs::write(path, b"unload code ran");
    }
}

// (as a DLL's detach runs: the loader calls each module's TLS callbacks with DLL_PROCESS_DETACH
// in the ordinary exit, after it has stopped the other threads)
#[cfg(windows)]
#[used]
#[link_section = ".CRT$XLY"]
static TLS_CALLBACK: unsafe extern "system" fn(*mut std::ffi::c_void, u32, *mut std::ffi::c_void) =
    tls_callback;

#[cfg(windows)]
unsafe extern "system" fn tls_callback(
    _module: *mut std::ffi::c_void,
    reason: u32,
    _reserved: *mut std::ffi::c_void,
) {
    const DLL_PROCESS_DETACH: u32 = 0;
    if reason == DLL_PROCESS_DETACH {
        unload_hook_ran();
    }
}

#[cfg(unix)]
extern "C" fn at_exit() {
    unload_hook_ran();
}

/// Writes `text` (NUL-terminated) to the C runtime's stdout, where it stays in the stream's
/// buffer while stdout is a pipe.
fn c_stdout(text: &std::ffi::CStr) {
    #[cfg(windows)]
    {
        extern "C" {
            fn __acrt_iob_func(index: u32) -> *mut std::ffi::c_void;
            fn fputs(text: *const std::ffi::c_char, stream: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: stream 1 of the C runtime is its stdout, open for the life of the process; the
        // text is NUL-terminated.
        unsafe { fputs(text.as_ptr(), __acrt_iob_func(1)) };
    }
    #[cfg(unix)]
    {
        extern "C" {
            static mut stdout: *mut libc::FILE;
        }
        // SAFETY: the C library's stdout stream, open for the life of the process; the text is
        // NUL-terminated.
        unsafe { libc::fputs(text.as_ptr(), stdout) };
    }
}

/// The child: arms the hook, leaves output in both stdout buffers, and ends with code 7, through
/// `exit_now` or the ordinary exit. Not a test of its own: without the variable it does nothing.
#[test]
fn exit_child() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let _ = MARKER_PATH.set(PathBuf::from(std::env::var_os(MARKER).unwrap()));
    #[cfg(unix)]
    // SAFETY: registers a plain function with no arguments.
    unsafe {
        libc::atexit(at_exit);
    }
    // (no newline: Rust's stdout keeps it until a flush)
    print!("[rust stdout]");
    c_stdout(c"[c stdout]");
    match role.as_str() {
        "now" => snowlark_ipc::exit_now(7),
        _ => std::process::exit(7),
    }
}

fn run_child(role: &str) -> (Output, bool) {
    let marker: PathBuf =
        std::env::temp_dir().join(format!("snowlark-ipc-exit-{}-{role}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["exit::exit_child", "--exact", "--nocapture"])
        .env(ROLE, role)
        .env(MARKER, &marker)
        .output()
        .unwrap();
    let ran = Path::new(&marker).exists();
    let _ = std::fs::remove_file(&marker);
    (out, ran)
}

/// The control: the ordinary exit runs the unload hook, as it runs every library's.
#[test]
fn the_ordinary_exit_runs_unload_code() {
    let (out, ran) = run_child("ordinary");
    assert_eq!(out.status.code(), Some(7));
    assert!(ran, "the unload hook did not run in the ordinary exit");
}

#[test]
fn exit_now_runs_no_unload_code_and_keeps_output_and_code() {
    let (out, ran) = run_child("now");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(7), "stdout: {stdout}");
    assert!(!ran, "the unload hook ran");
    assert!(stdout.contains("[rust stdout]"), "stdout: {stdout}");
    assert!(stdout.contains("[c stdout]"), "stdout: {stdout}");
}
