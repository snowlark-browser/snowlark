// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small integers kept in a wrapper's internal fields as Smis, read and written inline.
//!
//! Every binding call reads its receiver's handle from internal fields. Up to V8 15.2 the bindings
//! stored it with `SetAlignedPointerInInternalField`, which put the (even, so Smi-looking) value in the
//! field itself, and V8's header read it inline. From V8 15.8 that API allocates a garbage-collected
//! record on the C++ heap for every store and reads through it, out of line, after finding the current
//! isolate: a measurable cost on calls that do little else (a DOM getter) and on every wrapper made.
//!
//! Here the value goes in the field's tagged word, as a Smi: the GC skips it and a store of it needs no
//! write barrier. This library compresses pointers, so an embedder slot is two 32-bit halves: the tagged
//! word, and a handle of V8's C++ heap pointer table that its marker reads (an aligned pointer stored
//! through V8 goes there). Only the tagged half is ours, and it holds a 31-bit value ([`MAX`]): a wider
//! one is split over two fields by the caller. Reads and writes are plain 32-bit loads and stores at V8's
//! field offset for objects of the API object instance types, as V8's `Internals` reads them; any other
//! object (a global object lays its fields out elsewhere) goes through `Get/SetAlignedPointer...` with
//! the same Smi word, so both paths agree.
//!
//! The constants are V8's (`$v8/v8/include/v8-internal.h`, `Internals`) for this library's layout (64-bit,
//! pointer compression, no sandbox); a V8 upgrade re-checks them (`docs/v8-upgrade.md`). [`verify`],
//! run in every isolate the bindings install, writes and reads a probe both ways against V8's own
//! `Get/SetInternalField` and panics on a difference, so a layout that moved stops before any page runs.

use std::sync::atomic::{AtomicU32, Ordering};

/// `kApiTaggedSize` with pointer compression.
const TAGGED_SIZE: usize = 4;
/// `Internals::kMapInstanceTypeOffset`: `1 * kApiTaggedSize + kApiInt32Size`.
const MAP_INSTANCE_TYPE_OFFSET: usize = TAGGED_SIZE + 4;
/// `Internals::kJSAPIObjectWithEmbedderSlotsHeaderSize` with pointer compression:
/// `kJSObjectHeaderSize (3 * kApiTaggedSize) + kApiInt32Size`.
const EMBEDDER_SLOTS_OFFSET: usize = 3 * TAGGED_SIZE + 4;
/// `Internals::kEmbedderDataSlotSize`: `2 * kApiTaggedSize`; the tagged word is the slot's first half.
const EMBEDDER_SLOT_SIZE: usize = 2 * TAGGED_SIZE;
/// `kPtrComprCageBaseAlignment`: the cage base is an object's address with the low 32 bits cleared
/// (`Internals::GetPtrComprCageBaseFromOnHeapAddress`).
const CAGE_BASE_MASK: usize = !((1 << 32) - 1);
/// `Internals::kJSSpecialApiObjectType`, `kJSObjectType`, `kLastJSApiObjectType`.
const JS_SPECIAL_API_OBJECT_TYPE: u16 = 0x410;
const JS_OBJECT_TYPE: u16 = 0x421;
const LAST_JS_API_OBJECT_TYPE: u16 = 0x80A;
/// `kHeapObjectTag`; a word with this bit clear is a Smi.
const HEAP_OBJECT_TAG: u32 = 1;

/// The largest value a field holds: a non-negative 31-bit Smi's payload.
pub const MAX: u32 = (1 << 31) - 1;

/// `Internals::CanHaveInternalField`: a plain JS object, a special API object or an API object, whose
/// embedder slots start right after the header.
#[inline(always)]
fn can_have_internal_field(instance_type: u16) -> bool {
    instance_type == JS_SPECIAL_API_OBJECT_TYPE
        || instance_type.wrapping_sub(JS_OBJECT_TYPE) <= LAST_JS_API_OBJECT_TYPE - JS_OBJECT_TYPE
}

/// The tagged word of field `index` of `obj`, or `None` for an object whose fields V8 lays out elsewhere.
///
/// # Safety
///
/// `index` is below `obj`'s internal field count.
#[inline(always)]
unsafe fn field_word(obj: &v8::Object, index: i32) -> Option<&AtomicU32> {
    // SAFETY: a `&v8::Object` is the crate's view of a handle: a pointer to a slot holding the object's
    // full tagged address, alive while the handle is. A heap object's first word is its compressed map,
    // which is the map's offset from the cage base, and a map is far longer than the instance type's
    // offset. For the instance types tested, V8 lays the embedder slots out right after the header, and
    // the caller promised that field `index` exists. Objects are 4-byte aligned with pointer
    // compression, so is the tagged word; V8's concurrent marker reads it too, hence an atomic.
    unsafe {
        let object = *(obj as *const v8::Object as *const usize) - HEAP_OBJECT_TAG as usize;
        let map =
            (object & CAGE_BASE_MASK) + *(object as *const u32) as usize - HEAP_OBJECT_TAG as usize;
        if !can_have_internal_field(*((map + MAP_INSTANCE_TYPE_OFFSET) as *const u16)) {
            return None;
        }
        let at = object + EMBEDDER_SLOTS_OFFSET + EMBEDDER_SLOT_SIZE * index as usize;
        Some(&*(at as *const AtomicU32))
    }
}

/// The value [`write`] stored in field `index` of `obj`, or 0 when the field holds anything else (a
/// field never written holds `undefined`).
///
/// # Safety
///
/// `index` is below `obj`'s internal field count.
#[inline(always)]
pub unsafe fn read(obj: &v8::Object, index: i32) -> u32 {
    // SAFETY: as the caller promised.
    let word = match unsafe { field_word(obj, index) } {
        Some(w) => w.load(Ordering::Relaxed),
        None => return read_slow(obj, index),
    };
    if word & HEAP_OBJECT_TAG == 0 {
        word >> 1
    } else {
        0
    }
}

