// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Panics in engine code that V8 called.
//!
//! V8 calls the embedder through C ABI trampolines (`extern "C"`: function callbacks, interceptors,
//! Fast API functions, host hooks). A Rust panic cannot unwind through them: the process aborts with
//! "panic in a function that cannot unwind", taking every tab with it. So each callback that runs
//! engine code goes through [`catch`]: a panic is stopped on the Rust side of the boundary, recorded
//! as the thread's failure, and the caller terminates the running script (`TerminateExecution`), so
//! no more script runs on a page whose state is no longer trusted. Once the page's thread is back in
//! its own Rust code, [`resume`] re-raises the failure there, where unwinding is possible and the
//! tab's own panic handler turns it into a crashed tab.
//!
//! The failure is a fact of the thread (one page per thread), so it is kept in a thread-local; while
//! it is set, [`catch`] runs nothing more.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

thread_local! {
    static FAILURE: RefCell<Option<String>> = const { RefCell::new(None) };
    /// `FAILURE.is_some()`, read on every callback.
    static FAILED: Cell<bool> = const { Cell::new(false) };
}

/// The text of a panic payload (`panic!` with a message gives a `String` or a `&str`).
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "panic".to_owned())
}

/// Whether engine code on this thread panicked under V8 (and the failure is not yet taken).
#[inline]
pub fn failed() -> bool {
    FAILED.with(|f| f.get())
}

/// Takes this thread's recorded failure, if any.
pub fn take() -> Option<String> {
    FAILED.with(|f| f.set(false));
    FAILURE.with(|f| f.borrow_mut().take())
}

/// Records `message` as this thread's failure (the first one is kept: later panics are consequences).
pub fn record(message: String) {
    FAILURE.with(|f| {
        let mut f = f.borrow_mut();
        if f.is_none() {
            *f = Some(message);
        }
    });
    FAILED.with(|f| f.set(true));
}

/// Re-raises this thread's recorded failure as a panic, from Rust code that can unwind. The panic
/// hook already ran for the original panic, so this resumes without running it again.
pub fn resume() {
    if let Some(message) = take() {
        std::panic::resume_unwind(Box::new(message));
    }
}

/// Runs `f`, engine code that V8 called. `None` when it panicked (the panic is recorded) or when the
/// thread had already failed (then `f` does not run): the caller terminates the running script.
#[inline]
pub fn catch<R>(f: impl FnOnce() -> R) -> Option<R> {
    if failed() {
        return None;
    }
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(payload) => {
            record(panic_message(&*payload));
            None
        }
    }
}

/// The places `SNOWLARK_TEST_PANIC` can name (a testing switch: the value names the place, each place
/// panics once per process, the first time it is reached).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestPanic {
    /// The tab's thread, outside any engine call.
    Tab,
    /// Inside a V8 callback that runs engine code.
    Callback,
}

/// Whether the `SNOWLARK_TEST_PANIC` switch asks for a panic at `place` now (true once per process).
pub fn test_panic(place: TestPanic) -> bool {
    static SWITCH: OnceLock<Option<TestPanic>> = OnceLock::new();
    static FIRED: AtomicBool = AtomicBool::new(false);
    let wanted =
        *SWITCH.get_or_init(|| parse_test_panic(&std::env::var("SNOWLARK_TEST_PANIC").ok()?));
    wanted == Some(place) && !FIRED.swap(true, Ordering::Relaxed)
}

/// The place a `SNOWLARK_TEST_PANIC` value names (`restyle` is the style crate's own).
pub fn parse_test_panic(value: &str) -> Option<TestPanic> {
    match value.trim() {
        "tab" => Some(TestPanic::Tab),
        "callback" => Some(TestPanic::Callback),
        _ => None,
    }
}

/// The hard failures `SNOWLARK_TEST_CRASH` can cause, the ones `catch_unwind` cannot see: each ends the
/// process (or, for `Hang`, stops the thread for good), so the code that watches a tab's process has
/// something real to classify. `SNOWLARK_TEST_PANIC` is for the failures a panic hook can report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestCrash {
    /// Unbounded recursion: a stack overflow (exit code `0xC00000FD` on Windows).
    Stack,
    /// A write through a null pointer: an access violation (`0xC0000005`).
    Av,
    /// `std::process::abort`: a fast fail (`0xC0000409` on Windows).
    Abort,
    /// The thread sleeps forever and never returns to its loop.
    Hang,
}

