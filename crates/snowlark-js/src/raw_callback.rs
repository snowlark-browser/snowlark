// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Function callbacks that read V8's arguments in place, as Blink's generated bindings do.
//!
//! The `v8` crate's callback trampoline (`MapFnFrom<F> for FunctionCallback`) makes a `CallbackScope`
//! (an FFI call for the isolate, another for the crate's annex), then reads the receiver and the
//! return slot through further FFI shims. V8's own `FunctionCallbackInfo` is a view of a stack array
//! (`v8-function-callback.h`): `GetIsolate()`, `This()` and `GetReturnValue()` are inline reads of
//! `values_` at fixed indices, which is what Blink's callbacks compile to (`bind_gen/interface.py`,
//! `bind_callback_local_vars`: `info.This()`, `info.GetIsolate()`, no `HandleScope`). [`Info`] reads
//! them the same way; a callback that needs a scope (to allocate, throw or call script) makes one with
//! [`Info::with_scope`], which is what the crate's trampoline does.
//!
//! The isolate's embedder data slots are read inline too (V8's `Isolate::GetData` is
//! `Internals::GetEmbedderData`, a load at [`ISOLATE_EMBEDDER_DATA_OFFSET`]): that is how Blink finds
//! its per-isolate data (`V8PerIsolateData::From`). [`verify_embedder_data`] checks the offset against
//! the crate's FFI on every isolate that uses it, so a changed layout stops there.
//!
//! Each index and the offset are layouts of the V8 this crate links; `docs/v8-upgrade.md`
//! lists them, and `raw_callback::tests` checks every read against the crate's accessors.

use std::ffi::c_void;
use std::ptr::NonNull;

/// `v8::internal::Address`.
pub type Address = usize;

/// Indices of `FunctionCallbackInfo::values_` (V8 14, `v8-function-callback.h`, the enum under "Frame
/// block"; x64 has no embedded constant pool, so `I::kFrameCPSlotCount` is 0).
pub mod index {
    /// `kArgcIndex`: the argument count, a raw integer.
    pub const ARGC: usize = 0;
    /// `kIsolateIndex` (`kFirstApiArgumentIndex`, after argc, the exit frame's SP, type, FP and PC).
    pub const ISOLATE: usize = 5;
    /// `kReturnValueIndex`.
    pub const RETURN_VALUE: usize = 6;
    /// `kReceiverIndex` (after the context and the target).
    pub const RECEIVER: usize = 9;
    /// `kFirstJSArgumentIndex`.
    pub const FIRST_ARGUMENT: usize = 10;
}

/// The range of a Smi with pointer compression (`kSmiValueSize` 31): `kSmiMinValue..=kSmiMaxValue`.
pub const SMI_MIN: i32 = -(1 << 30);
pub const SMI_MAX: i32 = (1 << 30) - 1;

/// `Internals::kIsolateEmbedderDataOffset` of this build (pointer compression, no sandbox, x64):
/// where `Isolate::GetData(slot)` reads, from the isolate's address.
pub const ISOLATE_EMBEDDER_DATA_OFFSET: usize = 688;

/// The `v8` crate keeps its annex in the isolate's first data slot (`Isolate::ANNEX_SLOT`) and numbers
/// the embedder's slots after it (`INTERNAL_DATA_SLOT_COUNT`).
const CRATE_DATA_SLOTS: usize = 1;

/// The arguments of one function callback, read in place.
#[derive(Clone, Copy)]
pub struct Info<'a> {
    info: &'a v8::FunctionCallbackInfo,
    values: *mut Address,
}

