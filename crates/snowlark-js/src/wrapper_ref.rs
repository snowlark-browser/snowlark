// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A wrapper's handle: one V8 global handle that turns weak and strong in place.
//!
//! `v8::Global` and `v8::Weak` are two kinds of handle: making a wrapper weak with them creates a second
//! global handle (with a boxed record, a reference on the isolate and a callback V8 runs when the object
//! dies) and drops the first. A [`WrapperRef`] keeps one global handle's slot and makes that same slot
//! weak in the form that has no callback (`PersistentBase::SetWeak()`): when the object dies, V8 sets
//! the slot pointer held here to null during the collection, and frees the handle. Nothing is
//! allocated, and nothing is called back.
//!
//! **A weak `WrapperRef` must stay where it is**: V8 keeps the address of its slot pointer until the
//! object dies or [`WrapperRef::make_strong`] runs. Its owner keeps it in place (the wrapper table in
//! `snowlark-bindings` stores entries in pages that never move) and drops it there.
//!
//! The `v8` crate binds none of this, so it is reached by the MSVC x64 names of the functions behind
//! V8's inline handle code, as [`crate::cached_accessor`] does (see `docs/v8-upgrade.md`).

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::{null_mut, NonNull};

/// `v8::internal::Address`.
type Address = usize;

mod ffi {
    use super::Address;
    use std::ffi::c_void;

    // SAFETY (all declarations): the MSVC x64 names of `v8::api_internal` functions in the static library
    // the `v8` crate links, with their C++ signatures (`v8-persistent-handle.h`, `v8-internal.h`). Every
    // argument is a pointer or an integer, passed in a register.
    extern "C" {
        /// `internal::Address* GlobalizeReference(internal::Isolate*, internal::Address)`: a new strong
        /// global handle to the object at `value`. A `v8::Isolate*` is the `internal::Isolate*` itself.
        #[link_name = "?GlobalizeReference@api_internal@v8@@YAPEA_KPEAVIsolate@internal@2@_K@Z"]
        pub fn globalize_reference(isolate: *mut c_void, value: Address) -> *mut Address;
        /// `void MakeWeak(internal::Address** location_addr)`: the handle at `*location_addr` turns
        /// weak; when its object dies, V8 stores null at `location_addr` and frees the handle.
        #[link_name = "?MakeWeak@api_internal@v8@@YAXPEAPEA_K@Z"]
        pub fn make_weak(location_addr: *mut *mut Address);
        /// `void* ClearWeak(internal::Address* location)`: a weak handle turns strong again.
        #[link_name = "?ClearWeak@api_internal@v8@@YAPEAXPEA_K@Z"]
        pub fn clear_weak(location: *mut Address) -> *mut c_void;
        /// `void DisposeGlobal(internal::Address* location)`: frees a global handle, weak or strong.
        #[link_name = "?DisposeGlobal@api_internal@v8@@YAXPEA_K@Z"]
        pub fn dispose_global(location: *mut Address);
    }

    // SAFETY: the `v8` crate's own C shim (`binding.cc`, `v8__Local__New`): `Local<Data>::New(isolate,
    // other)`, a new local handle in the current handle scope to the object `other` points at.
    extern "C" {
        pub fn v8__Local__New(isolate: *mut c_void, other: *const c_void) -> *const c_void;
    }
}

/// One global handle to a wrapper, strong or weak (see the module documentation).
pub struct WrapperRef {
    /// The global handle's slot; null once a weak object died (V8 stores the null).
    slot: Cell<*mut Address>,
    weak: Cell<bool>,
}

/// The `v8::Isolate*` of `isolate`.
#[inline(always)]
fn raw_isolate(isolate: &v8::Isolate) -> *mut c_void {
    // SAFETY: `UnsafeRawIsolatePtr` is `repr(transparent)` over the isolate's pointer, which is valid
    // for as long as `isolate` is borrowed.
    unsafe {
        std::mem::transmute::<v8::UnsafeRawIsolatePtr, *mut c_void>(isolate.as_raw_isolate_ptr())
    }
}

impl WrapperRef {
    /// A strong handle to `obj`.
    #[inline]
    pub fn new(isolate: &v8::Isolate, obj: v8::Local<v8::Object>) -> Self {
        // SAFETY: a `Local` points at a slot holding the object's address; the isolate is the one
        // `obj` belongs to, entered on this thread.
        let slot = unsafe {
            let value = *(&*obj as *const v8::Object as *const Address);
            ffi::globalize_reference(raw_isolate(isolate), value)
        };
        WrapperRef {
            slot: Cell::new(slot),
            weak: Cell::new(false),
        }
    }