/// The kind a `SNOWLARK_TEST_CRASH` value names.
pub fn parse_test_crash(value: &str) -> Option<TestCrash> {
    match value.trim() {
        "stack" => Some(TestCrash::Stack),
        "av" => Some(TestCrash::Av),
        "abort" => Some(TestCrash::Abort),
        "hang" => Some(TestCrash::Hang),
        _ => None,
    }
}

/// Whether the `SNOWLARK_TEST_CRASH` switch is set to a valid kind and has not fired yet; true once per
/// process, the first time the caller's place is reached. The caller then calls [`crash`].
pub fn test_crash() -> Option<TestCrash> {
    static SWITCH: OnceLock<Option<TestCrash>> = OnceLock::new();
    static FIRED: AtomicBool = AtomicBool::new(false);
    let wanted =
        (*SWITCH.get_or_init(|| parse_test_crash(&std::env::var("SNOWLARK_TEST_CRASH").ok()?)))?;
    (!FIRED.swap(true, Ordering::Relaxed)).then_some(wanted)
}

/// Causes the failure `kind` names, on the calling thread. Returns never.
pub fn crash(kind: TestCrash) -> ! {
    match kind {
        TestCrash::Stack => {
            // (a frame the optimizer can neither remove nor turn into a loop)
            #[inline(never)]
            #[allow(unconditional_recursion)]
            fn recurse(depth: u64) -> u64 {
                let pad = [depth as u8; 256];
                let below = recurse(std::hint::black_box(depth) + 1);
                below + u64::from(std::hint::black_box(pad)[3])
            }
            std::process::exit(recurse(0) as i32);
        }
        TestCrash::Av => {
            // SAFETY: this is the point: an invalid write, which the process cannot survive.
            unsafe {
                std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1)
            };
            std::process::abort();
        }
        TestCrash::Abort => std::process::abort(),
        TestCrash::Hang => loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_is_caught_recorded_and_resumed() {
        // (a thread of its own: the failure is per thread)
        std::thread::spawn(|| {
            assert_eq!(catch(|| 7), Some(7));
            assert!(!failed());
            assert_eq!(catch(|| -> i32 { panic!("boom") }), None);
            assert!(failed());
            // While failed nothing more runs, and later panics don't replace the first message.
            let mut ran = false;
            assert_eq!(catch(|| ran = true), None);
            assert!(!ran);
            record("later".into());
            let r = catch_unwind(resume);
            let payload = r.unwrap_err();
            assert_eq!(panic_message(&*payload), "boom");
            assert!(!failed());
            // Nothing to resume: a no-op.
            resume();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn test_panic_switch_values() {
        assert_eq!(parse_test_panic("tab"), Some(TestPanic::Tab));
        assert_eq!(parse_test_panic(" callback "), Some(TestPanic::Callback));
        assert_eq!(parse_test_panic("restyle"), None);
        assert_eq!(parse_test_panic(""), None);
    }

    #[test]
    fn test_crash_switch_values() {
        assert_eq!(parse_test_crash("stack"), Some(TestCrash::Stack));
        assert_eq!(parse_test_crash(" av "), Some(TestCrash::Av));
        assert_eq!(parse_test_crash("abort"), Some(TestCrash::Abort));
        assert_eq!(parse_test_crash("hang"), Some(TestCrash::Hang));
        assert_eq!(parse_test_crash("tab"), None);
        assert_eq!(parse_test_crash(""), None);
    }

    /// Runs this test binary again, running only `crash_child` with the kind named, and returns the
    /// child (the crash ends a process, so the test cannot cause it in itself).
    fn child(kind: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "guard::tests::crash_child", "--nocapture"])
            .env("SNOWLARK_GUARD_CRASH_CHILD", kind)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Not a test on its own: the body the parent tests run in a child process.
    #[test]
    fn crash_child() {
        if let Some(kind) = std::env::var("SNOWLARK_GUARD_CRASH_CHILD")
            .ok()
            .and_then(|v| parse_test_crash(&v))
        {
            crash(kind);
        }
    }

    #[cfg(windows)]
    #[test]
    fn each_crash_kind_ends_the_process_with_its_own_status() {
        for (kind, code) in [
            ("stack", 0xC000_00FD_u32),
            ("av", 0xC000_0005),
            ("abort", 0xC000_0409),
        ] {
            let status = child(kind).wait().unwrap();
            assert_eq!(status.code().map(|c| c as u32), Some(code), "{kind}");
        }
    }

    #[test]
    fn hang_never_returns() {
        let mut c = child("hang");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert!(c.try_wait().unwrap().is_none(), "the hang ended");
        c.kill().unwrap();
        c.wait().unwrap();
    }
}
