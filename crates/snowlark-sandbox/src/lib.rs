// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The sandbox of page processes (`docs/process-isolation.md` §7, `docs/linux.md` stage 8).
//!
//! On Linux a sandboxed renderer is started in a user and a network namespace of its own
//! ([`Namespaces`], entered between fork and exec by the launcher): it has no network but the
//! browser's, and no capability anywhere. Once it has made what needs the system (V8, the text
//! engine), it locks itself down ([`lock_down`]): a seccomp-bpf filter on every thread refuses
//! the system calls that name a file by its path, make or connect a socket, start a process or
//! program, signal or trace another process, or reach the kernel's rarer surfaces. What it reads
//! afterwards comes as descriptors from the browser.
//!
//! Windows has no sandbox of its own here yet (M6 of the process isolation plan).

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{in_namespaces, lock_down, Namespaces};