impl<'a> Info<'a> {
    /// # Safety
    ///
    /// `info` is the pointer V8 passed to a function callback, and the result does not outlive it.
    #[inline(always)]
    pub unsafe fn new(info: *const v8::FunctionCallbackInfo) -> Info<'a> {
        Info {
            // SAFETY: as the caller promised.
            info: unsafe { &*info },
            // (`FunctionCallbackInfo`'s only member is `values_`)
            values: info as *mut Address,
        }
    }

    /// The crate's view of the same arguments.
    #[inline(always)]
    pub fn raw(&self) -> &'a v8::FunctionCallbackInfo {
        self.info
    }

    #[inline(always)]
    fn value(&self, i: usize) -> Address {
        // SAFETY: `values` is V8's array of this call, which has every index of [`index`].
        unsafe { *self.values.add(i) }
    }

    /// `GetIsolate()`, as the address the isolate's data slots are read from.
    #[inline(always)]
    pub fn isolate(&self) -> *mut c_void {
        self.value(index::ISOLATE) as *mut c_void
    }

    /// `Length()`.
    #[inline(always)]
    pub fn length(&self) -> i32 {
        self.value(index::ARGC) as i32
    }

    /// `This()`: a local handle whose slot is the receiver's entry in V8's array.
    #[inline(always)]
    pub fn this(&self) -> v8::Local<'a, v8::Object> {
        // SAFETY: a `Local` is a pointer to a slot holding the object's address (`Local::FromSlot`), and
        // the receiver's slot lives as long as the call.
        unsafe {
            let slot = NonNull::new_unchecked(self.values.add(index::RECEIVER) as *mut v8::Object);
            std::mem::transmute::<NonNull<v8::Object>, v8::Local<'a, v8::Object>>(slot)
        }
    }

    /// `info[i]` (`FunctionCallbackInfo::operator[]`): a local handle whose slot is the argument's entry
    /// in V8's array, or `undefined` past the last argument, from `undefined`, the slot V8's own
    /// `Undefined(isolate)` points at (the isolate's root table, which never moves).
    #[inline(always)]
    pub fn arg(&self, i: usize, undefined: Slot) -> v8::Local<'a, v8::Value> {
        let slot = if i < self.length() as usize {
            // SAFETY: V8's array of this call holds `length()` arguments after `FIRST_ARGUMENT`.
            unsafe { self.values.add(index::FIRST_ARGUMENT + i) as Slot }
        } else {
            undefined
        };
        // SAFETY: both slots outlive the call and hold a value's address.
        unsafe { local_at(slot) }
    }

    /// `isolate->GetData(slot)` for one of the embedder's slots (numbered as `v8::Isolate::get_data`
    /// numbers them), inline. Valid once [`verify_embedder_data`] passed for this isolate.
    #[inline(always)]
    pub fn embedder_data(&self, slot: u32) -> *mut c_void {
        let at = ISOLATE_EMBEDDER_DATA_OFFSET + (CRATE_DATA_SLOTS + slot as usize) * 8;
        // SAFETY: the isolate's data slots are at that offset of its `IsolateData`, which
        // `verify_embedder_data` checked against V8's own `GetData`.
        unsafe { *((self.isolate() as *const u8).add(at) as *const *mut c_void) }
    }

    /// Stores an object's (or oddball's) address as the call's result, with no handle, as
    /// `ReturnValue::Set(const Global&)` and `SetNull()` do.
    ///
    /// # Safety
    ///
    /// `value` is the address of a live V8 heap object of this isolate (read from a handle's slot
    /// while that handle keeps it, or an immovable root such as `null`). The return slot is a root
    /// of V8's, so the object stays alive from here on.
    #[inline(always)]
    pub unsafe fn set_return_address(&self, value: Address) {
        // SAFETY: the return slot is in V8's array of this call; see above for the value.
        unsafe { *self.values.add(index::RETURN_VALUE) = value }
    }

    /// Stores an integer as the result, as `ReturnValue::Set(int32_t)` does: inline when it fits a Smi
    /// (31 bits with pointer compression), else as a number made in a scope.
    #[inline(always)]
    pub fn set_return_i32(&self, value: i32) {
        if (SMI_MIN..=SMI_MAX).contains(&value) {
            self.set_return_smi(value);
        } else {
            self.with_scope(|_, _, mut rv| rv.set_int32(value));
        }
    }

    /// [`Self::set_return_i32`] for an unsigned integer (`ReturnValue::Set(uint32_t)`).
    #[inline(always)]
    pub fn set_return_u32(&self, value: u32) {
        if value <= SMI_MAX as u32 {
            self.set_return_smi(value as i32);
        } else {
            self.with_scope(|_, _, mut rv| rv.set_uint32(value));
        }
    }

    /// The Smi store of `ReturnValue::SetInternal` with pointer compression: the 32-bit tagged value
    /// (`value << 1`) in the slot's low half, the upper half left as it is (V8 reads a Smi from the low
    /// half alone; with static roots, as here, its own store leaves the cage base there).
    #[inline(always)]
    fn set_return_smi(&self, value: i32) {
        debug_assert!((SMI_MIN..=SMI_MAX).contains(&value));
        // SAFETY: the return slot is in V8's array of this call, 8-byte aligned (x64 is little-endian,
        // so its low half comes first); a Smi is not a heap pointer.
        unsafe {
            *(self.values.add(index::RETURN_VALUE) as *mut u32) = (value as u32) << 1;
        }
    }

    /// Runs `f` with a scope and the crate's views of the arguments: what the crate's trampoline gives
    /// every callback, for the paths that allocate, throw or call script.
    #[inline]
    pub fn with_scope<R>(
        &self,
        f: impl for<'s, 'i> FnOnce(
            &mut v8::PinScope<'s, 'i>,
            v8::FunctionCallbackArguments<'s>,
            v8::ReturnValue<'s, v8::Value>,
        ) -> R,
    ) -> R {
        // SAFETY: the arguments of a function callback that is running (as the crate's trampoline).
        let scope = std::pin::pin!(unsafe { v8::CallbackScope::new(self.info) });
        let mut scope = scope.init();
        let args = v8::FunctionCallbackArguments::from_function_callback_info(self.info);
        let rv = v8::ReturnValue::from_function_callback_info(self.info);
        f(&mut scope, args, rv)
    }
}

