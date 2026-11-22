// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The other processes of one executable file, and a background thread's priority.

use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::ProcessStatus::EnumProcesses;
use windows::Win32::System::Threading::{
    GetCurrentThread, OpenProcess, QueryFullProcessImageNameW, SetThreadPriority,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, THREAD_MODE_BACKGROUND_BEGIN,
};

/// The ids of the processes running now, other than this one, whose executable is `image` (compared
/// as Windows compares paths: without regard to case). A process this account may not query is not
/// counted.
pub fn others_running(image: &Path) -> Vec<u32> {
    let own = std::process::id();
    let want = normalized(image);
    let mut ids = vec![0u32; 4096];
    let mut needed = 0u32;
    loop {
        let bytes = (ids.len() * 4) as u32;
        // SAFETY: `ids` has room for `bytes` bytes of process ids, and `needed` is a valid u32 the
        // call writes.
        if unsafe { EnumProcesses(ids.as_mut_ptr(), bytes, &mut needed) }.is_err() {
            return Vec::new();
        }
        if needed < bytes {
            break;
        }
        // (the list filled the buffer: there may be more)
        ids.resize(ids.len() * 2, 0);
    }
    ids.truncate(needed as usize / 4);
    ids.into_iter()
        .filter(|&pid| pid != 0 && pid != own)
        .filter(|&pid| image_of(pid).is_some_and(|p| normalized(&p) == want))
        .collect()
}

/// The executable of process `pid`, when this account may ask.
fn image_of(pid: u32) -> Option<PathBuf> {
    // SAFETY: opening a process by id for the limited query right; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units, which the call fills and sets `len` to the length
    // written; `process` is the handle opened above.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    // SAFETY: `process` was opened above and is closed once.
    let _ = unsafe { CloseHandle(process) };
    ok.then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize])))
}

fn normalized(path: &Path) -> String {
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_lowercase()
}

/// The calling thread runs in background mode from now on: below normal CPU priority, and with low
/// disk and memory priority, so its work never slows what the user is waiting for.
pub fn background_thread() {
    // SAFETY: the pseudo-handle of the calling thread needs no closing; the call only changes the
    // thread's own scheduling.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_of_the_same_executable_is_found_and_this_process_is_not() {
        let me = std::env::current_exe().unwrap();
        assert!(!others_running(&me).contains(&std::process::id()));
        // (a second process of this test binary, running the test below that waits)
        let mut child = std::process::Command::new(&me)
            .args([
                "--ignored",
                "--exact",
                "win::processes::tests::a_process_that_stays_a_while",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let seen = others_running(&me).contains(&pid);
        let _ = child.kill();
        let _ = child.wait();
        assert!(seen);
        assert!(!others_running(&me).contains(&pid));
        assert!(others_running(Path::new(r"Q:\no\such\program.exe")).is_empty());
    }

    /// The other process of the test above.
    #[test]
    #[ignore = "started by a_child_of_the_same_executable_is_found_and_this_process_is_not"]
    fn a_process_that_stays_a_while() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[test]
    fn a_background_thread_still_runs() {
        let t = std::thread::spawn(|| {
            background_thread();
            40 + 2
        });
        assert_eq!(t.join().unwrap(), 42);
    }
}
