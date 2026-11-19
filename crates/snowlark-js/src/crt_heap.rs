// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The C runtime's heap functions, defined on mimalloc.
//!
//! Rust's allocations go to mimalloc (the `snowlark` binary's global allocator), but V8 and every
//! other C or C++ library in the process allocate through the C runtime: `operator new` and
//! `malloc`, which the static C runtime (`+crt-static`, `.cargo/config.toml`) sends to the Windows
//! heap. That was 1.2% of the mean Speedometer suite, most of it V8's compilers.
//!
//! The static C runtime is a library, and the linker takes a symbol from a library only when no
//! object or earlier library defines it, so defining these functions here replaces the runtime's
//! own for the whole program: its `operator new`, `operator delete` and aligned variants call
//! `malloc`, `free` and `_aligned_malloc`, and its internal allocations call the `_base` forms.
//! Every function that allocates, frees, resizes or measures runtime heap memory is defined, so
//! no block from one heap ever reaches the other's `free`. `heap_is_mimalloc` checks that the
//! link took these definitions; `crt_heap::tests` runs it.
//!
//! mimalloc frees any block it allocated, aligned or not, so `free` and `_aligned_free` are
//! interchangeable here (the runtime's are not; nothing correct depends on the difference).

use std::ffi::c_void;

/// mimalloc's functions: those `libmimalloc-sys` binds without its `extended` feature, and the
/// rest declared here from `mimalloc.h` (the feature would add a crate for its C types).
mod mi {
    use std::ffi::c_void;

    pub use libmimalloc_sys::{
        mi_free, mi_malloc, mi_malloc_aligned, mi_realloc, mi_realloc_aligned,
    };

    extern "C" {
        pub fn mi_calloc(count: usize, size: usize) -> *mut c_void;
        pub fn mi_recalloc(p: *mut c_void, count: usize, size: usize) -> *mut c_void;
        pub fn mi_expand(p: *mut c_void, newsize: usize) -> *mut c_void;
        pub fn mi_usable_size(p: *const c_void) -> usize;
        pub fn mi_malloc_aligned_at(size: usize, alignment: usize, offset: usize) -> *mut c_void;
        pub fn mi_realloc_aligned_at(
            p: *mut c_void,
            newsize: usize,
            alignment: usize,
            offset: usize,
        ) -> *mut c_void;
        pub fn mi_recalloc_aligned(
            p: *mut c_void,
            newcount: usize,
            size: usize,
            alignment: usize,
        ) -> *mut c_void;
        pub fn mi_recalloc_aligned_at(
            p: *mut c_void,
            newcount: usize,
            size: usize,
            alignment: usize,
            offset: usize,
        ) -> *mut c_void;
        pub fn mi_is_in_heap_region(p: *const c_void) -> bool;
    }
}

// SAFETY (all functions in this module): each forwards to the mimalloc function with the same
// contract as the runtime function it replaces (C11 / the runtime's documented semantics). A
// pointer passed in was returned by one of these functions, since they replace every runtime
// allocation entry; mimalloc's functions accept null where the runtime's do.

#[no_mangle]
unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_malloc(size) }
}

#[no_mangle]
unsafe extern "C" fn _malloc_base(size: usize) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_malloc(size) }
}

#[no_mangle]
unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    // SAFETY: see the module comment. mimalloc fails on an overflowing product, as calloc must.
    unsafe { mi::mi_calloc(count, size) }
}

#[no_mangle]
unsafe extern "C" fn _calloc_base(count: usize, size: usize) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_calloc(count, size) }
}

#[no_mangle]
unsafe extern "C" fn realloc(block: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_realloc(block, size) }
}

#[no_mangle]
unsafe extern "C" fn _realloc_base(block: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_realloc(block, size) }
}

#[no_mangle]
unsafe extern "C" fn _recalloc(block: *mut c_void, count: usize, size: usize) -> *mut c_void {
    // SAFETY: see the module comment. The memory past the block's old usable size is zeroed.
    unsafe { mi::mi_recalloc(block, count, size) }
}

#[no_mangle]
unsafe extern "C" fn _recalloc_base(block: *mut c_void, count: usize, size: usize) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_recalloc(block, count, size) }
}

#[no_mangle]
unsafe extern "C" fn _expand(block: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: see the module comment. Resizes in place or returns null, as the runtime's does.
    unsafe { mi::mi_expand(block, size) }
}

#[no_mangle]
unsafe extern "C" fn _expand_base(block: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_expand(block, size) }
}

#[no_mangle]
unsafe extern "C" fn free(block: *mut c_void) {
    // SAFETY: see the module comment.
    unsafe { mi::mi_free(block) }
}

