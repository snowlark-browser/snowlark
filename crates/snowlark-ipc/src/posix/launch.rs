// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Starting a child: exactly the descriptors it is given (every other one closes on exec), exactly
//! the environment on an allow-list, and killed when the browser goes.
//!
//! The kernel's parent-death signal (`PR_SET_PDEATHSIG`) is sent when the *thread* that started the
//! child ends, not the process, so every child is started from one thread that lives as long as
//! the process ("snowlark launcher"). The signal is set between fork and exec, and the child ends
//! there if the browser is already gone. Forking a large browser costs a copy of its page tables;
//! a fork server (a small process forked early that forks the children) would avoid that.
//!
//! [`JobLimits`]: `commit` is the child's `RLIMIT_DATA` (its private writable memory, which is what
//! a commit is: address space reserved but not yet writable is not counted), set as it starts.
//! `no_children` is not enforced by the launch: a sandboxed child's system call filter refuses new
//! processes once it has locked itself down ([`Launch::sandbox`]).

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::connection::{BrowserEnd, ChildEnd, Connection, Core};
use crate::error::Error;
use crate::posix::crash_dump::{CrashLink, CRASH_FD, CRASH_VAR};
use crate::posix::handle::Peer;
use crate::posix::io::{Link, PipeLink};
use crate::posix::{build_id, pipe};
use crate::untrusted::ProcessPolicy;
use crate::wire::{BuildId, Wire};

/// The variable that tells a child the descriptor of its end of the channel.
const PIPE_VAR: &str = "SNOWLARK_IPC_PIPE";
/// The variable that tells a child the browser's process id.
const PARENT_VAR: &str = "SNOWLARK_IPC_PARENT";
/// Set in a child started with [`Launch::sandbox`].
const SANDBOX_VAR: &str = "SNOWLARK_SANDBOXED";

/// Whether the browser started this process sandboxed ([`Launch::sandbox`]): it should lock
/// itself down once it has made what needs the system.
pub fn sandboxed() -> bool {
    std::env::var_os(SANDBOX_VAR).is_some()
}

/// What is left of the environment by default: what fontconfig, temporary files and the locale
/// read.
const BASE_ENV: [&str; 9] = [
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_DIRS",
    "FONTCONFIG_FILE",
    "FONTCONFIG_PATH",
];

/// What a child is limited to. It is always killed when the browser goes.
#[derive(Clone, Copy, Debug, Default)]
pub struct JobLimits {
    /// The child may start no process of its own (not enforced on Linux yet).
    pub no_children: bool,
    /// The most private writable memory the child may hold, in bytes.
    pub commit: Option<u64>,
}

/// A child process. Dropping this kills it.
pub struct Process {
    child: Mutex<Child>,
    /// The exit code once it has been seen.
    exit: Mutex<Option<u32>>,
    pid: u32,
    /// The dump of its crash, written by its crash watch.
    dump: Arc<Mutex<Option<PathBuf>>>,
}

impl Process {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The minidump of this child's crash, once one was written ([`Launch::crash_dumps`]).
    pub fn crash_dump(&self) -> Option<PathBuf> {
        self.dump.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn kill(&self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        // (`Child::kill` signals only a process not yet reaped, so never another one with its id)
        let _ = child.kill();
    }

    /// Waits for the process to end, at most `timeout`: its exit code, or `None` if it still runs.
    /// A process a signal ended gives the signal's number: 9 for [Process::kill], as on Windows.
    pub fn wait(&self, timeout: Duration) -> Option<u32> {
        let end = Instant::now() + timeout;
        let mut pause = Duration::from_micros(100);
        loop {
            if let Some(code) = self.poll() {
                return Some(code);
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            std::thread::sleep(pause.min(left));
            pause = (pause * 2).min(Duration::from_millis(5));
        }
    }

    fn poll(&self) -> Option<u32> {
        let mut exit = self.exit.lock().unwrap_or_else(|e| e.into_inner());
        if exit.is_none() {
            let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
            if let Ok(Some(status)) = child.try_wait() {
                *exit = Some(match (status.code(), status.signal()) {
                    (Some(code), _) => code as u32,
                    (None, Some(signal)) => signal as u32,
                    _ => u32::MAX,
                });
            }
        }
        *exit
    }

    /// Lowers the priority of every thread of the process below the browser's own (`background`:
    /// work that should wait for everything else, such as a spare renderer warming up), or restores
    /// it to the browser's. Raising it again may need a privilege the browser lacks
    /// ([`Process::is_background`] then stays true); a browser already at the lowest priority has
    /// none lower to give.
    pub fn set_background(&self, background: bool) {
        let own = own_nice();
        let nice = if background { (own + 10).min(19) } else { own };
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", self.pid)) else {
            return;
        };
        for task in tasks.flatten() {
            if let Some(tid) = task
                .file_name()
                .to_str()
                .and_then(|t| t.parse::<u32>().ok())
            {
                // SAFETY: sets the priority of a thread by id; one gone meanwhile just fails.
                unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) };
            }
        }
    }

    /// Whether the process runs at a priority below the browser's ([`Process::set_background`]).
    pub fn is_background(&self) -> bool {
        stat_fields(self.pid).is_some_and(|f| f.nice > own_nice() as i64)
    }

    /// The descriptors the process holds and its private memory.
    pub fn usage(&self) -> Option<Usage> {
        usage_of(&self.pid.to_string())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        // (reaped, so no zombie is left)
        let _ = child.wait();
    }
}