/// Where a local handle's value is: a slot holding the value's address (V8's `Local::FromSlot`).
pub type Slot = *const Address;

/// The slot `local` points at.
#[inline(always)]
pub fn slot_of<T>(local: v8::Local<'_, T>) -> Slot {
    // SAFETY: a `Local` is a non-null pointer to its slot.
    unsafe { std::mem::transmute::<v8::Local<'_, T>, Slot>(local) }
}

/// A local handle on `slot`.
///
/// # Safety
///
/// `slot` holds the address of a live value of type `T` for as long as `'a`, as a handle scope's or a
/// callback's slots and the isolate's root table do.
#[inline(always)]
pub unsafe fn local_at<'a, T>(slot: Slot) -> v8::Local<'a, T> {
    // SAFETY: as the caller promised; a `Local` is that pointer.
    unsafe { std::mem::transmute::<Slot, v8::Local<'a, T>>(slot) }
}

/// The address a local handle holds (`*reinterpret_cast<Address*>(*local)`).
#[inline(always)]
pub fn address_of<T>(local: v8::Local<'_, T>) -> Address {
    // SAFETY: a `Local` is a non-null pointer to a slot holding the address.
    unsafe { *(std::mem::transmute::<v8::Local<'_, T>, *const Address>(local)) }
}

/// The `extern "C"` function V8 calls for `f`, a function item or a closure that captures nothing
/// (so zero-sized: the trampoline makes it from nothing, as the crate's `UnitType` does).
///
/// `f` runs inside no scope: it reads what it needs from [`Info`] and makes a scope only on a path
/// that needs one. It must not panic: V8 cannot unwind, so a caller wraps engine code in
/// `crate::guard::catch` (the bindings' `rt::light` does).
pub fn callback<F>(f: F) -> v8::FunctionCallback
where
    F: for<'a> Fn(Info<'a>) + Copy,
{
    const {
        assert!(
            std::mem::size_of::<F>() == 0,
            "a raw callback captures nothing"
        );
    }
    let _ = f;
    trampoline::<F>
}

unsafe extern "C" fn trampoline<F>(info: *const v8::FunctionCallbackInfo)
where
    F: for<'a> Fn(Info<'a>) + Copy,
{
    // SAFETY: `F` is zero-sized (checked in `callback`, the only way here), so every value of it is
    // the same one.
    let f: F = unsafe { std::ptr::NonNull::<F>::dangling().as_ptr().read() };
    // SAFETY: V8 passes the arguments of this call.
    f(unsafe { Info::new(info) })
}

/// Checks [`ISOLATE_EMBEDDER_DATA_OFFSET`] on `isolate`: writes a probe to the embedder's data `slot`
/// through the crate (V8's `SetData`), reads it back inline, and restores the slot. Panics on a
/// mismatch: the inline reads would read something else.
pub fn verify_embedder_data(isolate: &mut v8::Isolate, slot: u32) {
    let old = isolate.get_data(slot);
    let probe = 0x5A5A_0000_0000_1234usize as *mut c_void;
    isolate.set_data(slot, probe);
    let read = read_embedder_data(isolate, slot);
    isolate.set_data(slot, old);
    assert!(
        read == probe,
        "V8's isolate layout changed: ISOLATE_EMBEDDER_DATA_OFFSET is wrong for this library \
         (see docs/v8-upgrade.md)"
    );
}

fn isolate_address(isolate: &v8::Isolate) -> *const u8 {
    // SAFETY: the raw isolate pointer is a pointer, whatever the crate's newtype around it.
    unsafe {
        std::mem::transmute::<v8::UnsafeRawIsolatePtr, *const u8>(isolate.as_raw_isolate_ptr())
    }
}

fn read_embedder_data(isolate: &v8::Isolate, slot: u32) -> *mut c_void {
    let at = ISOLATE_EMBEDDER_DATA_OFFSET + (CRATE_DATA_SLOTS + slot as usize) * 8;
    // SAFETY: `IsolateData` is far larger than its first few hundred bytes, where the slots are.
    unsafe { *(isolate_address(isolate).add(at) as *const *mut c_void) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Finds where `Isolate::GetData` reads by writing two probes through the crate and looking for
    /// them in the isolate's first 4 KiB.
    fn probe_offset(isolate: &mut v8::Isolate) -> usize {
        let base = isolate_address(isolate);
        let find = |isolate: &mut v8::Isolate, v: usize| -> Vec<usize> {
            isolate.set_data(0, v as *mut c_void);
            (0..4096 / 8)
                // SAFETY: inside `IsolateData` (see `read_embedder_data`).
                .filter(|i| unsafe { *(base.add(i * 8) as *const usize) } == v)
                .map(|i| i * 8)
                .collect()
        };
        let a = find(isolate, 0x1111_2222_3333_4444);
        let b = find(isolate, 0x5555_6666_7777_8888);
        isolate.set_data(0, std::ptr::null_mut());
        let both: Vec<usize> = a.into_iter().filter(|o| b.contains(o)).collect();
        assert_eq!(both.len(), 1, "one place holds the data slot");
        both[0] - CRATE_DATA_SLOTS * 8
    }

    /// The offset of the isolate's data slots is the one the inline reads use.
    #[test]
    fn embedder_data_offset_is_this_librarys() {
        crate::init();
        let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
        assert_eq!(probe_offset(isolate), ISOLATE_EMBEDDER_DATA_OFFSET);
        verify_embedder_data(isolate, 0);
    }

    /// What [`Info`] read in one call, beside what the crate's accessors give.
    #[derive(Debug, Default, PartialEq)]
    struct Seen {
        isolate: usize,
        length: i32,
        this_address: Address,
        data: usize,
        /// The addresses of `info[0]` to `info[length + 1]` (two past the last: `undefined`).
        args: Vec<Address>,
    }

    thread_local! {
        static UNDEFINED: std::cell::Cell<Slot> = const { std::cell::Cell::new(std::ptr::null()) };
    }

    thread_local! {
        static INLINE: RefCell<Option<Seen>> = const { RefCell::new(None) };
        static CRATE: RefCell<Option<Seen>> = const { RefCell::new(None) };
    }

    fn record(info: Info<'_>) {
        INLINE.with(|s| {
            *s.borrow_mut() = Some(Seen {
                isolate: info.isolate() as usize,
                length: info.length(),
                this_address: address_of(info.this()),
                data: info.embedder_data(1) as usize,
                args: (0..info.length() as usize + 2)
                    .map(|i| address_of(info.arg(i, UNDEFINED.with(|u| u.get()))))
                    .collect(),
            })
        });
        info.with_scope(|scope, args, mut rv| {
            let isolate: &v8::Isolate = scope;
            CRATE.with(|s| {
                *s.borrow_mut() = Some(Seen {
                    isolate: isolate_address(isolate) as usize,
                    length: args.length(),
                    this_address: address_of(args.this()),
                    data: isolate.get_data(1) as usize,
                    args: (0..args.length() + 2)
                        .map(|i| address_of(args.get(i)))
                        .collect(),
                })
            });
            // The receiver read in place is the crate's: same object.
            assert!(info.this() == args.this());
            // Each argument read in place is the crate's, and past the last it is `undefined`.
            let undefined = UNDEFINED.with(|u| u.get());
            for i in 0..args.length() + 2 {
                assert!(info.arg(i as usize, undefined).strict_equals(args.get(i)));
            }
            assert!(info.arg(args.length() as usize, undefined).is_undefined());
            rv.set_int32(0);
        });
        // The result written in place wins over the scope's.
        info.set_return_i32(-7);
    }

    /// Returns `info[0]` (an int32) through [`Info::set_return_i32`], and `info[1]`, when given, as a
    /// uint32 through [`Info::set_return_u32`].
    fn echo_int(info: Info<'_>) {
        let (a, b) = info.with_scope(|scope, args, _| {
            let a = args.get(0).int32_value(scope).unwrap();
            let b = (args.length() > 1).then(|| args.get(1).uint32_value(scope).unwrap());
            (a, b)
        });
        match b {
            Some(b) => info.set_return_u32(b),
            None => info.set_return_i32(a),
        }
    }

    fn record_null(info: Info<'_>) {
        let null = info.with_scope(|scope, _, _| address_of(v8::null(scope)));
        // SAFETY: `null` is a read-only root.
        unsafe { info.set_return_address(null) };
    }

    fn record_this(info: Info<'_>) {
        // SAFETY: the receiver is alive for the call.
        unsafe { info.set_return_address(address_of(info.this())) };
    }

    fn run<'s>(scope: &mut v8::PinScope<'s, '_>, source: &str) -> v8::Local<'s, v8::Value> {
        let code = v8::String::new(scope, source).unwrap();
        v8::Script::compile(scope, code, None)
            .unwrap()
            .run(scope)
            .unwrap()
    }

    /// Every slot [`Info`] reads in place holds what the crate's FFI accessors return, for calls
    /// with no arguments and with several, as a function and as a method, and the return slot
    /// written in place is the call's result.
    #[test]
    fn inline_reads_match_the_crates_accessors() {
        crate::init();
        let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
        isolate.set_data(1, 0xABCD as *mut c_void);
        verify_embedder_data(isolate, 1);
        v8::scope!(let scope, isolate);
        // `undefined`'s slot is the isolate's root table entry: the same in every scope.
        let undefined = slot_of(v8::undefined(scope));
        {
            v8::scope!(let inner, scope);
            assert_eq!(slot_of(v8::undefined(inner)), undefined);
        }
        UNDEFINED.with(|u| u.set(undefined));
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let global = context.global(scope);
        for (name, f) in [
            ("record", callback(record)),
            ("recordNull", callback(record_null)),
            ("recordThis", callback(record_this)),
            ("echoInt", callback(echo_int)),
        ] {
            let t = v8::FunctionTemplate::new_raw(scope, f);
            let func = t.get_function(scope).unwrap();
            let key = v8::String::new(scope, name).unwrap();
            global.set(scope, key.into(), func.into()).unwrap();
        }
        for (source, length) in [
            ("record()", 0),
            ("record(1, 'two', {})", 3),
            ("({ m: record }).m(1)", 1),
            ("record.call(globalThis, 1, 2, 3, 4, 5, 6, 7, 8)", 8),
        ] {
            let r = run(scope, source);
            assert_eq!(r.int32_value(scope), Some(-7), "{source}");
            let inline = INLINE.with(|s| s.borrow_mut().take()).unwrap();
            let crate_ = CRATE.with(|s| s.borrow_mut().take()).unwrap();
            assert_eq!(inline, crate_, "{source}");
            assert_eq!(inline.length, length, "{source}");
            assert_eq!(inline.data, 0xABCD, "{source}");
        }
        assert!(run(scope, "recordNull()").is_null());
        assert!(run(scope, "var o = { m: recordThis }; o.m() === o").is_true());
        // Integers at and past each end of the Smi range come back as themselves: inside it in place,
        // outside it as a number made by V8.
        let ints = [
            0,
            1,
            -1,
            SMI_MAX,
            SMI_MAX + 1,
            SMI_MIN,
            SMI_MIN - 1,
            i32::MAX,
            i32::MIN,
        ];
        for v in ints {
            let r = run(scope, &format!("echoInt({v})"));
            assert_eq!(r.number_value(scope), Some(f64::from(v)), "{v}");
            assert!(r.is_int32(), "{v}");
        }
        for v in [0u32, SMI_MAX as u32, SMI_MAX as u32 + 1, 1 << 31, u32::MAX] {
            let r = run(scope, &format!("echoInt(0, {v})"));
            assert_eq!(r.number_value(scope), Some(f64::from(v)), "{v}");
        }
        // A Smi result used as a number by script after many calls (optimized code reads it too).
        let sum = run(
            scope,
            "let s = 0; for (let i = 0; i < 20000; i++) s += echoInt(i % 7 - 3) + echoInt(0, 5); s",
        );
        // (20,000 fives, and -3..=3 cycling: 2,857 whole cycles sum to 0, the last call adds -3.)
        assert_eq!(sum.number_value(scope), Some(99_997.0));
    }
}
