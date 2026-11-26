// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minidumps of crashed children on Linux, written by the browser from outside, as a crash
//! handler process writes them for its clients in the established design (a client's signal
//! handler sends its crash context over a socket the handler gave it, and waits while the handler
//! attaches to it with `ptrace` and writes the dump).
//!
//! A child launched with crash dumps on ([`crate::Launch::crash_dumps`]) has one end of a
//! `SOCK_SEQPACKET` pair as descriptor [`CRASH_FD`] and [`CRASH_VAR`] naming it. Its handler for the
//! crash signals ([`install_crash_handler`]; `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`, `SIGABRT`,
//! `SIGTRAP`) lets the browser trace it, sends the crashing thread's context (its registers, the
//! signal and the thread's id) as one message, and waits for one byte back; then it hands the signal
//! on, so the child ends with it and its exit code says what happened as before. A thread in the
//! browser receives the message, writes the dump of the child with `minidump-writer` (which stops
//! and reads the child's threads through `ptrace`), keeps the [`KEPT_DUMPS`] newest in the folder,
//! and answers.
//!
//! What the child sends is its word: the browser takes the context only as the crash's description
//! in the dump and blames a thread only when it is one of the child's.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crash_handler::{CrashContext, CrashEventResult, CrashHandler};

use crate::error::Error;

/// The variable that tells a child its crash socket's descriptor.
pub(crate) const CRASH_VAR: &str = "SNOWLARK_CRASH_DUMP";

/// The descriptor a child launched with crash dumps on finds its crash socket at (its channel is
/// at 3).
pub(crate) const CRASH_FD: libc::c_int = 4;

/// How many dumps a folder keeps: the oldest go when a new one is written.
pub const KEPT_DUMPS: usize = 10;

/// The two ends of a child's crash socket, made at launch.
pub(crate) struct CrashLink {
    /// The browser's.
    pub(crate) server: OwnedFd,
    /// The child's, moved to [`CRASH_FD`] in it.
    pub(crate) client: OwnedFd,
}

impl CrashLink {
    pub(crate) fn new() -> Result<CrashLink, Error> {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors, owned at once below.
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if r != 0 {
            return Err(Error::Os(std::io::Error::last_os_error()));
        }
        // SAFETY: two new descriptors of ours.
        let (server, client) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Ok(CrashLink { server, client })
    }

    /// Watches child `pid` through `server` until its end of the socket closes: a crash it reports
    /// is written to a new file in `dir`, whose path goes to `dump`.
    pub(crate) fn watch(
        server: OwnedFd,
        pid: u32,
        dir: PathBuf,
        dump: Arc<Mutex<Option<PathBuf>>>,
    ) -> Result<(), Error> {
        std::thread::Builder::new()
            .name(format!("snowlark crash watch {pid}"))
            .spawn(move || {
                let mut buf = vec![0u8; std::mem::size_of::<CrashContext>() + 1];
                loop {
                    // SAFETY: a buffer of ours, its length; a descriptor this thread owns.
                    let n = unsafe {
                        libc::recv(server.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0)
                    };
                    if n <= 0 {
                        // (the child ended, or its end closed: no crash to write)
                        return;
                    }
                    let context = CrashContext::from_bytes(&buf[..n as usize]);
                    if let Some(path) = context.and_then(|c| write_dump(pid, c, &dir)) {
                        *dump.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
                    }
                    let ack = 1u8;
                    // SAFETY: one byte of ours; a child gone meanwhile makes it fail, nothing more.
                    unsafe {
                        libc::send(
                            server.as_raw_fd(),
                            (&ack as *const u8).cast(),
                            1,
                            libc::MSG_NOSIGNAL,
                        )
                    };
                }
            })
            .map_err(|_| Error::Refused)?;
        Ok(())
    }
}

/// Writes the dump of child `pid`, crashed as `context` says, to a new file in `dir`; keeps the
/// [`KEPT_DUMPS`] newest there.
fn write_dump(pid: u32, mut context: CrashContext, dir: &Path) -> Option<PathBuf> {
    // (the thread blamed must be the child's; the process is ours to name)
    let tid = context.tid;
    if !Path::new(&format!("/proc/{pid}/task/{tid}")).exists() {
        return None;
    }
    context.pid = pid as libc::pid_t;
    std::fs::create_dir_all(dir).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let path = dir.join(format!("renderer-{}-{pid}.dmp", stamp.as_millis()));
    let mut file = std::fs::File::create(&path).ok()?;
    let mut writer = minidump_writer::minidump_writer::MinidumpWriterConfig::new(pid as i32, tid);
    writer.set_crash_context(minidump_writer::CrashContextExt { inner: context });
    let written = writer.write(&mut file);
    drop(file);
    if let Err(e) = written {
        eprintln!("[crash] no dump of {pid}: {e}");
        let _ = std::fs::remove_file(&path);
        return None;
    }
    prune(dir);
    Some(path)
}

/// Keeps the [`KEPT_DUMPS`] newest `.dmp` files in `dir`.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dumps: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "dmp"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    dumps.sort();
    let extra = dumps.len().saturating_sub(KEPT_DUMPS);
    for (_, p) in dumps.into_iter().take(extra) {
        let _ = std::fs::remove_file(p);
    }
}

/// The child's handler, attached once for the life of the process.
static HANDLER: OnceLock<CrashHandler> = OnceLock::new();

/// In a child the browser launched with crash dumps on: attaches the signal handler that has the
/// browser write a dump. False when there is none to attach (no variable, or not a socket).
pub fn install_crash_handler() -> bool {
    let Some(fd) = std::env::var(CRASH_VAR)
        .ok()
        .and_then(|v| v.parse::<RawFd>().ok())
    else {
        return false;
    };
    // (the variable names the descriptor the launch put there, and nothing else)
    if fd != CRASH_FD || !is_seqpacket(fd) {
        return false;
    }
    if HANDLER.get().is_some() {
        return true;
    }
    // SAFETY: the closure only calls async-signal-safe functions (send, recv) on a descriptor that
    // stays open for the life of the process, with buffers it owns; it allocates nothing.
    let event = unsafe {
        crash_handler::make_crash_event(move |context: &CrashContext| {
            let bytes = context.as_bytes();
            libc::send(fd, bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL);
            let mut ack = 0u8;
            libc::recv(fd, (&mut ack as *mut u8).cast(), 1, 0);
            // (not handled: the signal goes on and ends the process)
            CrashEventResult::Handled(false)
        })
    };
    let Ok(handler) = CrashHandler::attach(event) else {
        return false;
    };
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() };
    handler.set_ptracer(Some(parent as u32));
    let _ = HANDLER.set(handler);
    true
}

/// Whether `fd` is an open `SOCK_SEQPACKET` socket.
fn is_seqpacket(fd: RawFd) -> bool {
    let mut kind: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: reads an option of a descriptor into a local of the size given.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    r == 0 && kind == libc::SOCK_SEQPACKET
}
