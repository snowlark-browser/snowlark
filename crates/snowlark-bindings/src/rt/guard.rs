// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Panic guards for the generated callbacks (see `snowlark_js::guard`): V8 calls every callback through
//! a C ABI trampoline that a panic cannot unwind through, so each one the generated templates install
//! is wrapped here. A panic in the engine code it runs is recorded as the page thread's failure and
//! the running script is terminated; the page's thread re-raises the failure once it is back in Rust.
//!
//! Each `guarded*` function takes a callback (a function item, so zero-sized) and returns a
//! zero-sized closure of the same signature, which is what V8's callback mapping requires.

use snowlark_js::guard::{self, TestPanic};

/// Runs `f`, a callback's body; on a panic (or after one on this thread) the script is terminated and
/// `default` returned.
#[inline(always)]
pub fn guard<'s, 'i, R>(
    scope: &mut v8::PinScope<'s, 'i>,
    default: R,
    f: impl FnOnce(&mut v8::PinScope<'s, 'i>) -> R,
) -> R {
    match guard::catch(|| {
        if guard::test_panic(TestPanic::Callback) {
            panic!("SNOWLARK_TEST_PANIC=callback: a panic in a V8 callback");
        }
        f(scope)
    }) {
        Some(r) => r,
        None => {
            scope.terminate_execution();
            default
        }
    }
}

/// A guarded function callback (operations, attribute getters and setters, constructors).
#[inline(always)]
pub fn guarded<F>(
    f: F,
) -> impl for<'s, 'i> Fn(
    &mut v8::PinScope<'s, 'i>,
    v8::FunctionCallbackArguments<'s>,
    v8::ReturnValue<'s, v8::Value>,
) + Copy
where
    F: for<'s, 'i> Fn(
            &mut v8::PinScope<'s, 'i>,
            v8::FunctionCallbackArguments<'s>,
            v8::ReturnValue<'s, v8::Value>,
        ) + Copy,
{
    move |scope, args, rv| guard(scope, (), |scope| f(scope, args, rv))
}

/// `v8::FunctionTemplate::builder` with the callback [`guarded`].
#[inline(always)]
pub fn builder<'s, F>(f: F) -> v8::FunctionBuilder<'s, v8::FunctionTemplate>
where
    F: for<'s2, 'i> Fn(
            &mut v8::PinScope<'s2, 'i>,
            v8::FunctionCallbackArguments<'s2>,
            v8::ReturnValue<'s2, v8::Value>,
        ) + Copy,
{
    v8::FunctionTemplate::builder(guarded(f))
}

/// A guarded function callback as V8 takes it, for functions made at run time.
#[inline(always)]
pub fn callback<F>(f: F) -> v8::FunctionCallback
where
    F: for<'s2, 'i> Fn(
            &mut v8::PinScope<'s2, 'i>,
            v8::FunctionCallbackArguments<'s2>,
            v8::ReturnValue<'s2, v8::Value>,
        ) + Copy,
{
    use v8::MapFnTo;
    guarded(f).map_fn_to()
}

macro_rules! guarded_interceptor {
    ($(#[$doc:meta])* $name:ident, ($($arg:ident: $ty:ty),*), $rv:ty $(, -> $ret:ty = $default:expr)?) => {
        $(#[$doc])*
        #[inline(always)]
        pub fn $name<F>(
            f: F,
        ) -> impl for<'s, 'i> Fn(&mut v8::PinScope<'s, 'i>, $($ty,)* v8::PropertyCallbackArguments<'s>, $rv) $(-> $ret)? + Copy
        where
            F: for<'s, 'i> Fn(&mut v8::PinScope<'s, 'i>, $($ty,)* v8::PropertyCallbackArguments<'s>, $rv) $(-> $ret)? + Copy,
        {
            move |scope, $($arg,)* args, rv| guard(scope, guarded_interceptor!(@default $($default)?), |scope| f(scope, $($arg,)* args, rv))
        }
    };
    (@default $default:expr) => { $default };
    (@default) => { () };
}

guarded_interceptor!(
    /// A guarded named property getter.
    guarded_named_get, (key: v8::Local<'s, v8::Name>), v8::ReturnValue<v8::Value>,
    -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded named property query.
    guarded_named_query, (key: v8::Local<'s, v8::Name>), v8::ReturnValue<v8::Integer>,
    -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded named property setter.
    guarded_named_set, (key: v8::Local<'s, v8::Name>, value: v8::Local<'s, v8::Value>),
    v8::ReturnValue<v8::Boolean>, -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded named property definer.
    guarded_named_define, (key: v8::Local<'s, v8::Name>, desc: &v8::PropertyDescriptor),
    v8::ReturnValue<v8::Boolean>, -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded indexed property definer.
    guarded_indexed_define, (index: u32, desc: &v8::PropertyDescriptor),
    v8::ReturnValue<v8::Boolean>, -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded named property deleter.
    guarded_named_delete, (key: v8::Local<'s, v8::Name>), v8::ReturnValue<v8::Boolean>,
    -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded indexed property getter.
    guarded_indexed_get, (index: u32), v8::ReturnValue<v8::Value>,
    -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded indexed property query.
    guarded_indexed_query, (index: u32), v8::ReturnValue<v8::Integer>,
    -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded indexed property setter.
    guarded_indexed_set, (index: u32, value: v8::Local<'s, v8::Value>),
    v8::ReturnValue<v8::Boolean>, -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded indexed property deleter.
    guarded_indexed_delete, (index: u32), v8::ReturnValue<v8::Boolean>,
    -> v8::Intercepted = v8::Intercepted::kNo
);
guarded_interceptor!(
    /// A guarded property enumerator (named or indexed).
    guarded_enumerate, (), v8::ReturnValue<v8::Array>
);

/// Runs `f`, a Fast API function's body: a panic is recorded and the running script terminated (a
/// fast call cannot throw), and `R::default()` returned.
///
/// # Safety
///
/// `options` is the options pointer V8 passed to the running fast call.
#[inline(always)]
pub unsafe fn fast_guard<R: Default>(
    options: *mut v8::fast_api::FastApiCallbackOptions,
    f: impl FnOnce() -> R,
) -> R {
    // (no check of an earlier failure here: the script was terminated then, and these are reads)
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            guard::record(guard::panic_message(&*payload));
            // SAFETY: V8 passes a valid options pointer for the duration of the fast call (the
            // caller's contract), whose isolate is the one running the call.
            let isolate: &v8::Isolate = unsafe { (*options).isolate_unchecked() };
            isolate.terminate_execution();
            R::default()
        }
    }
}