/// What a process holds: its descriptors and its private bytes (its anonymous memory, resident or
/// swapped), and the processor time its threads have used so far, user and kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub handles: u32,
    pub private_bytes: u64,
    pub cpu: Duration,
}

/// This process's [`Usage`].
pub fn own_usage() -> Option<Usage> {
    usage_of("self")
}

struct Stat {
    nice: i64,
    cpu: Duration,
}

/// `/proc/<pid>/stat`'s processor times and nice value.
/// The browser's own nice value (the main thread's, which its other threads share unless set).
fn own_nice() -> i32 {
    stat_fields(std::process::id()).map_or(0, |f| f.nice as i32)
}

fn stat_fields(pid: impl std::fmt::Display) -> Option<Stat> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // (the name in parentheses may hold spaces: fields count from after its closing one, where the
    // third field of the file is the first)
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let field = |n: usize| fields.get(n - 3).and_then(|f| f.parse::<i64>().ok());
    let ticks = (field(14)? + field(15)?).max(0) as u64;
    Some(Stat {
        nice: field(19)?,
        cpu: Duration::from_nanos(ticks.saturating_mul(1_000_000_000) / clock_ticks()),
    })
}

fn clock_ticks() -> u64 {
    // SAFETY: reads a system constant.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as u64
    } else {
        100
    }
}

fn usage_of(pid: &str) -> Option<Usage> {
    let handles = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count() as u32;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    Some(Usage {
        handles,
        private_bytes: (kb("RssAnon:") + kb("VmSwap:")) * 1024,
        cpu: stat_fields(pid)?.cpu,
    })
}

