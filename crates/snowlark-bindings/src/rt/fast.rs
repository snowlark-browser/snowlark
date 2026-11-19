// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fast API support (V8 `CFunction`s): attribute getters returning primitives get a C function next to the
//! slow callback, which optimized code calls directly (no `FunctionCallbackInfo`, no handle scope).

use super::*;
use v8::fast_api::{CFunctionInfo, FastApiCallbackOptions, Int64Representation, Type};

const RECEIVER: &[v8::fast_api::CTypeInfo] =
    &[Type::V8Value.as_info(), Type::CallbackOptions.as_info()];

/// `bool getter(receiver, options)`.
pub const FAST_GETTER_BOOL: CFunctionInfo =
    CFunctionInfo::new(Type::Bool.as_info(), RECEIVER, Int64Representation::Number);
/// `int32_t getter(receiver, options)`.
pub const FAST_GETTER_I32: CFunctionInfo =
    CFunctionInfo::new(Type::Int32.as_info(), RECEIVER, Int64Representation::Number);
/// `uint32_t getter(receiver, options)`.
pub const FAST_GETTER_U32: CFunctionInfo = CFunctionInfo::new(
    Type::Uint32.as_info(),
    RECEIVER,
    Int64Representation::Number,
);
/// `double getter(receiver, options)`.
pub const FAST_GETTER_F64: CFunctionInfo = CFunctionInfo::new(
    Type::Float64.as_info(),
    RECEIVER,
    Int64Representation::Number,
);

/// The engine and the receiver's handle in a fast call.
///
/// No brand check here: V8 takes the fast path only for receivers whose map it has verified against
/// the getter's `Signature` (TurboFan inlines an API getter only when the receiver is compatible with the
/// signature's template), so the receiver is a wrapper of the interface or of a derived one. Every
/// generated template has [`WRAPPER_FIELDS`] internal fields, and every wrapper gets its handle from
/// `init_wrapper` before script can see it. Each skipped internal-field read is a C++ call (~2 ns).
///
/// # Safety
///
/// Only for the generated Fast API C functions: `recv` and `options` are the arguments V8 passed to the
/// fast path of a getter whose template has a receiver signature.
#[inline(always)]
pub unsafe fn fast_receiver<'a, D: 'static>(
    recv: &v8::Local<v8::Object>,
    options: *mut FastApiCallbackOptions<'a>,
) -> Option<(&'a D, Handle)> {
    // SAFETY: the signature check above guarantees `recv` was made from one of our templates, so it is a
    // wrapper and holds the handle `init_wrapper` stored.
    let handle = unsafe { super::wrap::read_handle(recv) };
    // SAFETY: V8 passes a valid pointer to the options for the duration of the fast call (the caller's
    // contract), and the isolate in them is the one running the call, alive for at least as long ('a).
    let isolate: &'a v8::Isolate = unsafe { (*options).isolate_unchecked() };
    let st = isolate.get_slot::<Rc<State>>()?;
    Some((st.engine::<D>(), handle))
}
