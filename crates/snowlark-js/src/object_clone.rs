// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `v8::Object::Clone`: a shallow copy of an object (V8's `Factory::CopyJSObject`), with its map,
//! properties and internal fields. Blink makes a new wrapper by cloning a per-realm boilerplate of its
//! interface instead of instantiating the template each time (`V8PerContextData::CreateWrapperFromCache`).
//!
//! The `v8` crate does not bind it, so it is reached by its mangled name, as
//! [`crate::access_check::incumbent_context`] reaches `Isolate::GetIncumbentContext` (see
//! `docs/v8-upgrade.md`).

use std::ffi::c_void;

mod ffi {
    use std::ffi::c_void;

    // SAFETY: the Itanium x86-64 name of `Local<Object> v8::Object::Clone(Isolate*)` in the static
    // library the `v8` crate links (`v8-object.h`): `this`, then the isolate; the `Local` (one
    // trivially copyable pointer) comes back in a register.
    #[cfg(v8_abi_itanium)]
    extern "C" {
        #[link_name = "_ZN2v86Object5CloneEPNS_7IsolateE"]
        fn object_clone_itanium(this: *const c_void, isolate: *mut c_void) -> *const c_void;
    }

    /// `Object::Clone`, its `Local` stored through `ret`.
    #[cfg(v8_abi_itanium)]
    pub unsafe fn object_clone(
        this: *const c_void,
        ret: *mut *const c_void,
        isolate: *mut c_void,
    ) -> *mut *const c_void {
        // SAFETY: the caller's contract, as for the MSVC declaration.
        unsafe { *ret = object_clone_itanium(this, isolate) };
        ret
    }

    // SAFETY: the MSVC x64 name of `Local<Object> v8::Object::Clone(Isolate*)` in the static library
    // the `v8` crate links (`v8-object.h`). A member function returning a class: `this` comes first,
    // then the hidden pointer the `Local` (one pointer) is stored through, then the isolate.
    #[cfg(not(v8_abi_itanium))]
    extern "C" {
        #[link_name = "?Clone@Object@v8@@QEAA?AV?$Local@VObject@v8@@@2@PEAVIsolate@2@@Z"]
        pub fn object_clone(
            this: *const c_void,
            ret: *mut *const c_void,
            isolate: *mut c_void,
        ) -> *mut *const c_void;
    }
}

// SAFETY: the `v8` crate's own C shim (`binding.cc`): `Context::GetNumberOfEmbedderDataFields()`.
extern "C" {
    fn v8__Context__GetNumberOfEmbedderDataFields(this: *const v8::Context) -> u32;
}

/// How many embedder data fields `context` has, counting the two the `v8` crate keeps for itself
/// before the ones [`v8::Context::set_embedder_data`] numbers (reading past the last aborts).
pub fn embedder_data_fields(context: v8::Local<v8::Context>) -> u32 {
    // SAFETY: `context` is a live handle of the isolate entered on this thread.
    unsafe { v8__Context__GetNumberOfEmbedderDataFields(&*context) }
}

/// A shallow copy of `obj` in the current handle scope: same map (so same prototype and creation
/// context), same properties and internal fields.
pub fn clone<'s>(
    scope: &v8::PinScope<'s, '_>,
    obj: v8::Local<'_, v8::Object>,
) -> v8::Local<'s, v8::Object> {
    let mut out: *const c_void = std::ptr::null();
    // SAFETY: the isolate is entered on this thread and `obj` is one of its objects; a `Local` is one
    // pointer (`v8-local-handle.h`), here the object's handle slot, which is the C++ `this`. `out`
    // receives the new `Local`'s pointer, a handle in the current handle scope, which `scope` keeps
    // open for `'s`; `CopyJSObject` does not fail (it aborts on out of memory).
    unsafe {
        let isolate =
            std::mem::transmute::<v8::UnsafeRawIsolatePtr, *mut c_void>(scope.as_raw_isolate_ptr());
        ffi::object_clone(
            &*obj as *const v8::Object as *const c_void,
            &mut out,
            isolate,
        );
        let ptr = std::ptr::NonNull::new(out as *mut v8::Object)
            .expect("Object::Clone returned a handle");
        std::mem::transmute::<std::ptr::NonNull<v8::Object>, v8::Local<'s, v8::Object>>(ptr)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_clone_has_the_original_s_prototype_properties_and_internal_fields() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let context = {
            v8::scope!(let scope, &mut isolate);
            let c = v8::Context::new(scope, v8::ContextOptions::default());
            v8::Global::new(scope, c)
        };
        {
            v8::scope_with_context!(scope, &mut isolate, &context);
            let template = v8::ObjectTemplate::new(scope);
            template.set_internal_field_count(2);
            let obj = template.new_instance(scope).unwrap();
            let key = v8::String::new(scope, "k").unwrap();
            let value = v8::Integer::new(scope, 7);
            obj.set(scope, key.into(), value.into()).unwrap();
            let marker = v8::Integer::new(scope, 42);
            obj.set_internal_field(1, marker.into());

            let copy = super::clone(scope, obj);
            assert!(copy != obj, "a new object");
            assert_eq!(copy.internal_field_count(), 2);
            assert_eq!(
                copy.get_prototype(scope).unwrap(),
                obj.get_prototype(scope).unwrap()
            );
            let got = copy.get(scope, key.into()).unwrap();
            assert_eq!(got.integer_value(scope), Some(7));
            let field: v8::Local<v8::Value> = copy
                .get_internal_field(scope, 1)
                .unwrap()
                .try_into()
                .unwrap();
            assert!(field.strict_equals(marker.into()));
            // (shallow, but its own: a change to the copy leaves the original)
            let other = v8::Integer::new(scope, 8);
            copy.set(scope, key.into(), other.into()).unwrap();
            assert_eq!(
                obj.get(scope, key.into()).unwrap().integer_value(scope),
                Some(7)
            );
        }
    }
}
