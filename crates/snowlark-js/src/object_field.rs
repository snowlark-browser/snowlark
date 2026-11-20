// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Objects kept in internal fields, read and written without a bounds check per access.
//!
//! `v8::Object::get_internal_field` and `set_internal_field` ask V8 for the object's field count on every
//! call (a second call into the library), and a read then checks twice what kind of value it found. A
//! caller that knows the count (it checked once per object, or made the object from a template) reads
//! here with at most one call: the field's tagged word is read inline ([`crate::internal_field`]) and
//! compared with `undefined`'s, which a field never written holds, so a field without an object costs no
//! call at all, and whether a field holds a given object is one compare. Writes go through V8's own
//! `SetInternalField` (an object store needs V8's write barrier), without the count.
//!
//! A field read here holds `undefined`, a Smi or an object, never another heap value. Objects whose
//! fields V8 lays out elsewhere (a global object) take V8's own read and its checks of the value.

use std::ffi::c_int;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;

use crate::internal_field::field_word;

// The v8 crate's own C++ glue (`binding.cc`), linked with it: `GetInternalField` returns a handle in the
// isolate's current handle scope, as the crate's `get_internal_field` does after its bounds check.
extern "C" {
    fn v8__Object__GetInternalField(this: *const v8::Object, index: c_int) -> *const v8::Data;
    fn v8__Object__SetInternalField(this: *const v8::Object, index: c_int, data: *const v8::Data);
}

/// `kHeapObjectTag`; a word with this bit clear is a Smi.
const HEAP_OBJECT_TAG: u32 = 1;

/// The word a tagged field holds for the value behind `value`: its compressed address (this library
/// compresses pointers; `internal_field` has the layout).
#[inline(always)]
fn tagged_word(value: &v8::Data) -> u32 {
    // SAFETY: a `&v8::Data` points at a handle slot holding the value's full tagged address; a
    // compressed pointer is its low 32 bits.
    unsafe { *(value as *const v8::Data as *const usize) as u32 }
}

/// Field access for one isolate: the tagged word of its `undefined`. Make it once per batch of reads.
#[derive(Clone, Copy)]
pub struct Fields {
    undefined: u32,
}

impl Fields {
    pub fn new(scope: &v8::PinScope<'_, '_>) -> Fields {
        Fields {
            undefined: tagged_word(&v8::undefined(scope)),
        }
    }