#[no_mangle]
unsafe extern "C" fn _free_base(block: *mut c_void) {
    // SAFETY: see the module comment.
    unsafe { mi::mi_free(block) }
}

#[no_mangle]
unsafe extern "C" fn _msize(block: *mut c_void) -> usize {
    // SAFETY: see the module comment. The usable size, at least the size asked for.
    unsafe { mi::mi_usable_size(block) }
}

#[no_mangle]
unsafe extern "C" fn _msize_base(block: *mut c_void) -> usize {
    // SAFETY: see the module comment.
    unsafe { mi::mi_usable_size(block) }
}

#[no_mangle]
unsafe extern "C" fn _aligned_malloc(size: usize, alignment: usize) -> *mut c_void {
    // SAFETY: see the module comment. A non-power-of-two alignment fails, as in the runtime.
    unsafe { mi::mi_malloc_aligned(size, alignment) }
}

#[no_mangle]
unsafe extern "C" fn _aligned_offset_malloc(
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_malloc_aligned_at(size, alignment, offset) }
}

#[no_mangle]
unsafe extern "C" fn _aligned_realloc(
    block: *mut c_void,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_realloc_aligned(block, size, alignment) }
}

#[no_mangle]
unsafe extern "C" fn _aligned_offset_realloc(
    block: *mut c_void,
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_realloc_aligned_at(block, size, alignment, offset) }
}

#[no_mangle]
unsafe extern "C" fn _aligned_recalloc(
    block: *mut c_void,
    count: usize,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_recalloc_aligned(block, count, size, alignment) }
}

#[no_mangle]
unsafe extern "C" fn _aligned_offset_recalloc(
    block: *mut c_void,
    count: usize,
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    // SAFETY: see the module comment.
    unsafe { mi::mi_recalloc_aligned_at(block, count, size, alignment, offset) }
}

#[no_mangle]
unsafe extern "C" fn _aligned_msize(
    block: *mut c_void,
    _alignment: usize,
    _offset: usize,
) -> usize {
    // SAFETY: see the module comment.
    unsafe { mi::mi_usable_size(block) }
}

#[no_mangle]
unsafe extern "C" fn _aligned_free(block: *mut c_void) {
    // SAFETY: see the module comment.
    unsafe { mi::mi_free(block) }
}

/// Whether the C runtime's `malloc` hands out mimalloc's memory, that is, whether the link took
/// this module's definitions over the runtime's own.
pub fn heap_is_mimalloc() -> bool {
    extern "C" {
        #[link_name = "malloc"]
        fn crt_malloc(size: usize) -> *mut c_void;
        #[link_name = "free"]
        fn crt_free(block: *mut c_void);
    }
    // SAFETY: a plain allocation and its release; the pointer is only compared, never read.
    unsafe {
        let block = crt_malloc(24);
        let ours = !block.is_null() && mi::mi_is_in_heap_region(block);
        crt_free(block);
        ours
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        #[link_name = "_strdup"]
        fn crt_strdup(s: *const std::ffi::c_char) -> *mut std::ffi::c_char;
        #[link_name = "calloc"]
        fn crt_calloc(count: usize, size: usize) -> *mut c_void;
        #[link_name = "_aligned_malloc"]
        fn crt_aligned_malloc(size: usize, alignment: usize) -> *mut c_void;
        #[link_name = "free"]
        fn crt_free(block: *mut c_void);
        #[link_name = "_aligned_free"]
        fn crt_aligned_free(block: *mut c_void);
    }

    #[test]
    fn the_runtime_heap_is_mimalloc() {
        assert!(heap_is_mimalloc());
        // SAFETY: allocations through the runtime's entry points, checked and released.
        unsafe {
            // The runtime's own functions that allocate internally (through `_malloc_base`).
            let s = crt_strdup(c"snowlark".as_ptr());
            assert!(mi::mi_is_in_heap_region(s.cast()));
            crt_free(s.cast());

            let z = crt_calloc(1000, 8);
            assert!(mi::mi_is_in_heap_region(z));
            assert!(std::slice::from_raw_parts(z.cast::<u8>(), 8000)
                .iter()
                .all(|&b| b == 0));
            crt_free(z);

            let a = crt_aligned_malloc(100, 256);
            assert!(mi::mi_is_in_heap_region(a));
            assert_eq!(a as usize % 256, 0);
            crt_aligned_free(a);

            // A bad alignment fails rather than returning misaligned memory.
            assert!(crt_aligned_malloc(100, 24).is_null());

            // (the result escapes: LLVM may delete a `calloc` whose result is only compared with
            // null and take it as not null, which some codegen-unit splits of this crate do)
            let huge = crt_calloc(std::hint::black_box(usize::MAX / 2), 4);
            assert!(std::hint::black_box(huge).is_null());
        }
    }
}
