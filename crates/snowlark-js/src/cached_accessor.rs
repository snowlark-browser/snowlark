// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cached accessors: an accessor whose value V8 reads from a private property of the holder, so that
//! script gets it as a data property, inline caches and optimised code included, and the getter only
//! runs when called directly (through its property descriptor).
//!
//! The `v8` crate does not bind `FunctionTemplate::NewWithCache`, so this reaches it by its MSVC x64
//! name, as [`crate::access_check`] does for what it needs (see `docs/v8-upgrade.md`).
//!
//! **The private must be on the holder before script can read the accessor.** V8 checks that the
//! lookup of a template getter's private finds a data property, and takes the process down if not
//! (`LookupIterator::TryLookupCachedProperty`).

use std::ffi::c_void;
use std::ptr::{null, NonNull};

mod ffi {
    use std::ffi::c_void;

    // SAFETY (all declarations): these are the MSVC x64 names of the V8 functions in the static
    // library the `v8` crate links. A `Local` is one pointer, passed in a register; returned, it goes
    // through a hidden first argument, as it is not a plain C++03 aggregate.
    extern "C" {
        /// `static Isolate* v8::Isolate::GetCurrent()`.
        #[link_name = "?GetCurrent@Isolate@v8@@SAPEAV12@XZ"]
        pub fn get_current_isolate() -> *mut c_void;

        /// `static Local<FunctionTemplate> v8::FunctionTemplate::NewWithCache(Isolate*,
        /// FunctionCallback, Local<Private>, Local<Value>, Local<Signature>, int, SideEffectType)`.
        #[link_name = "?NewWithCache@FunctionTemplate@v8@@SA?AV?$Local@VFunctionTemplate@v8@@@2@PEAVIsolate@2@P6AXAEBV?$FunctionCallbackInfo@VValue@v8@@@2@@ZV?$Local@VPrivate@v8@@@2@V?$Local@VValue@v8@@@2@V?$Local@VSignature@v8@@@2@HW4SideEffectType@2@@Z"]
        pub fn new_with_cache(
            ret: *mut *const c_void,
            isolate: *mut c_void,
            callback: v8::FunctionCallback,
            cache_property: *const v8::Private,
            data: *const v8::Value,
            signature: *const v8::Signature,
            length: i32,
            side_effect_type: i32,
        ) -> *mut *const c_void;
    }
}

/// `SideEffectType::kHasSideEffect`, what the crate's own builder passes by default.
const HAS_SIDE_EFFECT: i32 = 0;

/// What V8 calls: `F`'s one possible value, with a callback's scope and arguments.
unsafe extern "C" fn trampoline<F>(info: *const v8::FunctionCallbackInfo)
where
    F: for<'s, 'i> Fn(
            &mut v8::PinScope<'s, 'i>,
            v8::FunctionCallbackArguments<'s>,
            v8::ReturnValue<'s, v8::Value>,
        ) + Copy,
{
    // SAFETY: V8 passes a valid info for the length of the call.
    let info = unsafe { &*info };
    v8::callback_scope!(unsafe scope, info);
    let args = v8::FunctionCallbackArguments::from_function_callback_info(info);
    let rv = v8::ReturnValue::from_function_callback_info(info);
    // SAFETY: `getter_template` asserted that `F` has no size, so a value of it holds no bytes to read.
    let f = unsafe { NonNull::<F>::dangling().as_ptr().read() };
    f(scope, args, rv);
}

/// A getter's template, with `cache` as the private its value is read from. `f` must have no size (a
/// function item, or a closure over them): it reaches V8 as a plain function.
pub fn getter_template<'s, F>(
    _scope: &v8::PinScope<'s, '_, ()>,
    f: F,
    cache: v8::Local<'s, v8::Private>,
    signature: Option<v8::Local<'s, v8::Signature>>,
) -> v8::Local<'s, v8::FunctionTemplate>
where
    F: for<'a, 'b> Fn(
            &mut v8::PinScope<'a, 'b>,
            v8::FunctionCallbackArguments<'a>,
            v8::ReturnValue<'a, v8::Value>,
        ) + Copy,
{
    const { assert!(std::mem::size_of::<F>() == 0) };
    let _ = f;
    let mut ret: *const c_void = null();
    // SAFETY: the scope keeps a handle scope open on this thread, with its isolate entered, so the new
    // `Local` lives as long as `'s`. An empty `Local` is a null pointer.
    unsafe {
        ffi::new_with_cache(
            &mut ret,
            ffi::get_current_isolate(),
            trampoline::<F>,
            &*cache,
            null(),
            signature.map_or(null(), |s| &*s as *const v8::Signature),
            0,
            HAS_SIDE_EFFECT,
        );
        let template = NonNull::new(ret as *mut v8::FunctionTemplate).expect("a function template");
        std::mem::transmute::<NonNull<v8::FunctionTemplate>, v8::Local<'s, v8::FunctionTemplate>>(
            template,
        )
    }
}