/// The processor time a thread of this process has used so far, user and kernel (`thread`: one
/// still running, or joinable).
pub fn thread_cpu(thread: &impl std::os::unix::thread::JoinHandleExt) -> Option<Duration> {
    let mut clock = 0;
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a thread the caller holds a handle to, so its pthread_t is valid; both outputs are
    // locals.
    unsafe {
        if libc::pthread_getcpuclockid(thread.as_pthread_t() as libc::pthread_t, &mut clock) != 0
            || libc::clock_gettime(clock, &mut time) != 0
        {
            return None;
        }
    }
    Some(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}

/// How to start a child.
pub struct Launch {
    exe: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    limits: JobLimits,
    stderr: bool,
    /// Where a crash of the child is written as a minidump.
    dumps: Option<PathBuf>,
    /// Whether the child runs sandboxed (`Launch::sandbox`).
    sandbox: bool,
}

impl Launch {
    pub fn new(exe: impl Into<PathBuf>) -> Launch {
        let mut launch = Launch {
            exe: exe.into(),
            args: Vec::new(),
            env: Vec::new(),
            limits: JobLimits::default(),
            stderr: true,
            dumps: None,
            sandbox: false,
        };
        for name in BASE_ENV {
            launch = launch.allow_env(name);
        }
        launch
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Launch {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Passes this variable on, if it is set here.
    pub fn allow_env(mut self, name: &str) -> Launch {
        if let Some(value) = std::env::var_os(name) {
            self.env.push((name.into(), value));
        }
        self
    }

    pub fn env(mut self, name: &str, value: impl AsRef<OsStr>) -> Launch {
        self.env.push((name.into(), value.as_ref().to_owned()));
        self
    }

    pub fn limits(mut self, limits: JobLimits) -> Launch {
        self.limits = limits;
        self
    }

    /// Whether the child writes to this process's stderr (on by default).
    pub fn stderr(mut self, on: bool) -> Launch {
        self.stderr = on;
        self
    }

    /// Writes a crash of the child as a minidump into `dir` (`crash_dump`: a child that calls
    /// [`crate::install_crash_handler`] reports its crash to the browser, which writes the dump).
    pub fn crash_dumps(mut self, dir: impl Into<PathBuf>) -> Launch {
        self.dumps = Some(dir.into());
        self
    }

    /// Starts the child sandboxed: in a user and a network namespace of its own where the system
    /// allows them (`snowlark_sandbox::Namespaces`), and told to lock itself down once it has made
    /// what needs the system ([`sandboxed`], `snowlark_sandbox::lock_down`).
    pub fn sandbox(mut self, on: bool) -> Launch {
        self.sandbox = on;
        self
    }

    /// Starts the child. It runs at once; [`Spawned::connect`] says hello.
    pub fn spawn(self) -> Result<Spawned, Error> {
        let (server, client) = pipe::pair()?;
        let limits = self.limits;
        let crash = match &self.dumps {
            Some(dir) => Some((CrashLink::new()?, dir.clone())),
            None => None,
        };
        let crash_client = crash.as_ref().map(|(link, _)| link.client.as_raw_fd());
        let child = on_launcher(move || self.start(&client, crash_client))??;
        let pid = child.id();
        let dump = Arc::new(Mutex::new(None));
        if let Some((link, dir)) = crash {
            // (the child holds its own copy now)
            drop(link.client);
            CrashLink::watch(link.server, pid, dir, dump.clone())?;
        }
        if let Some(commit) = limits.commit {
            let limit = libc::rlimit {
                rlim_cur: commit,
                rlim_max: commit,
            };
            // SAFETY: sets a limit of our own child, which has not been reaped; `limit` is a local.
            let r = unsafe {
                libc::prlimit(
                    pid as libc::pid_t,
                    libc::RLIMIT_DATA,
                    &limit,
                    std::ptr::null_mut(),
                )
            };
            if r != 0 {
                let e = std::io::Error::last_os_error();
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Os(e));
            }
        }
        Ok(Spawned {
            process: Arc::new(Process {
                child: Mutex::new(child),
                exit: Mutex::new(None),
                pid,
                dump,
            }),
            server,
        })
    }

    /// On the launcher thread. In the child, between fork and exec: the parent-death signal, its
    /// end of the channel as descriptor 3, and every other descriptor above 2 marked to close on
    /// exec, so the child holds exactly its standard streams and its channel whatever this process
    /// has open, and no other launch here can see its channel.
    fn start(self, client: &OwnedFd, crash: Option<RawFd>) -> Result<Child, Error> {
        let client = client.as_raw_fd();
        let parent = std::process::id() as libc::pid_t;
        let mut command = Command::new(&self.exe);
        command
            .args(&self.args)
            .env_clear()
            .envs(self.env)
            .env(PIPE_VAR, CHILD_FD.to_string())
            .env(PARENT_VAR, parent.to_string())
            .stdin(Stdio::null());
        if crash.is_some() {
            command.env(CRASH_VAR, CRASH_FD.to_string());
        }
        if self.sandbox {
            command.env(SANDBOX_VAR, "1");
        }
        // (made before the fork: the child only writes them)
        let namespaces = (self.sandbox && snowlark_sandbox::Namespaces::available())
            .then(snowlark_sandbox::Namespaces::new);
        if self.stderr {
            let out = std::io::stderr().as_fd().try_clone_to_owned();
            command
                .stdout(out.map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
                .stderr(Stdio::inherit());
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        // SAFETY: the closure runs in the forked child before exec and calls only
        // async-signal-safe functions on plain integers; it allocates nothing.
        unsafe {
            command.pre_exec(move || {
                let fail = || Err(std::io::Error::last_os_error());
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0,
                    0,
                    0,
                ) != 0
                {
                    return fail();
                }
                // (the browser went before the request: nothing would ever kill this child)
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                // (the crash socket out of the way first: it may be at the channel's place)
                let crash = match crash {
                    Some(fd) => {
                        let high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, CRASH_FD + 1);
                        if high < 0 {
                            return fail();
                        }
                        Some(high)
                    }
                    None => None,
                };
                let moved = if client == CHILD_FD {
                    libc::fcntl(CHILD_FD, libc::F_SETFD, 0)
                } else {
                    libc::dup2(client, CHILD_FD)
                };
                if moved < 0 {
                    return fail();
                }
                if let Some(high) = crash {
                    if libc::dup2(high, CRASH_FD) < 0 {
                        return fail();
                    }
                }
                let first = if crash.is_some() {
                    CRASH_FD + 1
                } else {
                    CHILD_FD + 1
                };
                if libc::syscall(libc::SYS_close_range, first, u32::MAX, CLOSE_RANGE_CLOEXEC) != 0 {
                    // (before Linux 5.11: one at a time, up to the descriptor limit)
                    let mut limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                        return fail();
                    }
                    for fd in first..limit.rlim_cur.min(1 << 20) as libc::c_int {
                        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    }
                }
                if let Some(namespaces) = &namespaces {
                    namespaces
                        .enter()
                        .map_err(std::io::Error::from_raw_os_error)?;
                }
                Ok(())
            });
        }
        command.spawn().map_err(Error::Os)
    }
}

/// The descriptor a child finds its channel at.
const CHILD_FD: libc::c_int = 3;
/// `close_range`'s flag: mark close-on-exec instead of closing.
const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
/// Runs `job` on the thread every child is started from, and waits for it.
fn on_launcher<R: Send + 'static>(job: impl FnOnce() -> R + Send + 'static) -> Result<R, Error> {
    type Job = Box<dyn FnOnce() + Send>;
    static LAUNCHER: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    let launcher = LAUNCHER.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        // (if the thread cannot start, every send below fails, and so does every launch)
        let _ = std::thread::Builder::new()
            .name("snowlark launcher".into())
            .spawn(move || {
                for job in rx {
                    job();
                }
            });
        Mutex::new(tx)
    });
    let (tx, rx) = channel();
    let job: Job = Box::new(move || {
        let _ = tx.send(job());
    });
    launcher
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(job)
        .map_err(|_| Error::Closed)?;
    rx.recv().map_err(|_| Error::Closed)
}

