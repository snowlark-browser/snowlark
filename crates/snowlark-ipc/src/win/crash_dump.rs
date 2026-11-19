// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minidumps of crashed children, written by the browser from outside, as Chromium's crashpad does
//! for a child process (`third_party/crashpad/client/crashpad_client_win.cc`,
//! `util/win/exception_handler_server.cc`).
//!
//! The child's unhandled-exception filter ([`install_crash_handler`]; crashpad's
//! `UnhandledExceptionHandler`) writes the crashing thread's id, the exception code and the address
//! of its `EXCEPTION_POINTERS` into a 16-byte section the browser made, signals an event, and sleeps
//! (60 s at most, crashpad's `kMillisecondsUntilTerminate`, then ends itself). A thread in the browser
//! (crashpad's `OnCrashDumpEvent`) writes the dump with `MiniDumpWriteDump`, which reads the
//! exception record out of the child (`ClientPointers`), then ends the child with the exception code,
//! so the exit code says what happened as before. A second crashing thread waits in the filter, as
//! in crashpad. A fast fail (`__fastfail`, Rust's `abort`) reaches no filter, so it leaves no dump
//! (crashpad needs a Windows Error Reporting module for those).
//!
//! What the child writes in the section is its word: the browser uses the code only as the exit
//! code it gives the child, and the address only for `MiniDumpWriteDump` to read from the child.

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::Debug::{
    MiniDumpWithIndirectlyReferencedMemory, MiniDumpWithThreadInfo, MiniDumpWriteDump,
    SetUnhandledExceptionFilter, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThreadId, SetEvent, Sleep, TerminateProcess,
    WaitForMultipleObjects, INFINITE,
};

use crate::error::Error;
use crate::section::Section;
use crate::win::handle::{owned, raw, RemoteHandle};

/// The variable that tells a child its crash event and section: `<event>,<section>`.
pub(crate) const CRASH_VAR: &str = "SNOWLARK_CRASH_DUMP";

/// The section: the thread id and the exception code at 0, the `EXCEPTION_POINTERS` address at 8.
const SECTION_SIZE: usize = 16;

/// How long a crashed child waits for its dump before it ends itself (crashpad's
/// `kMillisecondsUntilTerminate`).
const DUMP_WAIT_MS: u32 = 60_000;

/// How many dumps a folder keeps: the oldest go when a new one is written.
pub const KEPT_DUMPS: usize = 10;

/// The browser's end, made at launch: the event and the section.
pub(crate) struct CrashLink {
    pub(crate) event: OwnedHandle,
    pub(crate) section: Section,
}

impl CrashLink {
    pub(crate) fn new() -> Result<CrashLink, Error> {
        // SAFETY: an unnamed manual-reset event, owned at once.
        let event = owned(unsafe { CreateEventW(None, true, false, None)? });
        Ok(CrashLink {
            event,
            section: Section::create(SECTION_SIZE)?,
        })
    }

    /// Inheritable copies of the event and the section, for the child's handle list.
    pub(crate) fn child_copies(&self) -> Result<(OwnedHandle, OwnedHandle), Error> {
        Ok((
            inheritable_copy(raw(&self.event))?,
            inheritable_copy(raw(self.section.handle()))?,
        ))
    }

    /// The value of [`CRASH_VAR`] for the child that inherits `copies`.
    pub(crate) fn variable(copies: &(OwnedHandle, OwnedHandle)) -> String {
        format!(
            "{},{}",
            raw(&copies.0).0 as usize,
            raw(&copies.1).0 as usize
        )
    }

    /// Watches the child `process` (`pid`) until it ends: a crash it signals is written to a new
    /// file in `dir`, whose path goes to `dump`, and the child is ended with its exception code.
    pub(crate) fn watch(
        self,
        process: HANDLE,
        pid: u32,
        dir: PathBuf,
        dump: Arc<Mutex<Option<PathBuf>>>,
    ) -> Result<(), Error> {
        // A handle of our own to the process, so the watcher does not keep the child's job open
        // (which would keep the child alive after the browser drops it).
        let mut copy = HANDLE::default();
        // SAFETY: duplicating our own valid process handle into our own table; owned at once.
        unsafe {
            let me = GetCurrentProcess();
            DuplicateHandle(me, process, me, &mut copy, 0, false, DUPLICATE_SAME_ACCESS)?;
        }
        let process = owned(copy);
        std::thread::Builder::new()
            .name(format!("snowlark crash watch {pid}"))
            .spawn(move || {
                let handles = [raw(&self.event), raw(&process)];
                // SAFETY: two open handles of ours, for the life of the call.
                let woke = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
                if woke != WAIT_OBJECT_0 {
                    // (the child ended, or the wait failed: no crash to write)
                    return;
                }
                let head = self.section.load_u64(0).unwrap_or(0);
                let pointers = self.section.load_u64(8).unwrap_or(0);
                let (thread, code) = ((head >> 32) as u32, head as u32);
                let path = write_dump(&process, pid, &dir, thread, pointers);
                if let Some(path) = path {
                    *dump.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
                }
                // SAFETY: our own handle to the child; one already gone just fails.
                let _ = unsafe { TerminateProcess(raw(&process), code) };
            })
            .map_err(|_| Error::Refused)?;
        Ok(())
    }
}

