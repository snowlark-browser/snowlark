// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! V8 embedding for Snowlark: one process-wide platform, one isolate per page thread.
//!
//! This crate owns everything that touches the `v8` API. The DOM, style and layout crates never
//! see V8 types; the bindings layer (generated from Web IDL) is the only bridge between them.

pub mod access_check;
pub mod cached_accessor;
pub mod code_cache;
pub mod cpu_profile;
#[cfg(all(windows, target_env = "msvc", target_feature = "crt-static"))]
pub mod crt_heap;
pub mod ecma_regexp;
pub mod guard;
pub mod hooks;
pub mod internal_field;
pub mod module_graph;
pub mod native_profile;
pub mod object_clone;
pub mod object_field;
pub mod platform;
pub mod raw_callback;
pub mod rust_heap;
pub mod script_check;
pub mod streaming;
pub mod wrapper_ref;

use std::sync::Once;

static INIT: Once = Once::new();

/// V8's version.
pub fn v8_version() -> &'static str {
    v8::V8::get_version()
}

/// The browser's language (BCP 47): the first of `navigator.languages`
/// (`snowlark_render::svg_geometry::USER_LANGUAGES`, which `snowlark-page`'s tests hold equal to this).
pub const BROWSER_LANGUAGE: &str = "en-US";

/// Initializes the V8 platform once per process. Safe to call from any thread, any number of times.
pub fn init() {
    INIT.call_once(|| {
        // ICU's default locale is the browser's language, not the system's, before any isolate
        // exists (Chrome sets it from its UI language): `Intl`'s default locale and the time zone
        // names of `Date`'s strings follow `navigator.language`.
        v8::icu::set_default_locale(BROWSER_LANGUAGE);
        // Diagnostics: V8's own flags (`--trace-gc`, `--trace-deopt`...), for profiling sessions.
        if let Ok(flags) = std::env::var("SNOWLARK_V8_FLAGS") {
            v8::V8::set_flags_from_string(&flags);
        }
        // V8's random numbers (`Math.random`, its hash seeds and address randomization) seeded
        // from the system's generator through the `getrandom` call, which a sandboxed renderer
        // makes: it cannot open `/dev/urandom`, and V8 would seed from the clock instead.
        #[cfg(target_os = "linux")]
        v8::V8::set_entropy_source(entropy);
        // (each isolate's foreground tasks go to its event loop's queue: platform)
        let platform = platform::new_platform().make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

/// Reads the system's time zone now: ICU's default zone (which `Date` and `Intl` use) and the C
/// library's. Each is read once per process, the first time it is needed, from files (the zone's
/// link and its rules) that a sandboxed renderer can no longer open once it is locked down.
/// Returns ICU's zone id.
pub fn settle_time_zone() -> String {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            // (POSIX; the `libc` crate does not declare it)
            fn tzset();
        }
        // SAFETY: tzset takes nothing and reads only the TZ variable and the zone file into the C
        // library's own state.
        unsafe { tzset() };
    }
    v8::icu::get_default_time_zone()
}
/// Fills `buf` from the kernel's random number generator (V8's entropy source).
#[cfg(target_os = "linux")]
fn entropy(buf: &mut [u8]) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        let rest = &mut buf[filled..];
        // SAFETY: writes at most `rest.len()` bytes into `rest`.
        let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        filled += n as usize;
    }
    true
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn entropy_fills_the_buffer() {
    let mut a = [0u8; 64];
    let mut b = [0u8; 64];
    assert!(entropy(&mut a) && entropy(&mut b));
    assert_ne!(a, b);
    assert!(a.iter().any(|&x| x != 0));
}
/// Writes the execution profile of an instrumented V8 (a PGO training build, feature `pgo-train`;
/// `docs/v8-speed.md`), and does nothing otherwise. Call it just before the process exits: the
/// process ends through `ExitProcess`, which skips the C runtime's exit handlers that would write it.
pub fn write_pgo_profile() {
    #[cfg(feature = "pgo-train")]
    {
        extern "C" {
            fn __llvm_profile_write_file() -> i32;
        }
        // SAFETY: the profile runtime is linked in with the instrumented V8 library (a build without
        // it fails to link, not to run); writing the counters takes no arguments and is called once,
        // as the process exits.
        unsafe {
            __llvm_profile_write_file();
        }
    }
}

/// V8's tag for code caches: its version and the flags that change compiled code. A cache made
/// under another tag is not used.
pub fn code_cache_version_tag() -> u32 {
    init();
    v8::script_compiler::cached_data_version_tag()
}

/// A script runtime: an isolate with one context. One per page (per tab thread).
pub struct Runtime {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
}

impl Runtime {
    pub fn new() -> Runtime {
        init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let context = {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, v8::ContextOptions::default());
            v8::Global::new(scope, context)
        };
        Runtime { isolate, context }
    }

    /// Compiles and runs `source`, returning the completion value as a string (or the exception's).
    pub fn eval(&mut self, source: &str, name: &str) -> Result<String, String> {
        v8::scope_with_context!(scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);
        let code = v8::String::new(tc, source).ok_or("source too long")?;
        let origin = {
            let name = v8::String::new(tc, name).ok_or("name too long")?;
            v8::ScriptOrigin::new(
                tc,
                name.into(),
                0,
                0,
                false,
                0,
                None,
                false,
                false,
                false,
                None,
            )
        };
        let result = v8::Script::compile(tc, code, Some(&origin)).and_then(|s| s.run(tc));
        match result {
            Some(value) => Ok(value.to_rust_string_lossy(tc)),
            None => {
                let message = tc
                    .exception()
                    .map(|e| e.to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "unknown error".into());
                Err(message)
            }
        }
    }
}

impl Runtime {
    /// Whether `source` parses as a classic script (ECMAScript `ParseText(sourceText, Script)`):
    /// compiled, never run.
    pub fn parses(&mut self, source: &str) -> bool {
        v8::scope_with_context!(scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);
        let Some(code) = v8::String::new(tc, source) else {
            return false;
        };
        v8::Script::compile(tc, code, None).is_some()
    }

    /// Tells V8 to give memory back (after a large one-off compile).
    pub fn low_memory(&mut self) {
        self.isolate.low_memory_notification();
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_arithmetic() {
        let mut rt = Runtime::new();
        assert_eq!(rt.eval("6 * 7", "test.js").unwrap(), "42");
    }

    #[test]
    fn reports_exceptions() {
        let mut rt = Runtime::new();
        let err = rt
            .eval("throw new TypeError('nope')", "test.js")
            .unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn counts_garbage_collections() {
        init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        hooks::count_gcs(&mut isolate);
        assert_eq!(hooks::gc_count(&isolate), Some(0));
        isolate.low_memory_notification();
        assert!(hooks::gc_count(&isolate) > Some(0));
    }

    #[test]
    fn keeps_state_between_evals() {
        let mut rt = Runtime::new();
        rt.eval("globalThis.x = 40", "a.js").unwrap();
        assert_eq!(rt.eval("x + 2", "b.js").unwrap(), "42");
    }
}