/// A child that was started, with the browser's end of its channel.
pub struct Spawned {
    process: Arc<Process>,
    server: OwnedFd,
}

impl Spawned {
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Says hello and waits for the child's. Fails if it is another build, or does not answer in
    /// `timeout`. `quota` is what the child may hold in sections, in bytes.
    pub fn connect<Out: Wire, In: Wire>(
        self,
        policy: ProcessPolicy,
        quota: u64,
        timeout: Duration,
    ) -> Result<(BrowserEnd<Out, In>, Arc<Process>), Error> {
        let core = Core::new(true, build_id(), Peer::Child(self.process.clone()), quota);
        let link = PipeLink::new(self.server, &core)?;
        core.attach(Arc::new(Link(link.clone())));
        link.start();
        core.say_hello()?;
        let (connection, _) = Connection::new(core, Some(policy)).established(timeout)?;
        Ok((connection, self.process))
    }
}

/// A child's first call: asks to be killed when the browser goes, takes the channel it was given
/// and says hello. Fails if the browser is another build or does not answer in `timeout`.
pub fn connect_to_browser<Out: Wire, In: Wire>(
    timeout: Duration,
) -> Result<ChildEnd<Out, In>, Error> {
    connect_as(build_id(), timeout)
}

/// [`connect_to_browser`] under a given build id (for testing the check).
#[doc(hidden)]
pub fn connect_as<Out: Wire, In: Wire>(
    build: BuildId,
    timeout: Duration,
) -> Result<ChildEnd<Out, In>, Error> {
    let var = |name| std::env::var(name).ok().and_then(|v| v.parse::<i32>().ok());
    let (Some(fd), Some(parent)) = (var(PIPE_VAR), var(PARENT_VAR)) else {
        return Err(Error::Closed);
    };
    // (a process this one starts must not think the descriptor is its own)
    std::env::remove_var(PIPE_VAR);
    std::env::remove_var(PARENT_VAR);
    // SAFETY: asks for SIGKILL when the parent goes, then reads the parent's id: a parent gone
    // before the request shows as another parent, and this process ends.
    unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        );
        if libc::getppid() != parent {
            std::process::exit(1);
        }
    }
    // SAFETY: the descriptor named in our environment is the inherited end of the channel, ours
    // alone; it is marked close-on-exec so no process this one starts inherits it.
    let handle = unsafe {
        if fd < 3 || libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) != 0 {
            return Err(Error::Closed);
        }
        OwnedFd::from_raw_fd(fd)
    };
    let core = Core::new(false, build, Peer::Browser, 0);
    let link = PipeLink::new(handle, &core)?;
    core.attach(Arc::new(Link(link.clone())));
    link.start();
    core.say_hello()?;
    Ok(Connection::new(core, None).established(timeout)?.0)
}
