// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Starting a child: suspended, into a job of its own, inheriting exactly the handles on an explicit
//! list (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`) and exactly the environment on an allow-list.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Threading::*;

use crate::connection::{BrowserEnd, ChildEnd, Connection, Core};
use crate::error::Error;
use crate::untrusted::ProcessPolicy;
use crate::win::crash_dump::{CrashLink, CRASH_VAR};
use crate::win::handle::{owned, raw, Peer};
use crate::win::io::{Link, PipeLink};
use crate::win::{build_id, pipe};
use crate::wire::{BuildId, Wire};

/// The variable that tells a child its end of the pipe.
const PIPE_VAR: &str = "SNOWLARK_IPC_PIPE";

/// What is left of the environment by default.
const BASE_ENV: [&str; 4] = ["SystemRoot", "SystemDrive", "TEMP", "TMP"];

/// What the job limits. It always kills its process when the browser goes, and ends one that
/// crashes at once (no error reporting).
#[derive(Clone, Copy, Debug, Default)]
pub struct JobLimits {
    /// The child may start no process of its own.
    pub no_children: bool,
    /// The most the child may commit, in bytes.
    pub commit: Option<u64>,
}

/// A child process. Dropping this closes its job, which kills it.
pub struct Process {
    handle: OwnedHandle,
    _job: OwnedHandle,
    pid: u32,
    /// The minidump written when it crashed ([`Launch::crash_dumps`]).
    dump: Arc<Mutex<Option<PathBuf>>>,
}

impl Process {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The minidump the browser wrote of this child when it crashed, if it did
    /// ([`Launch::crash_dumps`]). Set before the child is ended, so it is there once
    /// [`Process::wait`] has seen the exit.
    pub fn crash_dump(&self) -> Option<PathBuf> {
        self.dump.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn handle(&self) -> &OwnedHandle {
        &self.handle
    }

    pub fn kill(&self) {
        // SAFETY: our own child's handle; a process already gone just fails.
        let _ = unsafe { TerminateProcess(raw(&self.handle), 9) };
    }

    /// Waits for the process to end, at most `timeout`: its exit code, or `None` if it still runs.
    pub fn wait(&self, timeout: Duration) -> Option<u32> {
        let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        // SAFETY: waiting on our own child's handle, and reading its exit code into a local.
        unsafe {
            if WaitForSingleObject(raw(&self.handle), ms) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0;
            GetExitCodeProcess(raw(&self.handle), &mut code).ok()?;
            Some(code)
        }
    }

    /// Runs the process at idle priority (`background`: work that should wait for everything
    /// else, such as a spare renderer warming up), or at normal priority again.
    pub fn set_background(&self, background: bool) {
        let class = if background {
            IDLE_PRIORITY_CLASS
        } else {
            NORMAL_PRIORITY_CLASS
        };
        // SAFETY: our own child's handle, opened with all access; one already gone just fails.
        let _ = unsafe { SetPriorityClass(raw(&self.handle), class) };
    }

    /// Whether the process runs at idle priority ([`Process::set_background`]).
    pub fn is_background(&self) -> bool {
        // SAFETY: our own child's handle, opened with all access.
        unsafe { GetPriorityClass(raw(&self.handle)) == IDLE_PRIORITY_CLASS.0 }
    }

    /// The handles the process holds and the memory it has committed.
    pub fn usage(&self) -> Option<Usage> {
        usage_of(raw(&self.handle))
    }
}

/// What a process holds: its handles and its private bytes (what Task Manager calls its memory),
/// and the processor time its threads have used so far, user and kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub handles: u32,
    pub private_bytes: u64,
    pub cpu: Duration,
}

/// This process's [`Usage`].
pub fn own_usage() -> Option<Usage> {
    // SAFETY: the pseudo handle of the current process, which needs no closing.
    usage_of(unsafe { GetCurrentProcess() })
}

fn usage_of(process: HANDLE) -> Option<Usage> {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    let mut handles = 0;
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: an open process handle with query access; both outputs are locals of the size given.
    unsafe {
        GetProcessHandleCount(process, &mut handles).ok()?;
        GetProcessMemoryInfo(
            process,
            std::ptr::from_mut(&mut memory).cast::<PROCESS_MEMORY_COUNTERS>(),
            memory.cb,
        )
        .ok()?;
    }
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: as above; the four outputs are locals.
    unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user).ok()? };
    Some(Usage {
        handles,
        private_bytes: memory.PrivateUsage as u64,
        cpu: filetime(kernel) + filetime(user),
    })
}