    /// The object in field `index` of `obj`, or `None` when it holds `undefined` or a Smi.
    ///
    /// # Safety
    ///
    /// `index` is below `obj`'s internal field count, and the field holds `undefined`, a Smi or an
    /// object.
    #[inline]
    pub unsafe fn object<'s>(
        &self,
        scope: &v8::PinScope<'s, '_>,
        obj: &v8::Object,
        index: usize,
    ) -> Option<v8::Local<'s, v8::Object>> {
        let _ = scope;
        // SAFETY: as the caller promised.
        let inline = unsafe { field_word(obj, index as i32) };
        if let Some(word) = inline {
            let word = word.load(Ordering::Relaxed);
            if word & HEAP_OBJECT_TAG == 0 || word == self.undefined {
                return None;
            }
        }
        // SAFETY: the field exists; V8 returns a handle in the current handle scope, which `scope`'s
        // lifetime bounds (the v8 crate ties it to the scope the same way).
        let data = NonNull::new(
            unsafe { v8__Object__GetInternalField(obj, index as c_int) } as *mut v8::Data
        )?;
        // SAFETY: `Local` is a `repr(C)` pointer to a handle slot and a marker.
        let data: v8::Local<'s, v8::Data> = unsafe { std::mem::transmute(data) };
        if inline.is_some() {
            // SAFETY: the field holds an object: not a Smi, not `undefined`, and nothing else (as the
            // caller promised).
            return Some(unsafe { v8::Local::<v8::Object>::cast_unchecked(data) });
        }
        let value = v8::Local::<v8::Value>::try_from(data).ok()?;
        v8::Local::<v8::Object>::try_from(value).ok()
    }

    /// Whether field `index` of `obj` holds `other`.
    ///
    /// # Safety
    ///
    /// As for [`Fields::object`].
    #[inline]
    pub unsafe fn holds(
        &self,
        scope: &v8::PinScope<'_, '_>,
        obj: &v8::Object,
        index: usize,
        other: v8::Local<v8::Object>,
    ) -> bool {
        // SAFETY: as the caller promised.
        match unsafe { field_word(obj, index as i32) } {
            Some(word) => word.load(Ordering::Relaxed) == tagged_word(&other),
            // SAFETY: as the caller promised.
            None => (unsafe { self.object(scope, obj, index) }) == Some(other),
        }
    }

    /// Stores `value` in field `index` of `obj`.
    ///
    /// # Safety
    ///
    /// `index` is below `obj`'s internal field count.
    #[inline]
    pub unsafe fn set(obj: &v8::Object, index: usize, value: v8::Local<v8::Data>) {
        // SAFETY: as the caller promised; V8 stores the value with its write barrier.
        unsafe { v8__Object__SetInternalField(obj, index as c_int, &*value) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads and identity checks agree with V8's own field access on API objects (inline path) and on a
    /// global object (V8's path), across collections that move the objects.
    #[test]
    fn object_fields_agree_with_v8() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let context = {
            v8::scope!(let scope, &mut isolate);
            let global = v8::ObjectTemplate::new(scope);
            global.set_internal_field_count(3);
            let c = v8::Context::new(
                scope,
                v8::ContextOptions {
                    global_template: Some(global),
                    ..Default::default()
                },
            );
            v8::Global::new(scope, c)
        };
        v8::scope_with_context!(scope, &mut isolate, &context);
        let template = v8::ObjectTemplate::new(scope);
        template.set_internal_field_count(3);
        let objects: Vec<_> = (0..100)
            .map(|_| {
                let o = template.new_instance(scope).unwrap();
                v8::Global::new(scope, o)
            })
            .collect();
        let global = scope.get_current_context().global(scope);
        // Each object i: field 0 never written, field 1 the next object (by V8), field 2 a Smi.
        for (i, o) in objects.iter().enumerate() {
            let o = v8::Local::new(scope, o);
            let next = v8::Local::new(scope, &objects[(i + 1) % objects.len()]);
            o.set_internal_field(1, next.into());
            o.set_internal_field(2, v8::Integer::new(scope, i as i32).into());
        }
        global.set_internal_field(1, v8::Local::new(scope, &objects[7]).into());
        global.set_internal_field(2, v8::undefined(scope).into());
        for _ in 0..3 {
            scope.low_memory_notification();
        }
        let f = Fields::new(scope);
        for (i, o) in objects.iter().enumerate() {
            let o = v8::Local::new(scope, o);
            let next = v8::Local::new(scope, &objects[(i + 1) % objects.len()]);
            let other = v8::Local::new(scope, &objects[(i + 2) % objects.len()]);
            // SAFETY: every instance has 3 fields holding undefined, an object or a Smi.
            unsafe {
                assert_eq!(f.object(scope, &o, 0), None);
                assert_eq!(f.object(scope, &o, 1), Some(next));
                assert_eq!(f.object(scope, &o, 2), None);
                assert!(f.holds(scope, &o, 1, next));
                assert!(!f.holds(scope, &o, 1, other));
                assert!(!f.holds(scope, &o, 0, next));
            }
        }
        let seven = v8::Local::new(scope, &objects[7]);
        // SAFETY: the global template has 3 fields.
        unsafe {
            assert_eq!(f.object(scope, &global, 1), Some(seven));
            assert_eq!(f.object(scope, &global, 2), None);
            assert!(f.holds(scope, &global, 1, seven));
        }
        // Writes, then V8 reads them; `undefined` clears a field.
        let a = v8::Local::new(scope, &objects[0]);
        let b = v8::Local::new(scope, &objects[1]);
        // SAFETY: as above.
        unsafe {
            Fields::set(&a, 0, b.into());
            Fields::set(&global, 2, a.into());
        }
        scope.low_memory_notification();
        fn read<'s>(
            scope: &v8::PinScope<'s, '_>,
            o: v8::Local<v8::Object>,
            i: usize,
        ) -> Option<v8::Local<'s, v8::Value>> {
            o.get_internal_field(scope, i)
                .and_then(|d| v8::Local::<v8::Value>::try_from(d).ok())
        }
        assert_eq!(read(scope, a, 0), Some(b.into()));
        assert_eq!(read(scope, global, 2), Some(a.into()));
        // SAFETY: as above.
        unsafe {
            Fields::set(&a, 0, v8::undefined(scope).into());
            assert_eq!(f.object(scope, &a, 0), None);
        }
        assert!(read(scope, a, 0).is_some_and(|v| v.is_undefined()));
    }
}
