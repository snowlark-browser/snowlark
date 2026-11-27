// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The renderer's system call filter: a classic BPF program over `struct seccomp_data`
//! (`linux/seccomp.h`), x86-64 only.
//!
//! Its default is to allow; it refuses, with `EPERM`, what a page process has no use for once its
//! libraries and fonts are open: naming a file by its path, making, binding or connecting a socket,
//! starting a process or a program, signalling or tracing another process, changing its
//! credentials, and the kernel's rarer surfaces (BPF, keyrings, io_uring, modules, mounts). A
//! call made for another architecture or through the x32 numbers ends the process.

use libc::sock_filter;

const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const JGE: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
const JSET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
/// The x32 system call numbers carry this bit.
const X32_BIT: u32 = 0x4000_0000;

/// `struct seccomp_data`: `nr`, `arch`, then `args[6]` from byte 16 (little-endian halves).
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg_low(i: u32) -> u32 {
    16 + 8 * i
}
const fn arg_high(i: u32) -> u32 {
    16 + 8 * i + 4
}

const AT_EMPTY_PATH: u32 = 0x1000;

/// Refused with `EPERM`. Numbers newer than every libc release we build with are written out.
const DENIED: &[i64] = &[
    // files by their path
    libc::SYS_open,
    libc::SYS_openat,
    437, // openat2
    libc::SYS_creat,
    libc::SYS_link,
    libc::SYS_linkat,
    libc::SYS_unlink,
    libc::SYS_unlinkat,
    libc::SYS_rename,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_mkdir,
    libc::SYS_mkdirat,
    libc::SYS_rmdir,
    libc::SYS_mknod,
    libc::SYS_mknodat,
    libc::SYS_symlink,
    libc::SYS_symlinkat,
    libc::SYS_readlink,
    libc::SYS_readlinkat,
    libc::SYS_chmod,
    libc::SYS_fchmodat,
    452, // fchmodat2
    libc::SYS_chown,
    libc::SYS_lchown,
    libc::SYS_fchownat,
    libc::SYS_truncate,
    libc::SYS_utime,
    libc::SYS_utimes,
    libc::SYS_utimensat,
    libc::SYS_futimesat,
    libc::SYS_access,
    libc::SYS_faccessat,
    439, // faccessat2
    libc::SYS_stat,
    libc::SYS_lstat,
    libc::SYS_statfs,
    libc::SYS_chdir,
    libc::SYS_chroot,
    libc::SYS_getxattr,
    libc::SYS_lgetxattr,
    libc::SYS_setxattr,
    libc::SYS_lsetxattr,
    libc::SYS_listxattr,
    libc::SYS_llistxattr,
    libc::SYS_removexattr,
    libc::SYS_lremovexattr,
    libc::SYS_inotify_add_watch,
    libc::SYS_fanotify_init,
    libc::SYS_fanotify_mark,
    libc::SYS_name_to_handle_at,
    libc::SYS_open_by_handle_at,
    libc::SYS_uselib,
    // mounts
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    428, // open_tree
    429, // move_mount
    430, // fsopen
    431, // fsconfig
    432, // fsmount
    433, // fspick
    442, // mount_setattr
    457, // statmount
    458, // listmount
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_acct,
    libc::SYS_quotactl,
    // sockets (pairs of local sockets are allowed below)
    libc::SYS_socket,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    // processes and programs
    libc::SYS_fork,
    libc::SYS_vfork,
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_tkill,
    424, // pidfd_send_signal
    434, // pidfd_open
    438, // pidfd_getfd
    libc::SYS_kcmp,
    libc::SYS_unshare,
    libc::SYS_setns,
    // credentials
    libc::SYS_setuid,
    libc::SYS_setgid,
    libc::SYS_setreuid,
    libc::SYS_setregid,
    libc::SYS_setresuid,
    libc::SYS_setresgid,
    libc::SYS_setfsuid,
    libc::SYS_setfsgid,
    libc::SYS_setgroups,
    libc::SYS_capset,
    libc::SYS_personality,
    // the kernel's rarer surfaces
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_userfaultfd,
    425, // io_uring_setup
    426, // io_uring_enter
    427, // io_uring_register
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_reboot,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
    libc::SYS_sethostname,
    libc::SYS_setdomainname,
    libc::SYS_iopl,
    libc::SYS_ioperm,
    libc::SYS_syslog,
    libc::SYS_vhangup,
    libc::SYS_lookup_dcookie,
];