    /// Whether the handle is weak (alive or not).
    #[inline(always)]
    pub fn is_weak(&self) -> bool {
        self.weak.get()
    }

    /// Whether the object died (only a weak handle's can).
    #[inline(always)]
    pub fn is_dead(&self) -> bool {
        self.slot.get().is_null()
    }

    /// A local handle to the object, unless it died.
    #[inline]
    pub fn get<'s>(&self, scope: &v8::PinScope<'s, '_, ()>) -> Option<v8::Local<'s, v8::Object>> {
        let slot = self.slot.get();
        if slot.is_null() {
            return None;
        }
        // SAFETY: `slot` is a live global handle of this isolate (V8 nulls it when a weak object dies,
        // which only happens in a collection, not while this runs). The new local lives in the scope's
        // handle scope, so as long as `'s`; an empty `Local` would be null, and `Local<Data>::New` of a
        // live handle is never empty.
        unsafe {
            let local = ffi::v8__Local__New(raw_isolate(scope), slot as *const c_void);
            let local = NonNull::new(local as *mut v8::Object)?;
            Some(std::mem::transmute::<
                NonNull<v8::Object>,
                v8::Local<'s, v8::Object>,
            >(local))
        }
    }

    /// Sets the object as a callback's return value with no local handle (V8's
    /// `ReturnValue::Set(const Global&)`): false when it died.
    #[inline]
    pub fn set_return(&self, rv: &mut v8::ReturnValue<v8::Value>) -> bool {
        let slot = self.slot.get();
        if slot.is_null() {
            return false;
        }
        // SAFETY: a `v8::ReturnValue` is one `internal::Address*`, the callback's return slot
        // (`RawReturnValue(usize)` in the `v8` crate, `value_` in `v8-function-callback.h`). With pointer
        // compression and static roots `ReturnValue::SetInternal` stores the compressed value in the
        // slot's low half and keeps the cage base in its high half: for an object of the main cage
        // (every JS object, and a global handle holds the full address), that is the object's full
        // address, which is what is stored here. The return slot is on V8's stack, a root, so the object
        // stays alive from here on even when this handle is weak.
        unsafe {
            let ret = *(rv as *mut v8::ReturnValue<v8::Value> as *const *mut Address);
            *ret = *slot;
        }
        true
    }

    /// [`Self::set_return`] for a callback that reads its arguments in place
    /// ([`crate::raw_callback`]): false when the object died.
    #[inline(always)]
    pub fn return_into(&self, info: &crate::raw_callback::Info<'_>) -> bool {
        let slot = self.slot.get();
        if slot.is_null() {
            return false;
        }
        // SAFETY: `slot` is a live global handle of the callback's isolate, so it holds the address of
        // a live object; the return slot is a root (see `set_return`).
        unsafe { info.set_return_address(*slot) };
        true
    }

    /// The handle turns weak, in place.
    ///
    /// # Safety
    ///
    /// `self` must not move until it is dropped or [`Self::make_strong`] runs: V8 writes to it when
    /// the object dies.
    #[inline]
    pub unsafe fn make_weak(&self) {
        if self.weak.get() || self.slot.get().is_null() {
            return;
        }
        self.weak.set(true);
        // SAFETY: the slot is a live strong global handle; the caller keeps `self` in place.
        unsafe { ffi::make_weak(self.slot.as_ptr()) };
    }

    /// A weak handle turns strong again: false when its object died.
    #[inline]
    pub fn make_strong(&self) -> bool {
        let slot = self.slot.get();
        if slot.is_null() {
            return false;
        }
        if self.weak.replace(false) {
            // SAFETY: a live weak global handle of this isolate.
            unsafe { ffi::clear_weak(slot) };
        }
        true
    }
}

impl Drop for WrapperRef {
    fn drop(&mut self) {
        let slot = self.slot.replace(null_mut());
        if !slot.is_null() {
            // SAFETY: a live global handle, freed once: the isolate is still alive (every owner drops
            // its wrappers before the isolate is disposed).
            unsafe { ffi::dispose_global(slot) };
        }
    }
}

impl std::fmt::Debug for WrapperRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrapperRef")
            .field("weak", &self.weak.get())
            .field("dead", &self.is_dead())
            .finish()
    }
}