/// The processor time a thread of this process has used so far, user and kernel (`thread`: its
/// handle, such as a `JoinHandle`'s, open while this runs).
pub fn thread_cpu(thread: &impl std::os::windows::io::AsRawHandle) -> Option<Duration> {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: a thread handle the caller holds open (std opens its threads with all access); the
    // four outputs are locals.
    unsafe {
        GetThreadTimes(
            raw(thread),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
        .ok()?
    };
    Some(filetime(kernel) + filetime(user))
}

/// A `FILETIME` span (100 ns units) as a duration.
fn filetime(t: FILETIME) -> Duration {
    let ticks = (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Duration::from_nanos(ticks.saturating_mul(100))
}

/// How to start a child.
pub struct Launch {
    exe: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    limits: JobLimits,
    stderr: bool,
    crash_dumps: Option<PathBuf>,
}

impl Launch {
    pub fn new(exe: impl Into<PathBuf>) -> Launch {
        let mut launch = Launch {
            exe: exe.into(),
            args: Vec::new(),
            env: Vec::new(),
            limits: JobLimits::default(),
            stderr: true,
            crash_dumps: None,
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

    /// A crash of the child (an unhandled exception it hears, once it has called
    /// [`crate::install_crash_handler`]) is written as a minidump into `dir`, by this process, before
    /// the child ends ([`crate::win::crash_dump`]); [`Process::crash_dump`] names the file.
    pub fn crash_dumps(mut self, dir: impl Into<PathBuf>) -> Launch {
        self.crash_dumps = Some(dir.into());
        self
    }

    /// Starts the child. It runs at once; [`Spawned::connect`] says hello.
    pub fn spawn(self) -> Result<Spawned, Error> {
        let (server, client) = pipe::pair()?;
        let job = make_job(self.limits)?;
        // Two launches here cannot take each other's handles: each lists its own. Only a
        // `CreateProcess` with no list, such as `std::process::Command` starting something else in
        // the moment before `client` closes below, could; the pipe would then outlive this child.
        make_inheritable(&client)?;
        let stderr = self.stderr.then(inheritable_stderr).flatten();
        let mut list: Vec<HANDLE> = vec![raw(&client)];
        list.extend(stderr.as_ref().map(raw));
        let crash = match &self.crash_dumps {
            Some(dir) => {
                // (inheritable copies for the child, closed here when `spawn` returns, as `client`)
                let link = CrashLink::new()?;
                let copies = link.child_copies()?;
                list.push(raw(&copies.0));
                list.push(raw(&copies.1));
                Some((link, copies, dir.clone()))
            }
            None => None,
        };
        let mut command = quote(self.exe.as_os_str());
        for a in &self.args {
            command.push(" ");
            command.push(quote(a));
        }
        let mut command: Vec<u16> = command.encode_wide().chain([0]).collect();
        let mut env = self.env;
        env.push((
            PIPE_VAR.into(),
            (raw(&client).0 as usize).to_string().into(),
        ));
        if let Some((_, copies, _)) = &crash {
            env.push((CRASH_VAR.into(), CrashLink::variable(copies).into()));
        }
        let block = env_block(&env);
        let exe = HSTRING::from(self.exe.as_os_str());
        let info = create(&exe, &mut command, &block, &list, stderr.as_ref().map(raw))?;
        let thread = owned(info.hThread);
        let process = owned(info.hProcess);
        // SAFETY: the process is suspended and its handle open. Assigned before its first
        // instruction, so it has no moment outside the job; only then does it run.
        unsafe {
            let assigned = AssignProcessToJobObject(raw(&job), raw(&process));
            if let Err(e) = assigned {
                let _ = TerminateProcess(raw(&process), 1);
                return Err(e.into());
            }
            if ResumeThread(raw(&thread)) == u32::MAX {
                let _ = TerminateProcess(raw(&process), 1);
                return Err(windows::core::Error::from_thread().into());
            }
        }
        let dump = Arc::new(Mutex::new(None));
        if let Some((link, _, dir)) = crash {
            if let Err(e) = link.watch(raw(&process), info.dwProcessId, dir, dump.clone()) {
                // SAFETY: our own child's handle, suspended no more but not yet handed out.
                let _ = unsafe { TerminateProcess(raw(&process), 1) };
                return Err(e);
            }
        }
        Ok(Spawned {
            process: Arc::new(Process {
                handle: process,
                _job: job,
                pid: info.dwProcessId,
                dump,
            }),
            server,
        })
    }
}

fn make_job(limits: JobLimits) -> Result<OwnedHandle, Error> {
    // SAFETY: an unnamed job, owned at once.
    let job = owned(unsafe { CreateJobObjectW(None, PCWSTR::null())? });
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // A child that crashes ends at once, with the exception's code as its exit code: no Windows
    // Error Reporting, which otherwise holds a crashed process for many seconds (a stack overflow in
    // a renderer took more than 20 s to end without it) and may show a dialog.
    let mut flags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if limits.no_children {
        flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        info.BasicLimitInformation.ActiveProcessLimit = 1;
    }
    if let Some(commit) = limits.commit {
        flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = commit as usize;
    }
    info.BasicLimitInformation.LimitFlags = flags;
    // SAFETY: the struct is of the size given, for the class given.
    unsafe {
        SetInformationJobObject(
            raw(&job),
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            std::mem::size_of_val(&info) as u32,
        )?;
    }
    Ok(job)
}

fn make_inheritable(handle: &OwnedHandle) -> Result<(), Error> {
    // SAFETY: an open handle of ours, closed when `spawn` returns, whatever happens.
    unsafe { SetHandleInformation(raw(handle), HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)? };
    Ok(())
}

/// An inheritable copy of this process's stderr, if it has one.
fn inheritable_stderr() -> Option<OwnedHandle> {
    // SAFETY: reading the standard handle, then duplicating it into a local that is owned at once.
    unsafe {
        let stderr = GetStdHandle(STD_ERROR_HANDLE).ok()?;
        if stderr.is_invalid() || stderr.0.is_null() {
            return None;
        }
        let mut copy = HANDLE::default();
        let me = GetCurrentProcess();
        DuplicateHandle(me, stderr, me, &mut copy, 0, true, DUPLICATE_SAME_ACCESS).ok()?;
        Some(owned(copy))
    }
}

/// `CreateProcessW`, suspended, inheriting `list` and nothing else.
fn create(
    exe: &HSTRING,
    command: &mut [u16],
    env: &[u16],
    list: &[HANDLE],
    stderr: Option<HANDLE>,
) -> Result<PROCESS_INFORMATION, Error> {
    let mut size = 0;
    // SAFETY: the first call only reports the size (and fails for doing so). The buffer is 8-aligned
    // and of that size. `list` outlives CreateProcessW, the attribute list's only reader, and the
    // attribute list is deleted before it goes out of scope, on every path after it was initialized.
    unsafe {
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        let mut memory = vec![0u64; size.div_ceil(8)];
        let attributes = LPPROC_THREAD_ATTRIBUTE_LIST(memory.as_mut_ptr().cast());
        InitializeProcThreadAttributeList(Some(attributes), 1, None, &mut size)?;
        let result = (|| -> Result<PROCESS_INFORMATION, Error> {
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(list.as_ptr().cast()),
                std::mem::size_of_val(list),
                None,
                None,
            )?;
            let mut startup = STARTUPINFOEXW::default();
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.lpAttributeList = attributes;
            if let Some(stderr) = stderr {
                startup.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
                startup.StartupInfo.hStdError = stderr;
                startup.StartupInfo.hStdOutput = stderr;
            }
            let mut info = PROCESS_INFORMATION::default();
            CreateProcessW(
                PCWSTR(exe.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_SUSPENDED
                    | EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_UNICODE_ENVIRONMENT
                    | CREATE_NO_WINDOW,
                Some(env.as_ptr().cast()),
                PCWSTR::null(),
                &startup.StartupInfo,
                &mut info,
            )?;
            Ok(info)
        })();
        DeleteProcThreadAttributeList(attributes);
        result
    }
}

/// A Windows environment block: sorted `NAME=value` strings, each ended by a NUL, and a final NUL.
fn env_block(env: &[(OsString, OsString)]) -> Vec<u16> {
    let mut lines: Vec<Vec<u16>> = env
        .iter()
        .map(|(n, v)| {
            let mut line: Vec<u16> = n.encode_wide().collect();
            line.push(b'=' as u16);
            line.extend(v.encode_wide());
            line
        })
        .collect();
    lines.sort_by_key(|l| String::from_utf16_lossy(l).to_uppercase());
    let mut block = Vec::new();
    for line in lines {
        block.extend(line);
        block.push(0);
    }
    block.push(0);
    block
}

/// One argument as `CommandLineToArgvW` reads it back.
fn quote(arg: &OsStr) -> OsString {
    let wide: Vec<u16> = arg.encode_wide().collect();
    let plain = !wide.is_empty()
        && !wide
            .iter()
            .any(|c| [b' ' as u16, b'\t' as u16, b'"' as u16].contains(c));
    if plain {
        return arg.to_owned();
    }
    let mut out = vec![b'"' as u16];
    let mut slashes = 0;
    for &c in &wide {
        if c == b'\\' as u16 {
            slashes += 1;
        } else {
            if c == b'"' as u16 {
                out.extend(std::iter::repeat_n(b'\\' as u16, slashes + 1));
            }
            slashes = 0;
        }
        out.push(c);
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes));
    out.push(b'"' as u16);
    OsString::from_wide(&out)
}

/// A child that was started, with the browser's end of its pipe.
pub struct Spawned {
    process: Arc<Process>,
    server: OwnedHandle,
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

/// A child's first call: takes the pipe it was given and says hello. Fails if the browser is another
/// build or does not answer in `timeout`.
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
    let value = std::env::var(PIPE_VAR)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .ok_or(Error::Closed)?;
    // (a process this one starts must not think the handle is its own)
    std::env::remove_var(PIPE_VAR);
    // SAFETY: the value in our environment is the inherited client end of the pipe, ours alone.
    let handle = unsafe { OwnedHandle::from_raw_handle(value as RawHandle) };
    let core = Core::new(false, build, Peer::Browser, 0);
    let link = PipeLink::new(handle, &core)?;
    core.attach(Arc::new(Link(link.clone())));
    link.start();
    core.say_hello()?;
    Ok(Connection::new(core, None).established(timeout)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arguments_stay_plain() {
        assert_eq!(quote(OsStr::new("--flag=1")), "--flag=1");
    }

    #[test]
    fn spaces_quotes_and_slashes_are_escaped() {
        assert_eq!(quote(OsStr::new("a b")), r#""a b""#);
        assert_eq!(quote(OsStr::new("")), r#""""#);
        assert_eq!(quote(OsStr::new(r#"a"b"#)), r#""a\"b""#);
        assert_eq!(quote(OsStr::new(r"dir with\")), r#""dir with\\""#);
        assert_eq!(quote(OsStr::new(r#"x\"y z"#)), r#""x\\\"y z""#);
    }

    #[test]
    fn the_environment_block_is_sorted_and_double_ended() {
        let env = [("b".into(), "2".into()), ("A".into(), "1".into())];
        let block = env_block(&env);
        assert_eq!(String::from_utf16_lossy(&block), "A=1\0b=2\0\0");
    }

    /// A thread that spins counts its own processor time, and the process's includes it.
    #[test]
    fn processor_time_of_a_thread_and_the_process() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let before = own_usage().unwrap().cpu;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let spin = std::thread::spawn(move || {
            let mut x = 0u64;
            while !flag.load(Ordering::Relaxed) {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        });
        // (the spinner's own time grows past 100 ms, however busy the machine is)
        let start = std::time::Instant::now();
        let mut spun = Duration::ZERO;
        while spun < Duration::from_millis(100) && start.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(10));
            spun = thread_cpu(&spin).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        assert!(spun >= Duration::from_millis(100), "{spun:?}");
        let after = own_usage().unwrap().cpu;
        assert!(after >= before + spun, "{before:?} {after:?} {spun:?}");
        spin.join().unwrap();
    }
}