/// `clone3`: `ENOSYS`, so the C library makes its threads with `clone`, whose flags the filter can
/// read (`clone3`'s are behind a pointer).
const CLONE3: i64 = 435;

/// Calls whose first argument is a process: allowed only on `pid`, the process itself.
const SIGNALS: &[i64] = &[
    libc::SYS_kill,
    libc::SYS_tgkill,
    libc::SYS_rt_sigqueueinfo,
    libc::SYS_rt_tgsigqueueinfo,
];

struct Program(Vec<sock_filter>);

impl Program {
    fn op(&mut self, code: u16, jt: u8, jf: u8, k: u32) {
        self.0.push(sock_filter { code, jt, jf, k });
    }
    fn load(&mut self, offset: u32) {
        self.op(LD_W_ABS, 0, 0, offset);
    }
    fn ret(&mut self, value: u32) {
        self.op(RET, 0, 0, value);
    }
    /// If the number loaded is `nr`, the next `len` instructions; past them otherwise.
    fn on(&mut self, nr: i64, len: u8) {
        self.op(JEQ, 0, len, nr as u32);
    }
    /// `nr` allowed when argument `arg`'s low half has any bit of `bits`, refused otherwise.
    fn when_bits(&mut self, nr: i64, arg: u32, bits: u32) {
        self.on(nr, 4);
        self.load(arg_low(arg));
        self.op(JSET, 0, 1, bits);
        self.ret(RET_ALLOW);
        self.ret(RET_ERRNO | libc::EPERM as u32);
    }
}

/// The renderer's filter for the process `pid`.
pub(super) fn renderer(pid: u32) -> Vec<sock_filter> {
    let eperm = RET_ERRNO | libc::EPERM as u32;
    let mut p = Program(Vec::with_capacity(DENIED.len() * 2 + 64));
    p.load(ARCH);
    p.op(JEQ, 1, 0, AUDIT_ARCH_X86_64);
    p.ret(RET_KILL_PROCESS);
    p.load(NR);
    p.op(JGE, 0, 1, X32_BIT);
    p.ret(RET_KILL_PROCESS);
    for &nr in DENIED {
        p.on(nr, 1);
        p.ret(eperm);
    }
    p.on(CLONE3, 1);
    p.ret(RET_ERRNO | libc::ENOSYS as u32);
    // threads only: a clone without CLONE_THREAD is a new process
    p.when_bits(libc::SYS_clone, 0, libc::CLONE_THREAD as u32);
    // the descriptor forms of the stat calls (the C library's fstat), not a path
    p.when_bits(libc::SYS_newfstatat, 3, AT_EMPTY_PATH);
    p.when_bits(libc::SYS_statx, 2, AT_EMPTY_PATH);
    // local socket pairs only
    p.on(libc::SYS_socketpair, 4);
    p.load(arg_low(0));
    p.op(JEQ, 0, 1, libc::AF_UNIX as u32);
    p.ret(RET_ALLOW);
    p.ret(eperm);
    for &nr in SIGNALS {
        p.on(nr, 6);
        p.load(arg_high(0));
        p.op(JEQ, 0, 3, 0);
        p.load(arg_low(0));
        p.op(JEQ, 0, 1, pid);
        p.ret(RET_ALLOW);
        p.ret(eperm);
    }
    p.ret(RET_ALLOW);
    p.0
}
