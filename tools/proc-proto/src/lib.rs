// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Prototype P0 of `docs/process-isolation.md` section 3, a throwaway: `proc-a` owns the window (and
//! drives the tests), `proc-b` owns the device and presents, plays video or watches. Results are in
//! `docs/process-isolation-p0.md`.

pub mod comp;
pub mod engine;
pub mod marks;
pub mod scene;
pub mod video;
pub mod videohost;
pub mod watch;
pub mod wire;

/// `--name value` from the command line.
pub fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

/// `--prio`: this thread runs above the busy machine's normal-priority work.
pub fn raise_thread() {
    if flag("--prio") {
        use windows::Win32::System::Threading::*;
        // SAFETY: a pseudo handle to the calling thread.
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
    }
}

/// Ends this process at once: DXGI's teardown at a normal exit has hung here, and only results that
/// are already on disk matter by now.
pub fn end_process() -> ! {
    use windows::Win32::System::Threading::*;
    // SAFETY: terminating ourselves.
    let _ = unsafe { TerminateProcess(GetCurrentProcess(), 0) };
    std::process::exit(0);
}

pub fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

pub fn arg_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    arg(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}