/// An inheritable copy of our own `handle`, with the same access.
fn inheritable_copy(handle: HANDLE) -> Result<OwnedHandle, Error> {
    let mut copy = HANDLE::default();
    // SAFETY: duplicating a valid handle of ours into our own table; owned at once.
    unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(me, handle, me, &mut copy, 0, true, DUPLICATE_SAME_ACCESS)?;
    }
    Ok(owned(copy))
}

/// Writes the dump of `process` (`pid`), crashed on `thread` with its exception pointers at
/// `pointers` in its own memory, to a new file in `dir`; keeps the [`KEPT_DUMPS`] newest there.
fn write_dump(
    process: &OwnedHandle,
    pid: u32,
    dir: &Path,
    thread: u32,
    pointers: u64,
) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let path = dir.join(format!("renderer-{}-{pid}.dmp", stamp.as_millis()));
    let file = std::fs::File::create(&path).ok()?;
    let exception = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: thread,
        ExceptionPointers: pointers as usize as *mut EXCEPTION_POINTERS,
        ClientPointers: true.into(),
    };
    // SAFETY: an open handle to a live, waiting process with read access; a file opened for
    // writing; the exception struct lives on this stack for the call, and its pointer is read in
    // the child's memory (`ClientPointers`), so a wrong one makes the call fail, nothing more.
    let written = unsafe {
        MiniDumpWriteDump(
            raw(process),
            pid,
            HANDLE(file.as_raw_handle()),
            MiniDumpWithThreadInfo | MiniDumpWithIndirectlyReferencedMemory,
            Some(&exception),
            None,
            None,
        )
    };
    drop(file);
    if written.is_err() {
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

/// The child's end: the event and the section, reached from the exception filter, which has no
/// argument of its own (crashpad keeps the same in globals: `g_signal_exception`,
/// `g_crash_exception_information`). Set once, before the filter is installed; never changed.
struct ChildCrash {
    event: OwnedHandle,
    section: Section,
}

static CHILD: OnceLock<ChildCrash> = OnceLock::new();

/// In a child the browser launched with crash dumps on: installs the unhandled-exception filter
/// that has the browser write a dump. False when there is none to install (no variable).
pub fn install_crash_handler() -> bool {
    let Some(value) = std::env::var_os(CRASH_VAR) else {
        return false;
    };
    let mut parts = value.to_str().unwrap_or("").split(',');
    let (Some(Ok(event)), Some(Ok(section))) = (
        parts.next().map(str::parse::<u64>),
        parts.next().map(str::parse::<u64>),
    ) else {
        return false;
    };
    let event = RemoteHandle::from_value(event).take();
    let Ok(section) = Section::from_remote(RemoteHandle::from_value(section), SECTION_SIZE as u64)
    else {
        return false;
    };
    if CHILD.set(ChildCrash { event, section }).is_err() {
        return false;
    }
    // SAFETY: installs a filter that only reads `CHILD` (set above, never changed) and calls
    // functions that neither allocate nor take locks.
    unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    true
}

/// The filter: crashpad's `UnhandledExceptionHandler`.
unsafe extern "system" fn filter(pointers: *const EXCEPTION_POINTERS) -> i32 {
    use std::sync::atomic::{AtomicBool, Ordering};
    static CRASHED: AtomicBool = AtomicBool::new(false);
    const CONTINUE_SEARCH: i32 = 0;
    let Some(child) = CHILD.get() else {
        return CONTINUE_SEARCH;
    };
    if CRASHED.swap(true, Ordering::AcqRel) {
        // A second thread: the browser is writing the first one's dump and will end the process.
        loop {
            // SAFETY: a plain sleep.
            unsafe { Sleep(INFINITE) };
        }
    }
    // SAFETY: the system passes the exception's pointers, valid while the filter runs.
    let code = unsafe {
        pointers
            .as_ref()
            .and_then(|p| p.ExceptionRecord.as_ref())
            .map_or(0, |r| r.ExceptionCode.0 as u32)
    };
    // SAFETY: plain calls on this thread and process.
    let thread = unsafe { GetCurrentThreadId() };
    child
        .section
        .store_u64(0, (u64::from(thread) << 32) | u64::from(code));
    child.section.store_u64(8, pointers as usize as u64);
    // SAFETY: our own event handle, open for the life of the process; then a sleep, and our own
    // process ended if the browser never answers.
    unsafe {
        let _ = SetEvent(raw(&child.event));
        Sleep(DUMP_WAIT_MS);
        let _ = TerminateProcess(GetCurrentProcess(), code);
    }
    CONTINUE_SEARCH
}
