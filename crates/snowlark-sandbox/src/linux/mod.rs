// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Linux: a user and a network namespace from the launcher, a seccomp-bpf filter from the process.

mod filter;
#[cfg(test)]
mod tests;

use std::io::Error;
use std::sync::OnceLock;

/// The namespaces a sandboxed process starts in: a user namespace whose one user and group are the
/// launcher's own (so files are what they were, and the process holds no capability once it runs
/// its program), and a network namespace with no device but its own loopback, down.
///
/// Made before the fork (it formats the identity maps), entered in the child before exec.
pub struct Namespaces {
    uid_map: Vec<u8>,
    gid_map: Vec<u8>,
}

impl Default for Namespaces {
    fn default() -> Self {
        Self::new()
    }
}

impl Namespaces {
    pub fn new() -> Namespaces {
        // SAFETY: getuid and getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Namespaces {
            uid_map: format!("{uid} {uid} 1\n").into_bytes(),
            gid_map: format!("{gid} {gid} 1\n").into_bytes(),
        }
    }

    /// Whether this system lets a process make them: tried once, in a child that only does so
    /// (a kernel without unprivileged user namespaces, or a security module that takes a new
    /// namespace's capabilities away, refuses).
    pub fn available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let ns = Namespaces::new();
            // SAFETY: the child calls only `enter` (async-signal-safe system calls on memory made
            // before the fork) and `_exit`; the parent waits for it.
            unsafe {
                let pid = libc::fork();
                if pid == 0 {
                    libc::_exit(if ns.enter() == Ok(true) { 0 } else { 1 });
                }
                if pid < 0 {
                    return false;
                }
                let mut status = 0;
                while libc::waitpid(pid, &mut status, 0) < 0 {
                    if Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                        return false;
                    }
                }
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
            }
        })
    }

    /// Moves this process into new user and network namespaces. `Ok(false)` when the kernel
    /// refuses a user namespace (nothing changed); an error when it gave one but not its maps or
    /// the network namespace (the process is half in: it should not go on).
    ///
    /// # Safety
    ///
    /// For a child between fork and exec, single-threaded: it makes only async-signal-safe system
    /// calls and allocates nothing.
    pub unsafe fn enter(&self) -> Result<bool, i32> {
        if libc::unshare(libc::CLONE_NEWUSER) != 0 {
            return Ok(false);
        }
        let errno = || *libc::__errno_location();
        // (a kernel before 3.19 has no setgroups file and needs none)
        let _ = write_file(c"/proc/self/setgroups".as_ptr(), b"deny");
        if !write_file(c"/proc/self/uid_map".as_ptr(), &self.uid_map)
            || !write_file(c"/proc/self/gid_map".as_ptr(), &self.gid_map)
        {
            return Err(errno());
        }
        if libc::unshare(libc::CLONE_NEWNET) != 0 {
            return Err(errno());
        }
        Ok(true)
    }
}

/// `bytes` written to the file at `path` in one `write`.
///
/// # Safety
///
/// `path` ends in a NUL. Async-signal-safe.
unsafe fn write_file(path: *const libc::c_char, bytes: &[u8]) -> bool {
    let fd = libc::open(path, libc::O_WRONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return false;
    }
    let written = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
    libc::close(fd);
    written == bytes.len() as isize
}

/// Whether this process runs in a user namespace other than the system's first: its uid map is
/// not the whole range.
pub fn in_namespaces() -> bool {
    std::fs::read_to_string("/proc/self/uid_map")
        .map(|map| {
            !map.split_whitespace()
                .nth(2)
                .is_some_and(|n| n == "4294967295")
        })
        .unwrap_or(false)
}

/// Locks this process down for good: no new privileges, and the renderer's system call filter
/// ([`filter`]) on every thread. Files, sockets and libraries it needs afterwards must be open
/// before, or come from the browser as descriptors.
pub fn lock_down() -> Result<(), Error> {
    const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
    const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;
    let program = filter::renderer(std::process::id());
    let fprog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: `fprog` points at `program`, alive across the call; the kernel copies it.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(Error::last_os_error());
        }
        let r = libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_TSYNC,
            &fprog as *const libc::sock_fprog,
        );
        match r {
            0 => Ok(()),
            // (TSYNC: the id of a thread that could not take the filter)
            r if r > 0 => Err(Error::other(format!(
                "thread {r} could not take the system call filter"
            ))),
            _ => Err(Error::last_os_error()),
        }
    }
}