/// Objects outside the API object types keep the Smi word through V8's aligned-pointer API, both ways.
#[cold]
fn read_slow(obj: &v8::Object, index: i32) -> u32 {
    // SAFETY: `read`'s caller promised that the field exists; [`write_slow`] stored an aligned value.
    (unsafe { obj.get_aligned_pointer_from_internal_field(index, 0) } as usize >> 1) as u32
}

/// Stores `value`, at most [`MAX`], in field `index` of `obj`, where [`read`] finds it.
///
/// # Safety
///
/// `index` is below `obj`'s internal field count.
#[inline(always)]
pub unsafe fn write(obj: &v8::Object, index: i32, value: u32) {
    debug_assert!(value <= MAX, "a 31-bit Smi");
    // SAFETY: as the caller promised. A Smi store needs no write barrier; the slot's other half, V8's
    // table handle, is left as it is.
    match unsafe { field_word(obj, index) } {
        Some(w) => w.store(value << 1, Ordering::Relaxed),
        None => write_slow(obj, index, value),
    }
}

#[cold]
fn write_slow(obj: &v8::Object, index: i32, value: u32) {
    obj.set_aligned_pointer_in_internal_field(
        index,
        ((value as usize) << 1) as *const std::ffi::c_void,
        0,
    );
}

/// Checks the inline reads and writes against V8's own `Get/SetInternalField` on a probe object made in
/// `scope`'s context, which must take the inline path. Panics otherwise: this library's object layout is
/// not the one [`read`] and [`write`] assume.
pub fn verify(scope: &mut v8::PinScope<'_, '_>) {
    let template = v8::ObjectTemplate::new(scope);
    template.set_internal_field_count(3);
    let obj = template
        .new_instance(scope)
        .expect("an object from a template");
    let fail = "V8's object layout changed: the inline internal field access is wrong for this \
                library (see docs/v8-upgrade.md)";
    // SAFETY: the template gives the object 3 fields.
    assert!(unsafe { field_word(&obj, 2) }.is_some(), "{fail}");
    // A field never written holds `undefined`: no value.
    // SAFETY: as above.
    assert_eq!(unsafe { read(&obj, 2) }, 0, "{fail}");
    // V8 writes a Smi (the largest 31-bit one), the inline read finds it.
    let smi = v8::Integer::new(scope, 0x3FFF_FFFF);
    obj.set_internal_field(2, smi.into());
    // SAFETY: as above.
    assert_eq!(unsafe { read(&obj, 2) }, 0x3FFF_FFFF, "{fail}");
    // The inline write, V8 reads it as that Smi; the neighbouring field is untouched.
    let neighbour = v8::Integer::new(scope, 0x1234_5678);
    obj.set_internal_field(0, neighbour.into());
    // SAFETY: as above.
    unsafe { write(&obj, 1, 0x0765_4321) };
    let int = |scope: &mut v8::PinScope<'_, '_>, i| {
        obj.get_internal_field(scope, i)
            .and_then(|d| v8::Local::<v8::Value>::try_from(d).ok())
            .and_then(|v| v.int32_value(scope))
    };
    assert_eq!(int(scope, 1), Some(0x0765_4321), "{fail}");
    assert_eq!(int(scope, 0), Some(0x1234_5678), "{fail}");
    assert_eq!(int(scope, 2), Some(0x3FFF_FFFF), "{fail}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// API objects (from a template) take the inline path and agree with V8's own field access; a global
    /// object, outside the API object types, keeps its value through the slow path.
    #[test]
    fn inline_field_access_agrees_with_v8() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let context = {
            v8::scope!(let scope, &mut isolate);
            let global = v8::ObjectTemplate::new(scope);
            global.set_internal_field_count(2);
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
        verify(scope);
        let template = v8::ObjectTemplate::new(scope);
        template.set_internal_field_count(6);
        // Values up to the largest, in every field, including the top bit of the 31.
        let value = |i: usize, field: usize| ((i * 6 + field + 1) as u32 * 0x0040_0007) & MAX;
        let objects: Vec<_> = (0..200usize)
            .map(|i| {
                let obj = template.new_instance(scope).unwrap();
                for field in 0..6 {
                    // SAFETY: the template gives every instance 6 fields.
                    unsafe { write(&obj, field as i32, value(i, field)) };
                }
                v8::Global::new(scope, obj)
            })
            .collect();
        // The values survive collections that move the objects.
        for _ in 0..3 {
            scope.low_memory_notification();
        }
        for (i, obj) in objects.iter().enumerate() {
            let obj = v8::Local::new(scope, obj);
            for field in 0..6 {
                // SAFETY: as above.
                assert_eq!(unsafe { read(&obj, field as i32) }, value(i, field));
            }
        }
        // SAFETY: as above.
        unsafe { write(&v8::Local::new(scope, &objects[0]), 5, MAX) };
        // SAFETY: as above.
        assert_eq!(unsafe { read(&v8::Local::new(scope, &objects[0]), 5) }, MAX);

        let global = scope.get_current_context().global(scope);
        // SAFETY: the global template has 2 fields.
        unsafe { write(&global, 1, 0x3BB8) };
        // SAFETY: as above.
        assert_eq!(unsafe { read(&global, 1) }, 0x3BB8);
        // SAFETY: as above.
        unsafe { write(&global, 0, MAX) };
        // SAFETY: as above.
        assert_eq!(unsafe { read(&global, 0) }, MAX);
    }
}
